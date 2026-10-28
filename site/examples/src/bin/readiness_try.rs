use std::io::ErrorKind;

use nuclei::io::{Interest, Ready};
use nuclei::net::{TcpListener, TcpStream};

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let client = TcpStream::connect(listener.local_addr()?).await?;
    let (server, _) = listener.accept().await?;

    // try_write hands bytes to the write-behind, or says WouldBlock when it is full.
    client.writable().await?;
    assert_eq!(client.try_write(b"ready")?, 5);

    // try_read answers from what the driver read ahead, never waiting.
    let mut buf = [0u8; 16];
    let n = loop {
        server.readable().await?;
        match server.try_read(&mut buf) {
            Ok(n) => break n,
            // Nothing read ahead yet: wait for readiness again.
            Err(e) if e.kind() == ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e),
        }
    };
    assert_eq!(&buf[..n], b"ready");

    // ready waits for either, and says which it found.
    let found: Ready = server
        .ready(Interest::READABLE | Interest::WRITABLE)
        .await?;
    assert!(found.is_writable());
    Ok(())
}
