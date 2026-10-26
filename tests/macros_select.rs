//! `select!`: the first branch to finish whose output matches wins, a
//! rejected output disables its branch, preconditions, `else`, `biased;`,
//! control flow out of handlers, and the losing futures dropped before the
//! handler runs.

use std::cell::Cell;
use std::future::{Future, IntoFuture, pending, ready};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use nuclei::sync::{mpsc, oneshot};

/// Counts its drops.
struct Counted(Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Pending forever, counting its polls.
struct CountPolls(Rc<Cell<usize>>);

impl Future for CountPolls {
    type Output = u32;

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<u32> {
        self.0.set(self.0.get() + 1);
        Poll::Pending
    }
}

#[test]
fn the_ready_branch_wins_and_its_handler_gives_the_value() {
    nuclei::block_on(async {
        let out = nuclei::select! {
            v = pending::<u32>() => v,
            v = ready(5u32) => v * 2,
        };
        assert_eq!(out, 10);
    });
}

#[test]
fn a_rejected_output_disables_its_branch_and_the_others_go_on() {
    nuclei::block_on(async {
        let (tx, mut rx) = mpsc::unbounded_channel::<u32>();
        drop(tx);
        let (late_tx, late_rx) = oneshot::channel::<u32>();
        let sender = nuclei::spawn(async move {
            nuclei::task::yield_now().await;
            late_tx.send(9).unwrap();
        });
        // The closed channel gives `None` at once, which `Some(n)` rejects:
        // the select waits on for the other branch.
        let out = nuclei::select! {
            biased;
            Some(n) = rx.recv() => n,
            Ok(n) = late_rx => n + 1,
        };
        assert_eq!(out, 10);
        sender.await;
    });
}

#[test]
fn every_branch_disabled_runs_else() {
    nuclei::block_on(async {
        let (tx, mut rx) = mpsc::unbounded_channel::<u32>();
        drop(tx);
        let out = nuclei::select! {
            Some(n) = rx.recv() => n,
            Some(n) = async { None::<u32> } => n,
            else => 77,
        };
        assert_eq!(out, 77);
    });
}

#[test]
#[should_panic(expected = "all branches of `select!` are disabled")]
fn every_branch_disabled_without_else_panics() {
    nuclei::block_on(async {
        nuclei::select! {
            Some(n) = async { None::<u32> } => n,
        }
    });
}

#[test]
fn a_false_precondition_evaluates_the_future_but_never_polls_it() {
    nuclei::block_on(async {
        let polls = Rc::new(Cell::new(0));
        let built = Cell::new(0);
        let make = || {
            built.set(built.get() + 1);
            CountPolls(Rc::clone(&polls))
        };
        let out = nuclei::select! {
            v = make(), if false => v,
            v = ready(3u32) => v,
        };
        assert_eq!(out, 3);
        assert_eq!(built.get(), 1, "the future expression is evaluated");
        assert_eq!(polls.get(), 0, "a disabled branch is never polled");

        let out = nuclei::select! {
            v = make(), if false => v,
            else => 4,
        };
        assert_eq!(out, 4);
        assert_eq!(polls.get(), 0);
    });
}

#[test]
fn biased_polls_in_the_order_written_and_fair_does_not() {
    nuclei::block_on(async {
        for _ in 0..100 {
            let out = nuclei::select! {
                biased;
                v = ready(1u32) => v,
                v = ready(2u32) => v,
            };
            assert_eq!(out, 1);
        }
        let mut seen = [0usize; 2];
        for _ in 0..1000 {
            let out = nuclei::select! {
                v = ready(0usize) => v,
                v = ready(1usize) => v,
            };
            seen[out] += 1;
        }
        // A fair select starts at a random branch: both win often.
        assert!(seen[0] > 300 && seen[1] > 300, "{seen:?}");
    });
}

#[test]
fn handlers_break_continue_return_and_try_out_of_the_code_around() {
    fn run() -> Result<u32, &'static str> {
        nuclei::block_on(async {
            let mut sum = 0;
            for i in 0..10u32 {
                nuclei::select! {
                    v = ready(i) => {
                        if v % 2 == 0 {
                            continue;
                        }
                        if v == 7 {
                            break;
                        }
                        sum += v;
                    }
                }
            }
            assert_eq!(sum, 1 + 3 + 5);
            let early: Result<u32, &'static str> = async {
                let v: u32 = nuclei::select! {
                    v = ready(Err::<u32, _>("boom")) => v?,
                };
                Ok(v)
            }
            .await;
            assert_eq!(early, Err("boom"));
            nuclei::select! {
                v = ready(41u32) => if v == 41 {
                    return Ok(v + 1);
                },
            }
            Err("the handler did not return")
        })
    }
    assert_eq!(run(), Ok(42));
}

