//! A process with no descriptor left (`EMFILE` under a lowered
//! `RLIMIT_NOFILE`): a call that needs a new descriptor (a connect, a socket,
//! a file opened) fails as std's does, a call on a descriptor already open (a
//! read, a write, a file's IO, a handle dropped with bytes to send) works as
//! std's does, and once descriptors are free every call works again. Each
//! case runs alone in a process of its own, for the limit stays lowered.
//! The accept cases are in src/net/tests/emfile.rs; the listener's stream of
//! connections is here.

use std::fs::File;
use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::unix::net::UnixStream as StdUnixStream;

use nuclei::net::{TcpListener, TcpStream, UdpSocket, UnixDatagram, UnixListener, UnixStream};

use crate::reset::{READERS, WRITERS, write_as};
use crate::support::*;

/// The limit lowered to what is open plus some (room for the cases that run
/// once they are free: on a ring without a table of files each socket takes
/// two), and those used up; `free` gives them back.
struct Starved {
    held: Vec<File>,
}

impl Starved {
    fn new() -> Self {
        let highest = std::fs::read_dir("/proc/self/fd")
            .or_else(|_| std::fs::read_dir("/dev/fd"))
            .unwrap()
            .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<libc::rlim_t>().ok())
            .max()
            .unwrap_or(0);
        let limit = libc::rlimit {
            rlim_cur: highest + 32,
            rlim_max: highest + 32,
        };
        // SAFETY: lowers this process's own limit with a valid struct.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
        let mut held = Vec::new();
        while let Ok(null) = File::open("/dev/null") {
            held.push(null);
        }
        Starved { held }
    }

    /// Takes the descriptors that came free since: the runtime closes a
    /// dropped handle's descriptor once its driver lets go of it, which may
    /// be after the limit fell. Says whether it took any.
    fn tighten(&mut self) -> bool {
        let before = self.held.len();
        while let Ok(null) = File::open("/dev/null") {
            self.held.push(null);
        }
        self.held.len() > before
    }

    /// Takes descriptors as they come free until none has for a few looks
    /// in a row, the runtime running meanwhile: handles dropped before the
    /// limit fell may close a little after (on a ring, once its thread
    /// handles their cancels).
    async fn settle(&mut self) {
        let mut quiet = 0;
        let deadline = std::time::Instant::now() + DEADLINE;
        while quiet < 5 && std::time::Instant::now() < deadline {
            nuclei::time::sleep(std::time::Duration::from_millis(20)).await;
            quiet = if self.tighten() { 0 } else { quiet + 1 };
        }
    }

    fn free(&mut self) {
        self.held.clear();
    }
}

fn out_of_descriptors(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(libc::EMFILE | libc::ENFILE))
}

/// The connections of a backlog the first accept to find no descriptor
/// closes: on macOS it takes one and drops it (XNU's `accept_nocancel`,
/// bsd/kern/uipc_syscalls.c), elsewhere it leaves the backlog whole; the
/// accepts after it take none until a descriptor is free.
const CLOSED: usize = if cfg!(target_vendor = "apple") { 1 } else { 0 };

/// How many of `clients` the listener's side closed: a closed one reads the
/// end or a reset, a served one has nothing to read yet.
fn closed(clients: &[impl std::os::fd::AsRawFd]) -> usize {
    clients
        .iter()
        .filter(|client| {
            let mut byte = 0u8;
            // SAFETY: a peek of at most one byte into our own byte, on a
            // descriptor the caller keeps open.
            let n = unsafe {
                libc::recv(
                    client.as_raw_fd(),
                    (&raw mut byte).cast(),
                    1,
                    libc::MSG_PEEK | libc::MSG_DONTWAIT,
                )
            };
            n == 0 || (n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::WouldBlock)
        })
        .count()
}

fn run(name: &str, case: impl AsyncFnOnce()) {
    if alone(name) {
        let rt = runtime();
        rt.block_on(async {
            // The runtime's own descriptors are made before the limit falls.
            round_trip().await;
            case().await;
        });
        rt.block_on(round_trip());
    }
}

