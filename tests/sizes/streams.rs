//! Byte streams, TCP and Unix: every call at every size, and `send_file`.

use super::*;

/// The calls of a byte stream the rows use, on TCP and Unix streams alike.
pub(crate) trait Conn: AsyncRead + AsyncWrite + Unpin + Sized + 'static {
    fn read_owned(&self, buf: Vec<u8>) -> impl Future<Output = (io::Result<usize>, Vec<u8>)>;
    fn write_owned(&self, buf: Vec<u8>) -> impl Future<Output = (io::Result<usize>, Vec<u8>)>;
    fn send_file(
        &self,
        file: &File,
        offset: u64,
        len: u64,
    ) -> impl Future<Output = io::Result<u64>>;
}

macro_rules! conn {
    ($($ty:ty),*) => {$(
        impl Conn for $ty {
            fn read_owned(&self, buf: Vec<u8>) -> impl Future<Output = (io::Result<usize>, Vec<u8>)> {
                <$ty>::read_owned(self, buf)
            }
            fn write_owned(&self, buf: Vec<u8>) -> impl Future<Output = (io::Result<usize>, Vec<u8>)> {
                <$ty>::write_owned(self, buf)
            }
            fn send_file(&self, file: &File, offset: u64, len: u64) -> impl Future<Output = io::Result<u64>> {
                <$ty>::send_file(self, file, offset, len)
            }
        }
    )*};
}

conn!(TcpStream);
#[cfg(unix)]
conn!(UnixStream);

pub(crate) fn tcp_pair() -> (TcpStream, TcpStream) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let a = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (b, _) = listener.accept().unwrap();
    (Handle::new(a).unwrap(), Handle::new(b).unwrap())
}

#[cfg(unix)]
pub(crate) fn unix_pair() -> (UnixStream, UnixStream) {
    let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
    (Handle::new(a).unwrap(), Handle::new(b).unwrap())
}

/// How many calls of a size a row makes: one, or for the write-behind many small ones.
fn once(_: usize) -> usize {
    1
}

fn many(n: usize) -> usize {
    match n {
        0..=16_385 => 16,
        16_386..=1_048_577 => 2,
        _ => 1,
    }
}

/// Runs a stream row: for every size `n` and both kinds of stream, `write` sends `times(n)` calls
/// of `n` bytes and `read` takes them in calls of `n`; then the writer's end closes, and the
/// reader's end must be at its end.
macro_rules! stream_rows {
    ($($name:ident: $write:ident, $read:ident, $times:ident;)*) => {$(
        #[test]
        fn $name() {
            for &n in sizes() {
                let total = n * $times(n);
                stream_case(concat!(stringify!($name), " tcp"), n, total, tcp_pair(), $write, $read);
                #[cfg(unix)]
                stream_case(concat!(stringify!($name), " unix"), n, total, unix_pair(), $write, $read);
            }
        }
    )*};
}

