use std::time::Duration;

use nuclei::sync::mpsc;
use nuclei::time::sleep;

#[nuclei::main]
async fn main() {
    let (tx, mut jobs) = mpsc::channel::<u32>(8);
    nuclei::spawn(async move {
        for job in 1..=3 {
            tx.send(job).await.unwrap();
        }
    })
    .detach();

    let mut done = Vec::new();
    loop {
        // The first branch to finish runs; the other futures are dropped.
        nuclei::select! {
            Some(job) = jobs.recv() => done.push(job),
            _ = sleep(Duration::from_secs(1)) => break,
            // Every branch disabled (the channel closed and gave None): else.
            else => break,
        }
    }
    assert_eq!(done, [1, 2, 3]);
}
