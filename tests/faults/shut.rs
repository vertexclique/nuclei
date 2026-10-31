//! Our `shut` and `abort`, as the peer and our own calls see them: a shut of the writing half
//! (or of both) delivers every byte written behind before the end, and a second one answers as
//! std's second shutdown; reads after a shut of the reading half (or of both) answer as std's do
//! after its shutdown, while writes go on; an abort resets the connection, where a FIN would
//! end a cut stream as if it were whole; `into_inner` waits for the runtime's linger at most.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::time::{Duration, Instant};

use nuclei::net::TcpStream;

use crate::reset::{Writer, write_as};
use crate::support::*;

/// Writes all of `bytes` into the write-behind with plain writes, none flushed, so what
/// follows finds them behind.
async fn write_behind<S: Stream>(s: &S, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        match write_some(s, bytes).await? {
            0 => return Err(io::ErrorKind::WriteZero.into()),
            n => bytes = &bytes[n..],
        }
    }
    Ok(())
}

/// Writes `bytes` until the write-behind is full and a write waits: the bytes accepted.
async fn fill_behind<S: Stream>(s: &S, bytes: &[u8]) -> usize {
    let mut sent = 0;
    while sent < bytes.len() {
        match nuclei::time::timeout(Duration::from_millis(200), write_some(s, &bytes[sent..])).await
        {
            Ok(Ok(n)) => sent += n,
            Ok(Err(err)) => panic!("a write before the write-behind filled: {err}"),
            Err(_) => return sent,
        }
    }
    panic!("the write-behind took {sent} bytes and never filled");
}

/// Waits until every byte the std end `peer` sent reached our end's kernel. A Unix stream's
/// send lands in its peer's queue as it returns, and only Linux's answer after a shutdown of
/// the reading half depends on bytes still on their way: there a TCP sender's queue is
/// empty once the receiver's kernel acknowledged every byte.
fn delivered<S: Stream>(peer: &S::Std) {
    #[cfg(target_os = "linux")]
    if S::NAME == "tcp" {
        use std::os::fd::AsRawFd;
        let fd = (peer as &dyn std::any::Any)
            .downcast_ref::<std::net::TcpStream>()
            .map(AsRawFd::as_raw_fd)
            .unwrap();
        eventually("the peer's bytes acknowledged", || {
            let mut queued: libc::c_int = 0;
            // SAFETY: SIOCOUTQ writes one int into our own.
            let rc = unsafe { libc::ioctl(fd, libc::TIOCOUTQ, &mut queued) };
            assert_eq!(rc, 0, "{}", io::Error::last_os_error());
            queued == 0
        });
    }
    #[cfg(not(target_os = "linux"))]
    let _ = peer;
}

/// What std answers to a second shutdown of `how` on a stream it shut so already.
fn std_second_shutdown<S: Stream>(how: Shutdown) -> Answer {
    let (ours, _peer) = S::std_pair(false);
    S::shutdown_std(&ours, how).unwrap();
    Answer::of(&S::shutdown_std(&ours, how), |_| 0)
}

/// What std's reads answer after it shuts `how` of a stream, having read the first byte of
/// `bytes`, the rest still unread: what came, and the read that ended it.
fn std_reads_after<S: Stream>(how: Shutdown, bytes: &[u8]) -> (Vec<u8>, Answer) {
    let (mut ours, mut peer) = S::std_pair(false);
    peer.write_all(bytes).unwrap();
    let mut first = [0u8; 1];
    ours.read_exact(&mut first).unwrap();
    delivered::<S>(&peer);
    S::shutdown_std(&ours, how).unwrap();
    let (got, end) = std_read_until_end(&mut ours, 4096);
    drop(peer);
    (got, Answer::of(&end, |n| *n))
}

