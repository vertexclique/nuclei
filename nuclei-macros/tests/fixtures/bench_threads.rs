#[::core::prelude::v1::bench] fn bench_spawn(_bencher: &mut test::Bencher) { ::nuclei::Runtime::builder().worker_threads(2).build().expect("the nuclei runtime failed to start").block_on(async {}) }
