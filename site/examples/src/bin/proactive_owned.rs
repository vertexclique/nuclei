use nuclei::net::{TcpListener, TcpStream};

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let server = nuclei::spawn(async move {
        let (stream, _) = listener.accept().await?;
        // Lend the driver this vector: the kernel reads into its spare capacity.
        let (n, buf) = stream.read_owned(Vec::with_capacity(64)).await;
        n?;
        // Lend it again for the reply; it comes back once the kernel took it all.
        let (sent, _buf) = stream.write_owned(buf).await;
        sent
    });

    let client = TcpStream::connect(addr).await?;
    let (sent, _) = client.write_owned(b"lent, not copied".to_vec()).await;
    sent?;
    let (n, reply) = client.read_owned(Vec::with_capacity(64)).await;
    assert_eq!(&reply[..n?], b"lent, not copied");
    server.await?;
    Ok(())
}
