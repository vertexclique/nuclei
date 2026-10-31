//! [`Sleep`]: a future that completes at a deadline, on the timer wheel of
//! the shard that polls it, and the timer both it and
//! [`SleepPrecise`](super::SleepPrecise) are built on, generic over the wheel
//! it arms: the millisecond one, or the microsecond one off it.

use core::cell::UnsafeCell;
use core::fmt;
use core::future::Future;
use core::marker::PhantomData;
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
/// task takes to yield. [`sleep_precise`](super::sleep_precise) waits to the
/// microsecond instead.
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
        timer: Timer::new(deadline.into()),
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
    timer: Timer<Coarse>,
}

impl Sleep {
    /// The deadline.
    pub fn deadline(&self) -> Instant {
        self.timer.deadline
    }

    /// Whether the deadline has passed.
    pub fn is_elapsed(&self) -> bool {
        self.timer.is_elapsed()
    }

    /// Moves the deadline; the sleep is pending again until it.
    pub fn reset(self: Pin<&mut Self>, deadline: impl Into<Instant>) {
        self.get_mut().timer.reset(deadline.into());
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.get_mut().timer.poll(cx)
    }
}

impl fmt::Debug for Sleep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sleep")
            .field("deadline", &self.deadline())
            .field("elapsed", &self.is_elapsed())
            .finish()
    }
}

/// Which of a shard's wheels a timer is armed in.
pub(crate) trait Grain {
    /// The microsecond wheel, off the millisecond one.
    const PRECISE: bool;
}

/// The millisecond wheel, of [`Sleep`].
pub(crate) struct Coarse;

impl Grain for Coarse {
    const PRECISE: bool = false;
}

/// The microsecond wheel, of [`SleepPrecise`](super::SleepPrecise).
pub(crate) struct Fine;

impl Grain for Fine {
    const PRECISE: bool = true;
}

