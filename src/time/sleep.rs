//! [`Sleep`]: a future that completes at a deadline, on the timer wheel of
//! the shard that polls it, and the timer both it and
//! [`SleepPrecise`](super::SleepPrecise) are built on, generic over the wheel
//! it arms: the millisecond one, or the microsecond one off it.
//!
//! A timer that completes spends one operation of the task's budget
//! (`coop`), as IO that completes does, so a loop over sleeps that are
//! already due yields. A refused poll leaves the timer completed: its next
//! poll answers `Ready`, and is charged then. A poll that answers `Pending`
//! spends nothing.

use core::cell::UnsafeCell;
use core::fmt;
use core::future::Future;
use core::marker::PhantomData;
use core::mem::ManuallyDrop;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::sync::Arc;
use std::task::Wake;
use std::time::Duration;

use super::{Instant, clock};

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
///
/// A sleep polled on a thread that runs no shard (another executor's, or a
/// blocking job's) is fired by a worker of the runtime; with no worker taking
/// tasks (a runtime of `worker_threads(0)`, or one shutting down) it goes to
/// that thread's own shard, which runs only inside that thread's `block_on`,
/// and until then it never ends. Await such a sleep in a task or in a
/// `block_on` instead.
#[inline(always)]
pub fn sleep(duration: Duration) -> Sleep {
    Sleep {
        timer: Timer::after(duration),
    }
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
        self.timer.deadline()
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

    #[inline(always)]
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

/// A timer that completed answers `Ready`, one operation of the task's budget
/// spent as an IO operation's is; a spent budget wakes the task and answers
/// `Pending`, and the timer stays completed, so its next poll answers `Ready`.
#[inline(always)]
fn ready(cx: &Context<'_>) -> Poll<()> {
    ready_in(context::local(), cx)
}

/// [`ready`] for a caller that holds the thread's context already.
#[inline(always)]
fn ready_in(here: &context::Here, cx: &Context<'_>) -> Poll<()> {
    if crate::coop::spend_in(&here.budget, cx.waker()) {
        Poll::Ready(())
    } else {
        Poll::Pending
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
    deadline: Deadline,
    /// Never dropped as it is: the drop releases what it holds, which leaves
    /// it holding nothing.
    state: ManuallyDrop<State>,
    grain: PhantomData<G>,
}

/// A timer's deadline, as it was made: the expiry checks and the wheels read
/// it in nanoseconds after the clock's origin, which only the poll that arms
/// the timer asks for.
#[derive(Clone, Copy)]
enum Deadline {
    /// Made from a duration: nanoseconds after the origin, which name its
    /// instant exactly.
    Nanos(u64),
    /// Given as an instant (`sleep_until`, `reset`), which its nanoseconds
    /// may not reach (before the origin, past their range).
    Given(Instant),
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
            deadline: Deadline::Given(deadline),
            state: ManuallyDrop::new(State::Idle),
            grain: PhantomData,
        }
    }

    /// A timer `duration` from now on the runtime's clock: the clock read
    /// once, in nanoseconds; one past their range is [`far_future`]'s, as an
    /// instant would be.
    #[inline(always)]
    pub(super) fn after(duration: Duration) -> Self {
        let span = duration
            .as_secs()
            .checked_mul(1_000_000_000)
            .and_then(|whole| whole.checked_add(u64::from(duration.subsec_nanos())));
        // Read outside a closure, which the clock's read would keep from
        // being inlined.
        let now = clock::now_nanos();
        match span.and_then(|span| now.checked_add(span)) {
            Some(at) => Self {
                deadline: Deadline::Nanos(at),
                state: ManuallyDrop::new(State::Idle),
                grain: PhantomData,
            },
            None => Self::new(far_future()),
        }
    }

    /// The deadline.
    pub(super) fn deadline(&self) -> Instant {
        match self.deadline {
            Deadline::Nanos(at) => Instant::from_std(clock::instant(at)),
            Deadline::Given(at) => at,
        }
    }

    /// The deadline in nanoseconds after the clock's origin.
    #[inline]
    fn nanos(&self) -> u64 {
        match self.deadline {
            Deadline::Nanos(at) => at,
            Deadline::Given(at) => clock::nanos(at.into_std()),
        }
    }

    /// Whether the deadline has passed.
    pub(super) fn is_elapsed(&self) -> bool {
        matches!(*self.state, State::Done) || clock::now_nanos() >= self.nanos()
    }

    /// Moves the deadline; the timer is pending again until it.
    pub(super) fn reset(&mut self, deadline: Instant) {
        self.release();
        self.deadline = Deadline::Given(deadline);
    }

    /// Lets go of the timer, wherever it is armed, and leaves the timer idle.
    #[inline]
    fn release(&mut self) {
        match *self.state {
            State::Idle => {}
            // Fired: nothing to let go of, and pending again (a reset).
            State::Done => *self.state = State::Idle,
            State::Local { .. } | State::Remote { .. } => self.release_armed(),
        }
    }

    #[inline(never)]
    fn release_armed(&mut self) {
        match core::mem::replace(&mut *self.state, State::Idle) {
            State::Local { owner, key } => {
                let (here, removed) = context::current(|current| match current {
                    Some(shard) if shard.is(&owner) => {
                        (true, shard.wheel_of(G::PRECISE).borrow_mut().remove(key))
                    }
                    _ => (false, None),
                });
                if !here {
                    owner.post_timer(TimerOp::Remove {
                        key,
                        precise: G::PRECISE,
                    });
                }
                // Dropped outside the wheel's borrow: it is user code.
                drop(removed);
            }
            State::Remote { shard, cell } => {
                cell.cancelled.store(true, Ordering::SeqCst);
                // The waker goes now: a fire racing the cancel finds none to
                // wake, and the task it belongs to is not held until then.
                // Its drop is user code, so the cancel is posted first.
                let waker = cell.waker.take();
                shard.post_timer(TimerOp::Cancel(cell));
                drop(waker);
            }
            State::Idle | State::Done => {}
        }
    }

    #[inline(always)]
    fn poll_local(&mut self, shard: &Shard, cx: &mut Context<'_>) -> Poll<()> {
        if let State::Local { owner, key } = &*self.state
            && shard.is(owner)
        {
            let key = *key;
            let wheel = shard.wheel_of(G::PRECISE);
            // A fired timer let go of its entry as it fired.
            if wheel.borrow().is_fired(key) {
                self.end_local();
                return ready(cx);
            }
            if wheel.borrow().wakes(key, cx.waker()) {
                return Poll::Pending;
            }
            // The clone and the drop of the old waker are user code, run with
            // the wheel let go; the timer may have fired meanwhile.
            let fresh = cx.waker().clone();
            let (old, fired) = {
                let mut wheel = wheel.borrow_mut();
                let old = wheel.replace_waker(key, fresh);
                (old, wheel.is_fired(key))
            };
            drop(old);
            if fired {
                self.end_local();
                return ready(cx);
            }
            return Poll::Pending;
        }
        // Not armed here: armed now (after letting go of an arming on
        // another shard, if it moved); a timer not armed yet has none.
        if !matches!(*self.state, State::Idle) {
            if let Some(polled) = self.poll_remote_cell(cx) {
                return polled;
            }
            self.release();
        }
        let at = self.nanos();
        if clock::now_nanos() >= at {
            *self.state = State::Done;
            return ready(cx);
        }
        let at = shard.time_of(G::PRECISE, at);
        let fresh = cx.waker().clone();
        let armed = shard.wheel_of(G::PRECISE).borrow_mut().insert(at, fresh);
        let key = match armed {
            Ok(key) => key,
            Err(unused) => {
                *self.state = State::Done;
                drop(unused);
                return ready(cx);
            }
        };
        *self.state = State::Local {
            owner: Arc::clone(&shard.shared),
            key,
        };
        Poll::Pending
    }

    fn poll_remote(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if let Some(polled) = self.poll_remote_cell(cx) {
            return polled;
        }
        self.release();
        // On an altered clock, the look at it and the post are one arm in
        // flight, which a jump of a paused clock waits for (`ForeignArm.tla`).
        let _arming = clock::any_altered().then(Arming::enter).flatten();
        let at = self.nanos();
        if clock::now_nanos() >= at {
            *self.state = State::Done;
            return ready(cx);
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
            at,
            cell: Arc::clone(&cell),
        });
        *self.state = State::Remote { shard, cell };
        Poll::Pending
    }

    /// Polls the cell of a timer armed by another shard, if it is one: `None`
    /// when it is not, or when its shard let go of it unfired (it is idle
    /// then, to be armed again where this poll runs).
    #[inline]
    fn poll_remote_cell(&mut self, cx: &mut Context<'_>) -> Option<Poll<()>> {
        let State::Remote { cell, .. } = &*self.state else {
            return None;
        };
        match poll_cell(cell, cx) {
            Found::Fired => {
                *self.state = State::Done;
                Some(ready(cx))
            }
            Found::Pending => Some(Poll::Pending),
            Found::Orphaned => {
                *self.state = State::Idle;
                None
            }
        }
    }

    /// Ends a timer armed here that fired.
    #[inline(always)]
    fn end_local(&mut self) {
        // The state is `Local`: its owner goes, and nothing else.
        if let State::Local { owner, .. } = core::mem::replace(&mut *self.state, State::Done) {
            drop(owner);
        }
    }

    #[inline(always)]
    pub(super) fn poll(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        // A sleep's usual end: armed on the shard this thread runs, and
        // fired.
        let here = context::local();
        if let State::Local { owner, key } = &*self.state
            && let Some(shard) = context::running_inside(here)
            && shard.is(owner)
            && shard.wheel_of(G::PRECISE).borrow().is_fired(*key)
        {
            self.end_local();
            return ready_in(here, cx);
        }
        self.poll_any(cx)
    }

    #[inline(never)]
    fn poll_any(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if let State::Done = *self.state {
            return ready(cx);
        }
        // On the shard this thread runs with no bracket to take, as `current`
        // would hand it over.
        if let Some(shard) = context::running_inside(context::local()) {
            return self.poll_local(shard, cx);
        }
        context::current(|current| match current {
            Some(shard) => self.poll_local(shard, cx),
            None => self.poll_remote(cx),
        })
    }
}

