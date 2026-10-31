//! `build_global`: a build that fails leaves no default, the first that
//! succeeds is the default, and every later one answers `AlreadyExists` and
//! leaves it. A process of its own: the default runtime is the process's.

use std::io::ErrorKind;

use nuclei::runtime::{Builder, RuntimeHandle, worker_index};

#[test]
fn a_second_build_global_answers_already_exists_and_the_first_stays_the_default() {
    let failed = Builder::new().thread_stack_size(1).build_global();
    assert_eq!(failed.unwrap_err().kind(), ErrorKind::InvalidInput);

    let first = Builder::new()
        .worker_threads(2)
        .pin_workers(false)
        .build_global()
        .expect("the first default starts");
    assert_eq!(first.workers(), 2);

    let second = Builder::new().worker_threads(1).build_global();
    let err = second.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::AlreadyExists, "{err}");

    // Taken for the default before its settings are looked at.
    let invalid = Builder::new().thread_stack_size(1).build_global();
    assert_eq!(invalid.unwrap_err().kind(), ErrorKind::AlreadyExists);

    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                let again = Builder::new().worker_threads(3).build_global();
                assert_eq!(again.unwrap_err().kind(), ErrorKind::AlreadyExists);
            });
        }
    });

    let current = RuntimeHandle::current();
    assert_eq!(current.id(), first.id());
    assert_eq!(current.workers(), 2);
    let on_default = nuclei::block_on(async {
        (
            RuntimeHandle::current().id(),
            nuclei::spawn(async { worker_index() }).await,
        )
    });
    assert_eq!(on_default.0, first.id());
    assert!(matches!(on_default.1, Some(0 | 1)), "{:?}", on_default.1);
}
