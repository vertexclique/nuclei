use nuclei::sync::{Owner, ReentrantMutex};

#[nuclei::main]
async fn main() {
    let state = ReentrantMutex::new(Vec::<&str>::new());
    // The lock is held by an owner, not a thread: tasks move between threads.
    let me = Owner::new();

    let outer = state.lock(&me).await;
    // The same owner takes it again without waiting for itself.
    let inner = state.lock(&me).await;
    assert!(inner.is_empty() && outer.is_empty());
    drop(inner);
    drop(outer);

    // Another owner waits until every hold of the first is gone.
    let other = Owner::new();
    assert!(state.try_lock(&other).is_some());
}
