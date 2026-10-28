//! The one-shot channel, the broadcast channel, the watch channel and the
//! cancellation token of `nuclei::sync`, uncontended: one thread, no waiting,
//! so each row is the cost of the operation's own atomic steps. These are the
//! numbers behind the progress claims in the type docs.
//!
//! Run with `cargo bench --bench sync_new`.
//!
//! - **oneshot**: a whole channel (create, send, receive), the same through
//!   `poll`, a poll that registers a waker followed by the send that wakes it,
//!   and a failed send into a dropped receiver.
//! - **broadcast**: a send and the receive that takes it, a receive that finds
//!   nothing, a send to nine receivers each taking it, a waiting receive woken
//!   by a send, and a subscription dropped.
//! - **watch**: a borrow, a borrow that marks seen, a send (the swap and the
//!   version bump, plus the wake of nobody), a send with a receiver waiting,
//!   a send and the `changed` that completes at once, `send_modify` and
//!   `send_if_modified` (copy-on-write, published by a compare-and-swap), and
//!   a `changed` poll that finds nothing.
//! - **token**: creating a token, cloning, `is_cancelled`, a child, a child
//!   of a cancelled token, a cancel with no descendant, a cancel over a tree of
//!   a hundred children, a cancelled token's wait, and a pending wait that is
//!   dropped.
//!
//! No async runtime is involved: futures are polled with a waker that does
//! nothing, or one that counts.

use std::future::Future;
use std::hint::black_box;
use std::pin::{Pin, pin};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Wake, Waker};

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use nuclei::sync::{CancellationToken, broadcast, oneshot, watch};

struct Counter(AtomicUsize);

