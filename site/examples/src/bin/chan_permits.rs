use std::time::Duration;

use nuclei::sync::mpsc::{self, error::TrySendError};

#[nuclei::main]
async fn main() {
    let (tx, mut rx) = mpsc::channel::<u32>(1);

    // reserve takes a slot first; the send that uses it cannot fail.
    let permit = tx.reserve().await.unwrap();
    permit.send(1);

    // The one slot is taken: try_send says so instead of waiting.
    assert!(matches!(tx.try_send(2), Err(TrySendError::Full(2))));
    // send_timeout gives up after a while, and gives the value back.
    let late = tx.send_timeout(3, Duration::from_millis(10)).await;
    assert_eq!(late.unwrap_err().into_inner(), 3);

    assert_eq!(rx.recv().await, Some(1));

    // A weak sender does not keep the channel open.
    let weak = tx.downgrade();
    drop(tx);
    assert!(weak.upgrade().is_none());
    assert_eq!(rx.recv().await, None);
}
