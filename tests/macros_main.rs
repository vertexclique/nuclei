//! `#[nuclei::main(threads = N)]`, compiled against the runtime and run as a
//! program of its own (no test harness): its tasks run on its N workers, and
//! its result is the program's.

use std::collections::HashSet;
use std::thread;

#[nuclei::main(threads = 4)]
async fn main() -> std::io::Result<()> {
    let tasks: Vec<_> = (0..64)
        .map(|_| {
            nuclei::spawn(async {
                (
                    thread::current().id(),
                    thread::current().name().map(str::to_owned),
                )
            })
        })
        .collect();
    let mut workers = HashSet::new();
    for task in tasks {
        let (id, name) = task.await;
        assert_eq!(name.as_deref(), Some("nuclei-worker"));
        workers.insert(id);
    }
    assert!(
        workers.len() <= 4,
        "{} threads ran the tasks",
        workers.len()
    );
    Ok(())
}
