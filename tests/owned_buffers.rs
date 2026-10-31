//! The owned calls with each kind of buffer: a `Vec<u8>` handed in and
//! bound back as one, an `AlignedBuf`, and a caller's own type, on files,
//! TCP, UDP and Unix sockets; and on Linux a file opened for direct IO read
//! and written with aligned buffers.

use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use nuclei::fs::File;
use nuclei::io::{AlignedBuf, IoBuf, IoBufMut};
use nuclei::net::{TcpListener, TcpStream, UdpSocket};

struct Temp(PathBuf);

impl Temp {
    fn new() -> Self {
        static N: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "nuclei-owned-buffers-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A caller's own buffer type.
#[derive(Debug, Default)]
struct Mine {
    mem: Box<[u8]>,
    len: usize,
}

impl Mine {
    fn with_room(cap: usize) -> Self {
        Self {
            mem: vec![0; cap].into_boxed_slice(),
            len: 0,
        }
    }

    fn of(bytes: &[u8]) -> Self {
        Self {
            mem: bytes.into(),
            len: bytes.len(),
        }
    }

    fn bytes(&self) -> &[u8] {
        &self.mem[..self.len]
    }
}

// SAFETY: the boxed memory never moves with the value.
unsafe impl IoBuf for Mine {
    fn stable_ptr(&self) -> *const u8 {
        self.mem.as_ptr()
    }

    fn bytes_init(&self) -> usize {
        self.len
    }

    fn bytes_total(&self) -> usize {
        self.mem.len()
    }
}

// SAFETY: as above, all of it writable.
unsafe impl IoBufMut for Mine {
    fn stable_mut_ptr(&mut self) -> *mut u8 {
        self.mem.as_mut_ptr()
    }

    unsafe fn set_init(&mut self, pos: usize) {
        self.len = pos;
    }
}

fn aligned(bytes: &[u8]) -> AlignedBuf {
    let mut buf = AlignedBuf::new(4096, 4096).unwrap();
    assert_eq!(buf.extend_from_slice(bytes), bytes.len());
    buf
}

async fn tcp_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap())
        .await
        .unwrap();
    let (server, _) = listener.accept().await.unwrap();
    (client, server)
}

async fn read_exactly(stream: &TcpStream, len: usize) -> Vec<u8> {
    let mut got: Vec<u8> = Vec::with_capacity(len);
    while got.len() < len {
        let (n, back): (io::Result<usize>, Vec<u8>) = stream.read_owned(got).await;
        assert_ne!(n.unwrap(), 0, "the stream ended early");
        got = back;
    }
    got
}

#[test]
fn a_vec_handed_to_every_owned_call_comes_back_as_a_vec() {
    nuclei::block_on(async {
        let t = Temp::new();
        let file = File::create_new(t.path("vec")).await.unwrap();
        let (n, back): (io::Result<usize>, Vec<u8>) = file.write_owned(b"vec bytes".to_vec()).await;
        assert_eq!((n.unwrap(), &back[..]), (9, &b"vec bytes"[..]));
        let (n, back): (io::Result<usize>, Vec<u8>) = file.write_at_owned(b"VEC".to_vec(), 0).await;
        assert_eq!((n.unwrap(), back.len()), (3, 3));
        let (n, back): (io::Result<usize>, Vec<u8>) =
            file.read_at_owned(Vec::with_capacity(16), 0).await;
        assert_eq!((n.unwrap(), &back[..]), (9, &b"VEC bytes"[..]));
        let reader = File::open(t.path("vec")).await.unwrap();
        let (n, back): (io::Result<usize>, Vec<u8>) =
            reader.read_owned(Vec::with_capacity(4)).await;
        assert_eq!((n.unwrap(), &back[..]), (4, &b"VEC "[..]));

        let (client, server) = tcp_pair().await;
        let (n, back): (io::Result<usize>, Vec<u8>) = client.write_owned(b"tcp".to_vec()).await;
        assert_eq!((n.unwrap(), &back[..]), (3, &b"tcp"[..]));
        assert_eq!(read_exactly(&server, 3).await, b"tcp");
        let (n, released): (io::Result<usize>, nuclei::net::Released) =
            client.write_zero_copy(b"zero copy".to_vec()).await;
        assert_eq!(n.unwrap(), 9);
        let back: Vec<u8> = released.await;
        assert_eq!(back, b"zero copy");
        assert_eq!(read_exactly(&server, 9).await, b"zero copy");

        let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (n, back): (io::Result<usize>, Vec<u8>) = a
            .send_to_owned(b"datagram".to_vec(), b.local_addr().unwrap())
            .await;
        assert_eq!((n.unwrap(), back.len()), (8, 8));
        let (got, back): (io::Result<(usize, std::net::SocketAddr)>, Vec<u8>) =
            b.recv_from_owned(Vec::with_capacity(64)).await;
        assert_eq!(got.unwrap(), (8, a.local_addr().unwrap()));
        assert_eq!(back, b"datagram");
        a.connect(b.local_addr().unwrap()).unwrap();
        b.connect(a.local_addr().unwrap()).unwrap();
        let (n, _): (io::Result<usize>, Vec<u8>) = a.send_owned(b"again".to_vec()).await;
        assert_eq!(n.unwrap(), 5);
        let (n, back): (io::Result<usize>, Vec<u8>) = b.recv_owned(Vec::with_capacity(64)).await;
        assert_eq!((n.unwrap(), &back[..]), (5, &b"again"[..]));
    });
}

