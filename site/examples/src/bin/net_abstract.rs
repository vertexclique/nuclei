use std::os::linux::net::SocketAddrExt;
use std::os::unix::net::SocketAddr;

use nuclei::net::{UnixListener, UnixStream};

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    // Linux: a name in the abstract namespace, with no file behind it.
    let name = format!("learn-{}", std::process::id());
    let addr = SocketAddr::from_abstract_name(name.as_bytes())?;
    let listener = UnixListener::bind_addr(&addr)?;
    let client = UnixStream::connect_addr(&addr).await?;
    let (server, _) = listener.accept().await?;
    client.send(b"no file").await?;
    let mut buf = [0u8; 7];
    assert_eq!(server.recv(&mut buf).await?, 7);
    Ok(())
}
