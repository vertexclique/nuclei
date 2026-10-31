//! The builder's settings checked in `build`: each setting a runtime cannot
//! work with is refused with `InvalidInput` naming it, the boundary values
//! start a runtime, `worker_cores` and `blocking_cores` place the threads, and
//! `io_uring` settings are per runtime.

use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use nuclei::config::IoUringConfiguration;
use nuclei::runtime::Builder;

fn base() -> Builder {
    Builder::new().worker_threads(1).pin_workers(false)
}

#[cfg(target_os = "linux")]
fn affinity() -> Vec<usize> {
    // SAFETY: a zeroed set of our own, filled by the call.
    unsafe {
        let mut set: libc::cpu_set_t = core::mem::zeroed();
        assert_eq!(
            libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &mut set),
            0
        );
        (0..libc::CPU_SETSIZE as usize)
            .filter(|&cpu| libc::CPU_ISSET(cpu, &set))
            .collect()
    }
}

#[cfg(target_os = "linux")]
fn allowed_cores() -> Vec<usize> {
    affinity()
}

#[cfg(not(target_os = "linux"))]
fn allowed_cores() -> Vec<usize> {
    vec![0]
}

fn no_cores() -> Vec<usize> {
    Vec::new()
}

#[test]
fn a_setting_a_runtime_cannot_work_with_is_refused_naming_it() {
    let small_ring = IoUringConfiguration {
        queue_len: 1,
        ..Default::default()
    };
    let odd_group = IoUringConfiguration {
        recv_buffers: 3,
        ..Default::default()
    };
    let tiny_buffers = IoUringConfiguration {
        recv_buffer_size: 100,
        ..Default::default()
    };
    let refused: Vec<(&str, Builder)> = vec![
        ("thread_stack_size", base().thread_stack_size(1)),
        ("max_nesting", base().max_nesting(0)),
        ("max_blocking_threads", base().max_blocking_threads(0)),
        (
            "handoff_after",
            base().handoff_after(Some(Duration::from_micros(500))),
        ),
        ("event_interval", base().event_interval(0)),
        ("global_queue_interval", base().global_queue_interval(0)),
        ("max_io_events_per_tick", base().max_io_events_per_tick(0)),
        (
            "max_worker_threads",
            base().worker_threads(2).max_worker_threads(1),
        ),
        ("worker_cores", base().worker_cores(no_cores())),
        ("worker_cores", base().worker_cores([100_000])),
        ("blocking_cores", base().blocking_cores(no_cores())),
        ("blocking_cores", base().blocking_cores([100_000])),
        ("queue_len", base().io_uring(small_ring)),
        ("recv_buffers", base().io_uring(odd_group)),
        ("recv_buffer_size", base().io_uring(tiny_buffers)),
    ];
    for (setting, builder) in refused {
        let Err(err) = builder.build() else {
            panic!("a runtime started for {setting}");
        };
        assert_eq!(err.kind(), ErrorKind::InvalidInput, "{setting}: {err}");
        assert!(err.to_string().contains(setting), "{setting}: {err}");
    }
}

#[test]
fn a_setter_only_records_so_a_mistake_waits_for_build() {
    let builder = Builder::new()
        .thread_stack_size(0)
        .max_nesting(0)
        .max_blocking_threads(0)
        .event_interval(0)
        .global_queue_interval(0)
        .max_io_events_per_tick(0)
        .worker_cores(no_cores())
        .blocking_cores(no_cores());
    assert!(builder.build().is_err());
}

#[test]
fn max_worker_threads_holds_an_unset_worker_count_and_leaves_a_set_one() {
    let one = Builder::new()
        .pin_workers(false)
        .max_worker_threads(1)
        .build()
        .expect("the runtime starts");
    assert_eq!(one.handle().workers(), 1);
    assert_eq!(one.block_on(one.spawn(async { 7 })), 7);

    let cores = std::thread::available_parallelism().map_or(1, usize::from);
    let held = Builder::new()
        .pin_workers(false)
        .max_worker_threads(3)
        .build()
        .expect("the runtime starts");
    assert_eq!(held.handle().workers(), cores.min(3));

    let exact = base()
        .worker_threads(2)
        .max_worker_threads(2)
        .build()
        .expect("the runtime starts");
    assert_eq!(exact.handle().workers(), 2);
}

