//! Socket, poll, process and provided-buffer tests of the ring layer, against the real kernel.

use std::{
    io::{Read, Write},
    mem::size_of,
    net::{SocketAddrV4, TcpListener, TcpStream, UdpSocket},
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    process::Command,
    ptr,
};

use super::{buf_ring::BufRing, tests::*, *};

fn sockaddr_in(addr: SocketAddrV4) -> libc::sockaddr_in {
    let mut sa: libc::sockaddr_in = zeroed();
    sa.sin_family = libc::AF_INET as libc::sa_family_t;
    sa.sin_port = addr.port().to_be();
    sa.sin_addr = libc::in_addr {
        s_addr: u32::from_ne_bytes(addr.ip().octets()),
    };
    sa
}

fn listener() -> (TcpListener, SocketAddrV4) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let std::net::SocketAddr::V4(addr) = l.local_addr().unwrap() else {
        unreachable!("bound to an IPv4 address")
    };
    (l, addr)
}

/// Gives every buffer of `pool` to the kernel.
fn fill(pool: &mut BufRing) {
    for bid in 0..pool.entries() {
        pool.add(bid).unwrap();
    }
    pool.commit();
}

#[test]
fn multishot_accept_serves_several_clients_until_cancelled() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    let (listener, addr) = listener();
    push(
        &mut ring,
        Sqe::accept_multishot(listener.as_raw_fd(), ptr::null_mut(), ptr::null_mut(), 0)
            .user_data(7),
    );
    ring.submit().unwrap();

    let clients: Vec<TcpStream> = (0..4u8)
        .map(|i| {
            let mut c = TcpStream::connect(addr).unwrap();
            c.write_all(&[i]).unwrap();
            c
        })
        .collect();
    let mut events = Events::default();
    let accepted = events.take(&mut ring, 4);
    for c in &accepted {
        assert_eq!(c.user_data, 7);
        assert!(c.res >= 0, "accept failed with {}", c.res);
        assert_ne!(
            c.flags & IORING_CQE_F_MORE,
            0,
            "multishot accept must stay armed"
        );
    }
    let mut got: Vec<u8> = accepted
        .iter()
        .map(|c| {
            // SAFETY: a successful accept completes with a new fd that nothing else owns.
            let mut s = unsafe { TcpStream::from_raw_fd(c.res) };
            let mut b = [0u8; 1];
            s.read_exact(&mut b).unwrap();
            b[0]
        })
        .collect();
    got.sort_unstable();
    assert_eq!(got, [0, 1, 2, 3]);

    push(&mut ring, Sqe::async_cancel(7).user_data(8));
    let mut ends = events.take(&mut ring, 2);
    ends.sort_by_key(|c| c.user_data);
    assert_eq!((ends[0].user_data, ends[0].res), (7, -libc::ECANCELED));
    assert_eq!(ends[0].flags & IORING_CQE_F_MORE, 0);
    assert_eq!((ends[1].user_data, ends[1].res), (8, 0));
    drop(clients);
}

#[test]
fn socket_connect_and_accept_run_through_the_ring() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    let (listener, addr) = listener();

    push(
        &mut ring,
        Sqe::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0, 0).user_data(1),
    );
    let fd = reap(&mut ring, 1)[0].res;
    assert!(fd >= 0, "socket failed with {fd}");
    // SAFETY: a successful socket op completes with a new fd that nothing else owns.
    let client = unsafe { OwnedFd::from_raw_fd(fd) };

    let target = sockaddr_in(addr);
    let mut peer: libc::sockaddr_storage = zeroed();
    let mut peer_len = size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    push(
        &mut ring,
        Sqe::accept(
            listener.as_raw_fd(),
            ptr::from_mut(&mut peer).cast(),
            &mut peer_len,
            0,
        )
        .user_data(2),
    );
    push(
        &mut ring,
        Sqe::connect(
            client.as_raw_fd(),
            ptr::from_ref(&target).cast(),
            size_of::<libc::sockaddr_in>() as libc::socklen_t,
        )
        .user_data(3),
    );
    let cqes = reap(&mut ring, 2);
    assert_eq!(cqes[1].res, 0, "connect failed");
    assert!(cqes[0].res >= 0, "accept failed with {}", cqes[0].res);
    // SAFETY: a successful accept completes with a new fd that nothing else owns.
    let _server_side = unsafe { OwnedFd::from_raw_fd(cqes[0].res) };

    let local = TcpStream::from(client).local_addr().unwrap();
    assert_eq!(peer_len as usize, size_of::<libc::sockaddr_in>());
    // SAFETY: `peer` is a zero-initialised sockaddr_storage, large and aligned enough for a
    // sockaddr_in, filled in by the accept.
    let peer_in = unsafe { &*ptr::from_ref(&peer).cast::<libc::sockaddr_in>() };
    assert_eq!(peer_in.sin_family as i32, libc::AF_INET);
    assert_eq!(u16::from_be(peer_in.sin_port), local.port());
}