#[test]
fn an_aligned_buffer_reads_and_writes_files_and_sockets() {
    nuclei::block_on(async {
        let t = Temp::new();
        let file = File::create_new(t.path("aligned")).await.unwrap();
        let (n, back) = file.write_owned(aligned(b"aligned bytes")).await;
        assert_eq!((n.unwrap(), back.alignment()), (13, 4096));
        let (n, back) = file.write_at_owned(aligned(b"ALIGNED"), 0).await;
        assert_eq!((n.unwrap(), &back[..]), (7, &b"ALIGNED"[..]));
        let into = AlignedBuf::new(4096, 4096).unwrap();
        let ptr = into.as_ptr();
        let (n, back) = file.read_at_owned(into, 0).await;
        assert_eq!((n.unwrap(), &back[..]), (13, &b"ALIGNED bytes"[..]));
        assert_eq!(
            back.as_ptr(),
            ptr,
            "the bytes were read into the caller's memory"
        );

        let (client, server) = tcp_pair().await;
        let (n, _) = client.write_owned(aligned(b"over tcp")).await;
        assert_eq!(n.unwrap(), 8);
        let (n, back) = server.read_owned(AlignedBuf::new(8, 8).unwrap()).await;
        let n = n.unwrap();
        assert!(n > 0 && n <= 8);
        assert_eq!(&back[..], &b"over tcp"[..n]);
        let (n, released) = client.write_zero_copy(aligned(b"released")).await;
        assert_eq!(n.unwrap(), 8);
        let back: AlignedBuf = released.await;
        assert_eq!(&back[..], b"released");

        let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (n, _) = a
            .send_to_owned(aligned(b"udp"), b.local_addr().unwrap())
            .await;
        assert_eq!(n.unwrap(), 3);
        let (n, back) = b.recv_owned(AlignedBuf::new(2, 2).unwrap()).await;
        assert_eq!(
            (n.unwrap(), &back[..]),
            (2, &b"ud"[..]),
            "cut to its capacity"
        );
    });
}

#[test]
fn a_callers_own_buffer_type_reads_and_writes_files_and_sockets() {
    nuclei::block_on(async {
        let t = Temp::new();
        let file = File::create_new(t.path("mine")).await.unwrap();
        let (n, back) = file.write_owned(Mine::of(b"my own type")).await;
        assert_eq!((n.unwrap(), back.bytes()), (11, &b"my own type"[..]));
        let (n, back) = file.read_at_owned(Mine::with_room(64), 3).await;
        assert_eq!((n.unwrap(), back.bytes()), (8, &b"own type"[..]));
        let reader = File::open(t.path("mine")).await.unwrap();
        let mut held = Mine::with_room(5);
        held.mem[0] = b'>';
        held.len = 1;
        let (n, back) = reader.read_owned(held).await;
        assert_eq!((n.unwrap(), back.bytes()), (4, &b">my o"[..]));

        let (client, server) = tcp_pair().await;
        let (n, _) = client.write_owned(Mine::of(b"mine over tcp")).await;
        assert_eq!(n.unwrap(), 13);
        let mut got = Vec::new();
        let mut buf = Mine::with_room(4);
        while got.len() < 13 {
            buf.len = 0;
            let (n, back) = server.read_owned(buf).await;
            assert_ne!(n.unwrap(), 0);
            got.extend_from_slice(back.bytes());
            buf = back;
        }
        assert_eq!(got, b"mine over tcp");
        let (n, released) = client.write_zero_copy(Mine::of(b"zc")).await;
        assert_eq!(n.unwrap(), 2);
        assert_eq!(released.await.bytes(), b"zc");

        let a = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let b = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let (n, back) = a
            .send_to_owned(Mine::of(b"mine"), b.local_addr().unwrap())
            .await;
        assert_eq!((n.unwrap(), back.bytes()), (4, &b"mine"[..]));
        let (got, back) = b.recv_from_owned(Mine::with_room(16)).await;
        assert_eq!(got.unwrap().0, 4);
        assert_eq!(back.bytes(), b"mine");
    });
}

