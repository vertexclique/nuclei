//! A runtime dropped in one of its own tasks, on one of its own threads:
//! the drop joins every other thread and returns, never joining its own
//! (proofs/tla/teardown).

use std::time::Duration;

use nuclei::runtime::{Builder, Runtime};

#[test]
fn a_runtime_dropped_in_one_of_its_own_tasks_returns() {
    for handoff in [None, Some(Duration::from_millis(10))] {
        let rt = Builder::new()
            .worker_threads(2)
            .pin_workers(false)
            .handoff_after(handoff)
            .build()
            .expect("the runtime starts");
        let (give, take) = std::sync::mpsc::channel::<Runtime>();
        let (dropped, done) = std::sync::mpsc::channel();
        rt.spawn(async move {
            let rt = take.recv().unwrap();
            drop(rt);
            dropped.send(()).unwrap();
        })
        .detach();
        give.send(rt).unwrap();
        done.recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| {
                panic!("a drop on the runtime's own thread never returned ({handoff:?})")
            });
    }
}
