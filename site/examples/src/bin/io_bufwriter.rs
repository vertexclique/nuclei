use nuclei::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use nuclei::net::{TcpListener, TcpStream};

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let stream = TcpStream::connect(listener.local_addr()?).await?;
    let (mut server, _) = listener.accept().await?;

    // Many small writes gather in the buffer and go out as one.
    let mut out = BufWriter::new(stream);
    for word in ["one ", "two ", "three"] {
        out.write_all(word.as_bytes()).await?;
    }
    out.flush().await?;
    out.shutdown().await?;

    let mut text = String::new();
    server.read_to_string(&mut text).await?;
    assert_eq!(text, "one two three");
    Ok(())
}
