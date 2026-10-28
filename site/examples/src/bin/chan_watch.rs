use nuclei::sync::watch;

#[nuclei::main]
async fn main() {
    // One value, read by many; each receiver sees the latest.
    let (tx, mut rx) = watch::channel(1u32);
    let watcher = nuclei::spawn(async move {
        // changed waits for a value this receiver has not seen.
        rx.changed().await.unwrap();
        *rx.borrow_and_update()
    });
    tx.send_modify(|v| *v += 1);
    assert_eq!(watcher.await, 2);
    assert_eq!(*tx.borrow(), 2);
}
