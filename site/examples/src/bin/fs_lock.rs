use nuclei::fs::{File, TryLockError};

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    let holder = File::create("app.lock").await?;
    let other = File::open("app.lock").await?;

    // Wait for the lock on the blocking pool; the worker runs on meanwhile.
    holder.lock_file(false).await?;
    // While it is held, another handle cannot have it.
    let refused = other.try_lock_file(false).await;
    assert!(matches!(refused, Err(TryLockError::WouldBlock)));

    // Released once the writes before it landed; then free for the other.
    holder.unlock_file().await?;
    other.try_lock_file(true).await?;
    other.unlock_file().await?;
    nuclei::fs::remove_file("app.lock").await
}
