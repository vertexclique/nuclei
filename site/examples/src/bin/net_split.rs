use nuclei::io::{AsyncReadExt, AsyncWriteExt};
use nuclei::net::{TcpListener, TcpStream};

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let mut client = TcpStream::connect(listener.local_addr()?).await?;
    let (stream, _) = listener.accept().await?;

    // Owned halves: the read half and the write half go to tasks of their own.
    let (mut read, mut write) = stream.into_split();
    let reader = nuclei::spawn(async move {
        let mut got = [0u8; 4];
        read.read_exact(&mut got).await?;
        std::io::Result::Ok((read, got))
    });
    let writer = nuclei::spawn(async move {
        write.write_all(b"pong").await?;
        write.flush().await?;
        std::io::Result::Ok(write)
    });

    client.write_all(b"ping").await?;
    let mut reply = [0u8; 4];
    client.read_exact(&mut reply).await?;
    assert_eq!(&reply, b"pong");

    // The two halves go back together into the stream.
    let (read, got) = reader.await?;
    assert_eq!(&got, b"ping");
    let stream = read.reunite(writer.await?).expect("halves of one stream");
    assert_eq!(stream.peer_addr()?, client.local_addr()?);
    Ok(())
}
