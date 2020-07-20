#[cfg(unix)]
fn main() -> std::io::Result<()> {
    use futures::prelude::*;
    use nuclei::*;
    use std::os::unix::net::UnixStream;

    drive(async {
        // Create a Unix stream that receives a byte on each signal occurrence.
        let (a, mut b) = Handle::<UnixStream>::pair()?;
        signal_hook::pipe::register(signal_hook::SIGINT, a)?;
        println!("Waiting for Ctrl-C...");

        // Receive a byte that indicates the Ctrl-C signal occurred.
        b.read_exact(&mut [0]).await?;

        println!("Done!");
        Ok(())
    })
}
