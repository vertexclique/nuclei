//! `Proactor::with_config` on a process of its own: set once, before any IO.

use nuclei::Proactor;
use nuclei::config::{IoUringConfiguration, NucleiConfig};

#[test]
fn a_configuration_is_set_once() {
    let config = NucleiConfig {
        iouring: IoUringConfiguration::low_latency_driven(256),
    };
    let p = Proactor::with_config(config);
    assert_eq!(p.config().iouring.queue_len, 256);
    assert!(std::ptr::eq(p, Proactor::get()));
    let again = std::panic::catch_unwind(|| {
        Proactor::with_config(NucleiConfig::default());
    });
    assert!(again.is_err(), "a second configuration is refused loudly");
}
