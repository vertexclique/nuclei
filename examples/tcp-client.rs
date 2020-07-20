use std::io;
use std::net::TcpStream;
use nuclei::*;
use futures::AsyncWriteExt;

fn main() -> io::Result<()> {
    drive(async {
        println!("Connecting to server");
        let mut stream =
            Handle::<TcpStream>::connect("127.0.0.1:7000").await?;
        println!("Connected to {}", stream.get_ref().peer_addr()?);

        let result = stream.write(b"hello world\n").await;
        println!("Wrote, success={:?}", result.is_ok());

        Ok(())
    })
}
