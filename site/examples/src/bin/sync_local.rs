use std::rc::Rc;

use nuclei::sync::local::{Mutex, mpsc, oneshot};

#[nuclei::main]
async fn main() {
    // Rc, not Arc: these tasks all stay on this thread's shard.
    let log = Rc::new(Mutex::new(Vec::new()));
    let (tx, mut rx) = mpsc::channel(2);
    for n in 1..=3u32 {
        let (tx, log) = (tx.clone(), Rc::clone(&log));
        nuclei::spawn_local(async move {
            log.lock().await.push(n);
            tx.send(n).await.unwrap();
        })
        .detach();
    }
    drop(tx);

    // A oneshot carries the sum back once every sender is gone.
    let (reply, sum) = oneshot::channel();
    nuclei::spawn_local(async move {
        let mut total = 0;
        while let Some(n) = rx.recv().await {
            total += n;
        }
        reply.send(total).unwrap();
    })
    .detach();

    assert_eq!(sum.await, Ok(6));
    assert_eq!(log.lock().await.len(), 3);
}
