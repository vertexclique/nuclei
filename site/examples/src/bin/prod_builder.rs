use std::time::Duration;

use nuclei::runtime::{RngSeed, UnhandledPanic};
use nuclei::{Builder, IoBackend};

fn main() -> std::io::Result<()> {
    let backend = if cfg!(target_os = "linux") {
        IoBackend::Epoll
    } else {
        nuclei::Proactor::backend()
    };
    let runtime = Builder::new()
        .worker_threads(4) // default: one per core
        .max_worker_threads(8) // default: four times worker_threads
        .pin_workers(true) // Linux; default true
        .thread_name("edge") // default nuclei-worker, nuclei-blocking
        .thread_stack_size(2 * 1024 * 1024) // default 8 MiB
        .max_nesting(64) // default 256
        .max_blocking_threads(128) // default 512
        .thread_keep_alive(Duration::from_secs(5)) // default 10 seconds
        .local_spawn_margin(16) // default 32
        .io_buffer_cap(32 * 1024 * 1024) // default 64 MiB per worker
        .event_interval(31) // default 61
        .global_queue_interval(15) // default 31
        .max_io_events_per_tick(256) // default: all that are ready
        .disable_lifo_slot()
        .rng_seed(RngSeed::from_bytes(b"replay"))
        .unhandled_panic(UnhandledPanic::ShutdownRuntime)
        .backend(backend) // default: the process's choice
        .build()?;

    let used = runtime.block_on(runtime.spawn(async { nuclei::Proactor::backend() }));
    println!("workers run {used:?}");
    runtime.shutdown_timeout(Duration::from_secs(1));
    Ok(())
}
