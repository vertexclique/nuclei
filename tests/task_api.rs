//! `nuclei::task`: ids, `join` and `JoinError`, `abort` and `AbortHandle`,
//! `yield_now`.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::thread;

use nuclei::runtime::Runtime;
use nuclei::task::{self, AbortHandle};

fn runtime(workers: usize) -> Runtime {
    Runtime::builder()
        .worker_threads(workers)
        .pin_workers(false)
        .build()
        .expect("the runtime starts")
}

/// Never ready; counts its drops and the thread it was dropped on.
struct Forever {
    drops: Arc<AtomicUsize>,
    dropped_on: Arc<Mutex<Option<thread::ThreadId>>>,
}

impl Future for Forever {
    type Output = ();
    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
        Poll::Pending
    }
}

impl Drop for Forever {
    fn drop(&mut self) {
        *self.dropped_on.lock().unwrap() = Some(thread::current().id());
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

fn forever() -> (
    Forever,
    Arc<AtomicUsize>,
    Arc<Mutex<Option<thread::ThreadId>>>,
) {
    let drops = Arc::new(AtomicUsize::new(0));
    let on = Arc::new(Mutex::new(None));
    (
        Forever {
            drops: Arc::clone(&drops),
            dropped_on: Arc::clone(&on),
        },
        drops,
        on,
    )
}

#[test]
fn every_task_has_its_own_id_and_sees_it() {
    let rt = runtime(2);
    rt.block_on(async {
        assert_eq!(task::try_id(), None, "block_on's own future is no task");
        let tasks: Vec<_> = (0..64).map(|_| task::spawn(async { task::id() })).collect();
        let mut ids = HashSet::new();
        for t in tasks {
            let id = t.id();
            assert_eq!(t.await, id, "a task sees the id its handle reports");
            assert!(ids.insert(id), "ids are unique");
        }
    });
}

#[test]
fn join_gives_the_output_the_panic_or_the_cancel() {
    let rt = runtime(2);
    rt.block_on(async {
        assert_eq!(task::spawn(async { 7 }).join().await.unwrap(), 7);

        let panicked = task::spawn(async { panic!("boom") });
        let id = panicked.id();
        let error = panicked.join().await.unwrap_err();
        assert!(error.is_panic() && !error.is_cancelled());
        assert_eq!(error.id(), id);
        assert_eq!(error.to_string(), format!("task {id} panicked: boom"));
        assert_eq!(*error.into_panic().downcast::<&str>().unwrap(), "boom");

        let (future, drops, _) = forever();
        let pending = task::spawn(future);
        pending.abort();
        let error = pending.join().await.unwrap_err();
        assert!(error.is_cancelled() && !error.is_panic());
        assert!(error.to_string().ends_with("was cancelled"));
        assert!(error.try_into_panic().is_err());
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "the cancelled future was dropped"
        );
    });
}

#[test]
fn an_abort_handle_cancels_from_another_thread_and_outlives_the_task() {
    let rt = runtime(2);
    rt.block_on(async {
        let (future, drops, _) = forever();
        let t = task::spawn(future);
        let handle: AbortHandle = t.abort_handle();
        assert_eq!(handle.id(), t.id());
        assert!(!handle.is_finished());
        let remote = handle.clone();
        thread::spawn(move || remote.abort()).join().unwrap();
        assert!(t.join().await.unwrap_err().is_cancelled());
        assert!(handle.is_finished());
        assert_eq!(drops.load(Ordering::SeqCst), 1);

        // Detached: the handle still reaches the task.
        let (future, drops, _) = forever();
        let t = task::spawn(future);
        let handle = t.abort_handle();
        t.detach();
        handle.abort();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while drops.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            task::yield_now().await;
        }
        assert_eq!(
            drops.load(Ordering::SeqCst),
            1,
            "the detached task's future was dropped"
        );

        // Aborting a finished task changes nothing.
        let done = task::spawn(async { 3 });
        let handle = done.abort_handle();
        assert_eq!(done.await, 3);
        handle.abort();
        assert!(handle.is_finished());
    });
}

#[test]
fn a_local_task_aborted_from_another_thread_drops_its_future_on_its_shard() {
    let rt = runtime(1);
    rt.block_on(async {
        let (future, drops, on) = forever();
        let t = task::spawn_local(async move {
            let _keep = std::rc::Rc::new(());
            future.await
        });
        let handle = t.abort_handle();
        t.detach();
        let shard = thread::current().id();
        // The last reference goes on another thread: the future must still
        // be dropped where the task runs.
        thread::spawn(move || {
            handle.abort();
            drop(handle);
        })
        .join()
        .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while drops.load(Ordering::SeqCst) == 0 && std::time::Instant::now() < deadline {
            task::yield_now().await;
        }
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(*on.lock().unwrap(), Some(shard));
    });
}

#[test]
fn yield_now_lets_the_shards_other_tasks_run() {
    let rt = runtime(1);
    let order = rt.block_on(async {
        let order = Arc::new(Mutex::new(Vec::new()));
        let make = |name: char| {
            let order = Arc::clone(&order);
            task::spawn_local(async move {
                for _ in 0..3 {
                    order.lock().unwrap().push(name);
                    task::yield_now().await;
                }
            })
        };
        let (a, b) = (make('a'), make('b'));
        a.await;
        b.await;
        Arc::try_unwrap(order).unwrap().into_inner().unwrap()
    });
    // Which task runs first is the shard's choice (its LIFO slot takes the
    // newest); each yield hands the turn to the other.
    assert_eq!(order.len(), 6);
    assert!(
        order.windows(2).all(|w| w[0] != w[1]),
        "the tasks did not alternate: {order:?}"
    );
}

#[test]
fn a_yield_in_block_on_lets_every_queued_task_run_first() {
    let rt = runtime(0);
    rt.block_on(async {
        let ran = Arc::new(AtomicUsize::new(0));
        for _ in 0..5 {
            let ran = Arc::clone(&ran);
            task::spawn_local(async move {
                ran.fetch_add(1, Ordering::SeqCst);
            })
            .detach();
        }
        task::yield_now().await;
        assert_eq!(
            ran.load(Ordering::SeqCst),
            5,
            "one pass of the queue runs before the yielding future goes on"
        );
    });
}
