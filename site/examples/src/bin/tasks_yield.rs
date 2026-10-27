use nuclei::task::{self, coop};

#[nuclei::main]
async fn main() {
    let busy = nuclei::spawn(async {
        let mut sum = 0u64;
        for n in 0..100_000u64 {
            sum += n;
            // A long loop that awaits nothing else spends the budget, so the
            // shard's other tasks get their turns.
            coop::consume_budget().await;
        }
        sum
    });
    // yield_now: a turn for the other tasks of this shard, at once.
    task::yield_now().await;
    assert_eq!(busy.await, 4_999_950_000);
}
