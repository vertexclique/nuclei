//! The intended compile shapes of the three macros.
//!
//! Each test is ignored because the expansions call the nuclei runtime entry
//! points (`block_on`, `Runtime::builder().worker_threads(N)`) and the
//! runtime crate does not expose them yet. Until it does, the token-level
//! comparison lives in the crate's unit tests against the expected text under
//! `tests/fixtures/`; these tests are the compile gate that turns on when
//! the runtime lands: apply each macro to the fixture's input and let the
//! expansion compile.

use std::fs;

fn fixture(name: &str) -> String {
    let path = format!("tests/fixtures/{name}");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("could not read {path}: {e}"))
}

#[test]
#[ignore = "compiles once the nuclei runtime ships its block_on and Runtime::builder"]
fn main_default_shape() {
    let text = fixture("main_default.rs");
    assert!(text.contains("::nuclei::block_on(async"));
}

#[test]
#[ignore = "compiles once the nuclei runtime ships its block_on and Runtime::builder"]
fn main_threads_shape() {
    let text = fixture("main_threads.rs");
    assert!(text.contains("::nuclei::Runtime::builder().worker_threads(4)"));
    assert!(text.contains(".block_on(async"));
}

#[test]
#[ignore = "compiles once the nuclei runtime ships its block_on and Runtime::builder"]
fn test_default_shape() {
    let text = fixture("test_default.rs");
    assert!(text.contains("#[::core::prelude::v1::test]"));
    assert!(text.contains("::nuclei::block_on(async"));
}

#[test]
#[ignore = "compiles once the nuclei runtime ships its block_on and Runtime::builder"]
fn test_threads_shape() {
    let text = fixture("test_threads.rs");
    assert!(text.contains("worker_threads(0)"));
}

#[test]
#[ignore = "compiles once the nuclei runtime ships its block_on and Runtime::builder"]
fn bench_default_shape() {
    let text = fixture("bench_default.rs");
    assert!(text.contains("#[::core::prelude::v1::bench]"));
    assert!(text.contains("_bencher: &mut test::Bencher"));
}

#[test]
#[ignore = "compiles once the nuclei runtime ships its block_on and Runtime::builder"]
fn bench_threads_shape() {
    let text = fixture("bench_threads.rs");
    assert!(text.contains("worker_threads(2)"));
}
