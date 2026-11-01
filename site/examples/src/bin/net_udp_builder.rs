use nuclei::net::{UdpSocket, UdpSocketBuilder};

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    // Options a UDP socket takes before it binds.
    let builder = UdpSocketBuilder::new_v4()?;
    builder.set_reuseport(true)?;
    builder.set_recv_buffer_size(256 * 1024)?;
    builder.set_tos(0x10)?;
    assert!(builder.recv_buffer_size()? >= 256 * 1024);
    let server = builder.bind("127.0.0.1:0".parse().unwrap())?;
    let addr = server.local_addr()?;

    // SO_REUSEPORT on both lets a second socket bind the same port.
    let second = UdpSocketBuilder::new_v4()?;
    second.set_reuseport(true)?;
    let second = second.bind(addr)?;
    assert_eq!(second.local_addr()?, addr);
    drop(second);

    // Bound, it is a UdpSocket like any other.
    let client = UdpSocket::bind("127.0.0.1:0").await?;
    client.send_to(b"ping", addr).await?;
    let mut buf = [0u8; 16];
    let (n, from) = server.recv_from(&mut buf).await?;
    assert_eq!((&buf[..n], from), (&b"ping"[..], client.local_addr()?));
    Ok(())
}
