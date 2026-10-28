#[nuclei::main(threads = 2)]
async fn main() {
    let stats = nuclei::stats();
    let workers = stats.shards.iter().filter(|s| s.worker.is_some()).count();
    println!("{workers} worker shards");
}
