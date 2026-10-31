//! With a default runtime that cannot start, `task::Builder` spawns answer an
//! error where the free functions panic. A process of its own: the backend
//! choice is made once.

use std::io::ErrorKind;

use nuclei::task::Builder;

#[test]
fn builder_spawns_answer_an_error_where_the_free_functions_panic() {
    // SAFETY: the only test of this process, so no other thread reads the environment.
    unsafe { std::env::set_var("NUCLEI_BACKEND", "none-such") };

    let spawn = Builder::new().spawn(async {});
    let spawn_blocking = Builder::new().spawn_blocking(|| 1);
    let spawn_local = Builder::new().spawn_local(async {});
    let named = Builder::new().name("doomed").spawn(async {});
    for (what, result) in [
        ("spawn", spawn.err()),
        ("spawn_blocking", spawn_blocking.err()),
        ("spawn_local", spawn_local.err()),
        ("a named spawn", named.err()),
    ] {
        let err = result.unwrap_or_else(|| panic!("{what} started a runtime"));
        assert_eq!(err.kind(), ErrorKind::Unsupported, "{what}: {err}");
        assert!(err.to_string().contains("none-such"), "{what}: {err}");
    }

    assert!(std::panic::catch_unwind(|| nuclei::spawn(async {})).is_err());
    assert!(std::panic::catch_unwind(|| nuclei::spawn_blocking(|| 1)).is_err());
    assert!(std::panic::catch_unwind(|| nuclei::spawn_local(async {})).is_err());
}