impl Wake for Counter {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

fn oneshot_rows(c: &mut Criterion) {
    let mut group = c.benchmark_group("oneshot");
    let mut cx = Context::from_waker(Waker::noop());

    group.bench_function("create_send_try_recv", |b| {
        b.iter(|| {
            let (tx, mut rx) = oneshot::channel();
            let _ = tx.send(black_box(1u64));
            black_box(rx.try_recv())
        });
    });

    group.bench_function("create_send_poll", |b| {
        b.iter(|| {
            let (tx, mut rx) = oneshot::channel();
            let _ = tx.send(black_box(1u64));
            black_box(Pin::new(&mut rx).poll(&mut cx))
        });
    });

    let counter = Arc::new(Counter(AtomicUsize::new(0)));
    let counting = Waker::from(Arc::clone(&counter));
    group.bench_function("create_poll_pending_send_wake_poll", |b| {
        let mut cx = Context::from_waker(&counting);
        b.iter(|| {
            let (tx, mut rx) = oneshot::channel();
            let pending = Pin::new(&mut rx).poll(&mut cx);
            let _ = tx.send(black_box(1u64));
            black_box((pending, Pin::new(&mut rx).poll(&mut cx)))
        });
    });

    group.bench_function("send_to_a_dropped_receiver", |b| {
        b.iter_batched(
            oneshot::channel::<u64>,
            |(tx, rx)| {
                drop(rx);
                black_box(tx.send(1))
            },
            BatchSize::SmallInput,
        );
    });

    group.bench_function("create_drop_both", |b| {
        b.iter(|| black_box(oneshot::channel::<u64>()));
    });
    group.finish();
}

fn watch_rows(c: &mut Criterion) {
    let mut group = c.benchmark_group("watch");
    let mut cx = Context::from_waker(Waker::noop());

    let (tx, mut rx) = watch::channel(0u64);
    group.bench_function("receiver_borrow", |b| b.iter(|| black_box(*rx.borrow())));
    group.bench_function("sender_borrow", |b| b.iter(|| black_box(*tx.borrow())));
    group.bench_function("receiver_borrow_and_update", |b| {
        b.iter(|| black_box(*rx.borrow_and_update()));
    });
    group.bench_function("has_changed", |b| {
        b.iter(|| black_box(rx.has_changed()));
    });
    let mut value = 0u64;
    group.bench_function("send", |b| {
        b.iter(|| {
            value += 1;
            black_box(tx.send(black_box(value)))
        });
    });
    group.bench_function("send_replace", |b| {
        b.iter(|| {
            value += 1;
            black_box(tx.send_replace(black_box(value)))
        });
    });
    group.bench_function("send_modify", |b| {
        b.iter(|| tx.send_modify(|v| *v += 1));
    });
    group.bench_function("send_if_modified_declined", |b| {
        b.iter(|| black_box(tx.send_if_modified(|_| false)));
    });
    group.bench_function("send_then_changed_ready", |b| {
        b.iter(|| {
            value += 1;
            let _ = tx.send(value);
            let mut changed = pin!(rx.changed());
            black_box(changed.as_mut().poll(&mut cx))
        });
    });

    // A changed() that has nothing to report: creating the wake, looking at the
    // version, registering as a waiter, and dropping the wait.
    group.bench_function("changed_pending_then_dropped", |b| {
        rx.mark_unchanged();
        b.iter(|| {
            let mut changed = pin!(rx.changed());
            black_box(changed.as_mut().poll(&mut cx))
        });
    });

    // A send that wakes a waiter that is there.
    let counter = Arc::new(Counter(AtomicUsize::new(0)));
    let counting = Waker::from(Arc::clone(&counter));
    group.bench_function("send_waking_one_waiter", |b| {
        let mut cx = Context::from_waker(&counting);
        b.iter(|| {
            rx.mark_unchanged();
            let mut changed = pin!(rx.changed());
            let pending = changed.as_mut().poll(&mut cx);
            value += 1;
            let _ = tx.send(value);
            black_box((pending, changed.as_mut().poll(&mut cx)))
        });
    });

    group.bench_function("channel_create_subscribe_drop", |b| {
        b.iter(|| {
            let (tx, rx) = watch::channel(black_box(0u64));
            let rx2 = tx.subscribe();
            black_box((tx, rx, rx2))
        });
    });
    group.finish();
}

fn broadcast_rows(c: &mut Criterion) {
    let mut group = c.benchmark_group("broadcast");
    let waker = Waker::noop();
    let (tx, mut rx) = broadcast::channel::<u64>(64);
    group.bench_function("send_then_try_recv", |b| {
        b.iter(|| {
            tx.send(black_box(1)).expect("a receiver");
            black_box(rx.try_recv().expect("sent"))
        })
    });
    group.bench_function("try_recv_empty", |b| b.iter(|| black_box(rx.try_recv())));
    let mut more: Vec<_> = (0..8).map(|_| tx.subscribe()).collect();
    group.bench_function("send_to_nine_then_each_takes_it", |b| {
        b.iter(|| {
            tx.send(black_box(1)).expect("a receiver");
            black_box(rx.try_recv().expect("sent"));
            for other in &mut more {
                black_box(other.try_recv().expect("sent"));
            }
        })
    });
    drop(more);
    group.bench_function("recv_pending_then_send_wakes_it", |b| {
        b.iter(|| {
            let mut recv = pin!(rx.recv());
            assert!(
                recv.as_mut()
                    .poll(&mut Context::from_waker(waker))
                    .is_pending()
            );
            tx.send(black_box(1)).expect("a receiver");
            black_box(recv.as_mut().poll(&mut Context::from_waker(waker)))
        })
    });
    group.bench_function("subscribe_drop", |b| {
        b.iter(|| drop(black_box(tx.subscribe())))
    });
    group.finish();
}

fn token_rows(c: &mut Criterion) {
    let mut group = c.benchmark_group("token");
    let mut cx = Context::from_waker(Waker::noop());

    group.bench_function("new", |b| b.iter(|| black_box(CancellationToken::new())));

    let token = CancellationToken::new();
    group.bench_function("clone", |b| b.iter(|| black_box(token.clone())));
    group.bench_function("is_cancelled", |b| {
        b.iter(|| black_box(token.is_cancelled()))
    });
    group.bench_function("child", |b| b.iter(|| black_box(token.child_token())));

    let cancelled = CancellationToken::new();
    cancelled.cancel();
    group.bench_function("child_of_cancelled", |b| {
        b.iter(|| black_box(cancelled.child_token()));
    });
    group.bench_function("cancelled_ready", |b| {
        b.iter(|| {
            let mut wait = pin!(cancelled.cancelled());
            black_box(wait.as_mut().poll(&mut cx))
        });
    });
    group.bench_function("cancelled_owned_ready", |b| {
        b.iter(|| {
            let mut wait = pin!(cancelled.clone().cancelled_owned());
            black_box(wait.as_mut().poll(&mut cx))
        });
    });
    group.bench_function("cancelled_pending_then_dropped", |b| {
        b.iter(|| {
            let mut wait = pin!(token.cancelled());
            black_box(wait.as_mut().poll(&mut cx))
        });
    });

    group.bench_function("new_cancel_is_cancelled", |b| {
        b.iter(|| {
            let token = CancellationToken::new();
            token.cancel();
            black_box(token.is_cancelled())
        });
    });
    group.bench_function("new_child_cancel_parent", |b| {
        b.iter(|| {
            let token = CancellationToken::new();
            let child = token.child_token();
            token.cancel();
            black_box(child.is_cancelled())
        });
    });
    group.bench_function("cancel_tree_of_100_children", |b| {
        b.iter_batched(
            || {
                let root = CancellationToken::new();
                let children: Vec<_> = (0..100).map(|_| root.child_token()).collect();
                (root, children)
            },
            |(root, children)| {
                root.cancel();
                black_box((root, children))
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(
    benches,
    oneshot_rows,
    broadcast_rows,
    watch_rows,
    token_rows
);
criterion_main!(benches);
