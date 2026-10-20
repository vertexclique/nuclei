//! Tests of `nuclei::sync::mpsc`: the behaviour of the established async
//! channel API it follows, checked call by call (the bounded and the unbounded
//! flavor, permits, closing, weak senders, receive-many, and the first come,
//! first served order of the bounded channel's waiting senders); where this
//! channel differs on purpose (a `try_recv` that never waits, the slots of a
//! waiting reservation coming back at the close) the test says so; drop
//! accounting, wakers, and threads racing sends, receives, closes and drops.
#![cfg(not(target_arch = "wasm32"))]

mod sync_support;

use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use nuclei::sync::mpsc::{
    self,
    error::{SendError, TryRecvError, TrySendError},
};
use sync_support::{Counted, Drops, Probe, Task, block_on};

#[path = "sync_mpsc/bounded.rs"]
mod bounded;
#[path = "sync_mpsc/closing.rs"]
mod closing;
#[path = "sync_mpsc/drops.rs"]
mod drops;
#[path = "sync_mpsc/fairness.rs"]
mod fairness;
#[path = "sync_mpsc/ordering.rs"]
mod ordering;
#[path = "sync_mpsc/permits.rs"]
mod permits;
#[path = "sync_mpsc/recv_many.rs"]
mod recv_many;
#[path = "sync_mpsc/threads.rs"]
mod threads;
#[path = "sync_mpsc/unbounded.rs"]
mod unbounded;
#[path = "sync_mpsc/weak.rs"]
mod weak;

/// Compiles only for a type that is `Debug`; the point is that `T` need not be.
fn is_debug<T: fmt::Debug>(_: &T) {}

/// A waker that does nothing, for polling by hand.
fn noop_context() -> Context<'static> {
    Context::from_waker(Waker::noop())
}

/// A waker that counts its wakes.
struct Counting(AtomicUsize);

impl Wake for Counting {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn counting_waker() -> (Arc<Counting>, Waker) {
    let counter = Arc::new(Counting(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&counter));
    (counter, waker)
}

fn wakes(counter: &Counting) -> usize {
    counter.0.load(Ordering::SeqCst)
}

fn scale(real: u64, miri: u64) -> u64 {
    if cfg!(miri) { miri } else { real }
}