pub async fn shut_writing_delivers_every_byte_behind<S: Stream>() {
    for how in [Shutdown::Write, Shutdown::Both] {
        let bytes = pattern(21, 300_000);
        let (ours, peer) = S::pair(true);
        let what = format!("{}: bytes behind, then shut({how:?})", S::NAME);
        let reader = std::thread::spawn(move || {
            let mut peer = peer;
            let got = std_read_until_end(&mut peer, 8192);
            (peer, got)
        });
        within(&what, write_behind(&ours, &bytes)).await.unwrap();
        within(&what, ours.shut(how)).await.unwrap();
        let (peer, (got, end)) = joined(reader).await;
        same_bytes(&what, &got, &bytes);
        assert!(matches!(end, Ok(0)), "{what}: the peer's end: {end:?}");
        let again = within(&what, ours.shut(how)).await;
        assert_eq!(
            Answer::of(&again, |_| 0),
            std_second_shutdown::<S>(how),
            "{what}: a second shut"
        );
        drop(peer);
    }
}

pub async fn reads_after_a_shut_answer_as_std<S: Stream>() {
    for how in [Shutdown::Read, Shutdown::Both] {
        for unread in [0, 3000] {
            let bytes = pattern(31 + unread as u64, unread + 1);
            let what = format!(
                "{}: shut({how:?}) with {unread} bytes unread past the first",
                S::NAME
            );
            let (want, want_end) = std_reads_after::<S>(how, &bytes);
            let (ours, mut peer) = S::pair(false);
            peer.write_all(&bytes).unwrap();
            let first = within(&what, read_exact(&ours, 1)).await.unwrap();
            assert_eq!(first, bytes[..1], "{what}: the first byte");
            delivered::<S>(&peer);
            within(&what, ours.shut(how)).await.unwrap();
            let (got, end) = within(&what, read_until_end(&ours, 4096)).await;
            same_bytes(&what, &got, &want);
            assert_eq!(
                Answer::of(&end, |n| *n),
                want_end,
                "{what}: the read that ended"
            );
            drop(peer);
        }
    }
}

pub async fn writes_go_on_after_a_shut_of_the_reading_half<S: Stream>() {
    let what = format!("{}: writes after shut(Read)", S::NAME);
    let (ours, peer) = S::pair(false);
    within(&what, ours.shut(Shutdown::Read)).await.unwrap();
    let bytes = pattern(41, 100_000);
    let reader = std::thread::spawn(move || {
        let mut peer = peer;
        std_read_until_end(&mut peer, 8192)
    });
    within(&what, write_as(&ours, Writer::Plain, &bytes))
        .await
        .unwrap();
    within(&what, ours.shut(Shutdown::Write)).await.unwrap();
    let (got, end) = joined(reader).await;
    same_bytes(&what, &got, &bytes);
    assert!(matches!(end, Ok(0)), "{what}: the peer's end: {end:?}");
}

/// The kind of error std's peer reads once std closed its end with `SO_LINGER` 0.
fn std_reset_kind() -> io::ErrorKind {
    let (ours, mut peer) = TcpStream::std_pair(false);
    drop(zero_linger(ours));
    let (_, end) = std_read_until_end(&mut peer, 64);
    end.expect_err("std's peer read the end of a reset stream")
        .kind()
}

pub async fn an_abort_resets_the_connection() {
    let want = std_reset_kind();
    assert!(RESET_READ.contains(&want), "std's reset reads {want:?}");
    for behind in [false, true] {
        let what = format!("an abort with bytes behind: {behind}");
        let (ours, peer) = TcpStream::pair(true);
        let bytes = pattern(51, 4 << 20);
        let sent = if behind {
            within(&what, fill_behind(&ours, &bytes)).await
        } else {
            0
        };
        ours.abort().unwrap();
        let reader = std::thread::spawn(move || {
            let mut peer = peer;
            std_read_until_end(&mut peer, 8192)
        });
        let (got, end) = joined(reader).await;
        assert!(
            got.len() <= sent && got == bytes[..got.len()],
            "{what}: the peer read {} bytes not all sent, or changed",
            got.len()
        );
        match end {
            Err(err) => assert_eq!(err.kind(), want, "{what}: {err}"),
            Ok(n) => panic!(
                "{what}: the peer read the end ({n}) after {} bytes",
                got.len()
            ),
        }
    }
}

