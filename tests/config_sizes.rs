//! The builder's sizes: a setting below what a runtime can work with makes
//! `build` fail with `InvalidInput` naming it; the boundary values start a
//! runtime.

use std::io::ErrorKind;
use std::time::Duration;

use nuclei::runtime::Builder;

fn base() -> Builder {
    Builder::new().worker_threads(1).pin_workers(false)
}

/// The least `io_buffer_cap`: on io_uring the default receive group (256
/// buffers of 16 KiB and their 16-byte descriptors, each mapped in whole
/// pages) and one 512-byte buffer beside it; elsewhere 0. Mirrored from the
/// runtime, and held to it from both sides below.
fn least_cap() -> usize {
    if nuclei::Proactor::backend() != nuclei::IoBackend::IoUring {
        return 0;
    }
    #[cfg(target_os = "linux")]
    {
        let config = nuclei::config::IoUringConfiguration::default();
        assert_eq!((config.recv_buffers, config.recv_buffer_size), (256, 16384));
        // SAFETY: a plain query.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) } as usize;
        let pages = |bytes: usize| bytes.div_ceil(page) * page;
        pages(256 * 16) + pages(256 * 16384) + 512
    }
    #[cfg(not(target_os = "linux"))]
    unreachable!("io_uring on a system that is not Linux")
}

#[test]
fn a_size_below_its_minimum_is_refused_naming_the_setting() {
    let mut refused: Vec<(&str, Builder)> = vec![
        ("thread_stack_size", base().thread_stack_size(0)),
        (
            "thread_stack_size",
            base().thread_stack_size(128 * 1024 - 1),
        ),
        ("max_nesting", base().max_nesting(0)),
        ("max_blocking_threads", base().max_blocking_threads(0)),
        ("handoff_after", base().handoff_after(Some(Duration::ZERO))),
        (
            "handoff_after",
            base().handoff_after(Some(Duration::from_micros(999))),
        ),
    ];
    let least = least_cap();
    if least > 0 {
        refused.push(("io_buffer_cap", base().io_buffer_cap(least - 1)));
    }
    for (setting, builder) in refused {
        let Err(err) = builder.build() else {
            panic!("a runtime started for {setting}");
        };
        assert_eq!(err.kind(), ErrorKind::InvalidInput, "{err}");
        assert!(err.to_string().contains(setting), "{err}");
    }
}

#[test]
fn the_boundary_values_and_the_defaults_start_a_runtime() {
    let accepted = [
        base(),
        base().thread_stack_size(128 * 1024),
        base().max_nesting(1),
        base().max_blocking_threads(1),
        base().max_worker_threads(1),
        base().worker_threads(2).max_worker_threads(1),
        base().handoff_after(Some(Duration::from_millis(1))),
        base().handoff_after(None),
        Builder::new().worker_threads(0).max_worker_threads(0),
        base().io_buffer_cap(least_cap()),
        base().local_spawn_margin(0),
    ];
    for builder in accepted {
        let rt = builder.build().expect("the runtime starts");
        assert_eq!(rt.block_on(async { 7 }), 7);
    }
}

#[test]
fn the_smallest_stack_runs_a_timer_and_a_connect_on_the_worker() {
    let rt = base()
        .thread_stack_size(128 * 1024)
        .build()
        .expect("the runtime starts");
    let task = rt.spawn(async {
        nuclei::time::sleep(Duration::from_millis(1)).await;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        nuclei::Handle::<std::net::TcpStream>::connect(addr)
            .await
            .expect("the worker connects");
    });
    rt.block_on(task);
}
