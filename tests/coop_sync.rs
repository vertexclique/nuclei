//! `task::coop` in `nuclei::sync`: a task looping on a channel, a semaphore or
//! a lock that is always ready yields once its budget is spent, so the
//! shard's other tasks run; a refused operation loses nothing; inside
//! `unconstrained` nothing yields.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use nuclei::runtime::Runtime;
use nuclei::sync::{
    Mutex, Owner, ReentrantMutex, RwLock, Semaphore, broadcast, mpsc, oneshot, watch,
};
use nuclei::task::{self, coop};

fn runtime() -> Runtime {
    Runtime::builder()
        .worker_threads(1)
        .pin_workers(false)
        .build()
        .expect("the runtime starts")
}

const LIMIT: usize = 3 * coop::BUDGET as usize;
const ROUNDS: usize = 2_000;

/// One busy loop: given the flag the other task raises, it runs up to
/// `ROUNDS` operations and answers the round at which it first saw the flag.
type Busy = Box<dyn FnOnce(Arc<AtomicBool>) -> Pin<Box<dyn Future<Output = Option<usize>>>>>;

/// Runs `busy` on one worker and, once it has started, a task that only
/// raises a flag; answers the round at which the busy task first saw it
/// (`None`: it never yielded before its end).
fn first_sight(unconstrained: bool, busy: Busy) -> Option<usize> {
    runtime().block_on(async move {
        let flag = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));
        let began = Arc::clone(&started);
        let work = busy(Arc::clone(&flag));
        let work = async move {
            began.store(true, Ordering::SeqCst);
            work.await
        };
        let busy = if unconstrained {
            task::spawn_local(async move { coop::unconstrained(work).await })
        } else {
            task::spawn_local(work)
        };
        while !started.load(Ordering::SeqCst) {
            task::yield_now().await;
        }
        let raiser = task::spawn_local(async move { flag.store(true, Ordering::SeqCst) });
        let first = busy.await;
        raiser.await;
        first
    })
}

fn busy<F, Fut>(f: F) -> Busy
where
    F: FnOnce(Arc<AtomicBool>) -> Fut + 'static,
    Fut: Future<Output = Option<usize>> + 'static,
{
    Box::new(move |flag| Box::pin(f(flag)))
}

fn bounded_recv() -> Busy {
    busy(|flag| async move {
        let (tx, mut rx) = mpsc::channel(ROUNDS);
        for i in 0..ROUNDS {
            tx.try_send(i).unwrap();
        }
        for i in 0..ROUNDS {
            if flag.load(Ordering::SeqCst) {
                return Some(i);
            }
            assert_eq!(rx.recv().await, Some(i), "a refused receive lost a value");
        }
        None
    })
}

fn bounded_send() -> Busy {
    busy(|flag| async move {
        let (tx, mut rx) = mpsc::channel(4);
        for i in 0..ROUNDS {
            if flag.load(Ordering::SeqCst) {
                return Some(i);
            }
            tx.send(i).await.unwrap();
            assert_eq!(rx.try_recv(), Ok(i));
        }
        None
    })
}

fn unbounded_recv() -> Busy {
    busy(|flag| async move {
        let (tx, mut rx) = mpsc::unbounded_channel();
        for i in 0..ROUNDS {
            tx.send(i).unwrap();
        }
        let mut buf = Vec::new();
        for i in 0..ROUNDS {
            if flag.load(Ordering::SeqCst) {
                return Some(i);
            }
            if i % 2 == 0 {
                assert_eq!(rx.recv().await, Some(i));
            } else {
                assert_eq!(rx.recv_many(&mut buf, 1).await, 1);
                assert_eq!(buf.pop(), Some(i));
            }
        }
        None
    })
}

fn oneshot_recv() -> Busy {
    busy(|flag| async move {
        for i in 0..ROUNDS {
            if flag.load(Ordering::SeqCst) {
                return Some(i);
            }
            let (tx, rx) = oneshot::channel();
            tx.send(i).unwrap();
            assert_eq!(rx.await, Ok(i));
        }
        None
    })
}

fn broadcast_recv() -> Busy {
    busy(|flag| async move {
        let (tx, mut rx) = broadcast::channel(ROUNDS);
        for i in 0..ROUNDS {
            tx.send(i).unwrap();
        }
        for i in 0..ROUNDS {
            if flag.load(Ordering::SeqCst) {
                return Some(i);
            }
            assert_eq!(rx.recv().await, Ok(i));
        }
        None
    })
}

fn watch_changed() -> Busy {
    busy(|flag| async move {
        let (tx, mut rx) = watch::channel(0);
        for i in 0..ROUNDS {
            if flag.load(Ordering::SeqCst) {
                return Some(i);
            }
            tx.send(i).unwrap();
            rx.changed().await.unwrap();
            assert_eq!(*rx.borrow_and_update(), i);
        }
        None
    })
}