/// A runtime whose dropped handles linger for 200 ms at most.
fn short_linger() -> nuclei::Runtime {
    nuclei::Runtime::builder()
        .worker_threads(1)
        .pin_workers(false)
        .write_behind_linger(Duration::from_millis(200))
        .build()
        .unwrap()
}

/// How long a wait for a linger of 200 ms may take on a loaded machine: far less than the
/// default minute.
const SHORT: Duration = Duration::from_secs(20);

pub fn into_inner_waits_at_most_the_linger() {
    let rt = short_linger();
    let (ours, peer) = TcpStream::pair(true);
    let bytes = pattern(71, 4 << 20);
    rt.block_on(async {
        within("filling the write-behind", fill_behind(&ours, &bytes)).await;
        let started = Instant::now();
        let std = ours.into_inner();
        let waited = started.elapsed();
        assert!(waited < SHORT, "into_inner waited {waited:?}");
        drop(std);
    });
    drop(peer);
}

macro_rules! shut_cases {
    ($module:ident, $ty:ty) => {
        mod $module {
            use crate::support::*;

            fn run(name: &str, case: impl AsyncFnOnce()) {
                if alone(name) {
                    let rt = runtime();
                    rt.block_on(case());
                    rt.block_on(round_trip());
                }
            }

            #[test]
            fn a_shut_of_the_writing_half_delivers_every_byte_behind() {
                run(
                    concat!(
                        "shut::",
                        stringify!($module),
                        "::a_shut_of_the_writing_half_delivers_every_byte_behind"
                    ),
                    super::shut_writing_delivers_every_byte_behind::<$ty>,
                );
            }

            #[test]
            fn reads_after_a_shut_of_the_reading_half_answer_as_std() {
                run(
                    concat!(
                        "shut::",
                        stringify!($module),
                        "::reads_after_a_shut_of_the_reading_half_answer_as_std"
                    ),
                    super::reads_after_a_shut_answer_as_std::<$ty>,
                );
            }

            #[test]
            fn writes_go_on_after_a_shut_of_the_reading_half() {
                run(
                    concat!(
                        "shut::",
                        stringify!($module),
                        "::writes_go_on_after_a_shut_of_the_reading_half"
                    ),
                    super::writes_go_on_after_a_shut_of_the_reading_half::<$ty>,
                );
            }

            #[test]
            fn shuts_hold_nothing() {
                let name = concat!("shut::", stringify!($module), "::shuts_hold_nothing");
                if alone(name) {
                    holds_steady(name, 3, || async {
                        super::shut_writing_delivers_every_byte_behind::<$ty>().await;
                        super::writes_go_on_after_a_shut_of_the_reading_half::<$ty>().await;
                    });
                }
            }
        }
    };
}

shut_cases!(tcp, nuclei::net::TcpStream);
#[cfg(unix)]
shut_cases!(unix, nuclei::net::UnixStream);

mod abort {
    use crate::support::*;

    #[test]
    fn an_abort_resets_the_connection() {
        let name = "shut::abort::an_abort_resets_the_connection";
        if alone(name) {
            let rt = runtime();
            rt.block_on(super::an_abort_resets_the_connection());
            rt.block_on(round_trip());
        }
    }

    #[test]
    fn into_inner_waits_at_most_the_linger() {
        if alone("shut::abort::into_inner_waits_at_most_the_linger") {
            super::into_inner_waits_at_most_the_linger();
        }
    }

    #[test]
    fn aborts_hold_nothing() {
        let name = "shut::abort::aborts_hold_nothing";
        if alone(name) {
            holds_steady(name, 3, super::an_abort_resets_the_connection);
        }
    }
}
