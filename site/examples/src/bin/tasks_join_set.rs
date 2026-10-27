use std::time::Duration;

use nuclei::task::JoinSet;
use nuclei::time::sleep;

#[nuclei::main]
async fn main() {
    let mut set = JoinSet::new();
    for n in [30u64, 10, 20] {
        set.spawn(async move {
            sleep(Duration::from_millis(n)).await;
            n
        });
    }
    // Joined in the order they finish, not the order they were spawned.
    let mut order = Vec::new();
    while let Some(done) = set.join_next().await {
        order.push(done.unwrap());
    }
    assert_eq!(order, [10, 20, 30]);

    // Dropping the set, or shutdown, cancels whatever is still in it.
    set.spawn(std::future::pending());
    set.shutdown().await;
    assert!(set.is_empty());
}
