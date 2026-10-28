use std::time::Duration;

fn main() {
    // The first call starts the default runtime: one worker shard per core.
    let sum = nuclei::block_on(async {
        nuclei::time::sleep(Duration::from_millis(10)).await;
        (1..=10).sum::<u32>()
    });
    assert_eq!(sum, 55);
}
