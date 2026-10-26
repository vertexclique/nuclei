//! [`Sleep`]: a future that completes at a deadline, on the timer wheel of
//! the shard that polls it.

use core::cell::UnsafeCell;
use core::fmt;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::sync::Arc;
use std::task::Wake;
use std::time::Duration;

use super::Instant;

use super::wheel::{TimerKey, Wheel};
use crate::rt::context;
use crate::rt::shard::{Shard, Shared};
use crate::sync::atomic_waker::AtomicWaker;
use crate::sync::internal::{AtomicBool, Ordering};

/// Waits until `duration` has passed.
///
/// The wait is at least `duration`, rounded up to the timer wheel's
/// millisecond; a busy shard may let it run later by the time its current
/// task takes to yield.
pub fn sleep(duration: Duration) -> Sleep {
    sleep_until(
        Instant::now()
            .checked_add(duration)
            .unwrap_or_else(far_future),
    )
}

/// Waits until `deadline`.
pub fn sleep_until(deadline: impl Into<Instant>) -> Sleep {
    Sleep {
        deadline: deadline.into(),
        state: State::Idle,
    }
}

/// A deadline nobody reaches: about 30 years from now.
pub(super) fn far_future() -> Instant {
    Instant::now() + Duration::from_secs(86_400 * 365 * 30)
}

/// A future that completes at its deadline. Made by [`sleep`] and
/// [`sleep_until`].
///
/// Polled on one of nuclei's shards, it arms a timer in that shard's wheel;
/// polled anywhere else (another runtime's thread), it asks a shard of the
/// current runtime (or the default one) to fire it. Dropping it cancels the
/// timer.
#[must_use = "a sleep does nothing unless awaited"]
pub struct Sleep {
    deadline: Instant,
    state: State,
}

enum State {
    /// Not armed yet.
    Idle,
    /// Armed in the wheel of `owner`'s shard.
    Local { owner: Arc<Shared>, key: TimerKey },
    /// Armed for this thread by `shard`, which fires `cell`.
    Remote {
        shard: Arc<Shared>,
        cell: Arc<RemoteTimer>,
    },
    /// Fired.
    Done,
}

impl Sleep {
    /// The deadline.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Whether the deadline has passed.
    pub fn is_elapsed(&self) -> bool {
        matches!(self.state, State::Done) || Instant::now() >= self.deadline
    }

    /// Moves the deadline; the sleep is pending again until it.
    pub fn reset(self: Pin<&mut Self>, deadline: impl Into<Instant>) {
        let deadline = deadline.into();
        let this = self.get_mut();
        this.release();
        this.deadline = deadline;
    }

    /// Lets go of the timer, wherever it is armed.
    fn release(&mut self) {
        match core::mem::replace(&mut self.state, State::Idle) {
            State::Local { owner, key } => {
                let here = context::current(|current| match current {
                    Some(shard) if shard.is(&owner) => {
                        shard.wheel().borrow_mut().remove(key);
                        true
                    }
                    _ => false,
                });
                if !here {
                    owner.post_timer(TimerOp::Remove(key));
                }
            }
            State::Remote { shard, cell } => {
                cell.cancelled.store(true, Ordering::SeqCst);
                // The waker goes now: a fire racing the cancel finds none to
                // wake, and the task it belongs to is not held until then.
                drop(cell.waker.take());
                shard.post_timer(TimerOp::Cancel(cell));
            }
            State::Idle | State::Done => {}
        }
    }

    fn poll_local(&mut self, shard: &Shard, cx: &mut Context<'_>) -> Poll<()> {
        if let State::Local { owner, key } = &self.state
            && shard.is(owner)
        {
            let key = *key;
            let mut wheel = shard.wheel().borrow_mut();
            if wheel.is_fired(key) {
                wheel.remove(key);
                drop(wheel);
                self.state = State::Done;
                return Poll::Ready(());
            }
            wheel.set_waker(key, cx.waker());
            return Poll::Pending;
        }
        if let State::Remote { cell, .. } = &self.state {
            return Self::poll_cell(cell, cx).map(|()| self.state = State::Done);
        }
        // Not armed here: armed now (after letting go of an arming on
        // another shard, if it moved).
        self.release();
        if Instant::now() >= self.deadline {
            self.state = State::Done;
            return Poll::Ready(());
        }
        let at = shard.wheel_time(self.deadline.into_std());
        let mut wheel = shard.wheel().borrow_mut();
        let key = wheel.insert(at, cx.waker());
        if wheel.is_fired(key) {
            wheel.remove(key);
            drop(wheel);
            self.state = State::Done;
            return Poll::Ready(());
        }
        drop(wheel);
        self.state = State::Local {
            owner: Arc::clone(&shard.shared),
            key,
        };
        Poll::Pending
    }

