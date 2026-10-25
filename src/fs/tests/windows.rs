//! Byte streams that are not sockets, on Windows: a pipe opened for
//! synchronous IO, whose calls are jobs of the blocking pool, and one
//! opened for overlapped IO, whose operations complete on the port.

use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};

use super::*;
use crate::io::raw::Raw;
use crate::io::source::Kind;
use crate::syscore::windows::{
    CreateNamedPipeW, CreatePipe, FILE_FLAG_OVERLAPPED, HANDLE, INVALID_HANDLE_VALUE,
    PIPE_ACCESS_DUPLEX,
};

/// An anonymous pipe: its ends are opened for synchronous IO.
fn synchronous_pipe() -> (File, File) {
    let (mut r, mut w): (HANDLE, HANDLE) = (core::ptr::null_mut(), core::ptr::null_mut());
    // SAFETY: the call writes two new handles into our own.
    assert_ne!(
        unsafe { CreatePipe(&mut r, &mut w, core::ptr::null(), 0) },
        0
    );
    // SAFETY: both handles are new and ours alone.
    unsafe {
        (
            File::from(OwnedHandle::from_raw_handle(r.cast())),
            File::from(OwnedHandle::from_raw_handle(w.cast())),
        )
    }
}

/// A named pipe and a client of it, both ends opened for overlapped IO.
fn overlapped_pipe() -> (File, File) {
    let name = format!(
        r"\\.\pipe\nuclei-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
    // SAFETY: a new pipe of our own name; the name lives for the call.
    let server = unsafe {
        CreateNamedPipeW(
            wide.as_ptr(),
            PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
            0,
            1,
            64 * 1024,
            64 * 1024,
            0,
            core::ptr::null(),
        )
    };
    assert_ne!(server, INVALID_HANDLE_VALUE);
    let client = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(FILE_FLAG_OVERLAPPED)
        .open(&name)
        .unwrap();
    // SAFETY: the handle is new and ours alone. The client is connected:
    // opening it connected the server end.
    (
        File::from(unsafe { OwnedHandle::from_raw_handle(server.cast()) }),
        client,
    )
}

fn raw(file: &File) -> Raw {
    file.as_raw_handle() as usize as Raw
}

/// Bytes go through `writer` and come out of `reader` whole and in order,
/// and the end of the writer is the reader's end of stream.
fn carries_bytes_then_the_end(reader: File, writer: File) {
    let rt = runtime();
    rt.block_on(async {
        let mut reader = Handle::new(reader).unwrap();
        assert_eq!(reader.source().kind(), Kind::Bytes);
        let data = pattern(300 * 1024);
        let expected = data.clone();
        let writer = spawn(async move {
            let mut writer = Handle::new(writer).unwrap();
            within(write_all(&mut writer, &data)).await.unwrap();
            within(flush(&mut writer)).await.unwrap();
        });
        let mut got = vec![0u8; expected.len()];
        let mut at = 0;
        while at < got.len() {
            match within(read_some(&mut reader, &mut got[at..]))
                .await
                .unwrap()
            {
                0 => panic!("the stream ended after {at} bytes"),
                n => at += n,
            }
        }
        within(writer).await;
        assert!(got == expected, "the bytes arrived changed or out of order");
        let mut more = [0u8; 8];
        assert_eq!(within(read_some(&mut reader, &mut more)).await.unwrap(), 0);
    });
}

#[test]
fn a_synchronous_pipe_carries_bytes_through_the_pools_jobs() {
    let (r, w) = synchronous_pipe();
    assert!(crate::io::sys::synchronous(raw(&r)).unwrap());
    carries_bytes_then_the_end(r, w);
}

#[test]
fn an_overlapped_pipe_carries_bytes_through_the_port() {
    let (server, client) = overlapped_pipe();
    assert!(!crate::io::sys::synchronous(raw(&server)).unwrap());
    carries_bytes_then_the_end(server, client);
}

/// The overlapped pipe's ends are duplex: each reads what the other wrote,
/// with lent buffers.
#[test]
fn an_overlapped_pipe_is_duplex_with_lent_buffers() {
    let rt = runtime();
    let (server, client) = overlapped_pipe();
    rt.block_on(async {
        let server = Handle::new(server).unwrap();
        let client = Handle::new(client).unwrap();
        let (n, _) = within(client.write_owned(b"ping".to_vec())).await;
        assert_eq!(n.unwrap(), 4);
        let (n, got) = within(server.read_owned(Vec::with_capacity(16))).await;
        assert_eq!((n.unwrap(), &got[..]), (4, &b"ping"[..]));
        let (n, _) = within(server.write_owned(b"pong".to_vec())).await;
        assert_eq!(n.unwrap(), 4);
        let (n, got) = within(client.read_owned(Vec::with_capacity(16))).await;
        assert_eq!((n.unwrap(), &got[..]), (4, &b"pong"[..]));
    });
}

/// A read that waits on a synchronous pipe holds a thread of the pool in
/// its call: handing the file back cancels that call, so it returns, and
/// the read loses no byte that came after.
#[test]
fn a_synchronous_pipes_waiting_read_is_cancelled_when_the_file_is_handed_back() {
    let rt = runtime();
    let (r, w) = synchronous_pipe();
    let reader = rt.block_on(async {
        let reader = Handle::new(r).unwrap();
        let mut buf = [0u8; 8];
        let mut waiting = Box::pin(read_some_ref(&reader, &mut buf));
        let polled = poll_fn(|cx| core::task::Poll::Ready(waiting.as_mut().poll(cx))).await;
        assert!(polled.is_pending(), "nothing was written yet");
        drop(waiting);
        // Let the job reach its call.
        crate::time::sleep(Duration::from_millis(20)).await;
        reader
    });
    let (tell, handed) = std::sync::mpsc::channel();
    std::thread::spawn(move || tell.send(reader.into_inner()).unwrap());
    let mut file = handed
        .recv_timeout(Duration::from_secs(10))
        .expect("the waiting read was never cancelled");
    (&w).write_all_now(b"after");
    let mut got = [0u8; 5];
    file.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"after");
}

/// A lent read dropped while its job waits in its call: the next read gets
/// every byte written after, none twice and none lost.
#[test]
fn a_dropped_read_on_a_synchronous_pipe_loses_nothing() {
    let rt = runtime();
    let (r, w) = synchronous_pipe();
    rt.block_on(async {
        let reader = Handle::new(r).unwrap();
        let mut lent = Box::pin(reader.read_owned(Vec::with_capacity(4)));
        let polled = poll_fn(|cx| core::task::Poll::Ready(lent.as_mut().poll(cx))).await;
        assert!(polled.is_pending(), "nothing was written yet");
        drop(lent);
        (&w).write_all_now(b"abcdefgh");
        let mut got = Vec::new();
        while got.len() < 8 {
            let (n, buf) = within(reader.read_owned(Vec::with_capacity(8))).await;
            assert!(n.unwrap() > 0);
            got.extend_from_slice(&buf);
        }
        assert_eq!(got, b"abcdefgh");
    });
}

async fn read_some_ref(r: &Handle<File>, buf: &mut [u8]) -> io::Result<usize> {
    let mut r = r;
    poll_fn(|cx| Pin::new(&mut r).poll_read(cx, buf)).await
}

/// Writes with std, from the test.
trait WriteNow {
    fn write_all_now(self, bytes: &[u8]);
}

impl WriteNow for &File {
    fn write_all_now(mut self, bytes: &[u8]) {
        std::io::Write::write_all(&mut self, bytes).unwrap();
    }
}
