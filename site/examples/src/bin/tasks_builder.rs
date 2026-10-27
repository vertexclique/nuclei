use nuclei::task;

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    // A task spawned with options: a name.
    let worker = task::Builder::new()
        .name("checksum")
        .spawn(async { task::id() })?;
    let id = worker.id();

    // Inside the task, task::id() is the same id the handle reports.
    assert_eq!(worker.await, id);
    // Outside any task there is none.
    assert!(task::try_id().is_none());
    Ok(())
}
