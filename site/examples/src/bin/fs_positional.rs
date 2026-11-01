use std::sync::Arc;

use nuclei::fs::File;
use nuclei::io::AsyncReadExt;

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    // One handle shared by four tasks, each writing its record at its offset.
    let file = Arc::new(File::create_new("records.bin").await?);
    let writers: Vec<_> = (0..4u8)
        .map(|i| {
            let file = Arc::clone(&file);
            nuclei::spawn(async move { file.write_all_at(&[i; 16], u64::from(i) * 16).await })
        })
        .collect();
    for writer in writers {
        writer.await?;
    }

    let mut record = [0u8; 16];
    file.read_exact_at(&mut record, 32).await?;
    assert_eq!(record, [2; 16]);

    // Many reads in one call, each into a buffer of its own, out together.
    let reads = (0..4u64)
        .map(|i| (Vec::<u8>::with_capacity(16), i * 16))
        .collect();
    for (i, (n, buf)) in (0u8..).zip(file.read_many_at(reads).await) {
        assert_eq!(n?, 16);
        assert!(buf.iter().all(|&b| b == i));
    }

    // Positional calls never move the cursor: a read at it starts at 0.
    let mut all = Vec::new();
    (&*file).read_to_end(&mut all).await?;
    assert_eq!(all.len(), 64);
    nuclei::fs::remove_file("records.bin").await
}