#[test]
fn send_recv_and_shutdown_on_a_socket_pair() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    let (a, b) = socketpair();
    let mut buf = [0u8; 16];
    push(
        &mut ring,
        Sqe::recv(b.as_raw_fd(), buf.as_mut_ptr(), 16, 0).user_data(1),
    );
    push(
        &mut ring,
        Sqe::send(a.as_raw_fd(), b"ping".as_ptr(), 4, 0).user_data(2),
    );
    let cqes = reap(&mut ring, 2);
    assert_eq!((cqes[0].res, cqes[1].res), (4, 4));
    assert_eq!(&buf[..4], b"ping");

    push(
        &mut ring,
        Sqe::shutdown(a.as_raw_fd(), libc::SHUT_WR).user_data(3),
    );
    assert_eq!(reap(&mut ring, 1)[0].res, 0);
    push(
        &mut ring,
        Sqe::recv(b.as_raw_fd(), buf.as_mut_ptr(), 16, 0).user_data(4),
    );
    assert_eq!(reap(&mut ring, 1)[0].res, 0, "end of stream after shutdown");
}

#[test]
fn sendmsg_and_recvmsg_gather_and_scatter() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    let (a, b) = socketpair();
    let (p, q) = (*b"he", *b"llo");
    let mut out = [
        libc::iovec {
            iov_base: p.as_ptr() as *mut _,
            iov_len: p.len(),
        },
        libc::iovec {
            iov_base: q.as_ptr() as *mut _,
            iov_len: q.len(),
        },
    ];
    let mut send: libc::msghdr = zeroed();
    send.msg_iov = out.as_mut_ptr();
    send.msg_iovlen = out.len() as _;
    push(
        &mut ring,
        Sqe::sendmsg(a.as_raw_fd(), &send, 0).user_data(1),
    );
    assert_eq!(reap(&mut ring, 1)[0].res, 5);

    let mut buf = [0u8; 16];
    let mut inp = [libc::iovec {
        iov_base: buf.as_mut_ptr().cast(),
        iov_len: buf.len(),
    }];
    let mut recv: libc::msghdr = zeroed();
    recv.msg_iov = inp.as_mut_ptr();
    recv.msg_iovlen = 1;
    push(
        &mut ring,
        Sqe::recvmsg(b.as_raw_fd(), &mut recv, 0).user_data(2),
    );
    assert_eq!(reap(&mut ring, 1)[0].res, 5);
    assert_eq!(&buf[..5], b"hello");
}

