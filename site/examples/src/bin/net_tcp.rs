use nuclei::io::{AsyncReadExt, AsyncWriteExt};
use nuclei::net::{TcpListener, TcpStream};

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    // Port 0: the system picks a free one.
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    // The server echoes every byte back until the client ends its side.
    let server = nuclei::spawn(async move {
        let (stream, peer) = listener.accept().await?;
        println!("connection from {}", peer.ip());
        let mut buf = [0u8; 1024];
        loop {
            let n = stream.recv(&mut buf).await?;
            if n == 0 {
                break;
            }
            stream.send(&buf[..n]).await?;
        }
        std::io::Result::Ok(())
    });

    // The client uses the same stream through the read and write helpers.
    let mut client = TcpStream::connect(addr).await?;
    client.write_all(b"hello").await?;
    client.shutdown().await?;
    let mut reply = String::new();
    client.read_to_string(&mut reply).await?;
    assert_eq!(reply, "hello");

    server.await?;
    Ok(())
}
