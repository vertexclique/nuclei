use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use nuclei::Builder;

fn main() -> std::io::Result<()> {
    let spawned = Arc::new(AtomicUsize::new(0));
    let polls = Arc::new(AtomicUsize::new(0));
    let runtime = Builder::new()
        .worker_threads(2)
        .on_thread_start(|| println!("worker up"))
        .on_task_spawn({
            let spawned = Arc::clone(&spawned);
            move |_task| {
                spawned.fetch_add(1, Ordering::Relaxed);
            }
        })
        .on_before_task_poll({
            let polls = Arc::clone(&polls);
            move |_task| {
                polls.fetch_add(1, Ordering::Relaxed);
            }
        })
        .build()?;

    runtime.block_on(async {
        for n in 0..10 {
            nuclei::spawn(async move { n }).await;
        }
    });
    assert_eq!(spawned.load(Ordering::Relaxed), 10);
    assert!(polls.load(Ordering::Relaxed) >= 10);
    Ok(())
}
