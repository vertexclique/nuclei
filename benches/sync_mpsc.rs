//! The multi-producer, single-consumer channels of `nuclei::sync::mpsc`: the
//! uncontended cost of one send and one receive (one thread, no waiting, so each
//! row is the cost of the operation's own atomic steps and its queue push and
//! pop), and the throughput of four senders on their own threads feeding one
//! receiver. These are the numbers behind the progress claims in the type docs.
//!
//! Run with `cargo bench --bench sync_mpsc`.
//!
//! - **bounded / unbounded, one thread**: a `try_send` followed by a
//!   `try_recv`; a `send` future polled to completion followed by a `recv`
//!   future; a batch of 32 sent and received in one `recv_many`; and the
//!   creation and drop of a channel.
//! - **contended**: four sender threads of 10,000 messages each, the bench
//!   thread receiving with `blocking_recv` until the channel ends. The row is
//!   per message (criterion's element throughput), counting thread start-up.
//!   The bounded channel holds 256 messages, so its senders park and wake
//!   (`blocking_send`) when it fills.
//!
//! No async runtime is involved: futures are polled with a waker that does
//! nothing.

use std::future::Future;
use std::hint::black_box;
use std::pin::pin;
use std::task::{Context, Waker};
use std::time::Instant;

use criterion::{BatchSize, Criterion, Throughput, criterion_group, criterion_main};
use nuclei::sync::mpsc;

const SENDERS: u64 = 4;
const PER_SENDER: u64 = 10_000;
const BOUND: usize = 256;
const BATCH: u64 = 32;

fn bounded_rows(c: &mut Criterion) {
    let mut group = c.benchmark_group("mpsc_bounded");
    let mut cx = Context::from_waker(Waker::noop());

    group.bench_function("try_send_try_recv", |b| {
        let (tx, mut rx) = mpsc::channel::<u64>(BOUND);
        b.iter(|| {
            tx.try_send(black_box(1)).expect("room");
            black_box(rx.try_recv().expect("a value"))
        });
    });

    group.bench_function("send_recv_poll", |b| {
        let (tx, mut rx) = mpsc::channel::<u64>(BOUND);
        b.iter(|| {
            let mut send = pin!(tx.send(black_box(1)));
            let _ = black_box(send.as_mut().poll(&mut cx));
            let mut recv = pin!(rx.recv());
            black_box(recv.as_mut().poll(&mut cx))
        });
    });

    group.bench_function("try_send_32_recv_many", |b| {
        let (tx, mut rx) = mpsc::channel::<u64>(BOUND);
        let mut batch = Vec::with_capacity(BATCH as usize);
        b.iter(|| {
            for i in 0..BATCH {
                tx.try_send(black_box(i)).expect("room");
            }
            {
                let mut recv = pin!(rx.recv_many(&mut batch, BATCH as usize));
                let _ = black_box(recv.as_mut().poll(&mut cx));
            }
            batch.clear();
        });
    });

    group.bench_function("reserve_send_try_recv", |b| {
        let (tx, mut rx) = mpsc::channel::<u64>(BOUND);
        b.iter(|| {
            tx.try_reserve().expect("room").send(black_box(1));
            black_box(rx.try_recv().expect("a value"))
        });
    });

    group.bench_function("create_drop", |b| {
        b.iter(|| black_box(mpsc::channel::<u64>(BOUND)));
    });

    group.finish();
}

fn unbounded_rows(c: &mut Criterion) {
    let mut group = c.benchmark_group("mpsc_unbounded");
    let mut cx = Context::from_waker(Waker::noop());

    group.bench_function("send_try_recv", |b| {
        let (tx, mut rx) = mpsc::unbounded_channel::<u64>();
        b.iter(|| {
            tx.send(black_box(1)).expect("receiver is alive");
            black_box(rx.try_recv().expect("a value"))
        });
    });

    group.bench_function("send_recv_poll", |b| {
        let (tx, mut rx) = mpsc::unbounded_channel::<u64>();
        b.iter(|| {
            tx.send(black_box(1)).expect("receiver is alive");
            let mut recv = pin!(rx.recv());
            black_box(recv.as_mut().poll(&mut cx))
        });
    });

    group.bench_function("send_32_recv_many", |b| {
        let (tx, mut rx) = mpsc::unbounded_channel::<u64>();
        let mut batch = Vec::with_capacity(BATCH as usize);
        b.iter(|| {
            for i in 0..BATCH {
                tx.send(black_box(i)).expect("receiver is alive");
            }
            {
                let mut recv = pin!(rx.recv_many(&mut batch, BATCH as usize));
                let _ = black_box(recv.as_mut().poll(&mut cx));
            }
            batch.clear();
        });
    });

    group.bench_function("create_drop", |b| {
        b.iter_batched(
            || (),
            |()| black_box(mpsc::unbounded_channel::<u64>()),
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

/// Four senders on their own threads, the calling thread receiving: the time
/// for `iters` rounds of `SENDERS * PER_SENDER` messages.
fn contended<S, R>(iters: u64, mut round: impl FnMut() -> (Vec<S>, R)) -> std::time::Duration
where
    S: FnOnce() + Send + 'static,
    R: FnOnce() -> u64,
{
    let mut total = std::time::Duration::ZERO;
    for _ in 0..iters {
        let (senders, receive) = round();
        let start = Instant::now();
        let handles: Vec<_> = senders.into_iter().map(std::thread::spawn).collect();
        let received = receive();
        for handle in handles {
            handle.join().expect("sender thread");
        }
        total += start.elapsed();
        assert_eq!(received, SENDERS * PER_SENDER);
    }
    total
}

fn contended_rows(c: &mut Criterion) {
    let mut group = c.benchmark_group("mpsc_contended_4_senders");
    group.throughput(Throughput::Elements(SENDERS * PER_SENDER));
    group.sample_size(20);

    group.bench_function("bounded_256", |b| {
        b.iter_custom(|iters| {
            contended(iters, || {
                let (tx, mut rx) = mpsc::channel::<u64>(BOUND);
                let senders: Vec<_> = (0..SENDERS)
                    .map(|_| {
                        let tx = tx.clone();
                        move || {
                            for i in 0..PER_SENDER {
                                tx.blocking_send(black_box(i)).expect("receiver is alive");
                            }
                        }
                    })
                    .collect();
                drop(tx);
                let receive = move || {
                    let mut received = 0;
                    while let Some(value) = rx.blocking_recv() {
                        black_box(value);
                        received += 1;
                    }
                    received
                };
                (senders, receive)
            })
        });
    });

    group.bench_function("unbounded", |b| {
        b.iter_custom(|iters| {
            contended(iters, || {
                let (tx, mut rx) = mpsc::unbounded_channel::<u64>();
                let senders: Vec<_> = (0..SENDERS)
                    .map(|_| {
                        let tx = tx.clone();
                        move || {
                            for i in 0..PER_SENDER {
                                tx.send(black_box(i)).expect("receiver is alive");
                            }
                        }
                    })
                    .collect();
                drop(tx);
                let receive = move || {
                    let mut received = 0;
                    while let Some(value) = rx.blocking_recv() {
                        black_box(value);
                        received += 1;
                    }
                    received
                };
                (senders, receive)
            })
        });
    });

    group.finish();
}

criterion_group!(benches, bounded_rows, unbounded_rows, contended_rows);
criterion_main!(benches);
