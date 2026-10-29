//! A configuration with a ring size the runtime cannot work with is refused
//! when the first runtime builds, as the configuration's mistake. The
//! process's configuration is set once, so this is a test binary of its own.

use std::io::ErrorKind;

use nuclei::config::{IoUringConfiguration, NucleiConfig};
use nuclei::runtime::Builder;

#[test]
fn a_ring_size_below_the_minimum_fails_the_build_naming_the_setting() {
    nuclei::Proactor::with_config(NucleiConfig {
        iouring: IoUringConfiguration {
            queue_len: 32,
            ..IoUringConfiguration::default()
        },
        ..NucleiConfig::default()
    });
    let built = Builder::new()
        .worker_threads(1)
        .backend(nuclei::IoBackend::IoUring)
        .build();
    let Err(err) = built else {
        // The environment names the poller: no ring reads the size.
        assert!(
            std::env::var("NUCLEI_BACKEND").is_ok_and(|b| b != "io_uring" && b != "auto"),
            "a runtime started on a ring of 32 entries"
        );
        return;
    };
    if err.kind() == ErrorKind::Unsupported {
        eprintln!("skipping: io_uring unavailable: {err}");
        return;
    }
    assert_eq!(err.kind(), ErrorKind::InvalidInput, "{err}");
    assert!(err.to_string().contains("queue_len"), "{err}");
}
