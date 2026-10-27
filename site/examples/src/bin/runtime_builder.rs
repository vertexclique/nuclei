use std::time::Duration;

use nuclei::runtime::Runtime;

fn main() -> std::io::Result<()> {
    let runtime = Runtime::builder()
        .worker_threads(4)
        .thread_name("api-worker")
        .max_blocking_threads(64)
        .build()?;

    let task = runtime.spawn(async { "served" });
    assert_eq!(runtime.block_on(task), "served");

    // Stop the workers, waiting for them one second at most.
    runtime.shutdown_timeout(Duration::from_secs(1));
    Ok(())
}