#[test]
fn multishot_recv_recycles_buffers_and_ends_when_the_pool_is_empty() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    let Some(mut pool) = buf_ring_or_skip(&ring, 7, 4, 64) else {
        return;
    };
    fill(&mut pool);
    let (a, b) = socketpair();
    push(
        &mut ring,
        Sqe::recv_multishot(b.as_raw_fd(), 7, 0).user_data(5),
    );
    ring.submit().unwrap();

    let mut events = Events::default();
    let mut used = Vec::new();
    for msg in [&b"aaaa"[..], b"bb", b"cccccc", b"d"] {
        write_fd(a.as_raw_fd(), msg);
        let c = events.next(&mut ring);
        assert_eq!((c.user_data, c.res), (5, msg.len() as i32));
        assert_eq!(
            c.flags & (IORING_CQE_F_MORE | IORING_CQE_F_BUFFER),
            IORING_CQE_F_MORE | IORING_CQE_F_BUFFER
        );
        let bid = BufRing::buffer_id(c.flags).expect("a provided buffer was used");
        assert_eq!(pool.buffer(bid, c.res as u32).unwrap(), msg);
        used.push(bid);
    }
    used.sort_unstable();
    assert_eq!(
        used,
        [0, 1, 2, 3],
        "four receives took four distinct buffers"
    );

    // The pool is dry: the next arrival ends the multishot, without MORE, and stays unread.
    write_fd(a.as_raw_fd(), b"eeeee");
    let end = events.next(&mut ring);
    assert_eq!((end.user_data, end.res), (5, -libc::ENOBUFS));
    assert_eq!(end.flags & IORING_CQE_F_MORE, 0);

    // Recycle (the ring index wraps: tail 4 -> 8) and re-arm; the kernel polls first and finds the data.
    for bid in used {
        pool.add(bid).unwrap();
    }
    pool.commit();
    assert_eq!(pool.published_tail(), 8);
    push(
        &mut ring,
        Sqe::recv_multishot(b.as_raw_fd(), 7, 0)
            .ioprio(IORING_RECVSEND_POLL_FIRST)
            .user_data(6),
    );
    let c = events.next(&mut ring);
    assert_eq!((c.user_data, c.res), (6, 5));
    assert_ne!(c.flags & IORING_CQE_F_MORE, 0);
    let bid = BufRing::buffer_id(c.flags).unwrap();
    assert_eq!(pool.buffer(bid, 5).unwrap(), b"eeeee");

    push(&mut ring, Sqe::async_cancel(6).user_data(7));
    let mut ends = events.take(&mut ring, 2);
    ends.sort_by_key(|c| c.user_data);
    assert_eq!((ends[0].user_data, ends[0].res), (6, -libc::ECANCELED));
    assert_eq!((ends[1].user_data, ends[1].res), (7, 0));
}

#[test]
fn one_shot_recv_selects_a_buffer_from_the_group() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    let Some(mut pool) = buf_ring_or_skip(&ring, 4, 2, 32) else {
        return;
    };
    fill(&mut pool);
    let (a, b) = socketpair();
    write_fd(a.as_raw_fd(), b"xyz");
    push(
        &mut ring,
        Sqe::recv_select(b.as_raw_fd(), 4, 0, 0).user_data(1),
    );
    let c = reap(&mut ring, 1)[0];
    assert_eq!(c.res, 3);
    assert_ne!(c.flags & IORING_CQE_F_BUFFER, 0);
    assert_eq!(c.flags & IORING_CQE_F_MORE, 0);
    let bid = BufRing::buffer_id(c.flags).unwrap();
    assert_eq!(pool.buffer(bid, 3).unwrap(), b"xyz");
}

#[test]
fn multishot_recvmsg_on_udp_carries_source_address_and_payload() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let server_addr = server.local_addr().unwrap();
    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
    let client_port = client.local_addr().unwrap().port();

    let Some(mut pool) = buf_ring_or_skip(&ring, 3, 4, 256) else {
        return;
    };
    fill(&mut pool);
    let mut msg: libc::msghdr = zeroed();
    msg.msg_namelen = size_of::<libc::sockaddr_in>() as _;
    push(
        &mut ring,
        Sqe::recvmsg_multishot(server.as_raw_fd(), &msg, 3, 0).user_data(4),
    );
    ring.submit().unwrap();

    let mut events = Events::default();
    for payload in [&b"hello udp"[..], b"second datagram"] {
        client.send_to(payload, server_addr).unwrap();
        let c = events.next(&mut ring);
        assert_eq!(c.user_data, 4);
        assert!(c.res > 0, "recvmsg failed with {}", c.res);
        assert_eq!(
            c.flags & (IORING_CQE_F_MORE | IORING_CQE_F_BUFFER),
            IORING_CQE_F_MORE | IORING_CQE_F_BUFFER
        );
        let bid = BufRing::buffer_id(c.flags).unwrap();
        let buf = pool.buffer(bid, c.res as u32).unwrap();
        let parts =
            IoUringRecvmsgOut::parse(buf, msg.msg_namelen, msg.msg_controllen as u32).unwrap();
        assert_eq!(parts.payload, payload);
        assert_eq!(parts.header.payloadlen as usize, payload.len());
        let name = parts.name;
        assert_eq!(name.len(), size_of::<libc::sockaddr_in>());
        assert_eq!(u16::from_ne_bytes([name[0], name[1]]), libc::AF_INET as u16);
        assert_eq!(u16::from_be_bytes([name[2], name[3]]), client_port);
        assert_eq!(&name[4..8], &[127, 0, 0, 1]);
        pool.add(bid).unwrap();
        pool.commit();
    }

    push(&mut ring, Sqe::async_cancel(4).user_data(5));
    let mut ends = events.take(&mut ring, 2);
    ends.sort_by_key(|c| c.user_data);
    assert_eq!((ends[0].user_data, ends[0].res), (4, -libc::ECANCELED));
    assert_eq!((ends[1].user_data, ends[1].res), (5, 0));
}

