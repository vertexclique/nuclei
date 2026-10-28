//! `task_local!`: a value per scoped future, seen in every poll wherever the
//! task runs, never by another task of the shard.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use nuclei::runtime::Runtime;
use nuclei::task;

nuclei::task_local! {
    static ID: u64;
    static NAME: String;
}

fn runtime(workers: usize) -> Runtime {
    Runtime::builder()
        .worker_threads(workers)
        .pin_workers(false)
        .build()
        .expect("the runtime starts")
}

#[test]
fn a_scope_sets_the_value_across_awaits_and_only_inside() {
    let rt = runtime(1);
    rt.block_on(async {
        assert!(ID.try_with(|_| ()).is_err());
        ID.scope(7, async {
            assert_eq!(ID.get(), 7);
            task::yield_now().await;
            assert_eq!(ID.with(|v| *v), 7, "the value survives a suspension");
            ID.scope(8, async { assert_eq!(ID.get(), 8) }).await;
            assert_eq!(ID.get(), 7, "an inner scope puts the outer value back");
        })
        .await;
        assert!(ID.try_with(|_| ()).is_err(), "unset once the scope ended");
    });
}

#[test]
fn tasks_of_one_shard_never_see_each_others_values() {
    let rt = runtime(1);
    rt.block_on(async {
        let tasks: Vec<_> = (0..8u64)
            .map(|i| {
                task::spawn_local(ID.scope(i, async move {
                    for _ in 0..4 {
                        assert_eq!(ID.get(), i);
                        task::yield_now().await;
                    }
                    ID.get()
                }))
            })
            .collect();
        for (i, t) in tasks.into_iter().enumerate() {
            assert_eq!(t.await, i as u64);
        }
    });
}

#[test]
fn sync_scope_and_two_keys() {
    let n = ID.sync_scope(3, || {
        NAME.sync_scope("x".to_string(), || ID.get() + NAME.get().len() as u64)
    });
    assert_eq!(n, 4);
    assert!(ID.try_with(|_| ()).is_err());
    assert_eq!(
        ID.try_with(|_| ()).unwrap_err().to_string(),
        "task-local value not set"
    );
}

#[test]
#[should_panic(expected = "task-local value not set")]
fn with_outside_a_scope_panics() {
    ID.with(|_| ());
}

#[test]
fn a_panic_in_the_scope_still_unsets_the_value() {
    let r = std::panic::catch_unwind(|| ID.sync_scope(5, || panic!("inside")));
    assert!(r.is_err());
    assert!(ID.try_with(|_| ()).is_err());
}

#[test]
fn a_scoped_future_dropped_early_sees_its_value_in_its_drop() {
    struct SeesValue(Arc<AtomicUsize>);
    impl Drop for SeesValue {
        fn drop(&mut self) {
            self.0
                .store(ID.try_with(|v| *v).unwrap_or(0) as usize, Ordering::SeqCst);
        }
    }
    let seen = Arc::new(AtomicUsize::new(0));
    let guard = SeesValue(Arc::clone(&seen));
    let fut = ID.scope(9, async move {
        let _guard = guard;
        std::future::pending::<()>().await;
    });
    let rt = runtime(1);
    rt.block_on(async {
        let mut fut = Box::pin(fut);
        // Poll once so the inner future holds the guard, then drop it.
        let _ = futures_lite_poll_once(fut.as_mut()).await;
        drop(fut);
    });
    assert_eq!(seen.load(Ordering::SeqCst), 9);
}

/// Polls `f` once.
async fn futures_lite_poll_once<F: std::future::Future + Unpin>(mut f: F) -> Option<F::Output> {
    std::future::poll_fn(|cx| match std::pin::Pin::new(&mut f).poll(cx) {
        std::task::Poll::Ready(v) => std::task::Poll::Ready(Some(v)),
        std::task::Poll::Pending => std::task::Poll::Ready(None),
    })
    .await
}
