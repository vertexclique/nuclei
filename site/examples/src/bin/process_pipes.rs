use nuclei::io::{AsyncReadExt, AsyncWriteExt};
use nuclei::process::{Command, Stdio};

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    let mut child = Command::new("sort")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;

    // The pipes are async streams.
    let mut stdin = child.stdin.take().expect("piped");
    stdin.write_all(b"pear\napple\nfig\n").await?;
    // Dropping stdin ends the child's input, so sort can finish.
    drop(stdin);

    let mut stdout = child.stdout.take().expect("piped");
    let mut sorted = String::new();
    stdout.read_to_string(&mut sorted).await?;
    assert_eq!(sorted, "apple\nfig\npear\n");
    assert!(child.wait().await?.success());
    Ok(())
}