#[test]
fn new_sockets_and_connects_fail_as_std_and_work_once_descriptors_are_free() {
    run(
        "emfile::new_sockets_and_connects_fail_as_std_and_work_once_descriptors_are_free",
        async || {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let path = std::env::temp_dir()
                .join(format!("nuclei-faults-emfile-{}.sock", std::process::id()));
            let _ = std::fs::remove_file(&path);
            let unix_listener = UnixListener::bind(&path).unwrap();
            let mut starved = Starved::new();
            starved.settle().await;
            let std_tcp = std::net::TcpStream::connect(addr).map(drop);
            let std_unix = StdUnixStream::connect(&path).map(drop);
            assert!(
                std_tcp.as_ref().is_err_and(out_of_descriptors),
                "{std_tcp:?}"
            );
            let want = Answer::of(&std_tcp, |_| 0);
            for what in [
                "a TCP connect",
                "a Unix connect",
                "a UDP bind",
                "a TCP bind",
                "a Unix stream pair",
                "a Unix datagram pair",
                "a TCP socket",
                "a file open",
            ] {
                starved.tighten();
                let got = match what {
                    "a TCP connect" => within(what, TcpStream::connect(addr)).await.map(drop),
                    "a Unix connect" => within(what, UnixStream::connect(&path)).await.map(drop),
                    "a UDP bind" => within(what, UdpSocket::bind("127.0.0.1:0")).await.map(drop),
                    "a TCP bind" => within(what, TcpListener::bind("127.0.0.1:0"))
                        .await
                        .map(drop),
                    "a Unix stream pair" => UnixStream::pair().map(drop),
                    "a Unix datagram pair" => UnixDatagram::pair().map(drop),
                    "a TCP socket" => nuclei::net::TcpSocket::new_v4().map(drop),
                    _ => within(what, nuclei::fs::File::open("/dev/null"))
                        .await
                        .map(drop),
                };
                assert_eq!(
                    Answer::of(&got, |_| 0),
                    want,
                    "{what} with no descriptor left"
                );
            }
            assert_eq!(Answer::of(&std_unix, |_| 0), want);
            starved.free();
            let client = within("connect", TcpStream::connect(addr)).await.unwrap();
            let (server, _) = within("accept", listener.accept()).await.unwrap();
            write_all(&client, b"back").await.unwrap();
            assert_eq!(read_exact(&server, 4).await.unwrap(), b"back");
            let unix = within("connect", UnixStream::connect(&path)).await.unwrap();
            let (peer, _) = within("accept", unix_listener.accept()).await.unwrap();
            write_all(&unix, b"back").await.unwrap();
            assert_eq!(read_exact(&peer, 4).await.unwrap(), b"back");
            UdpSocket::bind("127.0.0.1:0").await.unwrap();
            nuclei::fs::File::open("/dev/null").await.unwrap();
            let _ = std::fs::remove_file(&path);
        },
    );
}

/// Streams open before the limit fell read and write every way, as std's
/// do with no descriptor to spare.
async fn streams_open_before<S: Stream>() {
    for how in READERS {
        for writer in WRITERS {
            // The send_file case opens the file first.
            let bytes = pattern(21, 100_000);
            let file = TempFile::with("emfile-send", &bytes);
            let opened = nuclei::fs::File::open(&file.0).await.unwrap();
            let (ours, mut peer) = S::pair(false);
            // A first call made before the limit falls; one made after it is
            // `a_sockets_first_call_with_no_descriptor_left_works_as_std`.
            within("a first write", write_all(&ours, b"ping"))
                .await
                .unwrap();
            let mut four = [0u8; 4];
            peer.read_exact(&mut four).unwrap();
            let (mut std_ours, mut std_peer) = S::std_pair(false);
            let mut starved = Starved::new();
            std_ours.write_all(b"ping").unwrap();
            let mut four = [0u8; 4];
            std_peer.read_exact(&mut four).unwrap();
            let what = format!(
                "{}: no descriptor left, {how:?} reads and a {writer:?} write",
                S::NAME
            );
            let reader = std::thread::spawn(move || {
                let mut peer = peer;
                let mut got = vec![0u8; 100_000];
                peer.read_exact(&mut got).map(|()| (peer, got))
            });
            let wrote = match writer {
                crate::reset::Writer::SendFile => {
                    within(&what, async {
                        let sent = ours.send_file(&opened, 0, bytes.len() as u64).await?;
                        assert_eq!(sent, bytes.len() as u64);
                        Ok(())
                    })
                    .await
                }
                other => within(&what, write_as(&ours, other, &bytes)).await,
            };
            assert!(wrote.is_ok(), "{what}: the write: {wrote:?}");
            let (mut peer, got) = joined(reader).await.unwrap();
            same_bytes(&what, &got, &bytes);
            peer.write_all(&bytes).unwrap();
            S::shutdown_std(&peer, Shutdown::Write).unwrap();
            let (back, end) = within(&what, crate::reset::read_to_end_as(&ours, how, 8192)).await;
            same_bytes(&format!("{what}: the reads"), &back, &bytes);
            assert_eq!(end, Answer::Ok(0), "{what}: the end");
            starved.free();
            drop(std_peer);
        }
    }
}

