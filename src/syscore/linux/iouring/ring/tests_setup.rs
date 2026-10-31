//! Setup policy and ring option tests: flag selection, the fallbacks, kernel features, probing,
//! worker limits, disabled rings and the kernel submission thread.

use std::{
    io::{self, ErrorKind},
    time::Duration,
};

use super::{tests::*, *};

// ---------------------------------------------------------------------------------------------
// Setup policy.

const FLOOR: u32 = IORING_SETUP_SUBMIT_ALL | IORING_SETUP_CLAMP | IORING_SETUP_CQSIZE;

#[test]
fn flag_selection_without_a_submission_thread() {
    let cfg = RingConfig::new(8);
    let want = FLOOR | IORING_SETUP_SINGLE_ISSUER | IORING_SETUP_DEFER_TASKRUN;
    assert_eq!(setup_flags(&cfg, true), want | IORING_SETUP_NO_SQARRAY);
    assert_eq!(setup_flags(&cfg, false), want);
}

#[test]
fn flag_selection_with_a_submission_thread() {
    let mut cfg = RingConfig::new(8);
    cfg.sqpoll_idle_ms = Some(3);
    let f = setup_flags(&cfg, true);
    assert_eq!(f, FLOOR | IORING_SETUP_SQPOLL | IORING_SETUP_NO_SQARRAY);
    let clash = IORING_SETUP_DEFER_TASKRUN
        | IORING_SETUP_COOP_TASKRUN
        | IORING_SETUP_TASKRUN_FLAG
        | IORING_SETUP_SINGLE_ISSUER;
    assert_eq!(
        f & clash,
        0,
        "SQPOLL cannot be combined with task-work flags"
    );
    assert_eq!(setup_flags(&cfg, false) & IORING_SETUP_NO_SQARRAY, 0);
}

#[test]
fn flag_selection_options_add_their_flags() {
    let mut cfg = RingConfig::new(8);
    cfg.iopoll = true;
    cfg.start_disabled = true;
    let f = setup_flags(&cfg, false);
    assert_eq!(f & IORING_SETUP_IOPOLL, IORING_SETUP_IOPOLL);
    assert_eq!(f & IORING_SETUP_R_DISABLED, IORING_SETUP_R_DISABLED);
    assert_eq!(f & FLOOR, FLOOR);
}

#[test]
fn completion_queue_is_at_least_twice_the_submission_queue() {
    let mut cfg = RingConfig::new(8);
    assert_eq!(setup_params(&cfg, true).cq_entries, 16);
    cfg.cq_entries = Some(4);
    assert_eq!(setup_params(&cfg, true).cq_entries, 16);
    cfg.cq_entries = Some(1000);
    assert_eq!(setup_params(&cfg, true).cq_entries, 1000);
    assert_eq!(
        setup_params(&RingConfig::new(u32::MAX), true).cq_entries,
        u32::MAX
    );
}

#[test]
fn sqpoll_idle_time_reaches_the_params() {
    let mut cfg = RingConfig::new(8);
    cfg.sqpoll_idle_ms = Some(7);
    assert_eq!(setup_params(&cfg, true).sq_thread_idle, 7);
}

fn einval() -> io::Error {
    io::Error::from_raw_os_error(libc::EINVAL)
}

#[test]
fn setup_falls_back_once_without_the_index_array_flag() {
    let cfg = RingConfig::new(8);
    let mut seen = Vec::new();
    let (value, params) = negotiate(&cfg, |entries, p| {
        seen.push((entries, p.flags));
        if p.flags & IORING_SETUP_NO_SQARRAY != 0 {
            Err(einval())
        } else {
            Ok("ring")
        }
    })
    .unwrap();
    assert_eq!(value, "ring");
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[0], (8, setup_flags(&cfg, true)));
    assert_eq!(seen[1], (8, setup_flags(&cfg, false)));
    assert_eq!(params.flags, setup_flags(&cfg, false));
}

