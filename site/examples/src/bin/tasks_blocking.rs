#[nuclei::main]
async fn main() -> std::io::Result<()> {
    // Blocking calls go to the blocking pool, so no shard waits on them.
    let entries = nuclei::spawn_blocking(|| std::fs::read_dir(".").map(|dir| dir.count())).await?;
    println!("{entries} entries here");

    let digest =
        nuclei::spawn_blocking(|| (0..1_000_000u64).fold(0u64, |h, n| h.rotate_left(5) ^ n)).await;
    println!("digest {digest:x}");
    Ok(())
}
