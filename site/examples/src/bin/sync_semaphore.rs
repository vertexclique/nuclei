use std::sync::Arc;
use std::time::Duration;

use nuclei::sync::{Mutex, Semaphore};
use nuclei::time::sleep;

#[nuclei::main]
async fn main() {
    // At most two jobs at once, however many are spawned.
    let slots = Arc::new(Semaphore::new(2));
    let running = Arc::new(Mutex::new((0u32, 0u32))); // (now, most at once)
    let jobs: Vec<_> = (0..6)
        .map(|_| {
            let (slots, running) = (Arc::clone(&slots), Arc::clone(&running));
            nuclei::spawn(async move {
                let _permit = slots.acquire_owned().await.unwrap();
                {
                    let mut r = running.lock().await;
                    r.0 += 1;
                    r.1 = r.1.max(r.0);
                }
                sleep(Duration::from_millis(5)).await;
                running.lock().await.0 -= 1;
                // The permit goes back when it drops.
            })
        })
        .collect();
    for job in jobs {
        job.await;
    }
    assert_eq!(running.lock().await.1, 2);
    assert_eq!(slots.available_permits(), 2);
}