    fn poll_remote(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if let State::Remote { cell, .. } = &self.state {
            return Self::poll_cell(cell, cx).map(|()| self.state = State::Done);
        }
        self.release();
        if Instant::now() >= self.deadline {
            self.state = State::Done;
            return Poll::Ready(());
        }
        let cell = Arc::new(RemoteTimer {
            fired: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            waker: AtomicWaker::new(),
            key: UnsafeCell::new(None),
        });
        cell.waker.register(cx.waker());
        let shard = crate::rt::runtime::service_shard();
        shard.post_timer(TimerOp::Insert {
            at: self.deadline,
            cell: Arc::clone(&cell),
        });
        self.state = State::Remote { shard, cell };
        Poll::Pending
    }

    fn poll_cell(cell: &RemoteTimer, cx: &mut Context<'_>) -> Poll<()> {
        if cell.fired.load(Ordering::Acquire) {
            return Poll::Ready(());
        }
        cell.waker.register(cx.waker());
        if cell.fired.load(Ordering::Acquire) {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        if let State::Done = this.state {
            return Poll::Ready(());
        }
        let shard: *const Shard =
            context::current(|current| current.map_or(core::ptr::null(), |s| s as *const Shard));
        if shard.is_null() {
            this.poll_remote(cx)
        } else {
            // SAFETY: the shard this thread runs is kept alive by the guard
            // that entered it, further up this thread's stack.
            this.poll_local(unsafe { &*shard }, cx)
        }
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        self.release();
    }
}

impl fmt::Debug for Sleep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sleep")
            .field("deadline", &self.deadline)
            .field("elapsed", &self.is_elapsed())
            .finish()
    }
}

/// A timer a shard fires for a thread that is not one of its own.
pub(crate) struct RemoteTimer {
    fired: AtomicBool,
    cancelled: AtomicBool,
    waker: AtomicWaker,
    /// Where the timer is in the firing shard's wheel. Read and written by
    /// that shard's thread only (arming, cancelling and firing all run there).
    key: UnsafeCell<Option<TimerKey>>,
}

// SAFETY: every field but `key` is atomic; `key` is touched only by the
// thread of the shard that fires the timer (see its docs).
unsafe impl Send for RemoteTimer {}
// SAFETY: as for `Send`.
unsafe impl Sync for RemoteTimer {}

impl Wake for RemoteTimer {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        // Called by the firing shard once the wheel let go of the timer: the
        // fired entry is removed here, as nobody else holds its key.
        // SAFETY: on the firing shard's thread (see `key`).
        if let Some(key) = unsafe { (*self.key.get()).take() } {
            context::current(|current| {
                if let Some(shard) = current {
                    shard.wheel().borrow_mut().remove(key);
                }
            });
        }
        self.fired.store(true, Ordering::Release);
        self.waker.wake();
    }
}

/// Work on a shard's timer wheel from another thread.
pub(crate) enum TimerOp {
    /// Arm a timer that fires `cell`, for a thread with no shard.
    Insert { at: Instant, cell: Arc<RemoteTimer> },
    /// Remove a timer of this wheel armed by a sleep that moved away or was
    /// dropped on another thread.
    Remove(TimerKey),
    /// Cancel a timer armed by `Insert`.
    Cancel(Arc<RemoteTimer>),
}

impl TimerOp {
    /// Whether the shard must look at the op now (an arm moves its next
    /// deadline); a removal can wait for its next look.
    pub(crate) fn needs_wake(&self) -> bool {
        matches!(self, Self::Insert { .. })
    }

    /// Applies the op on its shard's thread.
    pub(crate) fn apply(self, shard: &Shard, wheel: &mut Wheel) {
        match self {
            Self::Insert { at, cell } => {
                if cell.cancelled.load(Ordering::SeqCst) {
                    return;
                }
                let waker = Waker::from(Arc::clone(&cell));
                let key = wheel.insert(shard.wheel_time(at.into_std()), &waker);
                if wheel.is_fired(key) {
                    wheel.remove(key);
                    cell.fired.store(true, Ordering::Release);
                    cell.waker.wake();
                } else {
                    // SAFETY: on this shard's thread (see `RemoteTimer::key`).
                    unsafe { *cell.key.get() = Some(key) };
                }
            }
            Self::Remove(key) => wheel.remove(key),
            Self::Cancel(cell) => {
                // SAFETY: on this shard's thread (see `RemoteTimer::key`).
                if let Some(key) = unsafe { (*cell.key.get()).take() } {
                    wheel.remove(key);
                }
            }
        }
    }
}
