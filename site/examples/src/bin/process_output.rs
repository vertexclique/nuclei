use nuclei::process::Command;

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    // Run a program to its end and collect what it printed.
    let output = Command::new("echo").arg("hello").output().await?;
    assert!(output.status.success());
    assert_eq!(output.stdout, b"hello\n");

    // Or just its exit status, its streams left as they are.
    let status = Command::new("true").status().await?;
    assert_eq!(status.code(), Some(0));
    Ok(())
}
