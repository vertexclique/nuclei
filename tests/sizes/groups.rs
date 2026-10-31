//! The io_uring receive group's buffer size, each in a process of its own.

use super::datagrams::{
    To, datagram_row, datagram_sizes, largest_into, one_into, recv_connected, recv_owned,
    send_connected, send_owned, short, whole,
};
use super::streams::{
    read_each, read_exact, read_owned, stream_case, tcp_pair, unix_pair, write_all, write_each,
    write_owned,
};
use super::*;

/// Set in a process of this test binary that sweeps a receive group of its own:
/// `<buffers>x<size>`.
const GROUP: &str = "NUCLEI_SIZES_GROUP";

macro_rules! group_rows {
    ($($name:ident: $buffers:expr, $size:expr;)*) => {$(
                #[test]
        fn $name() {
            group($buffers, $size);
        }
    )*};
}

// The smallest buffer, one that is no power of two, the default, a large one with few buffers,
// and one below, at and one above the room a multishot datagram needs.
group_rows! {
    group_of_512: 256, 512;
    group_of_1000: 256, 1000;
    group_of_16384: 256, 16384;
    group_of_1_mib: 4, 1 << 20;
    group_below_datagram_room: 8, DATAGRAM_ROOM.values[0] as u32 - 1;
    group_at_datagram_room: 8, DATAGRAM_ROOM.values[0] as u32;
    group_above_datagram_room: 8, DATAGRAM_ROOM.values[0] as u32 + 1;
}

/// Runs [`group_child`] in a process of its own whose shards have a receive group of `buffers`
/// of `size` bytes.
fn group(buffers: u16, size: u32) {
    use std::io::Read;
    if nuclei::Proactor::backend() != nuclei::IoBackend::IoUring {
        eprintln!("skipping: the receive group is io_uring's");
        return;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "groups::group_child", "--test-threads=1"])
        .env(GROUP, format!("{buffers}x{size}"))
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut out = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut got = String::new();
        out.read_to_string(&mut got).unwrap();
        got
    });
    let status = wait_until(&mut child, Instant::now() + 10 * DEADLINE);
    let out = reader.join().unwrap();
    assert!(
        status.success(),
        "the group of {buffers}x{size} failed:\n{out}"
    );
    assert!(
        out.contains("test result: ok. 1 passed"),
        "the group of {buffers}x{size} did not run:\n{out}"
    );
}

/// The child of [`group`]: streams read through a group of its buffer size, and datagrams larger
/// than one of its buffers into a caller's buffer that holds them.
#[test]
fn group_child() {
    let Some(group) = std::env::var_os(GROUP) else {
        return;
    };
    let group = group.into_string().unwrap();
    let (buffers, size) = group.split_once('x').unwrap();
    let (buffers, size): (u16, u32) = (buffers.parse().unwrap(), size.parse().unwrap());
    nuclei::Proactor::with_config(nuclei::config::NucleiConfig {
        iouring: nuclei::config::IoUringConfiguration {
            recv_buffers: buffers,
            recv_buffer_size: size,
            ..Default::default()
        },
        ..Default::default()
    });
    assert_eq!(nuclei::Proactor::backend(), nuclei::IoBackend::IoUring);
    let b = size as usize;
    let cap = (usize::from(buffers) * b / 8).max(b);
    let mut sizes: Vec<usize> = sizes()
        .iter()
        .copied()
        .filter(|&n| n < LARGE.values[0] - 1)
        .collect();
    sizes.extend([b - 1, b, b + 1, 2 * b, 2 * b + 1, cap - 1, cap, cap + 1]);
    sizes.sort_unstable();
    sizes.dedup();
    for &n in &sizes {
        let what = format!("group {group}");
        stream_case(
            &format!("{what} borrowed tcp"),
            n,
            n,
            tcp_pair(),
            write_each,
            read_each,
        );
        stream_case(
            &format!("{what} exact unix"),
            n,
            n,
            unix_pair(),
            write_all,
            read_exact,
        );
        stream_case(
            &format!("{what} owned tcp"),
            n,
            n,
            tcp_pair(),
            write_owned,
            read_owned,
        );
    }
    let datagrams = datagram_sizes::<UdpSocket>(&sizes);
    let datagrams: Vec<usize> = datagrams.into_iter().filter(|&n| n <= 1 << 17).collect();
    datagram_row::<UdpSocket, _, _>(
        &format!("group {group} udp"),
        &datagrams,
        To::Connected,
        send_connected,
        recv_connected,
        whole,
    );
    datagram_row::<UdpSocket, _, _>(
        &format!("group {group} udp cut"),
        &datagrams,
        To::Connected,
        send_connected,
        recv_connected,
        short,
    );
    datagram_row::<UdpSocket, _, _>(
        &format!("group {group} udp owned"),
        &datagrams,
        To::Connected,
        send_owned,
        recv_owned,
        whole,
    );
    datagram_row::<UdpSocket, _, _>(
        &format!("group {group} udp one into"),
        &datagrams,
        To::Connected,
        send_connected,
        recv_connected,
        one_into,
    );
    datagram_row::<UdpSocket, _, _>(
        &format!("group {group} udp largest into"),
        &datagrams,
        To::Connected,
        send_connected,
        recv_connected,
        largest_into,
    );
}
