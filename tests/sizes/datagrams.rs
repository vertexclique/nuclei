//! Datagrams, UDP and Unix: every call at every size, and every receive buffer size.

use super::*;

/// How a row's datagrams go: on a socket connected to its peer, or addressed to the peer from a
/// socket connected to nothing. macOS refuses an addressed send on a connected socket
/// (`EISCONN`), std's as well, where Linux sends it; so the addressed calls are made where every
/// system sends them.
#[derive(Clone, Copy, Debug)]
pub(crate) enum To {
    Connected,
    Addressed,
}

/// A datagram socket's calls, on UDP and Unix datagram sockets alike, each socket named, the
/// receiver connected to the sender, and the sender to the receiver for the connected calls.
pub(crate) trait Dgram: Sized + 'static {
    type Addr: Clone + PartialEq + std::fmt::Debug;
    /// The sender and the receiver, and their addresses.
    fn pair(to: To) -> ((Self, Self::Addr), (Self, Self::Addr));
    /// What std's own send of `len` bytes, made as `to` says, gives on a pair like this one.
    fn std_send(len: usize, to: To) -> io::Result<usize>;
    fn send(&self, buf: &[u8]) -> impl Future<Output = io::Result<usize>>;
    fn recv(&self, buf: &mut [u8]) -> impl Future<Output = io::Result<usize>>;
    fn readable(&self) -> impl Future<Output = io::Result<()>>;
    fn send_to(&self, buf: &[u8], to: &Self::Addr) -> impl Future<Output = io::Result<usize>>;
    fn recv_from(&self, buf: &mut [u8]) -> impl Future<Output = io::Result<(usize, Self::Addr)>>;
    fn send_owned(&self, buf: Vec<u8>) -> impl Future<Output = (io::Result<usize>, Vec<u8>)>;
    fn recv_owned(&self, buf: Vec<u8>) -> impl Future<Output = (io::Result<usize>, Vec<u8>)>;
    fn send_to_owned(
        &self,
        buf: Vec<u8>,
        to: &Self::Addr,
    ) -> impl Future<Output = (io::Result<usize>, Vec<u8>)>;
    fn recv_from_owned(
        &self,
        buf: Vec<u8>,
    ) -> impl Future<Output = (io::Result<(usize, Self::Addr)>, Vec<u8>)>;
}

/// What std's blocking send of `len` bytes from `a` gives, made as `to` says, the datagram
/// taken off `b` again; `$pair` makes the sockets and the receiver's address for a `To`.
macro_rules! std_send {
    ($pair:expr, $len:expr, $to:expr) => {{
        thread_local! {
            static CONNECTED: (StdDgram, StdDgram, StdAddr) = $pair(To::Connected);
            static ADDRESSED: (StdDgram, StdDgram, StdAddr) = $pair(To::Addressed);
        }
        let send = |(a, b, b_at): &(StdDgram, StdDgram, StdAddr)| {
            let sent = match $to {
                To::Connected => a.send(pattern($len)),
                To::Addressed => a.send_to(pattern($len), b_at),
            };
            if sent.is_ok() {
                b.recv(&mut vec![0; $len + 1]).unwrap();
            }
            sent
        };
        match $to {
            To::Connected => CONNECTED.with(send),
            To::Addressed => ADDRESSED.with(send),
        }
    }};
}

impl Dgram for UdpSocket {
    type Addr = std::net::SocketAddr;

    fn pair(to: To) -> ((Self, Self::Addr), (Self, Self::Addr)) {
        let (a, b) = std_udp_pair(to);
        let (a_at, b_at) = (a.local_addr().unwrap(), b.local_addr().unwrap());
        (
            (Handle::new(a).unwrap(), a_at),
            (Handle::new(b).unwrap(), b_at),
        )
    }

    fn std_send(len: usize, to: To) -> io::Result<usize> {
        type StdDgram = std::net::UdpSocket;
        type StdAddr = std::net::SocketAddr;
        std_send!(
            |to| {
                let (a, b) = std_udp_pair(to);
                let b_at = b.local_addr().unwrap();
                (a, b, b_at)
            },
            len,
            to
        )
    }

