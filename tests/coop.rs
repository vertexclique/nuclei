//! `task::coop`: a task that keeps completing operations yields once its
//! budget is spent, so the shard's other tasks run; `unconstrained` lifts the
//! budget.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use nuclei::runtime::Runtime;
use nuclei::task::{self, coop};

fn runtime() -> Runtime {
    Runtime::builder()
        .worker_threads(1)
        .pin_workers(false)
        .build()
        .expect("the runtime starts")
}

/// Runs a busy task on one worker and, once it has started, a task that
/// only raises a flag; answers the iteration at which the busy task first saw
/// the flag (`None`: it never yielded before its end).
fn first_sight(unconstrained: bool) -> Option<usize> {
    let rt = runtime();
    rt.block_on(async move {
        let flag = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));
        let (seen, began) = (Arc::clone(&flag), Arc::clone(&started));
        let busy = async move {
            began.store(true, Ordering::SeqCst);
            for i in 0..1000 {
                if seen.load(Ordering::SeqCst) {
                    return Some(i);
                }
                coop::consume_budget().await;
            }
            None
        };
        let busy = if unconstrained {
            task::spawn_local(async move { coop::unconstrained(busy).await })
        } else {
            task::spawn_local(busy)
        };
        // The raiser is spawned only once the busy task runs: it gets a turn
        // when the busy task yields, and only then.
        while !started.load(Ordering::SeqCst) {
            task::yield_now().await;
        }
        let raiser = task::spawn_local(async move { flag.store(true, Ordering::SeqCst) });
        let first = busy.await;
        raiser.await;
        first
    })
}

#[test]
fn a_busy_task_yields_once_its_budget_is_spent() {
    let first = first_sight(false).expect("the other task never ran");
    assert!(
        first <= 3 * coop::BUDGET as usize,
        "the busy task ran {first} operations before the other task got a turn"
    );
}

/// As [`first_sight`], the busy task looping on sleeps that are already due.
fn first_sight_sleeping(until_past: bool) -> Option<usize> {
    let rt = runtime();
    rt.block_on(async move {
        let flag = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicBool::new(false));
        let (seen, began) = (Arc::clone(&flag), Arc::clone(&started));
        let busy = task::spawn_local(async move {
            began.store(true, Ordering::SeqCst);
            for i in 0..1000 {
                if seen.load(Ordering::SeqCst) {
                    return Some(i);
                }
                if until_past {
                    nuclei::time::sleep_until(nuclei::time::Instant::now()).await;
                } else {
                    nuclei::time::sleep(std::time::Duration::ZERO).await;
                }
            }
            None
        });
        while !started.load(Ordering::SeqCst) {
            task::yield_now().await;
        }
        let raiser = task::spawn_local(async move { flag.store(true, Ordering::SeqCst) });
        let first = busy.await;
        raiser.await;
        first
    })
}

#[test]
fn a_loop_over_zero_length_sleeps_yields_within_a_budget() {
    let first = first_sight_sleeping(false).expect("the other task never ran");
    assert!(
        first <= 3 * coop::BUDGET as usize,
        "{first} sleeps before the other task got a turn"
    );
}

#[test]
fn a_loop_over_sleeps_until_the_past_yields_within_a_budget() {
    let first = first_sight_sleeping(true).expect("the other task never ran");
    assert!(
        first <= 3 * coop::BUDGET as usize,
        "{first} sleeps before the other task got a turn"
    );
}

#[test]
fn an_unconstrained_task_never_yields_for_the_budget() {
    assert_eq!(first_sight(true), None);
}

/// Awaits 3 * BUDGET handles of tasks that finished at once, one after the
/// other in one task, and answers how many polls of a handle were Pending.
fn pendings_awaiting_finished_handles(unconstrained: bool) -> usize {
    let rt = runtime();
    let count = 3 * coop::BUDGET as usize;
    let body = async move {
        let done = Arc::new(AtomicUsize::new(0));
        let handles: Vec<_> = (0..count)
            .map(|i| {
                let done = Arc::clone(&done);
                task::spawn_local(async move {
                    done.fetch_add(1, Ordering::SeqCst);
                    i
                })
            })
            .collect();
        while done.load(Ordering::SeqCst) < count {
            task::yield_now().await;
        }
        // Let every completion land before the first poll.
        task::yield_now().await;
        let mut pendings = 0;
        for handle in handles {
            let mut handle = std::pin::pin!(handle);
            std::future::poll_fn(|cx| {
                let polled = handle.as_mut().poll(cx);
                if polled.is_pending() {
                    pendings += 1;
                }
                polled
            })
            .await;
        }
        pendings
    };
    if unconstrained {
        rt.block_on(task::spawn_local(coop::unconstrained(body)))
    } else {
        rt.block_on(task::spawn_local(body))
    }
}

#[test]
fn awaiting_finished_handles_yields_once_the_budget_is_spent() {
    let pendings = pendings_awaiting_finished_handles(false);
    assert!(
        pendings >= 2,
        "{pendings} yields awaiting 3 budgets of handles"
    );
}

#[test]
fn awaiting_finished_handles_under_unconstrained_never_yields() {
    assert_eq!(pendings_awaiting_finished_handles(true), 0);
}

#[test]
fn has_budget_remaining_turns_false_when_the_budget_is_spent() {
    let rt = runtime();
    rt.block_on(task::spawn_local(async {
        assert!(coop::has_budget_remaining());
        // Spend the whole budget within one poll.
        let spent = std::future::poll_fn(|cx| {
            let mut n = 0;
            while coop::has_budget_remaining() {
                let mut step = std::pin::pin!(coop::consume_budget());
                assert!(step.as_mut().poll(cx).is_ready());
                n += 1;
            }
            std::task::Poll::Ready(n)
        })
        .await;
        assert_eq!(spent, coop::BUDGET as usize);
        // The next poll starts a fresh budget.
        coop::consume_budget().await;
        assert!(coop::has_budget_remaining());
    }));
}

#[cfg(unix)]
#[test]
fn a_read_loop_on_ready_data_yields_to_the_other_tasks() {
    use futures_lite_io::read_some;

    let rt = runtime();
    rt.block_on(async {
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        // Data always ready: the reader never waits on the kernel.
        let mut writer = std::os::unix::net::UnixStream::try_clone(&a).unwrap();
        std::thread::spawn(move || {
            use std::io::Write;
            let chunk = [7u8; 4096];
            while writer.write_all(&chunk).is_ok() {}
        });
        let reader = nuclei::Handle::<std::os::unix::net::UnixStream>::new(b).unwrap();
        let flag = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&flag);
        let busy = task::spawn_local(async move {
            let mut buf = [0u8; 16];
            for i in 0..100_000 {
                if seen.load(Ordering::SeqCst) {
                    return Some(i);
                }
                read_some(&reader, &mut buf).await;
            }
            None
        });
        let raiser = task::spawn_local(async move { flag.store(true, Ordering::SeqCst) });
        let first = busy.await;
        raiser.await;
        drop(a);
        let first = first.expect("the read loop never let the other task run");
        assert!(
            first <= 10 * coop::BUDGET as usize,
            "{first} reads before a yield"
        );
    });
}

/// One read, awaiting the futures-io trait directly.
#[cfg(unix)]
mod futures_lite_io {
    use futures_io::AsyncRead;
    use std::pin::Pin;

    pub async fn read_some<R>(reader: R, buf: &mut [u8]) -> usize
    where
        R: AsyncRead + Unpin,
    {
        let mut reader = reader;
        std::future::poll_fn(|cx| Pin::new(&mut reader).poll_read(cx, buf))
            .await
            .unwrap_or(0)
    }
}
