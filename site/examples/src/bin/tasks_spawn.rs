#[nuclei::main]
async fn main() {
    // Each task is placed on a shard, and there it stays.
    let tasks: Vec<_> = (0..8u64)
        .map(|n| nuclei::spawn(async move { n * n }))
        .collect();

    let mut total = 0;
    for task in tasks {
        total += task.await;
    }
    assert_eq!(total, 140);
}
