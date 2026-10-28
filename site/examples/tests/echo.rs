use std::time::Duration;

use nuclei::net::{TcpListener, TcpStream};
use nuclei::time::{self, Instant};

#[nuclei::test]
async fn echoes_a_message() -> std::io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    nuclei::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut buf = [0u8; 64];
        let n = stream.recv(&mut buf).await?;
        stream.send(&buf[..n]).await?;
        std::io::Result::Ok(())
    })
    .detach();

    let client = TcpStream::connect(addr).await?;
    client.send(b"ping").await?;
    let mut buf = [0u8; 4];
    client.recv(&mut buf).await?;
    assert_eq!(&buf, b"ping");
    Ok(())
}

// A runtime of its own with one worker, for a test that needs a known shape.
#[nuclei::test(threads = 1)]
async fn times_out() {
    let never = std::future::pending::<()>();
    assert!(
        time::timeout(Duration::from_millis(10), never)
            .await
            .is_err()
    );
}

// A paused clock: the day-long timeout passes at once.
#[nuclei::test(start_paused = true)]
async fn a_day_passes_at_once() {
    let start = Instant::now();
    let never = std::future::pending::<()>();
    assert!(
        time::timeout(Duration::from_secs(86_400), never)
            .await
            .is_err()
    );
    assert_eq!(start.elapsed(), Duration::from_secs(86_400));
}