impl<G: Grain> Drop for Timer<G> {
    fn drop(&mut self) {
        // What the state holds goes here; idle or done, it holds nothing.
        if matches!(*self.state, State::Local { .. } | State::Remote { .. }) {
            self.release_armed();
        }
    }
}

/// What a poll found in a remote timer's cell.
enum Found {
    /// The timer fired.
    Fired,
    /// It is armed, and the poll's waker is the one its fire wakes.
    Pending,
    /// Its shard let go of it unfired: nothing will fire it.
    Orphaned,
}

/// Registers the poll's waker in `cell`, then looks at both marks: a fire,
/// or an orphaning, that came before the register woke the waker the cell
/// held until then, which may be another task's (the sleep moved), and this
/// one would be woken by nothing (proofs/tla/timer, `M_recheck_fired_only`).
fn poll_cell(cell: &RemoteTimer, cx: &mut Context<'_>) -> Found {
    if cell.fired.load(Ordering::Acquire) {
        return Found::Fired;
    }
    #[cfg(test)]
    before_register();
    cell.waker.register(cx.waker());
    if cell.fired.load(Ordering::Acquire) {
        Found::Fired
    } else if cell.orphaned.load(Ordering::Acquire) {
        Found::Orphaned
    } else {
        Found::Pending
    }
}

