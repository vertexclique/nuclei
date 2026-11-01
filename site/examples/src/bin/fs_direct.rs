use nuclei::fs::OpenOptions;
use nuclei::io::AlignedBuf;

// Direct IO asks the memory, the offset and the length to be multiples of
// the device's block size; 4096 meets that on the common ones.
const BLOCK: usize = 4096;

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .custom_flags(libc::O_DIRECT)
        .open("direct.bin")
        .await?;

    // Two blocks in memory aligned to a block, handed to the kernel as they are.
    let mut blocks = AlignedBuf::new(2 * BLOCK, BLOCK)?;
    blocks.extend_from_slice(&[7u8; 2 * BLOCK]);
    let (written, _blocks) = file.write_at_owned(blocks, 0).await;
    assert_eq!(written?, 2 * BLOCK);

    // The second block, read straight into an aligned buffer of its own.
    let room = AlignedBuf::new(BLOCK, BLOCK)?;
    let (n, block) = file.read_at_owned(room, BLOCK as u64).await;
    assert_eq!(n?, BLOCK);
    assert!(block.iter().all(|&b| b == 7));
    nuclei::fs::remove_file("direct.bin").await
}
