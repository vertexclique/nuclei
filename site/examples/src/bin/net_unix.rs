use nuclei::io::{AsyncReadExt, AsyncWriteExt};
use nuclei::net::{UnixDatagram, UnixListener, UnixStream};

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    let dir = std::env::temp_dir().join(format!("learn-unix-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("server.sock");

    // Streams over a path in the file system.
    let listener = UnixListener::bind(&path)?;
    let mut client = UnixStream::connect(&path).await?;
    let (mut server, _) = listener.accept().await?;
    client.write_all(b"over a path").await?;
    let mut buf = [0u8; 11];
    server.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"over a path");

    // Who is at the other end: user, group and, where the system says, process.
    let peer = server.peer_cred()?;
    assert_eq!(peer.pid(), Some(std::process::id() as i32));

    // A connected pair, and datagrams from a socket bound to no address.
    let (a, b) = UnixStream::pair()?;
    a.send(b"paired").await?;
    assert_eq!(b.recv(&mut buf).await?, 6);
    let inbox = UnixDatagram::bind(dir.join("inbox.sock"))?;
    let sender = UnixDatagram::unbound()?;
    sender.send_to(b"note", dir.join("inbox.sock")).await?;
    assert_eq!(inbox.recv(&mut buf).await?, 4);

    std::fs::remove_dir_all(&dir)
}
