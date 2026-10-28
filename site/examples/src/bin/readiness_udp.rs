use std::io::ErrorKind;

use nuclei::net::UdpSocket;

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    let a = UdpSocket::bind("127.0.0.1:0").await?;
    let b = UdpSocket::bind("127.0.0.1:0").await?;

    // Nothing came yet: the try_ call answers at once instead of waiting.
    let mut buf = [0u8; 64];
    let empty = b.try_recv_from(&mut buf).unwrap_err();
    assert_eq!(empty.kind(), ErrorKind::WouldBlock);

    a.try_send_to(b"datagram", b.local_addr()?)?;
    // peek_sender names who sent the next datagram, which stays to be received.
    assert_eq!(b.peek_sender().await?, a.local_addr()?);
    b.readable().await?;
    let (n, from) = b.try_recv_from(&mut buf)?;
    assert_eq!((&buf[..n], from), (&b"datagram"[..], a.local_addr()?));
    Ok(())
}
