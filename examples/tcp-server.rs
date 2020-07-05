use proactor::*;
use std::net::{TcpListener, TcpStream};

use bastion::executor::blocking;
use futures::io;

async fn echo(stream: Handle<TcpStream>) -> io::Result<()> {
    io::copy(&stream, &mut &stream).await?;
    Ok(())
}

fn main() -> io::Result<()> {
    run(async {
        // Create a listener.
        let listener = Handle::<TcpListener>::bind("0.0.0.0:7000")?;
        println!("Listening on {}", listener.get_ref().local_addr()?);
        println!("Now start a TCP client.");

        // Accept clients in a loop.
        loop {
            let (stream, peer_addr) = listener.accept().await?;
            println!("Accepted client: {}", peer_addr);

            // Spawn a task that echoes messages from the client back to it.
            blocking(echo(stream));
        }
    })
}