    fn send(&self, buf: &[u8]) -> impl Future<Output = io::Result<usize>> {
        UdpSocket::send(self, buf)
    }
    fn recv(&self, buf: &mut [u8]) -> impl Future<Output = io::Result<usize>> {
        UdpSocket::recv(self, buf)
    }
    fn readable(&self) -> impl Future<Output = io::Result<()>> {
        UdpSocket::readable(self)
    }
    fn send_to(&self, buf: &[u8], to: &Self::Addr) -> impl Future<Output = io::Result<usize>> {
        UdpSocket::send_to(self, buf, *to)
    }
    fn recv_from(&self, buf: &mut [u8]) -> impl Future<Output = io::Result<(usize, Self::Addr)>> {
        UdpSocket::recv_from(self, buf)
    }
    fn send_owned(&self, buf: Vec<u8>) -> impl Future<Output = (io::Result<usize>, Vec<u8>)> {
        UdpSocket::send_owned(self, buf)
    }
    fn recv_owned(&self, buf: Vec<u8>) -> impl Future<Output = (io::Result<usize>, Vec<u8>)> {
        UdpSocket::recv_owned(self, buf)
    }
    fn send_to_owned(
        &self,
        buf: Vec<u8>,
        to: &Self::Addr,
    ) -> impl Future<Output = (io::Result<usize>, Vec<u8>)> {
        UdpSocket::send_to_owned(self, buf, *to)
    }
    fn recv_from_owned(
        &self,
        buf: Vec<u8>,
    ) -> impl Future<Output = (io::Result<(usize, Self::Addr)>, Vec<u8>)> {
        UdpSocket::recv_from_owned(self, buf)
    }
}

/// The sender and the receiver: the receiver connected to the sender, and the sender to the
/// receiver for the connected calls.
fn std_udp_pair(to: To) -> (std::net::UdpSocket, std::net::UdpSocket) {
    let a = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let b = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    if matches!(to, To::Connected) {
        a.connect(b.local_addr().unwrap()).unwrap();
    }
    b.connect(a.local_addr().unwrap()).unwrap();
    (a, b)
}

#[cfg(unix)]
impl Dgram for UnixDatagram {
    type Addr = std::path::PathBuf;

    fn pair(to: To) -> ((Self, Self::Addr), (Self, Self::Addr)) {
        let (a, a_at, b, b_at) = std_unix_pair(to);
        (
            (Handle::new(a).unwrap(), a_at),
            (Handle::new(b).unwrap(), b_at),
        )
    }

    fn std_send(len: usize, to: To) -> io::Result<usize> {
        type StdDgram = std::os::unix::net::UnixDatagram;
        type StdAddr = std::path::PathBuf;
        std_send!(
            |to| {
                let (a, _, b, b_at) = std_unix_pair(to);
                (a, b, b_at)
            },
            len,
            to
        )
    }

    fn send(&self, buf: &[u8]) -> impl Future<Output = io::Result<usize>> {
        UnixDatagram::send(self, buf)
    }
    fn recv(&self, buf: &mut [u8]) -> impl Future<Output = io::Result<usize>> {
        UnixDatagram::recv(self, buf)
    }
    fn readable(&self) -> impl Future<Output = io::Result<()>> {
        UnixDatagram::readable(self)
    }
    fn send_to(&self, buf: &[u8], to: &Self::Addr) -> impl Future<Output = io::Result<usize>> {
        UnixDatagram::send_to(self, buf, to.clone())
    }
    async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, Self::Addr)> {
        let (n, from) = UnixDatagram::recv_from(self, buf).await?;
        Ok((
            n,
            from.as_pathname()
                .unwrap_or(std::path::Path::new(""))
                .to_path_buf(),
        ))
    }
    fn send_owned(&self, buf: Vec<u8>) -> impl Future<Output = (io::Result<usize>, Vec<u8>)> {
        UnixDatagram::send_owned(self, buf)
    }
    fn recv_owned(&self, buf: Vec<u8>) -> impl Future<Output = (io::Result<usize>, Vec<u8>)> {
        UnixDatagram::recv_owned(self, buf)
    }
    fn send_to_owned(
        &self,
        buf: Vec<u8>,
        to: &Self::Addr,
    ) -> impl Future<Output = (io::Result<usize>, Vec<u8>)> {
        UnixDatagram::send_to_owned(self, buf, to.clone())
    }
    async fn recv_from_owned(&self, buf: Vec<u8>) -> (io::Result<(usize, Self::Addr)>, Vec<u8>) {
        let (got, buf) = UnixDatagram::recv_from_owned(self, buf).await;
        let got = got.map(|(n, from)| {
            (
                n,
                from.as_pathname()
                    .unwrap_or(std::path::Path::new(""))
                    .to_path_buf(),
            )
        });
        (got, buf)
    }
}

