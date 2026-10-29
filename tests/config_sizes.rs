//! The builder's sizes: a setting below what a runtime can work with makes
//! `build` fail with `InvalidInput` naming it; the boundary values start a
//! runtime.

use std::io::ErrorKind;
use std::time::Duration;

use nuclei::runtime::Builder;

fn base() -> Builder {
    Builder::new().worker_threads(1).pin_workers(false)
}

#[test]
fn a_size_below_its_minimum_is_refused_naming_the_setting() {
    let refused: Vec<(&str, Builder)> = vec![
        ("thread_stack_size", base().thread_stack_size(0)),
        ("thread_stack_size", base().thread_stack_size(64 * 1024 - 1)),
        ("max_nesting", base().max_nesting(0)),
        ("max_blocking_threads", base().max_blocking_threads(0)),
        ("handoff_after", base().handoff_after(Some(Duration::ZERO))),
        (
            "handoff_after",
            base().handoff_after(Some(Duration::from_micros(999))),
        ),
    ];
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
        base().thread_stack_size(64 * 1024),
        base().max_nesting(1),
        base().max_blocking_threads(1),
        base().max_worker_threads(1),
        base().worker_threads(2).max_worker_threads(1),
        base().handoff_after(Some(Duration::from_millis(1))),
        base().handoff_after(None),
        Builder::new().worker_threads(0).max_worker_threads(0),
        base().io_buffer_cap(0),
        base().local_spawn_margin(0),
    ];
    for builder in accepted {
        let rt = builder.build().expect("the runtime starts");
        assert_eq!(rt.block_on(async { 7 }), 7);
    }
}
