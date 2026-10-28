use std::sync::Arc;

use nuclei::sync::Mutex;

#[nuclei::main]
async fn main() {
    let counter = Arc::new(Mutex::new(0u32));
    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let counter = Arc::clone(&counter);
            nuclei::spawn(async move {
                // lock waits as a future: the shard runs other tasks meanwhile.
                *counter.lock().await += 1;
            })
        })
        .collect();
    for task in tasks {
        task.await;
    }
    assert_eq!(*counter.lock().await, 8);

    // An owned guard holds the lock by an Arc, so it can be moved into a task.
    let guard = Arc::clone(&counter).lock_owned().await;
    nuclei::spawn(async move { assert_eq!(*guard, 8) }).await;

    // try_lock never waits.
    assert!(counter.try_lock().is_ok());
}
