fn main() -> std::io::Result < () > { ::nuclei::Runtime::builder().worker_threads(4).build().expect("the nuclei runtime failed to start").block_on(async { Ok(()) }) }