pub(crate) fn stream_case<S, W, R>(
    row: &str,
    n: usize,
    total: usize,
    (a, b): (S, S),
    write: W,
    read: R,
) where
    S: Conn,
    W: AsyncFnOnce(S, usize, &'static [u8]) -> Option<S>,
    R: AsyncFnOnce(S, usize, usize) -> (Vec<u8>, S),
{
    let what = format!("{row} at {n} ({total} in all)");
    within(&what, async {
        let data = pattern(total);
        let (kept, (got, mut b)) = nuclei::join!(write(a, n, data), read(b, n, total));
        assert!(
            got == data,
            "{what}: {} bytes came, not the ones sent",
            got.len()
        );
        drop(kept);
        assert_eq!(b.read(&mut [0; 1]).await.unwrap(), 0, "{what}: more came");
    });
}

stream_rows! {
    borrowed: write_each, read_each, once;
    write_all_read_exact: write_all, read_exact, once;
    vectored: write_vectored, read_exact, once;
    owned: write_owned, read_owned, once;
    write_behind: write_each, read_exact, many;
    buffered: write_buffered, read_buffered, once;
    read_to_end: write_and_close, read_until_end, once;
    copy: copy_in, copy_out, once;
}

/// Each chunk of `n` bytes by `write` calls, each finishing what the one before took part of.
pub(crate) async fn write_each<S: Conn>(mut s: S, n: usize, data: &[u8]) -> Option<S> {
    if n == 0 {
        assert_eq!(s.write(&[]).await.unwrap(), 0);
    }
    for chunk in data.chunks(n.max(1)) {
        let mut rest = chunk;
        while !rest.is_empty() {
            let took = s.write(rest).await.unwrap();
            assert_ne!(took, 0, "a write took nothing");
            rest = &rest[took..];
        }
    }
    s.flush().await.unwrap();
    Some(s)
}

pub(crate) async fn write_all<S: Conn>(mut s: S, n: usize, data: &[u8]) -> Option<S> {
    s.write_all(&data[..data.len().min(n)]).await.unwrap();
    for chunk in data.chunks(n.max(1)).skip(1) {
        s.write_all(chunk).await.unwrap();
    }
    s.flush().await.unwrap();
    Some(s)
}

/// The bytes as three slices of uneven lengths.
async fn write_vectored<S: Conn>(mut s: S, _: usize, data: &[u8]) -> Option<S> {
    let (one, rest) = data.split_at(data.len() / 3);
    let (two, three) = rest.split_at(rest.len() / 4);
    let mut slices = [IoSlice::new(one), IoSlice::new(two), IoSlice::new(three)];
    let mut left = &mut slices[..];
    loop {
        let took = s.write_vectored(left).await.unwrap();
        if took == 0 {
            assert!(left.iter().all(|s| s.is_empty()), "a write took nothing");
            break;
        }
        IoSlice::advance_slices(&mut left, took);
    }
    s.flush().await.unwrap();
    Some(s)
}

pub(crate) async fn write_owned<S: Conn>(mut s: S, n: usize, data: &[u8]) -> Option<S> {
    for chunk in data
        .chunks(n.max(1))
        .chain(data.is_empty().then_some(&[][..]))
    {
        let (wrote, back) = s.write_owned(chunk.to_vec()).await;
        assert_eq!(wrote.unwrap(), chunk.len());
        assert!(back == chunk, "the buffer came back changed");
    }
    s.flush().await.unwrap();
    Some(s)
}

async fn write_buffered<S: Conn>(s: S, n: usize, data: &[u8]) -> Option<S> {
    let mut w = BufWriter::new(s);
    for chunk in data.chunks(n.max(1)) {
        w.write_all(chunk).await.unwrap();
    }
    w.flush().await.unwrap();
    Some(w.into_inner())
}

async fn write_and_close<S: Conn>(s: S, n: usize, data: &[u8]) -> Option<S> {
    let mut s = write_all(s, n, data).await?;
    s.shutdown().await.unwrap();
    None
}

async fn copy_in<S: Conn>(mut s: S, _: usize, mut data: &[u8]) -> Option<S> {
    let len = data.len() as u64;
    assert_eq!(nuclei::io::copy(&mut data, &mut s).await.unwrap(), len);
    s.shutdown().await.unwrap();
    None
}

pub(crate) async fn read_each<S: Conn>(mut s: S, n: usize, total: usize) -> (Vec<u8>, S) {
    let mut got = Vec::with_capacity(total);
    let mut buf = vec![0; n];
    if n == 0 {
        assert_eq!(s.read(&mut buf).await.unwrap(), 0);
    }
    while got.len() < total {
        let came = s.read(&mut buf).await.unwrap();
        assert_ne!(came, 0, "the stream ended at {} of {total}", got.len());
        got.extend_from_slice(&buf[..came]);
    }
    (got, s)
}

pub(crate) async fn read_exact<S: Conn>(mut s: S, n: usize, total: usize) -> (Vec<u8>, S) {
    let mut got = vec![0; total];
    if n == 0 {
        s.read_exact(&mut []).await.unwrap();
    }
    for chunk in got.chunks_mut(n.max(1)) {
        s.read_exact(chunk).await.unwrap();
    }
    (got, s)
}

pub(crate) async fn read_owned<S: Conn>(s: S, n: usize, total: usize) -> (Vec<u8>, S) {
    let mut got = Vec::with_capacity(total);
    loop {
        let (came, buf) = s.read_owned(Vec::with_capacity(n)).await;
        let came = came.unwrap();
        assert_eq!(came, buf.len());
        got.extend_from_slice(&buf);
        if got.len() >= total {
            break;
        }
        assert_ne!(came, 0, "the stream ended at {} of {total}", got.len());
    }
    (got, s)
}

async fn read_buffered<S: Conn>(s: S, n: usize, total: usize) -> (Vec<u8>, S) {
    let mut r = BufReader::new(s);
    let mut got = vec![0; total];
    for chunk in got.chunks_mut(n.max(1)) {
        r.read_exact(chunk).await.unwrap();
    }
    assert!(r.buffer().is_empty(), "the reader holds bytes past the end");
    (got, r.into_inner())
}

async fn read_until_end<S: Conn>(mut s: S, _: usize, total: usize) -> (Vec<u8>, S) {
    let mut got = Vec::new();
    assert_eq!(s.read_to_end(&mut got).await.unwrap(), total);
    (got, s)
}

async fn copy_out<S: Conn>(mut s: S, _: usize, total: usize) -> (Vec<u8>, S) {
    let mut got = Vec::new();
    assert_eq!(
        nuclei::io::copy(&mut s, &mut got).await.unwrap(),
        total as u64
    );
    (got, s)
}

/// Vectored writes of one less, as many and one more slices than one call hands the kernel.
#[cfg(unix)]
#[test]
fn vectored_writes_around_the_slice_limit() {
    let max = IOV_MAX.values[0];
    for count in [max - 1, max, max + 1] {
        for len in [1, 16] {
            let total = count * len;
            stream_case(
                &format!("{count} slices of {len}"),
                len,
                total,
                unix_pair(),
                async move |mut s, len: usize, data: &'static [u8]| {
                    let mut slices: Vec<IoSlice<'_>> = data.chunks(len).map(IoSlice::new).collect();
                    let mut left = &mut slices[..];
                    while !left.is_empty() {
                        let took = s.write_vectored(left).await.unwrap();
                        assert_ne!(took, 0);
                        IoSlice::advance_slices(&mut left, took);
                    }
                    s.flush().await.unwrap();
                    Some(s)
                },
                read_exact,
            );
        }
    }
}

