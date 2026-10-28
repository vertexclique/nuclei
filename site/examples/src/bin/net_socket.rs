use std::time::Duration;

use nuclei::net::TcpSocket;

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    // Set a socket up before it listens...
    let socket = TcpSocket::new_v4()?;
    socket.set_reuseaddr(true)?;
    socket.set_recv_buffer_size(256 * 1024)?;
    socket.bind("127.0.0.1:0".parse().unwrap())?;
    let listener = socket.listen(1024)?;
    let addr = listener.local_addr()?;

    // ...or before it connects.
    let socket = TcpSocket::new_v4()?;
    socket.set_nodelay(true)?;
    socket.set_keepalive(true)?;
    let stream = socket.connect(addr).await?;
    assert!(stream.nodelay()?);

    // Options of a stream that is connected already: linger on close.
    let (accepted, _) = listener.accept().await?;
    accepted.set_linger(Some(Duration::from_secs(1)))?;
    assert_eq!(accepted.linger()?, Some(Duration::from_secs(1)));
    Ok(())
}
