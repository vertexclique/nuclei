//! `init_with_config` for a default runtime that cannot start logs one error
//! and starts nothing. A process of its own: the backend choice is made once,
//! and the logger is the process's.

use std::sync::Mutex;

use nuclei::GlobalExecutorConfig;

static RECORDS: Mutex<Vec<(log::Level, String)>> = Mutex::new(Vec::new());

struct Capture;

impl log::Log for Capture {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        RECORDS
            .lock()
            .unwrap()
            .push((record.level(), record.args().to_string()));
    }

    fn flush(&self) {}
}

#[test]
fn a_default_runtime_that_cannot_start_is_logged_once_and_not_started() {
    // SAFETY: the only test of this process, so no other thread reads the environment.
    unsafe { std::env::set_var("NUCLEI_BACKEND", "none-such") };
    log::set_logger(&Capture).expect("no logger was set");
    log::set_max_level(log::LevelFilter::Trace);

    nuclei::init_with_config(GlobalExecutorConfig::default().with_min_threads(2));

    let errors: Vec<String> = RECORDS
        .lock()
        .unwrap()
        .iter()
        .filter(|(level, _)| *level == log::Level::Error)
        .map(|(_, text)| text.clone())
        .collect();
    assert_eq!(errors.len(), 1, "{errors:?}");
    assert!(errors[0].contains("none-such"), "{}", errors[0]);

    // No runtime was left behind: a second call tries again and reports again.
    nuclei::init_with_config(GlobalExecutorConfig::default());
    let again = RECORDS
        .lock()
        .unwrap()
        .iter()
        .filter(|(level, _)| *level == log::Level::Error)
        .count();
    assert_eq!(again, 2);
    assert!(
        nuclei::task::Builder::new().spawn(async {}).is_err(),
        "a runtime was started"
    );
}