/// `send_file` of every size, from the file's start and from an offset, asked for exactly the
/// bytes there are and for one more (the file ends first).
#[test]
fn send_file() {
    let dir = temp_dir();
    let path = dir.join("sent");
    for &n in sizes() {
        for offset in [0, OFFSET] {
            let data = pattern(n);
            std::fs::write(&path, [&vec![0x5A; offset as usize][..], data].concat()).unwrap();
            for ask in [n as u64, n as u64 + 1] {
                send_file_case(
                    &format!("send_file tcp at {n} from {offset} asking {ask}"),
                    tcp_pair(),
                    &path,
                    offset,
                    ask,
                    data,
                );
                #[cfg(unix)]
                send_file_case(
                    &format!("send_file unix at {n} from {offset} asking {ask}"),
                    unix_pair(),
                    &path,
                    offset,
                    ask,
                    data,
                );
            }
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}

fn send_file_case<S: Conn>(
    what: &str,
    (a, b): (S, S),
    path: &std::path::Path,
    offset: u64,
    ask: u64,
    data: &'static [u8],
) {
    within(what, async {
        let file = File::open(path).await.unwrap();
        let send = async {
            assert_eq!(
                a.send_file(&file, offset, ask).await.unwrap(),
                data.len() as u64
            );
            Some(a)
        };
        let (kept, (got, mut b)) =
            nuclei::join!(send, read_exact(b, data.len().max(1), data.len()));
        assert!(got == data, "{what}: not the bytes of the file");
        drop(kept);
        assert_eq!(b.read(&mut [0; 1]).await.unwrap(), 0, "{what}: more came");
    });
}
