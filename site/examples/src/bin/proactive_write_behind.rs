use nuclei::io::{AsyncReadExt, AsyncWriteExt};
use nuclei::net::{TcpListener, TcpStream};

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let reader = nuclei::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let mut all = Vec::new();
        stream.read_to_end(&mut all).await?;
        std::io::Result::Ok(all.len())
    });

    let mut stream = TcpStream::connect(addr).await?;
    // Each write is copied into the stream's write-behind and taken at once.
    for _ in 0..1000 {
        stream.write_all(&[b'x'; 1024]).await?;
    }
    // flush waits until the kernel has every byte, and reports a failed write.
    stream.flush().await?;
    // shutdown flushes, then ends the write side: the reader sees the end.
    stream.shutdown().await?;

    assert_eq!(reader.await?, 1000 * 1024);
    Ok(())
}