#[test]
fn a_worker_core_the_process_may_use_starts_one_worker_pinned_or_not() {
    let core = allowed_cores()[0];
    for pin in [true, false] {
        let rt = Builder::new()
            .pin_workers(pin)
            .worker_cores([core])
            .build()
            .expect("the runtime starts");
        assert_eq!(rt.handle().workers(), 1, "pin_workers({pin})");
        #[cfg(target_os = "linux")]
        assert_eq!(
            rt.block_on(rt.spawn(async { affinity() })),
            [core],
            "pin_workers({pin})"
        );
        #[cfg(not(target_os = "linux"))]
        assert_eq!(rt.block_on(rt.spawn(async { 1 })), 1);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn pinned_workers_take_the_listed_cores_in_order() {
    let cores = allowed_cores();
    if cores.len() < 2 {
        eprintln!("skipped: needs two allowed cores, have {}", cores.len());
        return;
    }
    let listed = [cores[1], cores[0]];
    let rt = Builder::new()
        .pin_workers(true)
        .worker_cores(listed)
        .build()
        .expect("the runtime starts");
    let h = rt.handle();
    assert_eq!(h.workers(), 2);
    for (worker, core) in listed.into_iter().enumerate() {
        let task = h
            .spawn_on_worker(worker, async { affinity() })
            .expect("the worker takes tasks");
        assert_eq!(rt.block_on(task), [core], "worker {worker}");
    }
}

#[test]
fn blocking_cores_place_the_pool_threads() {
    let core = allowed_cores()[0];
    let rt = base()
        .blocking_cores([core])
        .build()
        .expect("the runtime starts");
    #[cfg(target_os = "linux")]
    assert_eq!(rt.block_on(rt.spawn_blocking(affinity)), [core]);
    #[cfg(not(target_os = "linux"))]
    assert_eq!(rt.block_on(rt.spawn_blocking(|| 1)), 1);
}

fn echo_on(rt: &nuclei::Runtime) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let client = rt.spawn(async move {
        let stream = nuclei::Handle::<TcpStream>::connect(addr).await.unwrap();
        stream.send(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        let mut got = 0;
        while got < buf.len() {
            got += stream.recv(&mut buf[got..]).await.unwrap();
        }
        buf
    });
    let (mut peer, _) = listener.accept().unwrap();
    let mut heard = [0u8; 4];
    peer.read_exact(&mut heard).unwrap();
    peer.write_all(&heard).unwrap();
    assert_eq!(&rt.block_on(client), b"ping");
}

#[test]
fn a_valid_io_uring_configuration_builds_and_runs_io() {
    let valid = [
        IoUringConfiguration::interrupt_driven(256),
        IoUringConfiguration {
            queue_len: 128,
            recv_buffers: 0,
            ..Default::default()
        },
        IoUringConfiguration {
            queue_len: 64,
            recv_buffers: 64,
            recv_buffer_size: 512,
            ..Default::default()
        },
    ];
    for config in valid {
        let rt = base()
            .io_uring(config.clone())
            .build()
            .unwrap_or_else(|err| {
                panic!("{config:?}: {err}");
            });
        assert_eq!(rt.block_on(rt.spawn(async { 3 })), 3);
        echo_on(&rt);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn an_io_uring_runtime_runs_its_workers_on_rings() {
    use nuclei::{IoBackend, Proactor};
    if Proactor::backend() != IoBackend::IoUring {
        eprintln!("skipped: the process does not run io_uring");
        return;
    }
    let rt = base()
        .io_uring(IoUringConfiguration::interrupt_driven(256))
        .build()
        .expect("the runtime starts");
    assert_eq!(rt.block_on(rt.spawn(async { 1 })), 1);
    let stats = rt.stats();
    assert_eq!(stats.shards.len(), 1);
    assert_eq!(stats.shards[0].backend, IoBackend::IoUring);
}

#[test]
fn the_io_uring_settings_are_checked_on_every_platform_and_per_runtime() {
    let bad = IoUringConfiguration {
        queue_len: 1,
        ..Default::default()
    };
    assert!(base().io_uring(bad).build().is_err());
    let ok = base().build().expect("another runtime is unaffected");
    assert_eq!(ok.block_on(ok.spawn(async { 2 })), 2);
}
