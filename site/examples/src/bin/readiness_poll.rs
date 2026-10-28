use std::future::poll_fn;

use nuclei::io::ReadBuf;
use nuclei::net::{TcpListener, TcpStream};

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let client = nuclei::spawn(TcpStream::connect(listener.local_addr()?));

    // poll_accept: the accept as a poll, for a future or a stream of your own.
    let (server, _) = poll_fn(|cx| listener.poll_accept(cx)).await?;
    let client = client.await?;

    client.send(b"peek").await?;
    // poll_peek: the bytes the next receive gives, left where they are.
    let mut storage = [0u8; 4];
    let mut buf = ReadBuf::new(&mut storage);
    let n = poll_fn(|cx| server.poll_peek(cx, &mut buf)).await?;
    assert_eq!(n, 4);
    assert_eq!(buf.filled(), b"peek");
    Ok(())
}
