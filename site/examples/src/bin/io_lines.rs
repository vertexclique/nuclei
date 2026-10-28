use nuclei::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    let (rx, mut tx) = io::simplex(1024);
    tx.write_all(b"GET /\nHost: nuclei.rs\n\nbody").await?;
    drop(tx);

    // A buffer in front of the reader, then a line at a time.
    let mut lines = BufReader::new(rx).lines();
    let mut head = Vec::new();
    while let Some(line) = lines.next_line().await? {
        if line.is_empty() {
            break;
        }
        head.push(line);
    }
    assert_eq!(head, ["GET /", "Host: nuclei.rs"]);
    Ok(())
}
