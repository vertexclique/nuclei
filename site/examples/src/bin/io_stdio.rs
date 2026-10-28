use nuclei::io::{self, AsyncWriteExt};

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    let mut out = io::stdout();
    out.write_all(b"written to stdout\n").await?;
    out.flush().await?;
    Ok(())
}
