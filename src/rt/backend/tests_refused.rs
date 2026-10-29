//! A kernel that refuses a feature a shard's ring needs (provided buffer
//! rings, as Ubuntu's 6.8 kernels from 6.8.0-136 on do) is told apart from a
//! configuration mistake, and never stops a runtime from starting: `auto`
//! runs the poller, and a named io_uring fails with the feature's name. The
//! refusal is made by a seccomp filter answering the registration with
//! `EINVAL`, the kernel's own answer there.

use std::io::ErrorKind;
use std::net::{TcpListener, TcpStream};

use super::{Backend, IoBackend, decide, usable, usable_with};
use crate::Handle;
use crate::rt::shard::ring::ShardRing;
use crate::syscore::linux::iouring::ring::buf_ring::BufRing;
use crate::syscore::linux::iouring::ring::{Ring, RingConfig};
use crate::syscore::linux::probe::tests::{
    found_or_skip, install, refuse_buffer_rings, without_buffer_rings,
};
use crate::syscore::linux::probe::{Probe, probe};

const FEATURE: &str = "provided buffer rings";

/// A shard's ring configuration with a receive group of `buffers`.
fn config(buffers: u16) -> RingConfig {
    RingConfig {
        recv_buffers: buffers,
        recv_buffer_size: 4096,
        ..RingConfig::new(64)
    }
}

/// Whether the kernel under test gives buffer rings when nothing refuses them.
fn kernel_has_buffer_rings() -> bool {
    Ring::new(&RingConfig::new(8)).is_ok_and(|ring| BufRing::new(&ring, 0, 8, 4096).is_ok())
}

/// A connected pair on the runtime's own IO, and a word sent there and back.
async fn round_trip() {
    let listener = Handle::<TcpListener>::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.get_ref().local_addr().unwrap();
    let client = Handle::<TcpStream>::connect(addr).await.unwrap();
    let (server, _) = listener.accept().await.unwrap();
    for (from, to) in [(&client, &server), (&server, &client)] {
        from.send(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        let mut got = 0;
        while got < buf.len() {
            let n = to.recv(&mut buf[got..]).await.unwrap();
            assert_ne!(n, 0, "the peer closed before the word arrived");
            got += n;
        }
        assert_eq!(&buf, b"ping");
    }
}

/// Builds a runtime that names no backend, checks every shard runs the
/// poller, and runs a TCP round trip on its worker.
fn default_runtime_runs_the_poller() {
    let rt = crate::runtime::Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .build()
        .expect("a runtime that names no backend starts on a kernel that refuses a ring feature");
    let backend = rt.block_on(async {
        crate::runtime::spawn(async {
            round_trip().await;
            crate::Proactor::backend()
        })
        .await
    });
    assert_eq!(backend, IoBackend::Epoll);
    let stats = crate::stats::stats();
    assert!(!stats.shards.is_empty());
    assert!(stats.shards.iter().all(|s| s.backend == IoBackend::Epoll));
}

/// The buffer ring registration the kernel refuses is `Unsupported` and
/// names the feature; nuclei's own checks stay `InvalidInput`, before the
/// call.
#[test]
fn a_buffer_ring_the_kernel_refuses_is_unsupported() {
    let Some(has) = without_buffer_rings(|| {
        let ring = Ring::new(&RingConfig::new(8)).unwrap();
        let refused = BufRing::new(&ring, 0, 8, 4096).unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::Unsupported, "{refused}");
        assert!(refused.to_string().contains(FEATURE), "{refused}");
        let mistake = BufRing::new(&ring, 0, 3, 4096).unwrap_err();
        assert_eq!(mistake.kind(), ErrorKind::InvalidInput, "{mistake}");
        kernel_has_buffer_rings()
    }) else {
        return;
    };
    assert!(!has, "the filter let a buffer ring through");
    if kernel_has_buffer_rings() {
        let ring = Ring::new(&RingConfig::new(8)).unwrap();
        assert!(BufRing::new(&ring, 0, 8, 4096).is_ok());
    } else {
        eprintln!("this kernel refuses buffer rings of its own accord");
    }
}