fn eventfd() -> OwnedFd {
    // SAFETY: eventfd takes no pointers.
    let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    assert!(fd >= 0);
    // SAFETY: eventfd returned a fresh descriptor that nothing else owns.
    unsafe { OwnedFd::from_raw_fd(fd) }
}

fn drain(fd: RawFd) -> u64 {
    let mut v = [0u8; 8];
    // SAFETY: `v` is a valid 8-byte buffer, the size an eventfd read needs.
    let n = unsafe { libc::read(fd, v.as_mut_ptr().cast(), 8) };
    assert_eq!(n, 8);
    u64::from_ne_bytes(v)
}

#[test]
fn multishot_poll_on_an_eventfd_fires_per_event_until_removed() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    let efd = eventfd();
    let fd = efd.as_raw_fd();
    push(
        &mut ring,
        Sqe::poll_add_multi(fd, libc::POLLIN as u32).user_data(9),
    );
    ring.submit().unwrap();

    let mut events = Events::default();
    for round in 1..=2u64 {
        write_fd(fd, &round.to_ne_bytes());
        let c = events.next(&mut ring);
        assert_eq!(c.user_data, 9);
        assert_ne!(c.res as u32 & libc::POLLIN as u32, 0, "{c:?}");
        assert_ne!(
            c.flags & IORING_CQE_F_MORE,
            0,
            "multishot poll must stay armed"
        );
        assert_eq!(drain(fd), round);
    }

    push(&mut ring, Sqe::poll_remove(9).user_data(10));
    let mut ends = events.take(&mut ring, 2);
    ends.sort_by_key(|c| c.user_data);
    assert_eq!((ends[0].user_data, ends[0].res), (9, -libc::ECANCELED));
    assert_eq!(ends[0].flags & IORING_CQE_F_MORE, 0);
    assert_eq!((ends[1].user_data, ends[1].res), (10, 0));

    // One-shot poll: already readable, completes at once and does not stay armed.
    write_fd(fd, &1u64.to_ne_bytes());
    push(
        &mut ring,
        Sqe::poll_add(fd, libc::POLLIN as u32).user_data(11),
    );
    let c = events.next(&mut ring);
    assert_eq!(c.user_data, 11);
    assert_ne!(c.res as u32 & libc::POLLIN as u32, 0);
    assert_eq!(c.flags & IORING_CQE_F_MORE, 0);
}

#[test]
fn waitid_reports_a_child_exit() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    if !ring.register_probe().unwrap().contains(Opcode::Waitid) {
        eprintln!("skipping: this kernel has no waitid operation (needs Linux 6.7)");
        return;
    }
    let mut child = Command::new("true").spawn().unwrap();
    let pid = child.id();
    let mut info: libc::siginfo_t = zeroed();
    // WNOWAIT leaves the zombie for `Child::wait` to reap.
    let options = (libc::WEXITED | libc::WNOWAIT) as u32;
    push(
        &mut ring,
        Sqe::waitid(libc::P_PID, pid, &mut info, options).user_data(1),
    );
    assert_eq!(reap(&mut ring, 1)[0].res, 0);
    assert_eq!(info.si_code, libc::CLD_EXITED);
    // SAFETY: a CLD_EXITED siginfo from waitid has the child fields set.
    assert_eq!(unsafe { info.si_pid() }, pid as i32);
    assert!(child.wait().unwrap().success());
}

// ---------------------------------------------------------------------------------------------
// Provided buffer rings.

#[test]
fn buffer_ring_rejects_bad_shapes() {
    let Some(ring) = ring_or_skip() else { return };
    for entries in [0, 3, 6, 65535, 32769] {
        let err = BufRing::new(&ring, 1, entries, 16).unwrap_err();
        assert_eq!(
            err.kind(),
            std::io::ErrorKind::InvalidInput,
            "{entries} entries"
        );
    }
    assert_eq!(
        BufRing::new(&ring, 1, 4, 0).unwrap_err().kind(),
        std::io::ErrorKind::InvalidInput
    );
    // Past nuclei's own checks, only a kernel without buffer rings refuses it.
    if buf_ring_or_skip(&ring, 2, 4, 16).is_some() {
        BufRing::new(&ring, 1, 32768, 1).expect("the largest ring is accepted");
    }
}