#[test]
fn patterns_with_mut_ref_and_references_bind_as_written() {
    nuclei::block_on(async {
        let out = nuclei::select! {
            mut n = ready(5u32) => {
                n += 1;
                n
            }
        };
        assert_eq!(out, 6);
        let out = nuclei::select! {
            ref s = ready(String::from("abc")) => s.len(),
        };
        assert_eq!(out, 3);
        let five = 5u32;
        let out = nuclei::select! {
            &n = ready(&five) => n,
        };
        assert_eq!(out, 5);
        let out = nuclei::select! {
            (Some(mut a), [b, ..]) = ready((Some(1u32), [2u32, 3])) => {
                a += b;
                a
            }
        };
        assert_eq!(out, 3);
        let out = nuclei::select! {
            Ok(n @ 1..=9) | Err(n) = ready(Ok::<u32, u32>(4)) => n,
        };
        assert_eq!(out, 4);
    });
}

#[test]
fn the_losing_futures_are_dropped_before_the_handler_runs() {
    nuclei::block_on(async {
        let drops = Arc::new(AtomicUsize::new(0));
        let (_keep, mut rx) = mpsc::unbounded_channel::<u32>();
        let held = Counted(Arc::clone(&drops));
        let loser = async move {
            let _held = held;
            pending::<u32>().await
        };
        let seen = nuclei::select! {
            v = loser => v as usize,
            // `rx` is borrowed by its future; the handler may use it again.
            v = async { 3u32 } => {
                let again = rx.try_recv().is_err();
                assert!(again);
                drops.load(Ordering::SeqCst) + v as usize
            }
        };
        assert_eq!(seen, 1 + 3, "the loser was dropped before the handler");
    });
}

#[test]
fn into_futures_and_many_branches_work() {
    struct Later(u32);
    impl IntoFuture for Later {
        type Output = u32;
        type IntoFuture = std::future::Ready<u32>;
        fn into_future(self) -> Self::IntoFuture {
            ready(self.0)
        }
    }
    nuclei::block_on(async {
        let out = nuclei::select! {
            biased;
            v = pending::<u32>() => v,
            v = pending::<u32>() => v,
            v = pending::<u32>() => v,
            v = pending::<u32>() => v,
            v = pending::<u32>() => v,
            v = pending::<u32>() => v,
            v = pending::<u32>() => v,
            v = pending::<u32>() => v,
            v = pending::<u32>() => v,
            v = Later(11) => v,
            v = pending::<u32>() => v,
        };
        assert_eq!(out, 11);
    });
}

#[test]
fn a_select_loop_takes_every_message_then_ends() {
    let rt = nuclei::runtime::Runtime::builder()
        .worker_threads(2)
        .pin_workers(false)
        .build()
        .expect("the runtime starts");
    rt.block_on(async {
        let (a_tx, mut a) = mpsc::unbounded_channel::<u32>();
        let (b_tx, mut b) = mpsc::unbounded_channel::<u32>();
        let producers = [
            nuclei::spawn(async move {
                for i in 0..500 {
                    a_tx.send(i).unwrap();
                }
            }),
            nuclei::spawn(async move {
                for i in 500..1000 {
                    b_tx.send(i).unwrap();
                }
            }),
        ];
        let mut got = Vec::new();
        loop {
            nuclei::select! {
                Some(v) = a.recv() => got.push(v),
                Some(v) = b.recv() => got.push(v),
                else => break,
            }
        }
        for p in producers {
            p.await;
        }
        got.sort_unstable();
        assert_eq!(got, (0..1000).collect::<Vec<_>>());
    });
}