fn semaphore_acquire() -> Busy {
    busy(|flag| async move {
        let sem = Semaphore::new(2);
        for i in 0..ROUNDS {
            if flag.load(Ordering::SeqCst) {
                return Some(i);
            }
            drop(sem.acquire().await.unwrap());
        }
        assert_eq!(sem.available_permits(), 2);
        None
    })
}

fn mutex_lock() -> Busy {
    busy(|flag| async move {
        let mutex = Mutex::new(0);
        for i in 0..ROUNDS {
            if flag.load(Ordering::SeqCst) {
                return Some(i);
            }
            *mutex.lock().await += 1;
        }
        None
    })
}

fn rwlock_read_and_write() -> Busy {
    busy(|flag| async move {
        let lock = RwLock::new(0);
        for i in 0..ROUNDS {
            if flag.load(Ordering::SeqCst) {
                return Some(i);
            }
            if i % 2 == 0 {
                assert_eq!(*lock.read().await, i / 2);
            } else {
                *lock.write().await += 1;
            }
        }
        None
    })
}

fn reentrant_lock() -> Busy {
    busy(|flag| async move {
        let mutex = ReentrantMutex::new(());
        let owner = Owner::new();
        let _outer = mutex.lock(&owner).await;
        for i in 0..ROUNDS {
            if flag.load(Ordering::SeqCst) {
                return Some(i);
            }
            drop(mutex.lock(&owner).await);
        }
        None
    })
}

/// A named busy loop, made fresh for each run.
type Case = (&'static str, fn() -> Busy);

fn all() -> Vec<Case> {
    vec![
        ("bounded recv", bounded_recv),
        ("bounded send", bounded_send),
        ("unbounded recv", unbounded_recv),
        ("oneshot recv", oneshot_recv),
        ("broadcast recv", broadcast_recv),
        ("watch changed", watch_changed),
        ("semaphore acquire", semaphore_acquire),
        ("mutex lock", mutex_lock),
        ("rwlock read and write", rwlock_read_and_write),
        ("reentrant lock", reentrant_lock),
    ]
}

#[test]
fn every_loop_on_a_ready_primitive_yields_within_its_budget() {
    for (name, make) in all() {
        let first = first_sight(false, make()).unwrap_or_else(|| panic!("{name} never yielded"));
        assert!(
            first <= LIMIT,
            "{name}: {first} operations before the other task got a turn"
        );
    }
}

#[test]
fn an_unconstrained_loop_on_a_ready_primitive_never_yields() {
    for (name, make) in all() {
        assert_eq!(first_sight(true, make()), None, "{name} yielded");
    }
}

#[test]
fn outside_a_runtime_nothing_yields() {
    let (tx, mut rx) = mpsc::channel(ROUNDS);
    for i in 0..ROUNDS {
        tx.try_send(i).unwrap();
    }
    let waker = std::task::Waker::noop();
    let mut cx = std::task::Context::from_waker(waker);
    for i in 0..ROUNDS {
        assert_eq!(
            rx.poll_recv(&mut cx),
            std::task::Poll::Ready(Some(i)),
            "a poll off any runtime refused"
        );
    }
}

#[test]
fn a_receive_refused_for_the_budget_takes_the_value_on_the_next_poll() {
    runtime().block_on(task::spawn_local(async {
        let (tx, mut rx) = mpsc::channel(1);
        tx.try_send(7).unwrap();
        let mut cx_polls = 0;
        let got = std::future::poll_fn(|cx| {
            cx_polls += 1;
            if cx_polls == 1 {
                while coop::has_budget_remaining() {
                    let mut step = std::pin::pin!(coop::consume_budget());
                    assert!(step.as_mut().poll(cx).is_ready());
                }
                assert!(rx.poll_recv(cx).is_pending(), "the spent budget refuses");
                assert_eq!(rx.len(), 1, "a refused receive takes nothing");
                return std::task::Poll::Pending;
            }
            rx.poll_recv(cx)
        })
        .await;
        assert_eq!(got, Some(7));
        assert_eq!(cx_polls, 2, "the refusal woke its task at once");
    }));
}

#[test]
fn a_lock_refused_for_the_budget_is_free_for_the_next_poll() {
    runtime().block_on(task::spawn_local(async {
        let mutex = Arc::new(Mutex::new(0));
        let mut polls = 0;
        let mut lock = std::pin::pin!(mutex.lock());
        let guard = std::future::poll_fn(|cx| {
            polls += 1;
            if polls == 1 {
                while coop::has_budget_remaining() {
                    let mut step = std::pin::pin!(coop::consume_budget());
                    assert!(step.as_mut().poll(cx).is_ready());
                }
                assert!(
                    lock.as_mut().poll(cx).is_pending(),
                    "the spent budget refuses"
                );
                assert!(
                    mutex.try_lock().is_ok(),
                    "a refused lock gave the lock back"
                );
                return std::task::Poll::Pending;
            }
            lock.as_mut().poll(cx)
        })
        .await;
        drop(guard);
        assert_eq!(polls, 2, "the refusal woke its task at once");
    }));
}