#[test]
fn a_tcp_stream_open_before_reads_and_writes_with_no_descriptor_left() {
    run(
        "emfile::a_tcp_stream_open_before_reads_and_writes_with_no_descriptor_left",
        streams_open_before::<TcpStream>,
    );
}

#[test]
fn a_unix_stream_open_before_reads_and_writes_with_no_descriptor_left() {
    run(
        "emfile::a_unix_stream_open_before_reads_and_writes_with_no_descriptor_left",
        streams_open_before::<UnixStream>,
    );
}

#[test]
fn datagram_sockets_open_before_send_and_receive_with_no_descriptor_left() {
    run(
        "emfile::datagram_sockets_open_before_send_and_receive_with_no_descriptor_left",
        async || {
            let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let (ua, ub) = UnixDatagram::pair().unwrap();
            let to = b.local_addr().unwrap();
            // Each socket registers with a send of its own before the limit
            // falls; the first receive from the pair's unnamed end comes
            // after, so naming that sender is tried with no descriptor left.
            let back = a.local_addr().unwrap();
            within("a first send", a.send_to(b"warm", to))
                .await
                .unwrap();
            within("a first send", b.send_to(b"warm", back))
                .await
                .unwrap();
            within("a first send", ua.send(b"warm")).await.unwrap();
            within("a first send", ub.send(b"warm")).await.unwrap();
            let mut warm = [0u8; 8];
            within("the first datagram", b.recv_from(&mut warm))
                .await
                .unwrap();
            let mut starved = Starved::new();
            // The first datagram from the pair's unnamed end, received with no
            // descriptor left.
            let (n, first) = within(
                "the first Unix datagram",
                ub.recv_owned(Vec::with_capacity(8)),
            )
            .await;
            assert_eq!(
                n.ok(),
                Some(4),
                "the first Unix datagram with no descriptor left"
            );
            assert_eq!(first, b"warm");
            for owned in [false, true] {
                let what = format!("datagrams with no descriptor left, owned {owned}");
                let sent = if owned {
                    within(&what, a.send_to_owned(b"udp".to_vec(), to)).await.0
                } else {
                    within(&what, a.send_to(b"udp", to)).await
                };
                assert_eq!(sent.unwrap(), 3, "{what}");
                let mut buf = [0u8; 8];
                let (n, _) = within(&what, b.recv_from(&mut buf)).await.unwrap();
                assert_eq!(&buf[..n], b"udp", "{what}");
                let sent = if owned {
                    within(&what, ua.send_owned(b"unix".to_vec())).await.0
                } else {
                    within(&what, ua.send(b"unix")).await
                };
                assert_eq!(sent.unwrap(), 4, "{what}");
                let (n, buf) = within(&what, ub.recv_owned(Vec::with_capacity(8))).await;
                assert_eq!(n.unwrap(), 4, "{what}");
                assert_eq!(buf, b"unix", "{what}");
            }
            starved.free();
        },
    );
}

/// A handle dropped with bytes still to send: they reach the peer, as std's
/// do when its socket closes with bytes in the kernel's buffer. The drain
/// goes on with the handle's own object, which takes no descriptor.
#[test]
fn a_handle_dropped_with_bytes_to_send_delivers_them_with_no_descriptor_left() {
    run(
        "emfile::a_handle_dropped_with_bytes_to_send_delivers_them_with_no_descriptor_left",
        async || {
            for tiny in [false, true] {
                let bytes = pattern(41, 2 << 20);
                let (ours, mut peer) = TcpStream::pair(tiny);
                within("a first write", write_all(&ours, b"ping"))
                    .await
                    .unwrap();
                let mut four = [0u8; 4];
                peer.read_exact(&mut four).unwrap();
                let mut starved = Starved::new();
                let what = format!("a handle dropped with bytes behind, tiny {tiny}");
                // As much as the write-behind takes, whatever went out.
                let mut at = 0;
                while at < bytes.len() {
                    match ours.try_write(&bytes[at..]) {
                        Ok(n) => at += n,
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                        Err(err) => panic!("{what}: {err}"),
                    }
                }
                drop(ours);
                let reader = std::thread::spawn(move || {
                    let mut peer = peer;
                    std_read_until_end(&mut peer, 65_536)
                });
                let (got, end) = joined(reader).await;
                starved.free();
                assert!(end.is_ok(), "{what}: the peer's end: {end:?}");
                same_bytes(&what, &got, &bytes[..at]);
            }
        },
    );
}

