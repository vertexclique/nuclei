use std::rc::Rc;

use nuclei::runtime::{Runtime, worker_index};

fn main() -> std::io::Result<()> {
    let runtime = Runtime::builder().worker_threads(2).build()?;
    let handle = runtime.handle();
    assert_eq!(handle.workers(), 2);

    // A task placed on worker 1 runs there for its whole life.
    let task = handle.spawn_on_worker(1, async { worker_index() })?;
    assert_eq!(runtime.block_on(task), Some(1));

    // One task per worker, its future made there: it need not be Send.
    let each = handle.on_each_worker(|| async {
        let here = Rc::new(worker_index());
        nuclei::task::yield_now().await;
        *here
    });
    let mut seen = runtime.block_on(each.join_all());
    seen.sort();
    assert_eq!(seen, [Some(0), Some(1)]);

    // A worker the runtime does not have is refused.
    assert!(handle.spawn_on_worker(2, async {}).is_err());
    Ok(())
}
