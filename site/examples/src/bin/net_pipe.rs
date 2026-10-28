use nuclei::io::{AsyncReadExt, AsyncWriteExt};
use nuclei::net::unix::pipe;

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    // An anonymous pipe: what goes into the sender comes out of the receiver.
    let (mut sender, mut receiver) = pipe::pipe()?;
    let writer = nuclei::spawn(async move {
        sender.write_all(b"through a pipe").await?;
        // Dropping the sender ends the stream for the reader.
        drop(sender);
        std::io::Result::Ok(())
    });
    let mut text = String::new();
    receiver.read_to_string(&mut text).await?;
    assert_eq!(text, "through a pipe");
    writer.await
}