#[test]
fn buffer_ring_add_and_buffer_check_their_bounds() {
    let Some(ring) = ring_or_skip() else { return };
    let Some(mut pool) = buf_ring_or_skip(&ring, 1, 4, 16) else {
        return;
    };
    assert_eq!((pool.bgid(), pool.entries(), pool.buf_size()), (1, 4, 16));
    assert!(pool.add(4).is_err(), "bid 4 is outside a 4-buffer pool");
    for bid in 0..4 {
        pool.add(bid).unwrap();
    }
    assert!(
        pool.add(0).is_err(),
        "a fifth uncommitted descriptor does not fit"
    );
    pool.commit();
    pool.add(0).unwrap();

    assert_eq!(pool.buffer(3, 16).unwrap().len(), 16);
    assert_eq!(pool.buffer(0, 0).unwrap().len(), 0);
    assert!(pool.buffer(4, 1).is_none());
    assert!(pool.buffer(0, 17).is_none());
    assert!(
        pool.buffer(0, 16).unwrap().iter().all(|&b| b == 0),
        "buffers start zeroed"
    );
}

#[test]
fn buffer_ring_writes_descriptors_and_keeps_the_tail_intact() {
    let Some(ring) = ring_or_skip() else { return };
    let Some(mut pool) = buf_ring_or_skip(&ring, 2, 4, 32) else {
        return;
    };
    let base = pool.buffer(0, 1).unwrap().as_ptr() as u64;
    pool.add(2).unwrap();
    pool.add(0).unwrap();
    assert_eq!(
        pool.published_tail(),
        0,
        "nothing is visible before the commit"
    );
    pool.commit();
    assert_eq!(pool.published_tail(), 2);
    let d = pool.descriptor(0);
    assert_eq!((d.addr, d.len, d.bid), (base + 64, 32, 2));
    let d = pool.descriptor(1);
    assert_eq!((d.addr, d.len, d.bid), (base, 32, 0));

    // Second lap: the third descriptor wraps onto slot 0, whose reserved field is the tail itself.
    pool.add(1).unwrap();
    pool.add(3).unwrap();
    pool.add(0).unwrap();
    assert_eq!(
        pool.published_tail(),
        2,
        "writing slot 0 must not touch the tail"
    );
    pool.commit();
    assert_eq!(pool.published_tail(), 5);
    let bids: Vec<u16> = (0..4).map(|slot| pool.descriptor(slot).bid).collect();
    assert_eq!(bids, [0, 0, 1, 3]);
    assert_eq!(pool.descriptor(0).resv, 5, "the tail overlays descriptor 0");
}

#[test]
fn buffer_id_decodes_the_completion_flags() {
    assert_eq!(BufRing::buffer_id(0), None);
    assert_eq!(BufRing::buffer_id(IORING_CQE_F_MORE), None);
    assert_eq!(
        BufRing::buffer_id(IORING_CQE_F_BUFFER | (5 << IORING_CQE_BUFFER_SHIFT)),
        Some(5)
    );
    assert_eq!(
        BufRing::buffer_id(IORING_CQE_F_BUFFER | IORING_CQE_F_MORE | (0xffff << 16)),
        Some(0xffff)
    );
    assert_eq!(
        BufRing::buffer_id(7 << 16),
        None,
        "a buffer id without the buffer flag means nothing"
    );
}

#[test]
fn dropping_a_buffer_ring_unregisters_its_group() {
    let Some(ring) = ring_or_skip() else { return };
    let Some(first) = buf_ring_or_skip(&ring, 9, 2, 16) else {
        return;
    };
    let clash = BufRing::new(&ring, 9, 2, 16).unwrap_err();
    assert_eq!(clash.raw_os_error(), Some(libc::EEXIST));
    drop(first);
    BufRing::new(&ring, 9, 2, 16).expect("the group id is free again after the drop");
}

#[test]
fn a_buffer_ring_outlives_its_ring() {
    let Some(ring) = ring_or_skip() else { return };
    let Some(pool) = buf_ring_or_skip(&ring, 1, 2, 16) else {
        return;
    };
    drop(ring);
    assert_eq!(pool.buffer(1, 16).unwrap().len(), 16);
    drop(pool);
}

// ---------------------------------------------------------------------------------------------
// Multishot recvmsg layout.

