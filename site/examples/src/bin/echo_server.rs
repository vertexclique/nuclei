use nuclei::net::TcpListener;

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:7000").await?;
    loop {
        let (stream, _) = listener.accept().await?;
        nuclei::spawn(async move {
            let mut buf = [0u8; 1024];
            while let Ok(n @ 1..) = stream.recv(&mut buf).await {
                if stream.send(&buf[..n]).await.is_err() {
                    break;
                }
            }
        })
        .detach();
    }
}
