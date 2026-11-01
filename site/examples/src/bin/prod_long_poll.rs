use std::sync::mpsc;
use std::time::Duration;

use nuclei::{Builder, task};

fn main() -> std::io::Result<()> {
    let (told, heard) = mpsc::channel();
    let runtime = Builder::new()
        .worker_threads(1)
        .handoff_after(Some(Duration::from_millis(10)))
        // Called on the watch's thread for a poll that runs past the handoff.
        .on_long_poll(move |task| {
            let _ = told.send(task.name().map(str::to_owned));
        })
        .build()?;

    let name = runtime.block_on(async move {
        let blocker = task::Builder::new()
            .name("report")
            .worker(0)
            .spawn(async move {
                // A blocking call inside a poll, on purpose: it waits for the report of itself.
                heard.recv_timeout(Duration::from_secs(10))
            })?;
        Ok::<_, std::io::Error>(blocker.await)
    })?;
    assert_eq!(name, Ok(Some("report".to_owned())));
    Ok(())
}
