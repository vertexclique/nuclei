#[::core::prelude::v1::test] fn spins() { ::nuclei::Runtime::builder().worker_threads(0).build().expect("the nuclei runtime failed to start").block_on(async { 1 }) }
