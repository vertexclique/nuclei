//! Signals: a stream of the deliveries of one signal (unix) or console
//! control event (Windows), and [`ctrl_c`](crate::signal::ctrl_c).
//!
//! Each signal has a count of its deliveries, which the process's handler
//! raises. A listener remembers the count it last saw: a poll that finds
//! the count moved is one item, however many deliveries that was. A
//! listener that waits registers its waker, then looks at the count once
//! more; whoever dispatches looks at the count after the delivery raised
//! it, then wakes the listeners of each signal whose count moved, so no
//! delivery is missed (the register-then-look pattern of the sync module).
//!
//! On unix a signal handler may do little: it raises the count and writes
//! to one wake descriptor of the process, never closed, which every shard
//! with a listener reads through a pump task of its own and then
//! dispatches (see `unix`). On Windows the console control handler runs on
//! a thread of its own and dispatches itself.

use core::task::{Context, Poll};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use crate::sync::atomic_waker::AtomicWaker;

#[cfg(unix)]
pub mod unix;
#[cfg(windows)]
pub mod windows;

/// Signal numbers below this: every signal of every unix nuclei runs on,
/// and every console control event.
const SLOTS: usize = 65;

/// One signal's deliveries.
struct Slot {
    /// Deliveries so far.
    count: AtomicU64,
    /// The count the last dispatch woke the listeners for.
    dispatched: AtomicU64,
    /// Listeners alive.
    listeners: AtomicUsize,
}

static DELIVERIES: [Slot; SLOTS] = [const {
    Slot {
        count: AtomicU64::new(0),
        dispatched: AtomicU64::new(0),
        listeners: AtomicUsize::new(0),
    }
}; SLOTS];

/// A listener as dispatch sees it: its signal and its waker.
struct Waiter {
    signum: usize,
    waker: AtomicWaker,
}

/// Every listener alive, by id.
fn waiters() -> &'static kovan_map::HashMap<u64, Arc<Waiter>> {
    static WAITERS: OnceLock<kovan_map::HashMap<u64, Arc<Waiter>>> = OnceLock::new();
    // 64 is the smallest table the map accepts.
    WAITERS.get_or_init(|| kovan_map::HashMap::with_capacity(64))
}

/// A delivery of `signum`: what a handler does first. Only atomics, so a
/// unix signal handler may call it.
fn delivered(signum: usize) {
    if let Some(slot) = DELIVERIES.get(signum) {
        slot.count.fetch_add(1, Ordering::SeqCst);
    }
}

/// Wakes the listeners of every signal whose count moved since the last
/// dispatch. Never from a unix signal handler: a waker may do anything.
fn dispatch() {
    let mut moved = 0u128;
    for (signum, slot) in DELIVERIES.iter().enumerate() {
        let now = slot.count.load(Ordering::SeqCst);
        if slot.dispatched.swap(now, Ordering::SeqCst) != now {
            moved |= 1 << signum;
        }
    }
    if moved == 0 {
        return;
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if moved & (1 << libc::SIGCHLD) != 0 {
        crate::process::reap_fallback();
    }
    for (_, waiter) in waiters().iter() {
        if moved & (1 << waiter.signum) != 0 {
            waiter.waker.wake();
        }
    }
}

/// Whether `signum` has a listener alive.
#[cfg(windows)]
fn listened(signum: usize) -> bool {
    DELIVERIES
        .get(signum)
        .is_some_and(|slot| slot.listeners.load(Ordering::SeqCst) > 0)
}

/// One listener of `signum`: the platform streams wrap it.
struct Listener {
    id: u64,
    signum: usize,
    seen: u64,
    waiter: Arc<Waiter>,
}

impl Listener {
    /// A listener of `signum` (below [`SLOTS`]), which sees the deliveries
    /// from now on.
    fn new(signum: usize) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let id = NEXT.fetch_add(1, Ordering::Relaxed);
        let slot = &DELIVERIES[signum];
        slot.listeners.fetch_add(1, Ordering::SeqCst);
        let waiter = Arc::new(Waiter {
            signum,
            waker: AtomicWaker::new(),
        });
        waiters().insert(id, Arc::clone(&waiter));
        Self {
            id,
            signum,
            seen: slot.count.load(Ordering::SeqCst),
            waiter,
        }
    }

    /// Ready once the signal was delivered since the last item; `wait`
    /// makes sure something dispatches for this waiter.
    fn poll_recv(&mut self, cx: &mut Context<'_>, wait: impl FnOnce()) -> Poll<Option<()>> {
        let count = &DELIVERIES[self.signum].count;
        if self.take(count.load(Ordering::SeqCst)) {
            return Poll::Ready(Some(()));
        }
        self.waiter.waker.register(cx.waker());
        wait();
        if self.take(count.load(Ordering::SeqCst)) {
            return Poll::Ready(Some(()));
        }
        Poll::Pending
    }

    fn take(&mut self, now: u64) -> bool {
        let moved = now != self.seen;
        self.seen = now;
        moved
    }
}

impl core::fmt::Debug for Listener {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Listener")
            .field("signum", &self.signum)
            .finish_non_exhaustive()
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        waiters().remove(&self.id);
        DELIVERIES[self.signum]
            .listeners
            .fetch_sub(1, Ordering::SeqCst);
    }
}

/// Completes once the process gets a Ctrl-C: `SIGINT` on unix, the
/// console's `CTRL_C_EVENT` on Windows. Listening starts with the first
/// poll; on unix the default action of `SIGINT` (ending the process) is
/// replaced for good from then on.
///
/// # Errors
///
/// When the handler cannot be installed.
pub async fn ctrl_c() -> std::io::Result<()> {
    #[cfg(unix)]
    let mut listener = unix::signal(unix::SignalKind::interrupt())?;
    #[cfg(windows)]
    let mut listener = windows::ctrl_c()?;
    listener.recv().await;
    Ok(())
}

#[cfg(test)]
mod tests;
