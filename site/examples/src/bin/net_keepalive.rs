use std::time::Duration;

use nuclei::io::AsyncWriteExt;
use nuclei::net::{TcpKeepalive, TcpSocket};

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    // A listener's options, set before it listens; incoming_cpu and defer_accept are Linux's.
    let socket = TcpSocket::new_v4()?;
    socket.set_reuseport(true)?;
    socket.set_incoming_cpu(0)?;
    socket.set_defer_accept(Some(Duration::from_secs(1)))?;
    socket.bind("127.0.0.1:0".parse().unwrap())?;
    let listener = socket.listen(128)?;

    // Probes after a minute idle, then every 10 seconds; 5 unanswered drop the connection.
    let socket = TcpSocket::new_v4()?;
    socket.set_keepalive_timing(TcpKeepalive {
        time: Some(Duration::from_secs(60)),
        interval: Some(Duration::from_secs(10)),
        retries: Some(5),
    })?;
    // Sent bytes may go unacknowledged for 30 seconds before the connection drops.
    socket.set_user_timeout(Some(Duration::from_secs(30)))?;
    let mut client = socket.connect(listener.local_addr()?).await?;
    assert_eq!(client.keepalive_timing()?.retries, Some(5));
    assert_eq!(client.user_timeout()?, Some(Duration::from_secs(30)));

    // With defer_accept, the connection is handed to accept once its first bytes came.
    client.write_all(b"hello").await?;
    client.flush().await?;
    let (server, _) = listener.accept().await?;
    server.set_quickack(true)?;
    Ok(())
}
