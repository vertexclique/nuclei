use nuclei::sync::mpsc;

#[nuclei::main]
async fn main() {
    // Bounded: a send waits while the channel holds 16 messages.
    let (tx, mut rx) = mpsc::channel::<String>(16);
    for worker in 0..3 {
        let tx = tx.clone();
        nuclei::spawn(async move {
            tx.send(format!("hello from {worker}")).await.unwrap();
        })
        .detach();
    }
    // The channel closes when the last sender drops, and recv gives None.
    drop(tx);
    let mut got = Vec::new();
    while let Some(message) = rx.recv().await {
        got.push(message);
    }
    got.sort();
    assert_eq!(got, ["hello from 0", "hello from 1", "hello from 2"]);
}
