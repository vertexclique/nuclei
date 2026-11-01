use nuclei::fs::{Allocate, File};

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    let log = File::create_new("log.bin").await?;
    // Reserve 1 MiB up front, so writes into it cannot run out of space.
    log.allocate(0, 1 << 20, Allocate::KeepSize).await?;
    assert_eq!(log.metadata().await?.len(), 0);

    log.write_all_at(b"entry", 0).await?;
    // Start writing the range back now; sync_data is what makes it durable.
    log.sync_range(0, 5).await?;

    // Copy the range into another file, in the kernel where it can.
    let copy = File::create_new("copy.bin").await?;
    assert_eq!(log.copy_range_to(&copy, 0, 0, 5).await?, 5);
    let mut back = [0u8; 5];
    copy.read_exact_at(&mut back, 0).await?;
    assert_eq!(&back, b"entry");

    nuclei::fs::remove_file("log.bin").await?;
    nuclei::fs::remove_file("copy.bin").await
}
