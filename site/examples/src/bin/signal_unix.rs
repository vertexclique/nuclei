use nuclei::process::Command;
use nuclei::signal::unix::{SignalKind, signal};

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    // Listening starts here; the signal's default action is replaced.
    let mut reload = signal(SignalKind::user_defined1())?;
    let mut hangup = signal(SignalKind::hangup())?;

    // Send this process SIGUSR1, as an operator would with `kill -USR1`.
    let pid = std::process::id().to_string();
    Command::new("kill").args(["-USR1", &pid]).status().await?;

    nuclei::select! {
        _ = reload.recv() => println!("reloading"),
        _ = hangup.recv() => println!("hung up"),
    }
    Ok(())
}
