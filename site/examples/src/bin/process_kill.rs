use nuclei::process::Command;

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    // kill ends the child and waits for it to go.
    let mut child = Command::new("sleep").arg("60").spawn()?;
    child.kill().await?;
    assert!(!child.wait().await?.success());

    // start_kill only starts it; wait to see it go.
    let mut child = Command::new("sleep").arg("60").spawn()?;
    child.start_kill()?;
    child.wait().await?;

    // kill_on_drop: dropping the Child ends the process too.
    let child = Command::new("sleep").arg("60").kill_on_drop(true).spawn()?;
    drop(child);
    Ok(())
}