/// A timer that completes at its deadline, in a wheel of grain `G` of the
/// shard that polls it. Dropping it cancels it.
pub(super) struct Timer<G: Grain> {
    pub(super) deadline: Instant,
    state: State,
    grain: PhantomData<G>,
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

impl<G: Grain> Timer<G> {
    pub(super) fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            state: State::Idle,
            grain: PhantomData,
        }
    }

    /// Whether the deadline has passed.
    pub(super) fn is_elapsed(&self) -> bool {
        matches!(self.state, State::Done) || Instant::now() >= self.deadline
    }

    /// Moves the deadline; the timer is pending again until it.
    pub(super) fn reset(&mut self, deadline: Instant) {
        self.release();
        self.deadline = deadline;
    }

    /// Lets go of the timer, wherever it is armed.
    fn release(&mut self) {
        match core::mem::replace(&mut self.state, State::Idle) {
            State::Local { owner, key } => {
                let here = context::current(|current| match current {
                    Some(shard) if shard.is(&owner) => {
                        shard.wheel_of(G::PRECISE).borrow_mut().remove(key);
                        true
                    }
                    _ => false,
                });
                if !here {
                    owner.post_timer(TimerOp::Remove {
                        key,
                        precise: G::PRECISE,
                    });
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
            let mut wheel = shard.wheel_of(G::PRECISE).borrow_mut();
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
            return poll_cell(cell, cx).map(|()| self.state = State::Done);
        }
        // Not armed here: armed now (after letting go of an arming on
        // another shard, if it moved).
        self.release();
        if Instant::now() >= self.deadline {
            self.state = State::Done;
            return Poll::Ready(());
        }
        let at = shard.time_of(G::PRECISE, self.deadline.into_std());
        let mut wheel = shard.wheel_of(G::PRECISE).borrow_mut();
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
            return poll_cell(cell, cx).map(|()| self.state = State::Done);
        }
        self.release();
        if Instant::now() >= self.deadline {
            self.state = State::Done;
            return Poll::Ready(());
        }
        let cell = Arc::new(RemoteTimer {
            fired: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            orphaned: AtomicBool::new(false),
            precise: G::PRECISE,
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

    pub(super) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if let State::Done = self.state {
            return Poll::Ready(());
        }
        if let State::Remote { cell, .. } = &self.state
            && cell.orphaned.load(Ordering::Acquire)
        {
            // The shard that held the timer went without firing it, and holds
            // it no more: it is armed again where this poll runs.
            self.state = State::Idle;
        }
        context::current(|current| match current {
            Some(shard) => self.poll_local(shard, cx),
            None => self.poll_remote(cx),
        })
    }
}

impl<G: Grain> Drop for Timer<G> {
    fn drop(&mut self) {
        self.release();
    }
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

/// A timer a shard fires for a thread that is not one of its own.
pub(crate) struct RemoteTimer {
    fired: AtomicBool,
    cancelled: AtomicBool,
    /// The shard let go of the timer without firing it, as it went: the
    /// sleep arms it again where it is polled next.
    orphaned: AtomicBool,
    /// Armed in the firing shard's microsecond wheel, not its millisecond
    /// one.
    precise: bool,
    waker: AtomicWaker,
    /// Where the timer is in the firing shard's wheel. Read and written by
    /// that shard's thread only (arming, cancelling and firing all run there).
    key: UnsafeCell<Option<TimerKey>>,
}

// SAFETY: every field but `key` is atomic or never written after the cell is
// made; `key` is touched only by the thread of the shard that fires the timer
// (see its docs).
unsafe impl Send for RemoteTimer {}
// SAFETY: as for `Send`.
unsafe impl Sync for RemoteTimer {}

impl RemoteTimer {
    /// Fires the timer: called by the firing shard once the wheel let go of
    /// it. The fired entry is removed here, as nobody else holds its key.
    fn fire(&self) {
        // SAFETY: on the firing shard's thread (see `key`).
        if let Some(key) = unsafe { (*self.key.get()).take() } {
            context::current(|current| {
                if let Some(shard) = current {
                    shard.wheel_of(self.precise).borrow_mut().remove(key);
                }
            });
        }
        self.fired.store(true, Ordering::Release);
        self.waker.wake();
    }

    /// The shard that was to fire the timer let go of it unfired, as it went:
    /// unless it fired or was cancelled, the sleep's task is woken, to arm it
    /// again where it is polled (or be dropped, its own shard gone too). The
    /// cell keeps the task's waker no longer, so the task, which holds the
    /// cell, is not kept by it for good (proofs/tla/timer, `NoStrandedWaker`).
    pub(crate) fn orphan(&self) {
        if self.fired.load(Ordering::Acquire) || self.cancelled.load(Ordering::SeqCst) {
            return;
        }
        self.orphaned.store(true, Ordering::Release);
        self.waker.wake();
    }
}

/// A remote timer as its shard's wheel holds it: fired when the wheel wakes
/// it, orphaned when the wheel drops it unfired (the shard went).
struct Armed(Arc<RemoteTimer>);

impl Wake for Armed {
    fn wake(self: Arc<Self>) {
        self.0.fire();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fire();
    }
}

impl Drop for Armed {
    fn drop(&mut self) {
        // A fire or a cancel came first: nothing to do.
        self.0.orphan();
    }
}

/// Work on a shard's timer wheels from another thread.
pub(crate) enum TimerOp {
    /// Arm a timer that fires `cell`, for a thread with no shard, in the
    /// wheel `cell` names.
    Insert { at: Instant, cell: Arc<RemoteTimer> },
    /// Remove a timer of this shard (of its microsecond wheel when
    /// `precise`) armed by a sleep that moved away or was dropped on another
    /// thread.
    Remove { key: TimerKey, precise: bool },
    /// Cancel a timer armed by `Insert`.
    Cancel(Arc<RemoteTimer>),
}

impl TimerOp {
    /// Whether the shard must look at the op now (an arm moves its next
    /// deadline); a removal can wait for its next look.
    pub(crate) fn needs_wake(&self) -> bool {
        matches!(self, Self::Insert { .. })
    }

    /// The op's shard went and applies it never: an arm request orphans its
    /// timer; a removal or a cancel had nothing left to do.
    pub(crate) fn orphan(self) {
        if let Self::Insert { cell, .. } = self {
            cell.orphan();
        }
    }

    /// Applies the op on its shard's thread; `wheel` is the shard's
    /// millisecond wheel, which the caller holds.
    pub(crate) fn apply(self, shard: &Shard, wheel: &mut Wheel) {
        match self {
            Self::Insert { at, cell } => {
                if cell.cancelled.load(Ordering::SeqCst) {
                    return;
                }
                let waker = Waker::from(Arc::new(Armed(Arc::clone(&cell))));
                let at = shard.time_of(cell.precise, at.into_std());
                let fired = if cell.precise {
                    arm(&mut shard.wheel_of(true).borrow_mut(), at, &waker)
                } else {
                    arm(wheel, at, &waker)
                };
                match fired {
                    None => {
                        cell.fired.store(true, Ordering::Release);
                        cell.waker.wake();
                    }
                    // SAFETY: on this shard's thread (see `RemoteTimer::key`).
                    Some(key) => unsafe { *cell.key.get() = Some(key) },
                }
            }
            Self::Remove {
                key,
                precise: false,
            } => wheel.remove(key),
            Self::Remove { key, precise: true } => shard.wheel_of(true).borrow_mut().remove(key),
            Self::Cancel(cell) => {
                // SAFETY: on this shard's thread (see `RemoteTimer::key`).
                if let Some(key) = unsafe { (*cell.key.get()).take() } {
                    if cell.precise {
                        shard.wheel_of(true).borrow_mut().remove(key);
                    } else {
                        wheel.remove(key);
                    }
                }
            }
        }
    }
}

/// Arms a timer for `at` in `wheel`: its key, or `None` when the deadline
/// passed already (the entry is gone again).
fn arm(wheel: &mut Wheel, at: u64, waker: &Waker) -> Option<TimerKey> {
    let key = wheel.insert(at, waker);
    if wheel.is_fired(key) {
        wheel.remove(key);
        None
    } else {
        Some(key)
    }
}
