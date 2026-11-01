use nuclei::fs::File;
use nuclei::io::AsyncSeekExt;

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    let mut file = File::create_new("record.bin").await?;

    // A header and a body from two buffers, as one write: nothing comes between.
    let parts = vec![b"LEN=4;".to_vec(), b"body".to_vec()];
    let (written, parts) = file.write_vectored_owned(parts).await;
    assert_eq!(written?, 10);
    // Both buffers come back, for the next record.
    assert_eq!(parts.len(), 2);

    // Read it back into two rooms: the first fills before the second.
    file.rewind().await?;
    let rooms = [Vec::<u8>::with_capacity(6), Vec::with_capacity(64)];
    let (n, [header, body]) = file.read_vectored_owned(rooms).await;
    assert_eq!(n?, 10);
    assert_eq!((&header[..], &body[..]), (&b"LEN=4;"[..], &b"body"[..]));
    nuclei::fs::remove_file("record.bin").await
}
