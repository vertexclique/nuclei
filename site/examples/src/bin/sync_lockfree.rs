use std::sync::Arc;

use nuclei::sync::{ArrayQueue, Atom, HopscotchMap, SegQueue};

#[nuclei::main]
async fn main() {
    // A concurrent map: every operation is lock-free, values are cloned out.
    let sessions = Arc::new(HopscotchMap::new());
    let writers: Vec<_> = (0..4u32)
        .map(|n| {
            let sessions = Arc::clone(&sessions);
            nuclei::spawn(async move { sessions.insert(n, format!("user-{n}")) })
        })
        .collect();
    for writer in writers {
        writer.await;
    }
    assert_eq!(sessions.get(&2).as_deref(), Some("user-2"));
    assert_eq!(sessions.len(), 4);

    // Queues: unbounded, and bounded with a push that refuses when full.
    let work = SegQueue::new();
    work.push("job");
    assert_eq!(work.pop(), Some("job"));
    let ring = ArrayQueue::new(1);
    assert!(ring.push(1).is_ok());
    assert_eq!(ring.push(2), Err(2));

    // Atom: a value read without a lock and replaced whole.
    let config = Atom::new(String::from("v1"));
    config.store(String::from("v2"));
    assert_eq!(config.load_clone(), "v2");
}
