use std::cell::Cell;

use nuclei::runtime::Runtime;

nuclei::shard_local! {
    // One counter per worker, made on its first use there.
    static SERVED: Cell<u64> = Cell::new(0);
}

fn main() -> std::io::Result<()> {
    let runtime = Runtime::builder().worker_threads(2).build()?;
    let handle = runtime.handle();
    for worker in [0, 0, 0, 1] {
        let served = handle.spawn_on_worker(worker, async {
            SERVED.with(|n| n.set(n.get() + 1));
        })?;
        runtime.block_on(served);
    }

    // Each worker reads its own counter.
    let counts = handle.on_each_worker(|| async { SERVED.with(Cell::get) });
    let mut counts = runtime.block_on(counts.join_all());
    counts.sort();
    assert_eq!(counts, [1, 3]);

    // Code that runs on no worker has no value.
    assert!(SERVED.try_with(Cell::get).is_err());
    Ok(())
}