fn recvmsg_buffer(
    name: &[u8],
    control: &[u8],
    payload: &[u8],
    namelen: u32,
    controllen: u32,
) -> Vec<u8> {
    let mut b = Vec::new();
    for w in [
        name.len() as u32,
        control.len() as u32,
        payload.len() as u32,
        0,
    ] {
        b.extend_from_slice(&w.to_ne_bytes());
    }
    b.extend_from_slice(name);
    b.resize(16 + namelen as usize, 0);
    b.extend_from_slice(control);
    b.resize(16 + (namelen + controllen) as usize, 0);
    b.extend_from_slice(payload);
    b
}

#[test]
fn recvmsg_out_splits_name_control_and_payload() {
    let buf = recvmsg_buffer(b"NAME", b"CTL", b"payload", 16, 8);
    let p = IoUringRecvmsgOut::parse(&buf, 16, 8).unwrap();
    assert_eq!(
        (p.header.namelen, p.header.controllen, p.header.payloadlen),
        (4, 3, 7)
    );
    assert_eq!(
        (p.name, p.control, p.payload),
        (&b"NAME"[..], &b"CTL"[..], &b"payload"[..])
    );
}

#[test]
fn recvmsg_out_rejects_short_buffers_and_clamps_claims() {
    assert_eq!(IoUringRecvmsgOut::parse(&[0; 15], 0, 0), None);
    let buf = recvmsg_buffer(b"NAME", b"", b"xy", 16, 0);
    assert_eq!(IoUringRecvmsgOut::parse(&buf[..20], 16, 0), None);
    // A header claiming a longer name than the msghdr reserved is cut to the reserved size.
    let mut lying = recvmsg_buffer(b"NAMENAME", b"", b"xy", 4, 0);
    lying[..4].copy_from_slice(&99u32.to_ne_bytes());
    let p = IoUringRecvmsgOut::parse(&lying, 4, 0).unwrap();
    assert_eq!(p.name.len(), 4);
    // A payload shorter than the header claims is cut to what the buffer holds.
    let mut short = recvmsg_buffer(b"", b"", b"xy", 0, 0);
    short[8..12].copy_from_slice(&50u32.to_ne_bytes());
    assert_eq!(
        IoUringRecvmsgOut::parse(&short, 0, 0).unwrap().payload,
        b"xy"
    );
}

/// A fixed file keeps the socket it was given: once the descriptor it came from is closed, and
/// its number taken by another socket, a read on the slot still reads the first socket.
#[test]
fn a_fixed_file_outlives_the_descriptor_it_came_from() {
    let Some(mut ring) = ring_or_skip() else {
        return;
    };
    ring.register_files_sparse(8).unwrap();
    let (mine, peer) = socketpair();
    ring.update_file(3, mine.as_raw_fd()).unwrap();
    // The descriptor closes and its number names another socket, in one atomic step (so no
    // other thread of the test process can take the number in between).
    let number = std::os::fd::IntoRawFd::into_raw_fd(mine);
    let (other, other_peer) = socketpair();
    // SAFETY: `number` is this test's own descriptor; dup2 replaces it atomically.
    assert_eq!(unsafe { libc::dup2(other.as_raw_fd(), number) }, number);
    // SAFETY: `number` is this test's own descriptor.
    let reused = unsafe { OwnedFd::from_raw_fd(number) };
    write_fd(peer.as_raw_fd(), b"first");
    write_fd(other_peer.as_raw_fd(), b"other");
    let mut buf = [0u8; 16];
    push(
        &mut ring,
        Sqe::recv(3, buf.as_mut_ptr(), buf.len() as u32, 0)
            .flags(IOSQE_FIXED_FILE)
            .user_data(7),
    );
    let cqe = reap(&mut ring, 1)[0];
    assert_eq!(cqe.user_data, 7);
    assert_eq!(cqe.res, 5, "{cqe:?}");
    assert_eq!(
        &buf[..5],
        b"first",
        "the slot read the socket that took the number"
    );
    // Emptied, the slot names nothing.
    ring.update_file(3, -1).unwrap();
    push(
        &mut ring,
        Sqe::recv(3, buf.as_mut_ptr(), buf.len() as u32, 0)
            .flags(IOSQE_FIXED_FILE)
            .user_data(8),
    );
    assert_eq!(reap(&mut ring, 1)[0].res, -libc::EBADF);
    drop((reused, other, other_peer, peer));
}
