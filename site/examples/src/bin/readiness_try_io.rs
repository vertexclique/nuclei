use std::io::ErrorKind;

use nuclei::io::Interest;
use nuclei::net::UdpSocket;

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    let a = UdpSocket::bind("127.0.0.1:0").await?;
    let b = UdpSocket::bind("127.0.0.1:0").await?;
    let to = b.local_addr()?;

    // A call of your own on the socket, made when it may be ready. The socket
    // is non-blocking, so a call that cannot go on answers WouldBlock and the
    // wait goes on.
    let sent = a
        .async_io(Interest::WRITABLE, || a.get_ref().send_to(b"own call", to))
        .await?;
    assert_eq!(sent, 8);

    let mut buf = [0u8; 64];
    let n = b
        .async_io(Interest::READABLE, || b.get_ref().recv(&mut buf))
        .await?;
    assert_eq!(&buf[..n], b"own call");

    // try_io makes the call once, now, or answers WouldBlock.
    let again = b.try_io(Interest::READABLE, || b.get_ref().recv(&mut buf));
    assert_eq!(again.unwrap_err().kind(), ErrorKind::WouldBlock);
    Ok(())
}