#[cfg(test)]
thread_local! {
    /// What a test runs in a poll of a remote timer's cell right before it
    /// registers its waker, once: the moment a shard going meets a poll in.
    pub(super) static BEFORE_REGISTER: core::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { core::cell::RefCell::new(None) };
}

#[cfg(test)]
fn before_register() {
    if let Some(hook) = BEFORE_REGISTER.with(|hook| hook.borrow_mut().take()) {
        hook();
    }
}

/// A remote arm counted in flight on its runtime's altered clock, from before
/// its look at the clock until it posts or gives up: counted out when dropped,
/// an unwind included.
struct Arming(Arc<crate::rt::runtime::Inner>);

impl Arming {
    /// Counts the arm in and waits for a jump being decided to end; `None`
    /// when no runtime is running or its clock is not altered.
    #[cold]
    fn enter() -> Option<Self> {
        let inner = crate::rt::runtime::running_inner()?;
        if !inner.clock.is_altered() {
            return None;
        }
        inner.clock.arm_began();
        let arming = Self(inner);
        arming.0.clock.wait_advance();
        Some(arming)
    }
}

impl Drop for Arming {
    fn drop(&mut self) {
        self.0.clock.arm_ended();
        // A shard that tried to jump while this arm was in flight parked
        // without one: the jump is tried again, as when a blocking job ends.
        crate::rt::paused::try_advance(&self.0, None);
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
    /// it, its entry gone with the fire.
    fn fire(&self) {
        // SAFETY: on the firing shard's thread (see `key`).
        unsafe { *self.key.get() = None };
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
    /// Arm a timer that fires `cell` at `at` (nanoseconds after the clock's
    /// origin), for a thread with no shard, in the wheel `cell` names.
    Insert { at: u64, cell: Arc<RemoteTimer> },
    /// Remove a timer of this shard (of its microsecond wheel when
    /// `precise`) armed by a sleep that moved away or was dropped on another
    /// thread.
    Remove { key: TimerKey, precise: bool },
    /// Cancel a timer armed by `Insert`.
    Cancel(Arc<RemoteTimer>),
}

impl TimerOp {
    /// Whether the shard must look at the op now: an arm moves its next
    /// deadline, and while a clock is paused or advanced any op does, since
    /// a shard holding one is not idle and a paused clock jumps only while
    /// every shard is (proofs/tla/clock, `M_post_no_wake`). Otherwise a
    /// removal can wait for the shard's next look.
    pub(crate) fn needs_wake(&self) -> bool {
        matches!(self, Self::Insert { .. }) || super::clock::any_altered()
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
    ///
    /// Runs no user code: the waker of a timer already due goes to `fired`
    /// to be woken, and a waker the op removes is answered, to be dropped,
    /// both once the caller let go of the wheel.
    pub(crate) fn apply(
        self,
        shard: &Shard,
        wheel: &mut Wheel,
        fired: &mut Vec<Waker>,
    ) -> Option<Waker> {
        match self {
            Self::Insert { at, cell } => {
                if cell.cancelled.load(Ordering::SeqCst) {
                    return None;
                }
                let waker = Waker::from(Arc::new(Armed(Arc::clone(&cell))));
                let at = shard.time_of(cell.precise, at);
                let armed = if cell.precise {
                    shard.wheel_of(true).borrow_mut().insert(at, waker)
                } else {
                    wheel.insert(at, waker)
                };
                match armed {
                    Err(unused) => {
                        cell.fired.store(true, Ordering::Release);
                        fired.extend(cell.waker.take());
                        Some(unused)
                    }
                    Ok(key) => {
                        // SAFETY: on this shard's thread (see `RemoteTimer::key`).
                        unsafe { *cell.key.get() = Some(key) };
                        None
                    }
                }
            }
            Self::Remove {
                key,
                precise: false,
            } => wheel.remove(key),
            Self::Remove { key, precise: true } => shard.wheel_of(true).borrow_mut().remove(key),
            Self::Cancel(cell) => {
                // SAFETY: on this shard's thread (see `RemoteTimer::key`).
                let key = unsafe { (*cell.key.get()).take() }?;
                if cell.precise {
                    shard.wheel_of(true).borrow_mut().remove(key)
                } else {
                    wheel.remove(key)
                }
            }
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;
    use crate::rt::shard::Limits;

    /// Turns of the loop until the wheel arms `armed` timers: the posted
    /// changes are applied at a look the loop makes when it likes.
    fn turns_until(shard: &Shard, armed: usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while shard.wheel().borrow().armed() != armed {
            assert!(
                std::time::Instant::now() < deadline,
                "the wheel never held {armed}"
            );
            shard.block_on(crate::task::yield_now());
        }
    }

    #[test]
    fn a_remote_sleeps_cancel_is_posted_before_its_waker_drops() {
        let shard = Shard::new(Limits {
            max_nesting: 8,
            buffer_cap: crate::buf::DEFAULT_CAP,
            backend: crate::rt::backend::chosen().unwrap(),
        })
        .unwrap();
        let _current = context::enter(&shard);
        let (bomb, waker) = crate::time::tests::bomb();
        let cell = Arc::new(RemoteTimer {
            fired: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            orphaned: AtomicBool::new(false),
            precise: false,
            waker: AtomicWaker::new(),
            key: UnsafeCell::new(None),
        });
        cell.waker.register(&waker);
        let at = clock::now_nanos() + 3_600_000_000_000;
        shard.shared.post_timer(TimerOp::Insert {
            at,
            cell: Arc::clone(&cell),
        });
        let timer = Timer::<Coarse> {
            deadline: Deadline::Nanos(at),
            state: ManuallyDrop::new(State::Remote {
                shard: Arc::clone(&shard.shared),
                cell,
            }),
            grain: PhantomData,
        };
        turns_until(&shard, 1);
        bomb.arm();
        assert!(catch_unwind(AssertUnwindSafe(|| drop(timer))).is_err());
        turns_until(&shard, 0);
    }
}
