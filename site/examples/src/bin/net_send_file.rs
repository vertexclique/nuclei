use std::net::Shutdown;

use nuclei::fs::File;
use nuclei::io::AsyncReadExt;
use nuclei::net::{TcpListener, TcpStream};

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    nuclei::fs::write("page.html", vec![b'x'; 64 * 1024]).await?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let reader = nuclei::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let mut all = Vec::new();
        stream.read_to_end(&mut all).await?;
        std::io::Result::Ok(all.len())
    });

    let stream = TcpStream::connect(addr).await?;
    // send_file: the kernel sends from the page cache, the bytes never pass through the program.
    let file = File::open("page.html").await?;
    assert_eq!(stream.send_file(&file, 0, 64 * 1024).await?, 64 * 1024);

    // write_zero_copy: the kernel sends from the buffer, which comes back once it let go.
    let (written, released) = stream.write_zero_copy(vec![b'y'; 2 * 1024 * 1024]).await;
    assert_eq!(written?, 2 * 1024 * 1024);
    let buf = released.await;
    assert_eq!(buf.len(), 2 * 1024 * 1024);

    stream.shut(Shutdown::Write).await?;
    assert_eq!(reader.await?, 64 * 1024 + 2 * 1024 * 1024);
    nuclei::fs::remove_file("page.html").await
}