#[test]
fn setup_takes_the_preferred_flags_when_the_kernel_accepts_them() {
    let cfg = RingConfig::new(8);
    let mut calls = 0;
    let (_, params) = negotiate(&cfg, |_, _| {
        calls += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(calls, 1);
    assert_eq!(params.flags, setup_flags(&cfg, true));
}

#[test]
fn setup_reports_missing_kernel_features_after_two_refusals() {
    let mut calls = 0;
    let err = negotiate::<()>(&RingConfig::new(8), |_, _| {
        calls += 1;
        Err(einval())
    })
    .unwrap_err();
    assert_eq!(calls, 2);
    assert_eq!(err.kind(), ErrorKind::Unsupported);
    assert!(
        err.to_string().contains("lacks the io_uring features"),
        "{err}"
    );
    let refusal = err.get_ref().and_then(|e| e.downcast_ref::<Refusal>());
    assert_eq!(refusal, Some(&Refusal::FloorFlags));
}

#[test]
fn setup_does_not_retry_other_errors() {
    let mut calls = 0;
    let err = negotiate::<()>(&RingConfig::new(8), |_, _| {
        calls += 1;
        Err(io::Error::from_raw_os_error(libc::EPERM))
    })
    .unwrap_err();
    assert_eq!((calls, err.raw_os_error()), (1, Some(libc::EPERM)));

    calls = 0;
    let err = negotiate::<()>(&RingConfig::new(8), |_, _| {
        calls += 1;
        Err(io::Error::from_raw_os_error(libc::ENOSYS))
    })
    .unwrap_err();
    assert_eq!((calls, err.kind()), (1, ErrorKind::Unsupported));
    let refusal = err.get_ref().and_then(|e| e.downcast_ref::<Refusal>());
    assert_eq!(refusal, Some(&Refusal::NotInKernel));
}

#[test]
fn zero_entries_are_rejected_before_any_syscall() {
    let err = Ring::new(&RingConfig::new(0)).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::InvalidInput);
}

#[test]
fn config_maps_from_the_public_configuration() {
    let c = IoUringConfiguration::interrupt_driven(128);
    let r = RingConfig::from(&c);
    assert_eq!((r.entries, r.sqpoll_idle_ms, r.iopoll), (128, None, false));
    assert_eq!(r.iowq_max_workers, None);

    // The default is the interrupt-driven profile (no SQPOLL thread), with
    // the io-wq limits set.
    let r = RingConfig::from(&IoUringConfiguration::default());
    assert_eq!(r.entries, 1 << 11);
    assert_eq!(r.sqpoll_idle_ms, None);
    assert!(!r.iopoll);
    assert_eq!(r.iowq_max_workers, Some([256, 512]));

    let c = IoUringConfiguration {
        per_numa_bounded_worker_count: None,
        per_numa_unbounded_worker_count: Some(7),
        ..IoUringConfiguration::io_poll(64)
    };
    let r = RingConfig::from(&c);
    assert!(r.iopoll);
    assert_eq!(r.iowq_max_workers, Some([0, 7]));
}

#[test]
fn ring_is_created_with_the_floor_flags_and_features() {
    let Some(ring) = ring_or_skip() else { return };
    let flags = ring.flags();
    let want = FLOOR | IORING_SETUP_SINGLE_ISSUER | IORING_SETUP_DEFER_TASKRUN;
    assert_eq!(flags & want, want);
    for (bit, name) in REQUIRED_FEATURES {
        assert_ne!(ring.features() & bit, 0, "missing {name}");
    }
    assert_eq!((ring.sq_entries(), ring.cq_entries()), (64, 128));
    assert_eq!(ring.sq_space(), 64);
}

#[test]
fn ring_works_with_the_submission_index_array() {
    // A kernel older than 6.6 rejects NO_SQARRAY: the fallback ring keeps the identity index array.
    let cfg = RingConfig::new(8);
    let setup = |e, p: &mut IoUringParams| {
        if p.flags & IORING_SETUP_NO_SQARRAY != 0 {
            Err(einval())
        } else {
            io_uring_setup(e, p)
        }
    };
    let Some(mut ring) = Ring::with_setup(&cfg, setup).map_or_else(unavailable, Some) else {
        return;
    };
    assert_eq!(ring.flags() & IORING_SETUP_NO_SQARRAY, 0);
    for i in 0..20u64 {
        push(&mut ring, Sqe::nop().user_data(i));
        ring.submit_and_wait(1, None).unwrap();
        let got: Vec<_> = ring.completions().collect();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].user_data, i);
    }
}

// ---------------------------------------------------------------------------------------------
// Ring options.

