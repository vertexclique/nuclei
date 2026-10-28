use nuclei::net::UdpSocket;

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    let server = UdpSocket::bind("127.0.0.1:0").await?;
    let server_addr = server.local_addr()?;

    // Datagrams to an address, and received with their sender's address.
    let client = UdpSocket::bind("127.0.0.1:0").await?;
    client.send_to(b"ping", server_addr).await?;
    let mut buf = [0u8; 64];
    let (n, from) = server.recv_from(&mut buf).await?;
    assert_eq!((&buf[..n], from), (&b"ping"[..], client.local_addr()?));

    // A connected socket sends to, and receives from, its one peer.
    let peer = UdpSocket::connect(server_addr).await?;
    peer.send(b"hi").await?;
    let (_, from) = server.recv_from(&mut buf).await?;
    server.send_to(b"hello", from).await?;
    let n = peer.recv(&mut buf).await?;
    assert_eq!(&buf[..n], b"hello");
    Ok(())
}
