//! The sync module's loom models: every interleaving each reaches within its
//! preemption bound. loom is a dev-dependency only, so the models are this
//! crate's own unit tests, built with `--cfg loom`:
//!
//! ```text
//! RUSTFLAGS="--cfg loom" cargo test --release --lib -- sync::loom_tests
//! ```

mod mpsc;
mod primitives;
mod signals;
