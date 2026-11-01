use std::future::poll_fn;

use nuclei::io::ReadBuf;
use nuclei::net::UdpSocket;

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    let server = UdpSocket::bind("127.0.0.1:0").await?;
    let client = UdpSocket::bind("127.0.0.1:0").await?;
    let to = server.local_addr()?;

    // poll_send_to: a datagram's send as a poll.
    let sent = poll_fn(|cx| client.poll_send_to(cx, b"ping", to)).await?;
    assert_eq!(sent, 4);

    // poll_recv_from: the datagram fills the ReadBuf, and the answer is its sender.
    let mut storage = [0u8; 16];
    let mut buf = ReadBuf::new(&mut storage);
    let from = poll_fn(|cx| server.poll_recv_from(cx, &mut buf)).await?;
    assert_eq!((buf.filled(), from), (&b"ping"[..], client.local_addr()?));
    Ok(())
}
