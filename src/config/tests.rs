//! The io_uring sizes: every setting outside what a ring can work with is
//! refused as the configuration's mistake, its boundary values and the
//! defaults are accepted.

use std::io::ErrorKind;

use super::{IoUringConfiguration, MAX_RECV_BUFFER_SIZE, MIN_QUEUE_LEN, MIN_RECV_BUFFER_SIZE};

fn with(change: impl FnOnce(&mut IoUringConfiguration)) -> IoUringConfiguration {
    let mut config = IoUringConfiguration::default();
    change(&mut config);
    config
}

#[test]
fn the_defaults_and_the_profiles_pass() {
    assert!(IoUringConfiguration::default().validate().is_ok());
    for config in [
        IoUringConfiguration::interrupt_driven(2048),
        IoUringConfiguration::low_latency_driven(2048),
        IoUringConfiguration::kernel_poll_only(2048),
        IoUringConfiguration::io_poll(2048),
    ] {
        assert!(config.validate().is_ok(), "{config:?}");
    }
}

#[test]
fn a_profile_fits_its_receive_group_to_its_queue() {
    for (queue_len, buffers) in [(64, 64), (100, 128), (128, 128), (129, 256), (2048, 256)] {
        for config in [
            IoUringConfiguration::interrupt_driven(queue_len),
            IoUringConfiguration::low_latency_driven(queue_len),
            IoUringConfiguration::kernel_poll_only(queue_len),
            IoUringConfiguration::io_poll(queue_len),
        ] {
            assert_eq!(config.recv_buffers, buffers, "{config:?}");
            assert!(config.validate().is_ok(), "{config:?}");
        }
    }
}

#[test]
fn a_size_outside_its_range_is_refused_naming_the_setting_and_its_boundaries_pass() {
    let refused: Vec<(&str, IoUringConfiguration)> = vec![
        ("queue_len", with(|c| c.queue_len = MIN_QUEUE_LEN - 1)),
        ("queue_len", with(|c| c.queue_len = 0)),
        // The group would take more than half the completion queue.
        (
            "recv_buffers",
            with(|c| {
                c.queue_len = 64;
                c.recv_buffers = 128;
            }),
        ),
        (
            "recv_buffers",
            with(|c| {
                c.queue_len = 128;
                c.recv_buffers = 256;
            }),
        ),
        ("recv_buffers", with(|c| c.recv_buffers = 3)),
        ("recv_buffers", with(|c| c.recv_buffers = 40000)),
        (
            "recv_buffer_size",
            with(|c| c.recv_buffer_size = MIN_RECV_BUFFER_SIZE - 1),
        ),
        ("recv_buffer_size", with(|c| c.recv_buffer_size = 0)),
        (
            "recv_buffer_size",
            with(|c| c.recv_buffer_size = MAX_RECV_BUFFER_SIZE + 1),
        ),
    ];
    for (setting, config) in refused {
        let err = config
            .validate()
            .expect_err(&format!("{config:?} was accepted"));
        assert_eq!(err.kind(), ErrorKind::InvalidInput, "{err}");
        assert!(err.to_string().contains(setting), "{err}");
    }
    let accepted = [
        with(|c| {
            c.queue_len = MIN_QUEUE_LEN;
            c.recv_buffers = 0;
        }),
        with(|c| c.queue_len = u32::MAX),
        with(|c| {
            c.queue_len = 64;
            c.recv_buffers = 64;
        }),
        // 129 entries make a ring of 256 and a completion queue of 512.
        with(|c| {
            c.queue_len = 129;
            c.recv_buffers = 256;
        }),
        with(|c| c.recv_buffers = 0),
        with(|c| c.recv_buffers = 1),
        with(|c| {
            c.queue_len = 32768;
            c.recv_buffers = 32768;
        }),
        with(|c| c.recv_buffer_size = MIN_RECV_BUFFER_SIZE),
        with(|c| c.recv_buffer_size = MAX_RECV_BUFFER_SIZE),
        // Not read while there is no group.
        with(|c| {
            c.recv_buffers = 0;
            c.recv_buffer_size = 1;
        }),
    ];
    for config in accepted {
        assert!(config.validate().is_ok(), "{config:?}");
    }
}