/// Two Unix datagram sockets bound in a directory of their own: the receiver connected to the
/// sender, and the sender to the receiver for the connected calls.
#[cfg(unix)]
fn std_unix_pair(
    to: To,
) -> (
    std::os::unix::net::UnixDatagram,
    std::path::PathBuf,
    std::os::unix::net::UnixDatagram,
    std::path::PathBuf,
) {
    use std::os::unix::net::UnixDatagram as Std;
    let dir = temp_dir();
    let (a_at, b_at) = (dir.join("a"), dir.join("b"));
    let (a, b) = (Std::bind(&a_at).unwrap(), Std::bind(&b_at).unwrap());
    if matches!(to, To::Connected) {
        a.connect(&b_at).unwrap();
    }
    b.connect(&a_at).unwrap();
    (a, a_at, b, b_at)
}

/// The largest datagram std sends on a pair of `D`, found by halving.
fn largest_datagram<D: Dgram>() -> usize {
    let (mut fits, mut fails) = (0, 2 << 20);
    while fails - fits > 1 {
        let mid = fits + (fails - fits) / 2;
        if D::std_send(mid, To::Connected).is_ok() {
            fits = mid;
        } else {
            fails = mid;
        }
    }
    fits
}

/// The sizes a datagram row sweeps: `sizes`, and one below, at and one above the largest
/// datagram the kernel takes.
pub(crate) fn datagram_sizes<D: Dgram>(sizes: &[usize]) -> Vec<usize> {
    let max = largest_datagram::<D>();
    let mut sizes = sizes.to_vec();
    sizes.extend([max - 1, max, max + 1]);
    sizes.sort_unstable();
    sizes.dedup();
    sizes
}

/// A datagram row: for every size, a datagram and a buffer `shape` makes of it and of the
/// largest datagram the kernel takes, the datagram sent by `send` and received by `recv` into
/// the buffer. What std refuses must fail with std's error; what it sends must come whole, or
/// cut to the buffer as std cuts it.
macro_rules! datagram_rows {
    ($($name:ident: $to:ident, $send:ident, $recv:ident, $shape:expr;)*) => {$(
        #[test]
        fn $name() {
            let udp = datagram_sizes::<UdpSocket>(sizes());
            datagram_row::<UdpSocket, _, _>(concat!(stringify!($name), " udp"), &udp, To::$to, $send, $recv, $shape);
            #[cfg(unix)]
            {
                let unix = datagram_sizes::<UnixDatagram>(sizes());
                datagram_row::<UnixDatagram, _, _>(concat!(stringify!($name), " unix"), &unix, To::$to, $send, $recv, $shape);
            }
        }
    )*};
}

/// A datagram's length and the buffer it is received into, from a size and the largest datagram.
pub(crate) type Shape = fn(usize, usize) -> (usize, usize);

