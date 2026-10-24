//! `init_with_config` on a process of its own: the configuration it gives
//! is the default runtime's.

use std::sync::atomic::{AtomicUsize, Ordering};

use nuclei::{GlobalExecutorConfig, block_on, spawn, spawn_more_threads, stop_thread};

static NAMED: AtomicUsize = AtomicUsize::new(0);

#[test]
fn the_configuration_sets_the_workers_their_names_and_their_maximum() {
    nuclei::init_with_config(
        GlobalExecutorConfig::default()
            .with_env_var("NUCLEI_TEST_THREADS_UNSET")
            .with_min_threads(2)
            .with_max_threads(3)
            .with_thread_name_fn(|| {
                NAMED.fetch_add(1, Ordering::SeqCst);
                "configured-worker".to_string()
            }),
    );
    assert_eq!(
        NAMED.load(Ordering::SeqCst),
        2,
        "two workers started, each named"
    );
    let name = block_on(spawn(async {
        std::thread::current().name().map(str::to_string)
    }));
    assert_eq!(name.as_deref(), Some("configured-worker"));
    assert_eq!(
        block_on(spawn_more_threads(5)).unwrap(),
        1,
        "one more fits under three"
    );
    assert!(
        block_on(stop_thread()),
        "three run, above the two of the minimum"
    );
    assert!(!block_on(stop_thread()), "the minimum stays");
    // A configuration after the start is not used.
    nuclei::init_with_config(GlobalExecutorConfig::default().with_min_threads(9));
    assert_eq!(NAMED.load(Ordering::SeqCst), 3);
}
