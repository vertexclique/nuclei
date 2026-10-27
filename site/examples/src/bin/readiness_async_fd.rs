use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

use nuclei::io::unix::AsyncFd;

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    // A descriptor of your own (here a std socket), in non-blocking mode.
    let (mine, mut theirs) = UnixStream::pair()?;
    mine.set_nonblocking(true)?;
    let fd = AsyncFd::new(mine)?;

    theirs.write_all(b"edge")?;
    let mut buf = [0u8; 4];
    let n = loop {
        // Wait until the driver says it may be readable...
        let mut guard = fd.readable().await?;
        // ...then make the call; WouldBlock clears the readiness and waits again.
        match guard.try_io(|inner| inner.get_ref().read(&mut buf)) {
            Ok(result) => break result?,
            Err(_would_block) => continue,
        }
    };
    assert_eq!(&buf[..n], b"edge");
    Ok(())
}
