use std::io::ErrorKind;
use std::net::Shutdown;

use nuclei::io::{AsyncReadExt, AsyncWriteExt};
use nuclei::net::{TcpListener, TcpStream};

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;

    // shut waits until the kernel has the bytes written behind, then ends the write side.
    let mut client = TcpStream::connect(addr).await?;
    let (mut server, _) = listener.accept().await?;
    client.write_all(b"every byte").await?;
    client.shut(Shutdown::Write).await?;
    let mut got = Vec::new();
    server.read_to_end(&mut got).await?;
    assert_eq!(got, b"every byte");

    // abort drops what is behind and resets the connection: the peer reads an error.
    let client = TcpStream::connect(addr).await?;
    let (server, _) = listener.accept().await?;
    client.abort()?;
    let mut buf = [0u8; 16];
    let reset = server.recv(&mut buf).await.unwrap_err();
    assert_eq!(reset.kind(), ErrorKind::ConnectionReset);
    Ok(())
}
