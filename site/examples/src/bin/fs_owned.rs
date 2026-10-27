use nuclei::fs::File;

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    // Lend a vector to the write: the bytes are written from it, no copy.
    let file = File::create("lent.bin").await?;
    let (written, _buf) = file.write_owned(vec![7u8; 4096]).await;
    assert_eq!(written?, 4096);
    drop(file);

    // And to the read: they land in its spare capacity.
    let file = File::open("lent.bin").await?;
    let (n, buf) = file.read_owned(Vec::with_capacity(8192)).await;
    assert_eq!(n?, 4096);
    assert!(buf.iter().all(|&b| b == 7));
    nuclei::fs::remove_file("lent.bin").await
}
