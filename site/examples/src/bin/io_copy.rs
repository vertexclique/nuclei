use nuclei::io::{self, AsyncReadExt, AsyncWriteExt};

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    // copy: everything a reader gives, into a writer, then a flush.
    let mut source = io::repeat(b'x').take(1 << 20);
    let copied = io::copy(&mut source, &mut io::sink()).await?;
    assert_eq!(copied, 1 << 20);

    // copy_bidirectional: a proxy between two streams, both ways at once.
    let (mut client, mut front) = io::duplex(1024);
    let (mut back, mut backend) = io::duplex(1024);
    let proxy = nuclei::spawn(async move { io::copy_bidirectional(&mut front, &mut back).await });
    client.write_all(b"request").await?;
    let mut buf = [0u8; 7];
    backend.read_exact(&mut buf).await?;
    backend.write_all(b"reply").await?;
    let mut reply = [0u8; 5];
    client.read_exact(&mut reply).await?;
    assert_eq!(&reply, b"reply");

    // Each side that ends ends its direction; the proxy answers both counts.
    drop(client);
    drop(backend);
    assert_eq!(proxy.await?, (7, 5));
    Ok(())
}