/// A listener's streams of connections with no descriptor left: `incoming`
/// answers the error and goes on; both serve the backlog once descriptors
/// are free, less the connection macOS's accept closed (`CLOSED`).
#[test]
fn incoming_connections_answer_the_error_and_serve_the_backlog_after() {
    run(
        "emfile::incoming_connections_answer_the_error_and_serve_the_backlog_after",
        async || {
            use futures_core::Stream as _;
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let path =
                std::env::temp_dir().join(format!("nuclei-faults-inc-{}.sock", std::process::id()));
            let _ = std::fs::remove_file(&path);
            let unix = UnixListener::bind(&path).unwrap();
            const CLIENTS: usize = 3;
            let clients: Vec<_> = (0..CLIENTS)
                .map(|_| std::net::TcpStream::connect(addr).unwrap())
                .collect();
            let unix_clients: Vec<_> = (0..CLIENTS)
                .map(|_| StdUnixStream::connect(&path).unwrap())
                .collect();
            let mut starved = Starved::new();
            let mut tcp_in = listener.incoming();
            let mut unix_in = unix.incoming();
            let first = within(
                "incoming",
                core::future::poll_fn(|cx| core::pin::Pin::new(&mut tcp_in).poll_next(cx)),
            )
            .await;
            assert!(first.unwrap().is_err_and(|e| out_of_descriptors(&e)));
            let first = within(
                "incoming",
                core::future::poll_fn(|cx| core::pin::Pin::new(&mut unix_in).poll_next(cx)),
            )
            .await;
            assert!(first.unwrap().is_err_and(|e| out_of_descriptors(&e)));
            starved.free();
            let mut served = (Vec::new(), Vec::new());
            for _ in 0..CLIENTS - CLOSED {
                let next = within(
                    "incoming",
                    core::future::poll_fn(|cx| core::pin::Pin::new(&mut tcp_in).poll_next(cx)),
                )
                .await;
                served.0.push(next.unwrap().unwrap());
                let next = within(
                    "incoming",
                    core::future::poll_fn(|cx| core::pin::Pin::new(&mut unix_in).poll_next(cx)),
                )
                .await;
                served.1.push(next.unwrap().unwrap());
            }
            assert_eq!(closed(&clients), CLOSED, "the TCP connections closed");
            assert_eq!(closed(&unix_clients), CLOSED, "the Unix connections closed");
            drop((served, clients, unix_clients));
            let _ = std::fs::remove_file(&path);
        },
    );
}

/// A runtime built with no descriptor left fails with the reason, and one
/// built once they are free runs; the running one keeps working throughout.
#[test]
fn a_runtime_built_with_no_descriptor_left_fails_and_one_after_runs() {
    if !alone("emfile::a_runtime_built_with_no_descriptor_left_fails_and_one_after_runs") {
        return;
    }
    let rt = runtime();
    rt.block_on(round_trip());
    let mut starved = Starved::new();
    let built = nuclei::Runtime::builder().worker_threads(1).build();
    assert!(
        built.as_ref().is_err_and(out_of_descriptors),
        "a runtime with no descriptor left: {:?}",
        built.as_ref().map(drop)
    );
    assert_eq!(rt.block_on(rt.spawn_blocking(|| 7)), 7);
    rt.block_on(nuclei::time::sleep(std::time::Duration::from_millis(1)));
    starved.free();
    let other = nuclei::Runtime::builder()
        .worker_threads(1)
        .build()
        .unwrap();
    other.block_on(round_trip());
    rt.block_on(round_trip());
}