#[test]
fn a_full_buffer_reads_nothing_and_an_empty_one_writes_nothing() {
    nuclei::block_on(async {
        let t = Temp::new();
        std::fs::write(t.path("f"), b"data").unwrap();
        let file = File::open(t.path("f")).await.unwrap();
        let mut full = AlignedBuf::new(4, 4).unwrap();
        full.extend_from_slice(b"full");
        let (n, back) = file.read_owned(full).await;
        assert_eq!((n.unwrap(), &back[..]), (0, &b"full"[..]));
        let full = AlignedBuf::zeroed(8, 8).unwrap();
        let (n, _) = file.read_at_owned(full, 0).await;
        assert_eq!(n.unwrap(), 0);
        let writer = File::create_new(t.path("g")).await.unwrap();
        let (n, back) = writer.write_owned(AlignedBuf::new(16, 16).unwrap()).await;
        assert_eq!((n.unwrap(), back.capacity()), (0, 16));
        let (n, _) = writer.write_at_owned(Mine::default(), 0).await;
        assert_eq!(n.unwrap(), 0);
    });
}

#[test]
fn an_owned_read_of_a_closed_peer_gives_the_buffer_back() {
    nuclei::block_on(async {
        let (client, server) = tcp_pair().await;
        drop(client);
        let into = AlignedBuf::new(64, 64).unwrap();
        let ptr = into.as_ptr();
        let (n, back) = server.read_owned(into).await;
        assert_eq!(n.unwrap(), 0);
        assert_eq!(back.as_ptr(), ptr);
    });
}

#[cfg(unix)]
#[test]
fn unix_sockets_take_aligned_and_callers_buffers() {
    use nuclei::net::{UnixDatagram, UnixStream};
    nuclei::block_on(async {
        let (a, b) = UnixStream::pair().unwrap();
        let (n, _) = a.write_owned(Mine::of(b"unix stream")).await;
        assert_eq!(n.unwrap(), 11);
        let (n, back) = b.read_owned(AlignedBuf::new(64, 64).unwrap()).await;
        let n = n.unwrap();
        assert_eq!(&back[..], &b"unix stream"[..n]);

        let (a, b) = UnixDatagram::pair().unwrap();
        let (n, _) = a.send_owned(aligned(b"unix datagram")).await;
        assert_eq!(n.unwrap(), 13);
        let (n, back) = b.recv_owned(Mine::with_room(64)).await;
        assert_eq!((n.unwrap(), back.bytes()), (13, &b"unix datagram"[..]));
    });
}

#[cfg(target_os = "linux")]
mod direct {
    use std::os::unix::fs::OpenOptionsExt;

    use super::*;

    /// A file of 64 KiB opened for direct IO, or `None` (said) where the
    /// filesystem refuses it.
    fn direct_file(t: &Temp) -> Option<File> {
        let path = t.path("direct");
        std::fs::write(&path, (0..65536u32).map(|i| i as u8).collect::<Vec<_>>()).unwrap();
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_DIRECT)
            .open(&path)
        {
            Ok(file) => Some(nuclei::io::Handle::new(file).unwrap()),
            Err(err) => {
                eprintln!("skipping: {} refuses O_DIRECT: {err}", t.0.display());
                None
            }
        }
    }

    #[test]
    fn a_file_opened_for_direct_io_reads_and_writes_aligned_buffers() {
        let t = Temp::new();
        let Some(file) = direct_file(&t) else {
            return;
        };
        nuclei::block_on(async {
            for len in [4096, 8192, 65536] {
                let (n, back) = file
                    .read_at_owned(AlignedBuf::new(len, 4096).unwrap(), 0)
                    .await;
                assert_eq!(n.unwrap(), len);
                assert!(back.iter().enumerate().all(|(i, &b)| b == i as u8));
            }
            let mut block = AlignedBuf::zeroed(4096, 4096).unwrap();
            block.fill(0xab);
            let (n, _) = file.write_at_owned(block, 4096).await;
            assert_eq!(n.unwrap(), 4096);
            let (n, back) = file
                .read_at_owned(AlignedBuf::new(4096, 4096).unwrap(), 4096)
                .await;
            assert_eq!(n.unwrap(), 4096);
            assert!(back.iter().all(|&b| b == 0xab));
            let (n, back) = file.read_owned(AlignedBuf::new(4096, 4096).unwrap()).await;
            assert_eq!(n.unwrap(), 4096);
            assert_eq!(back[1], 1);
        });
    }

    #[test]
    fn a_direct_read_into_memory_off_its_alignment_answers_as_the_kernel() {
        let t = Temp::new();
        let Some(file) = direct_file(&t) else {
            return;
        };
        nuclei::block_on(async {
            let mut off = AlignedBuf::new(8192, 4096).unwrap();
            off.extend_from_slice(&[0; 1]);
            let (n, _) = file.read_at_owned(off, 0).await;
            match n {
                Err(err) => assert_eq!(err.raw_os_error(), Some(libc::EINVAL)),
                // Some filesystems serve a misaligned direct read buffered.
                Ok(n) => assert!(n > 0),
            }
        });
    }
}
