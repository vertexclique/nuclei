use std::time::Duration;

use nuclei::Builder;
use nuclei::io::{AsyncReadExt, AsyncWriteExt};
use nuclei::net::{TcpListener, TcpStream};

fn main() -> std::io::Result<()> {
    let runtime = Builder::new()
        .worker_threads(1)
        .count_io(true) // the io-stats feature
        .record_latencies(true) // poll times; with io-stats, IO latencies too
        .build()?;
    runtime.block_on(runtime.spawn(async {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let mut client = TcpStream::connect(listener.local_addr()?).await?;
        let (mut server, _) = listener.accept().await?;
        client.write_all(b"ping").await?;
        client.flush().await?;
        let mut buf = [0u8; 4];
        server.read_exact(&mut buf).await?;
        Ok::<_, std::io::Error>(())
    }))?;
    // A worker publishes its counts each time it looks at its IO and timers: a sleep makes it look.
    runtime.block_on(runtime.spawn(nuclei::time::sleep(Duration::from_millis(1))));

    let stats = runtime.stats();
    let worker = &stats.shards[0];
    let io = worker.io.unwrap();
    assert_eq!((io.accepts, io.bytes_read, io.bytes_written), (1, 4, 4));
    let polls = worker.poll_times.as_ref().unwrap();
    for (from, until, samples) in polls.buckets() {
        println!("{samples} polls of {from:?} to {until:?}");
    }
    // Readiness backends (epoll, kqueue) have no wait in the kernel to time.
    if let Some(io) = &worker.io_latencies {
        let timed = [&io.reads, &io.writes, &io.accepts, &io.connects, &io.files];
        println!(
            "{} IO operations timed",
            timed.iter().map(|h| h.count()).sum::<u64>()
        );
    }
    Ok(())
}