/// A file opened before the process ran out of descriptors reads, writes
/// and appends as std's does: its calls run on its own descriptor, and none
/// needs a new one.
#[test]
fn a_file_open_before_reads_writes_and_appends_with_no_descriptor_left() {
    run(
        "emfile::a_file_open_before_reads_writes_and_appends_with_no_descriptor_left",
        async || {
            use nuclei::io::{AsyncReadExt, AsyncWriteExt};
            let bytes = pattern(31, 200_000);
            let file = TempFile::with("emfile-file", &bytes);
            let log = TempFile::with("emfile-log", b"");
            let mut reading = nuclei::fs::File::open(&file.0).await.unwrap();
            let owned = nuclei::fs::File::open(&file.0).await.unwrap();
            let mut writing = nuclei::fs::OpenOptions::new()
                .write(true)
                .open(&file.0)
                .await
                .unwrap();
            let mut appending = nuclei::fs::OpenOptions::new()
                .append(true)
                .open(&log.0)
                .await
                .unwrap();
            let mut starved = Starved::new();
            starved.settle().await;
            assert!(
                File::open(&file.0).is_err_and(|e| out_of_descriptors(&e)),
                "the limit left a descriptor to open with"
            );
            let mut got = Vec::new();
            let read = within("a read", reading.read_to_end(&mut got)).await;
            assert!(read.is_ok(), "a read with no descriptor left: {read:?}");
            same_bytes("a read with no descriptor left", &got, &bytes);
            let (n, buf) =
                within("an owned read", owned.read_owned(Vec::with_capacity(1000))).await;
            assert!(n.is_ok(), "an owned read with no descriptor left: {n:?}");
            same_bytes("an owned read", &buf, &bytes[..1000]);
            let wrote = within("a write", async {
                writing.write_all(&bytes[..5000]).await?;
                writing.flush().await
            })
            .await;
            assert!(wrote.is_ok(), "a write with no descriptor left: {wrote:?}");
            let (n, _) = within(
                "an owned write",
                writing.write_owned(bytes[..7000].to_vec()),
            )
            .await;
            assert!(n.is_ok(), "an owned write with no descriptor left: {n:?}");
            let synced = within("a sync", writing.sync_all()).await;
            assert!(synced.is_ok(), "a sync with no descriptor left: {synced:?}");
            let appended = within("an append", async {
                appending.write_all(b"appended").await?;
                appending.flush().await
            })
            .await;
            assert!(
                appended.is_ok(),
                "an append with no descriptor left: {appended:?}"
            );
            starved.free();
            assert_eq!(std::fs::read(&log.0).unwrap(), b"appended");
        },
    );
}

/// `send_file` with no descriptor left sends the range, as `sendfile(2)` on
/// the same socket and file does.
#[test]
fn send_file_with_no_descriptor_left_sends_the_range() {
    run(
        "emfile::send_file_with_no_descriptor_left_sends_the_range",
        async || {
            let bytes = pattern(32, 100_000);
            let file = TempFile::with("emfile-send-file", &bytes);
            let opened = nuclei::fs::File::open(&file.0).await.unwrap();
            let (ours, peer) = <TcpStream as Stream>::pair(false);
            let reader = std::thread::spawn(move || {
                let mut peer = peer;
                let mut got = vec![0u8; 100_000];
                peer.read_exact(&mut got).map(|()| got)
            });
            let mut starved = Starved::new();
            starved.settle().await;
            let sent = within("send_file", ours.send_file(&opened, 0, bytes.len() as u64)).await;
            starved.free();
            assert_eq!(
                Answer::of(&sent, |n| *n as usize),
                Answer::Ok(bytes.len()),
                "send_file"
            );
            same_bytes("send_file", &joined(reader).await.unwrap(), &bytes);
        },
    );
}

/// A socket's first call with no descriptor left (made before the limit
/// fell, never used since) works as std's does: registering it with a
/// shard's driver takes no descriptor, whatever the kernel's ring offers.
#[test]
fn a_sockets_first_call_with_no_descriptor_left_works_as_std() {
    run(
        "emfile::a_sockets_first_call_with_no_descriptor_left_works_as_std",
        async || {
            let (tcp, mut tcp_peer) = <TcpStream as Stream>::pair(false);
            let (unix, mut unix_peer) = <UnixStream as Stream>::pair(false);
            let (ua, ub) = UnixDatagram::pair().unwrap();
            let mut starved = Starved::new();
            starved.settle().await;
            let wrote = within("a TCP write", write_all(&tcp, b"tcp")).await;
            let unix_wrote = within("a Unix write", write_all(&unix, b"unix")).await;
            let sent = within("a Unix datagram", ua.send(b"dgram")).await;
            let mut buf = [0u8; 8];
            let got = within("a Unix datagram", ub.recv(&mut buf)).await;
            starved.free();
            assert!(wrote.is_ok(), "a TCP stream's first write: {wrote:?}");
            assert!(
                unix_wrote.is_ok(),
                "a Unix stream's first write: {unix_wrote:?}"
            );
            assert!(sent.is_ok(), "a Unix datagram's first send: {sent:?}");
            assert_eq!(got.ok(), Some(5), "a Unix datagram's first receive");
            let mut three = [0u8; 3];
            tcp_peer.read_exact(&mut three).unwrap();
            assert_eq!(&three, b"tcp");
            let mut four = [0u8; 4];
            unix_peer.read_exact(&mut four).unwrap();
            assert_eq!(&four, b"unix");
        },
    );
}
