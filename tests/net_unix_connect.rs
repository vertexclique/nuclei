//! A Unix connect to a listener whose backlog is full. On Linux it waits, holding its own socket
//! and a timer, and connects on that socket once a place frees, as std's blocking connect does;
//! a drop of it frees everything, a thread of the blocking pool included. macOS refuses it, and
//! so does nuclei, with std's own answer.
#![cfg(unix)]

#[cfg(any(target_os = "linux", target_os = "android"))]
use std::io;
#[cfg(any(target_os = "linux", target_os = "android"))]
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::time::Duration;

use nuclei::net::{UnixListener, UnixSocket, UnixStream};

/// The longest a step may take on a loaded machine.
const DEADLINE: Duration = Duration::from_secs(60);

/// A socket path of this test's own, removed before and after.
struct Path(PathBuf);

impl Path {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("nuclei-unix-connect-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        Self(path)
    }
}

impl Drop for Path {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A listener at `path` with the smallest backlog: Linux lets one connection wait past a backlog
/// of 0, where macOS took 64 and more; there a backlog of 1 holds a few.
fn listener(path: &Path) -> UnixListener {
    let socket = UnixSocket::new_stream().unwrap();
    socket.bind(&path.0).unwrap();
    let backlog = if cfg!(any(target_os = "linux", target_os = "android")) {
        0
    } else {
        1
    };
    socket.listen(backlog).unwrap()
}

/// `SO_RCVBUF` of the socket `fd`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn recv_buffer(fd: RawFd) -> libc::c_int {
    let mut size: libc::c_int = 0;
    let mut len = size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: the kernel writes one int into our own.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            (&mut size as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    assert_eq!(rc, 0, "{}", io::Error::last_os_error());
    size
}

/// Sets `SO_RCVBUF` of the socket `fd` to `size`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn set_recv_buffer(fd: RawFd, size: libc::c_int) {
    // SAFETY: the kernel reads one int of our own.
    let rc = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_RCVBUF,
            (&size as *const libc::c_int).cast(),
            size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    assert_eq!(rc, 0, "{}", io::Error::last_os_error());
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn a_connect_to_a_full_backlog_waits_and_connects_its_own_socket_once_a_place_frees() {
    let path = Path::new("waits");
    nuclei::block_on(async {
        let listener = listener(&path);
        let first = std::os::unix::net::UnixStream::connect(&path.0).unwrap();
        let socket = UnixSocket::new_stream().unwrap();
        set_recv_buffer(socket.as_raw_fd(), 8192);
        let set = recv_buffer(socket.as_raw_fd());
        let mut connect = core::pin::pin!(socket.connect(&path.0));
        assert!(
            nuclei::time::timeout(Duration::from_millis(100), &mut connect)
                .await
                .is_err(),
            "a connect to a full backlog answered at once"
        );
        let (_taken, _) = listener.accept().await.unwrap();
        let stream = nuclei::time::timeout(DEADLINE, connect)
            .await
            .expect("the connect never took the freed place")
            .unwrap();
        assert_eq!(
            recv_buffer(stream.as_raw_fd()),
            set,
            "the stream is not the socket the caller set up"
        );
        let (server, _) = listener.accept().await.unwrap();
        stream.send(b"x").await.unwrap();
        let mut got = [0u8; 1];
        assert_eq!(server.recv(&mut got).await.unwrap(), 1);
        assert_eq!(&got, b"x");
        drop(first);
    });
}

#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn a_dropped_connect_to_a_full_backlog_frees_everything() {
    let path = Path::new("dropped");
    let rt = nuclei::Runtime::builder()
        .worker_threads(1)
        .pin_workers(false)
        .max_blocking_threads(1)
        .build()
        .unwrap();
    rt.block_on(async {
        let listener = listener(&path);
        let first = std::os::unix::net::UnixStream::connect(&path.0).unwrap();
        assert!(
            nuclei::time::timeout(Duration::from_millis(100), UnixStream::connect(&path.0))
                .await
                .is_err(),
            "a connect to a full backlog answered at once"
        );
        let job = nuclei::spawn_blocking(|| 7);
        assert_eq!(
            nuclei::time::timeout(DEADLINE, job)
                .await
                .expect("the blocking pool's one thread is held by a dropped connect"),
            7
        );
        let (_taken, _) = listener.accept().await.unwrap();
        assert!(
            nuclei::time::timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_err(),
            "the dropped connect still made a connection"
        );
        drop(first);
    });
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
#[test]
fn a_connect_to_a_full_backlog_is_answered_as_std() {
    let path = Path::new("refused");
    let _listener = nuclei::block_on(async { listener(&path) });
    let mut held = Vec::new();
    let want = loop {
        match std::os::unix::net::UnixStream::connect(&path.0) {
            Ok(stream) => held.push(stream),
            Err(err) => break err,
        }
        assert!(
            held.len() < 16,
            "the backlog took {} connections",
            held.len()
        );
    };
    let err = nuclei::block_on(nuclei::time::timeout(
        DEADLINE,
        UnixStream::connect(&path.0),
    ))
    .expect("a connect to a full backlog never answered")
    .unwrap_err();
    assert_eq!(
        (err.kind(), err.raw_os_error()),
        (want.kind(), want.raw_os_error()),
        "{err}"
    );
}
