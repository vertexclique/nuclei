#[nuclei::main]
async fn main() {
    nuclei::spawn(async {}).await;

    let stats = nuclei::stats();
    for shard in &stats.shards {
        println!(
            "{:?} on {:?}: {} tasks, {} polls, {} IO in flight, {} of {} buffer bytes",
            shard.worker,
            shard.backend,
            shard.tasks,
            shard.polls,
            shard.ops_in_flight,
            shard.buffer_bytes,
            shard.buffer_cap,
        );
        for (kind, count) in shard.errors.iter() {
            println!("  {count} errors of kind {kind:?}");
        }
    }
    println!("{} blocking threads", stats.blocking_threads);
}
