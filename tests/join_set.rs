//! `JoinSet`: outputs in finishing order, every task joined once, cancels on
//! drop, abort and shutdown, detach, try and join_all.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use nuclei::runtime::Runtime;
use nuclei::sync::oneshot;
use nuclei::task::{self, JoinSet};

fn runtime(workers: usize) -> Runtime {
    Runtime::builder()
        .worker_threads(workers)
        .pin_workers(false)
        .build()
        .expect("the runtime starts")
}

/// Counts its drops.
struct Counted(Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn outputs_come_back_in_the_order_the_tasks_finish() {
    let rt = runtime(2);
    rt.block_on(async {
        let mut set = JoinSet::new();
        let mut gates = Vec::new();
        for i in 0..5u32 {
            let (tx, rx) = oneshot::channel::<()>();
            gates.push(Some(tx));
            set.spawn(async move {
                rx.await.unwrap();
                i
            });
        }
        assert_eq!(set.len(), 5);
        for i in [3usize, 0, 4, 1, 2] {
            gates[i].take().unwrap().send(()).unwrap();
            assert_eq!(set.join_next().await.unwrap().unwrap(), i as u32);
        }
        assert!(set.is_empty());
        assert!(set.join_next().await.is_none());
    });
}

#[test]
fn every_task_is_joined_exactly_once() {
    let rt = runtime(4);
    rt.block_on(async {
        let mut set: JoinSet<u32> = (0..1000u32).map(|i| async move { i }).collect();
        set.extend((1000..1100u32).map(|i| async move { i }));
        let mut seen = HashSet::new();
        while let Some(out) = set.join_next().await {
            assert!(seen.insert(out.unwrap()), "an output came back twice");
        }
        assert_eq!(seen.len(), 1100);
    });
}

#[test]
fn panics_and_cancels_come_back_as_join_errors() {
    let rt = runtime(2);
    rt.block_on(async {
        let mut set = JoinSet::new();
        set.spawn(async { panic!("member") });
        let pending = set.spawn(std::future::pending::<()>());
        pending.abort();
        let mut panics = 0;
        let mut cancels = 0;
        while let Some(out) = set.join_next().await {
            let error = out.unwrap_err();
            if error.is_panic() {
                panics += 1;
            } else {
                cancels += 1;
            }
        }
        assert_eq!((panics, cancels), (1, 1));
    });
}

#[test]
fn abort_all_shutdown_and_drop_cancel_and_drop_the_futures() {
    let rt = runtime(2);
    rt.block_on(async {
        let drops = Arc::new(AtomicUsize::new(0));
        let pending = |drops: &Arc<AtomicUsize>| {
            let counted = Counted(Arc::clone(drops));
            async move {
                let _counted = counted;
                std::future::pending::<()>().await
            }
        };

        let mut set = JoinSet::new();
        for _ in 0..4 {
            set.spawn(pending(&drops));
        }
        set.abort_all();
        let mut cancelled = 0;
        while let Some(out) = set.join_next().await {
            assert!(out.unwrap_err().is_cancelled());
            cancelled += 1;
        }
        assert_eq!(cancelled, 4);
        assert_eq!(drops.load(Ordering::SeqCst), 4);

        for _ in 0..3 {
            set.spawn(pending(&drops));
        }
        set.shutdown().await;
        assert!(set.is_empty());
        assert_eq!(drops.load(Ordering::SeqCst), 7);

        let mut dropped = JoinSet::new();
        for _ in 0..2 {
            dropped.spawn(pending(&drops));
        }
        drop(dropped);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while drops.load(Ordering::SeqCst) < 9 && std::time::Instant::now() < deadline {
            task::yield_now().await;
        }
        assert_eq!(
            drops.load(Ordering::SeqCst),
            9,
            "dropping the set cancels its tasks"
        );
    });
}

#[test]
fn detach_all_lets_the_tasks_finish_without_the_set() {
    let rt = runtime(2);
    rt.block_on(async {
        let (tx, rx) = oneshot::channel::<u32>();
        let mut set = JoinSet::new();
        set.spawn(async move {
            tx.send(5).unwrap();
        });
        set.detach_all();
        assert!(set.is_empty());
        assert_eq!(rx.await.unwrap(), 5, "the detached task ran to its end");
    });
}

#[test]
fn try_join_next_answers_only_finished_tasks() {
    let rt = runtime(2);
    rt.block_on(async {
        let (tx, rx) = oneshot::channel::<()>();
        let mut set = JoinSet::new();
        set.spawn(async move {
            rx.await.unwrap();
            1
        });
        assert!(set.try_join_next().is_none());
        tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut got = None;
        while got.is_none() && std::time::Instant::now() < deadline {
            got = set.try_join_next();
            if got.is_none() {
                task::yield_now().await;
            }
        }
        assert_eq!(got.unwrap().unwrap(), 1);
        assert!(set.is_empty());
    });
}

#[test]
fn join_all_gives_every_output_and_ids_match() {
    let rt = runtime(2);
    rt.block_on(async {
        let mut set = JoinSet::new();
        for i in 0..10u32 {
            set.spawn(async move { i });
        }
        let mut all = set.join_all().await;
        all.sort_unstable();
        assert_eq!(all, (0..10).collect::<Vec<_>>());

        let mut set = JoinSet::new();
        let handle = set.spawn_local(async { task::id() });
        let (id, inner) = set.join_next_with_id().await.unwrap().unwrap();
        assert_eq!(id, handle.id());
        assert_eq!(inner, id, "the task saw its own id");
    });
}

#[test]
#[should_panic(expected = "member panic")]
fn join_all_resumes_a_members_panic() {
    let rt = runtime(1);
    rt.block_on(async {
        let mut set = JoinSet::new();
        set.spawn(async { panic!("member panic") });
        set.join_all().await;
    });
}

/// A task detached with its slot wakes that slot's old waker as it ends,
/// after a new member took the slot's place: the stale wake costs a look
/// and nothing else, and the new member's output comes back.
#[test]
fn a_detached_task_waking_a_reused_slot_meets_no_member_of_its_own() {
    let rt = runtime(2);
    rt.block_on(async {
        let mut set = JoinSet::new();
        let (tx, rx) = oneshot::channel::<()>();
        set.spawn(async move {
            rx.await.unwrap();
            0u32
        });
        // Polled once, so the task holds the slot's waker.
        assert!(set.try_join_next().is_none());
        set.detach_all();
        let (newer_tx, newer_rx) = oneshot::channel::<()>();
        set.spawn(async move {
            newer_rx.await.unwrap();
            1u32
        });
        assert!(set.try_join_next().is_none());
        // The detached task ends and wakes its old slot's waker.
        tx.send(()).unwrap();
        task::yield_now().await;
        newer_tx.send(()).unwrap();
        assert_eq!(set.join_next().await.unwrap().unwrap(), 1);
        assert!(set.join_next().await.is_none());
    });
}

/// Slots are kept and reused: many rounds of spawning and joining through
/// a set leave as many slots as tasks at once, and every output comes back.
#[test]
fn slots_are_reused_round_after_round() {
    let rt = runtime(2);
    rt.block_on(async {
        let mut set = JoinSet::new();
        let mut total = 0u64;
        for round in 0..200u64 {
            for i in 0..4 {
                set.spawn(async move { round * 4 + i });
            }
            while let Some(out) = set.join_next().await {
                total += out.unwrap();
            }
        }
        assert_eq!(total, (0..800).sum::<u64>());
    });
}