#[test]
fn probe_reports_every_opcode_the_builders_use() {
    let Some(ring) = ring_or_skip() else { return };
    let set = ring.register_probe().unwrap();
    // Waitid needs Linux 6.7, newer than the 6.1 floor: its test probes for it on its own.
    use Opcode::*;
    for op in [
        Nop,
        Read,
        Write,
        Readv,
        Writev,
        Openat,
        Close,
        Statx,
        Fsync,
        Fallocate,
        Accept,
        Connect,
        Send,
        Recv,
        Sendmsg,
        Recvmsg,
        Shutdown,
        Socket,
        Timeout,
        TimeoutRemove,
        AsyncCancel,
        PollAdd,
        PollRemove,
        MsgRing,
    ] {
        assert!(set.contains(op), "kernel does not implement {op:?}");
    }
}

#[test]
fn opcode_set_tracks_raw_opcodes() {
    let mut set = OpcodeSet::default();
    assert!(!set.contains(Opcode::Nop));
    set.insert(Opcode::Nop as u8);
    set.insert(Opcode::Waitid as u8);
    set.insert(255);
    assert!(set.contains(Opcode::Nop) && set.contains(Opcode::Waitid));
    assert!(!set.contains(Opcode::Read));
}

#[test]
fn iowq_worker_limits_round_trip() {
    let Some(ring) = ring_or_skip() else { return };
    let before = ring.register_iowq_max_workers([0, 0]).unwrap();
    assert!(before[0] > 0 && before[1] > 0, "{before:?}");
    assert_eq!(ring.register_iowq_max_workers([2, 3]).unwrap(), before);
    assert_eq!(ring.register_iowq_max_workers([0, 0]).unwrap(), [2, 3]);
}

#[test]
fn configured_worker_limits_apply_at_creation() {
    let mut cfg = RingConfig::new(8);
    cfg.iowq_max_workers = Some([2, 5]);
    let Some(ring) = ring_with_or_skip(cfg) else {
        return;
    };
    assert_eq!(ring.register_iowq_max_workers([0, 0]).unwrap(), [2, 5]);
}

#[test]
fn disabled_ring_refuses_work_until_enabled() {
    let mut cfg = RingConfig::new(8);
    cfg.start_disabled = true;
    cfg.iowq_max_workers = Some([3, 4]);
    let Some(mut ring) = ring_with_or_skip(cfg) else {
        return;
    };
    push(&mut ring, Sqe::nop().user_data(5));
    assert!(
        ring.submit().is_err(),
        "a disabled ring must refuse to submit"
    );
    ring.enable().unwrap();
    let cqes = reap(&mut ring, 1);
    assert_eq!((cqes[0].user_data, cqes[0].res), (5, 0));
    assert_eq!(ring.register_iowq_max_workers([0, 0]).unwrap(), [3, 4]);
}

#[test]
fn polled_ring_can_be_created() {
    let mut cfg = RingConfig::new(8);
    cfg.iopoll = true;
    let Some(ring) = ring_with_or_skip(cfg) else {
        return;
    };
    assert_eq!(ring.flags() & IORING_SETUP_IOPOLL, IORING_SETUP_IOPOLL);
}

#[test]
fn submission_thread_ring_round_trips_and_wakes() {
    let mut cfg = RingConfig::new(8);
    cfg.sqpoll_idle_ms = Some(2);
    let Some(mut ring) = ring_with_or_skip(cfg) else {
        return;
    };
    assert_ne!(ring.flags() & IORING_SETUP_SQPOLL, 0);
    assert_eq!(
        ring.flags() & (IORING_SETUP_DEFER_TASKRUN | IORING_SETUP_SINGLE_ISSUER),
        0
    );

    push(&mut ring, Sqe::nop().user_data(1));
    ring.submit_and_wait(1, Some(Duration::from_secs(5)))
        .unwrap();
    assert_eq!(
        ring.completions().map(|c| c.user_data).collect::<Vec<_>>(),
        [1]
    );

    // Once the thread idles out it sleeps; a plain submit must wake it with no help from a wait.
    wait_until("the submission thread to sleep", || {
        ring.sq_thread_asleep().then_some(())
    });
    push(&mut ring, Sqe::nop().user_data(2));
    ring.submit().unwrap();
    let cqe = wait_until("the woken thread's completion", || {
        ring.completions().next()
    });
    assert_eq!((cqe.user_data, cqe.res), (2, 0));
}
