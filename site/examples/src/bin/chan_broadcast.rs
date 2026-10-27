use nuclei::sync::broadcast::{self, error::RecvError};

#[nuclei::main]
async fn main() {
    // Every receiver sees every message sent after it subscribed.
    let (tx, mut first) = broadcast::channel::<&str>(2);
    let mut second = tx.subscribe();
    tx.send("deploy").unwrap();
    assert_eq!(first.recv().await, Ok("deploy"));
    assert_eq!(second.recv().await, Ok("deploy"));

    // A receiver that falls more than the capacity behind is told how far.
    for event in ["a", "b", "c"] {
        tx.send(event).unwrap();
    }
    assert_eq!(first.recv().await, Err(RecvError::Lagged(1)));
    assert_eq!(first.recv().await, Ok("b"));

    // A weak sender: it can send again once upgraded, while a strong one lives.
    let weak = tx.downgrade();
    assert!(weak.upgrade().is_some());
    drop(tx);
    assert!(weak.upgrade().is_none());
}
