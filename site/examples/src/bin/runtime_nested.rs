/// A synchronous helper, deep in a call stack, that needs a future's output.
fn lookup(key: u32) -> u32 {
    nuclei::block_on(async move { key * 2 })
}

#[nuclei::main]
async fn main() {
    let task = nuclei::spawn(async {
        // block_on nests: the shard runs its other tasks while it waits.
        lookup(21)
    });
    assert_eq!(task.await, 42);
}
