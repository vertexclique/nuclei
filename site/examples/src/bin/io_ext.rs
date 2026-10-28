use nuclei::io::{self, AsyncReadExt, AsyncWriteExt};

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    // Two connected in-memory streams, 64 bytes of buffer each way.
    let (mut client, mut server) = io::duplex(64);

    // A frame: a length, big-endian, then that many bytes.
    let body = b"framed";
    client.write_u32(body.len() as u32).await?;
    client.write_all(body).await?;

    let len = server.read_u32().await?;
    let mut frame = vec![0u8; len as usize];
    server.read_exact(&mut frame).await?;
    assert_eq!(frame, body);

    // Ending one side ends the other's reads.
    drop(client);
    let mut rest = Vec::new();
    assert_eq!(server.read_to_end(&mut rest).await?, 0);
    Ok(())
}
