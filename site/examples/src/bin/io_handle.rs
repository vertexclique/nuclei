use nuclei::Handle;

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    // Any std socket or file, taken over: Handle::new makes its IO async.
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let listener = Handle::new(std_listener)?;
    let addr = listener.get_ref().local_addr()?;

    let client = Handle::new(std::net::TcpStream::connect(addr)?)?;
    let (server, _) = listener.accept().await?;
    client.send(b"hi").await?;
    let mut buf = [0u8; 2];
    server.recv(&mut buf).await?;

    // into_inner gives std's object back once the bytes written behind went out.
    let std_stream: std::net::TcpStream = client.into_inner();
    assert_eq!(std_stream.peer_addr()?, addr);
    Ok(())
}
