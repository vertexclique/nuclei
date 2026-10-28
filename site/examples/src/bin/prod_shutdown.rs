use std::time::Duration;

use nuclei::net::{TcpListener, TcpStream};
use nuclei::sync::CancellationToken;
use nuclei::task::JoinSet;
use nuclei::time::timeout;

async fn serve(listener: TcpListener, shutdown: CancellationToken) -> std::io::Result<()> {
    let mut connections = JoinSet::new();
    // Take connections until the token is cancelled.
    while let Some(accepted) = shutdown.run_until_cancelled(listener.accept()).await {
        let (stream, _) = accepted?;
        let stop = shutdown.child_token();
        connections.spawn(async move {
            let mut buf = [0u8; 1024];
            while let Some(Ok(n @ 1..)) = stop.run_until_cancelled(stream.recv(&mut buf)).await {
                let _ = stream.send(&buf[..n]).await;
            }
        });
    }
    // Let the open connections finish, for five seconds at most.
    let drained = timeout(Duration::from_secs(5), connections.join_all()).await;
    if drained.is_err() {
        eprintln!("some connections did not finish in time");
    }
    Ok(())
}

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let shutdown = CancellationToken::new();

    // Ctrl-C cancels the token.
    let on_ctrl_c = shutdown.clone();
    nuclei::spawn(async move {
        if nuclei::signal::ctrl_c().await.is_ok() {
            on_ctrl_c.cancel();
        }
    })
    .detach();

    let server = nuclei::spawn(serve(listener, shutdown.clone()));

    // Here a client comes and goes, then the program asks for the shutdown itself.
    let client = TcpStream::connect(addr).await?;
    client.send(b"hi").await?;
    let mut buf = [0u8; 2];
    client.recv(&mut buf).await?;
    shutdown.cancel();
    server.await
}