pub(crate) fn datagram_row<D, S, R>(
    row: &str,
    sizes: &[usize],
    to: To,
    send: S,
    recv: R,
    shape: Shape,
) where
    D: Dgram,
    S: AsyncFn(&D, &D::Addr, &'static [u8]) -> io::Result<usize>,
    R: AsyncFn(&D, Vec<u8>) -> Received<D::Addr>,
{
    let max = largest_datagram::<D>();
    for &size in sizes {
        let (n, room) = shape(size, max);
        let what = format!("{row} at {size}: {n} bytes into {room}");
        let expected = D::std_send(n, to);
        within(&what, async {
            let ((a, a_at), (b, b_at)) = D::pair(to);
            let data = pattern(n);
            let sent = send(&a, &b_at, data).await;
            match (&expected, sent) {
                (Ok(_), Ok(sent)) => assert_eq!(sent, n, "{what}: sent"),
                (Err(want), Err(got)) => {
                    assert_eq!(
                        got.raw_os_error(),
                        want.raw_os_error(),
                        "{what}: {got} for {want}"
                    );
                    return;
                }
                (want, got) => panic!("{what}: std gives {want:?}, nuclei {got:?}"),
            }
            let (got, buf) = recv(&b, vec![0; room]).await;
            let (came, from) = got.unwrap_or_else(|err| panic!("{what}: {err}"));
            let cut = n.min(room);
            assert_eq!(came, cut, "{what}: the length into {room} bytes");
            assert!(buf[..cut] == data[..cut], "{what}: not the bytes sent");
            if let Some(from) = from {
                assert_eq!(from, a_at, "{what}: the sender");
            }
        });
    }
}

/// A datagram of the size into room for it and a byte more, or into one byte short of it.
pub(crate) fn whole(n: usize, _: usize) -> (usize, usize) {
    (n, n + 1)
}

pub(crate) fn short(n: usize, _: usize) -> (usize, usize) {
    (n, n.saturating_sub(1))
}

/// A buffer of the size, for a datagram of one byte, or for the largest datagram.
pub(crate) fn one_into(n: usize, _: usize) -> (usize, usize) {
    (1, n)
}

pub(crate) fn largest_into(n: usize, max: usize) -> (usize, usize) {
    (max, n)
}

datagram_rows! {
    datagrams_connected: Connected, send_connected, recv_connected, whole;
    datagrams_connected_cut: Connected, send_connected, recv_connected, short;
    datagrams_addressed: Addressed, send_addressed, recv_addressed, whole;
    datagrams_owned: Connected, send_owned, recv_owned, whole;
    datagrams_owned_cut: Connected, send_owned, recv_owned, short;
    datagrams_owned_addressed: Addressed, send_owned_addressed, recv_owned_addressed, whole;
    buffers_connected: Connected, send_connected, recv_connected, one_into;
    buffers_connected_largest: Connected, send_connected, recv_connected, largest_into;
    buffers_owned: Connected, send_owned, recv_owned, one_into;
    buffers_owned_largest: Connected, send_owned, recv_owned, largest_into;
    datagrams_after_readable: Connected, send_connected, recv_after_readable, whole;
}

pub(crate) async fn send_connected<D: Dgram>(d: &D, _: &D::Addr, data: &[u8]) -> io::Result<usize> {
    d.send(data).await
}

async fn send_addressed<D: Dgram>(d: &D, to: &D::Addr, data: &[u8]) -> io::Result<usize> {
    d.send_to(data, to).await
}

pub(crate) async fn send_owned<D: Dgram>(d: &D, _: &D::Addr, data: &[u8]) -> io::Result<usize> {
    let (sent, back) = d.send_owned(data.to_vec()).await;
    assert!(back == data, "the buffer came back changed");
    sent
}

async fn send_owned_addressed<D: Dgram>(d: &D, to: &D::Addr, data: &[u8]) -> io::Result<usize> {
    let (sent, back) = d.send_to_owned(data.to_vec(), to).await;
    assert!(back == data, "the buffer came back changed");
    sent
}

pub(crate) type Received<A> = (io::Result<(usize, Option<A>)>, Vec<u8>);

pub(crate) async fn recv_connected<D: Dgram>(d: &D, mut buf: Vec<u8>) -> Received<D::Addr> {
    let got = d.recv(&mut buf).await.map(|n| (n, None));
    (got, buf)
}

/// A receive after the socket said it is readable.
async fn recv_after_readable<D: Dgram>(d: &D, buf: Vec<u8>) -> Received<D::Addr> {
    if let Err(err) = d.readable().await {
        return (Err(err), buf);
    }
    recv_connected(d, buf).await
}

async fn recv_addressed<D: Dgram>(d: &D, mut buf: Vec<u8>) -> Received<D::Addr> {
    let got = d.recv_from(&mut buf).await.map(|(n, from)| (n, Some(from)));
    (got, buf)
}

/// Into the spare capacity of a buffer of `buf.len()` bytes.
pub(crate) async fn recv_owned<D: Dgram>(d: &D, buf: Vec<u8>) -> Received<D::Addr> {
    let (got, buf) = d.recv_owned(Vec::with_capacity(buf.len())).await;
    if let Ok(n) = got {
        assert_eq!(n, buf.len(), "the length and the buffer disagree");
    }
    (got.map(|n| (n, None)), buf)
}

async fn recv_owned_addressed<D: Dgram>(d: &D, buf: Vec<u8>) -> Received<D::Addr> {
    let (got, buf) = d.recv_from_owned(Vec::with_capacity(buf.len())).await;
    (got.map(|(n, from)| (n, Some(from))), buf)
}