/// A shard's ring on such a kernel fails `Unsupported` while it asks for a
/// receive group, builds without one, and still calls a bad group size the
/// configuration's mistake.
#[test]
fn a_shard_ring_tells_a_refusal_from_a_mistake() {
    let Some(()) = without_buffer_rings(|| {
        let Err(refused) = ShardRing::new(&config(256), 64) else {
            panic!("a ring with a group was made past a refused registration");
        };
        assert_eq!(refused.kind(), ErrorKind::Unsupported, "{refused}");
        assert!(refused.to_string().contains(FEATURE), "{refused}");
        assert!(ShardRing::new(&config(0), 64).is_ok());
        let Err(mistake) = ShardRing::new(&config(3), 64) else {
            panic!("a ring with a group of 3 buffers was made");
        };
        assert_eq!(mistake.kind(), ErrorKind::InvalidInput, "{mistake}");
        assert!(
            mistake
                .to_string()
                .contains("recv_buffers and recv_buffer_size"),
            "{mistake}"
        );
    }) else {
        return;
    };
}

/// The choice of the backend sees the refusal: not usable with a group,
/// usable without; `auto` picks the poller, a named io_uring fails with the
/// feature.
#[test]
fn the_backend_choice_sees_a_refused_buffer_ring() {
    let Some(()) = without_buffer_rings(|| {
        let why = usable_with(config(256)).unwrap_err();
        assert!(why.contains(FEATURE), "{why}");
        assert_eq!(usable_with(config(0)), Ok(()));
        let refused = || usable_with(config(256));
        assert!(matches!(decide(None, None, refused), Ok(Backend::Poll)));
        let err = decide(None, Some(IoBackend::IoUring), refused).unwrap_err();
        assert!(err.contains(FEATURE), "{err}");
        let err = decide(Some("io_uring"), None, refused).unwrap_err();
        assert!(err.contains(FEATURE), "{err}");
        assert!(matches!(
            decide(None, None, || usable_with(config(0))),
            Ok(Backend::RingOrPoll(_))
        ));
    }) else {
        return;
    };
}

/// A process on such a kernel, alone in its own: a runtime that names no
/// backend starts on the poller and serves TCP; one that names io_uring
/// fails to build, naming the missing feature and not a configuration
/// mistake.
#[test]
fn a_runtime_on_a_kernel_without_buffer_rings_runs_the_poller() {
    if !crate::alone::alone(
        "rt::backend::tests_refused::a_runtime_on_a_kernel_without_buffer_rings_runs_the_poller",
    ) {
        return;
    }
    if std::env::var("NUCLEI_BACKEND").is_ok_and(|b| !b.is_empty()) {
        return;
    }
    if !found_or_skip(&probe()) {
        return;
    }
    if let Err(e) = install(refuse_buffer_rings()) {
        eprintln!("skipping: cannot install a seccomp filter here: {e}");
        return;
    }
    default_runtime_runs_the_poller();
    let Err(err) = crate::runtime::Builder::new()
        .worker_threads(1)
        .pin_workers(false)
        .backend(IoBackend::IoUring)
        .build()
    else {
        panic!(
            "a runtime that asked for io_uring by name started on a kernel that refuses a ring feature"
        );
    };
    assert_eq!(err.kind(), ErrorKind::Unsupported, "{err}");
    assert!(err.to_string().contains(FEATURE), "{err}");
    assert!(
        !err.to_string()
            .contains("recv_buffers and recv_buffer_size"),
        "{err}"
    );
}

/// For a job on a real distribution kernel that refuses a ring feature:
/// `NUCLEI_EXPECT_REFUSED` names it, and this process must find io_uring
/// unusable for that reason, with its default runtime on the poller. Does
/// nothing where the variable is unset.
#[test]
fn a_kernel_expected_to_refuse_a_feature_gives_the_poller() {
    let Some(refused) = std::env::var("NUCLEI_EXPECT_REFUSED")
        .ok()
        .filter(|m| !m.is_empty())
    else {
        return;
    };
    if std::env::var("NUCLEI_BACKEND").is_ok_and(|b| !b.is_empty() && b != "auto") {
        return;
    }
    assert_eq!(
        probe(),
        Probe::Available,
        "the kernel has no io_uring at all"
    );
    let why =
        usable().expect_err("io_uring was found usable on a kernel expected to refuse a feature");
    assert!(why.contains(&refused), "{why}");
    default_runtime_runs_the_poller();
}
