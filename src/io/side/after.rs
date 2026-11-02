//! What a step under a side's guard leaves for once the guard is let go
//! ([`After`]): the wakes and drops that may run code of their own, parked
//! while a non-owning view of a source is live and the thread unwinds
//! ([`Unwound`]); and the wakers of the tasks waiting on a side ([`Wakers`]).

use core::mem::MaybeUninit;
use core::task::Waker;

use crate::buf::PoolBuf;

/// What a step leaves for once no guard is held: wakers to wake, wakers and
/// buffers to drop (either may run code that reaches a source), and what
/// the source must do next.
///
/// Almost every step leaves nothing, so the slots start empty and unwritten,
/// and `held` says which of them hold a value and which flags are raised: a
/// step that left nothing is made with one store and settled with one look.
pub(crate) struct After {
    /// `WAKE`, `DROP`, `MORE`, `KICK`: the slot holds a value; `STARVED`,
    /// `REGISTER`: the flag is raised.
    held: u8,
    /// The first waker to wake: almost every step wakes one task at most.
    wake: MaybeUninit<Waker>,
    /// The first buffer to drop: almost every step drops one at most.
    drop_one: MaybeUninit<PoolBuf>,
    more: MaybeUninit<Box<More>>,
    /// A side of a ring source wants an operation the ring's thread, the
    /// one of this shard, must submit.
    #[cfg(ring)]
    kick: MaybeUninit<std::sync::Arc<crate::rt::shard::Shared>>,
}

/// `After::held`: the slots that hold a value.
const WAKE: u8 = 1;
const DROP: u8 = 2;
const MORE: u8 = 4;
#[cfg(ring)]
const KICK: u8 = 8;
/// `After::held`: a side found the pool empty and must hear when a buffer
/// comes back.
const STARVED: u8 = 16;
/// `After::held`: work was left to the driver, so the source must be
/// registered.
const REGISTER: u8 = 32;
/// `After::held`: the step ran in a call marked into a shard the monitor
/// watches (`context::mark_call`), which its settle ends before it wakes or
/// drops what the step left. The sign bit, which an owner's light settle tests
/// only past its two cases of a step no mark made, so theirs stay as they were.
const UNMARK: u8 = 128;

#[cfg(ring)]
const SLOTS: u8 = WAKE | DROP | MORE | KICK;
#[cfg(not(ring))]
const SLOTS: u8 = WAKE | DROP | MORE;

impl Default for After {
    #[inline(always)]
    fn default() -> Self {
        Self {
            held: 0,
            wake: MaybeUninit::uninit(),
            drop_one: MaybeUninit::uninit(),
            more: MaybeUninit::uninit(),
            #[cfg(ring)]
            kick: MaybeUninit::uninit(),
        }
    }
}

/// What a step leaves beyond one waker and one buffer, allocated only when
/// a step has more.
#[derive(Default)]
struct More {
    wake: Vec<Waker>,
    forget: Wakers,
    bufs: Vec<PoolBuf>,
}

impl After {
    fn more(&mut self) -> &mut More {
        if self.held & MORE == 0 {
            self.more.write(Box::default());
            self.held |= MORE;
        }
        // SAFETY: `MORE` says the slot holds a value.
        unsafe { self.more.assume_init_mut() }
    }

    /// Whether a side found the pool empty: the source must hear when a
    /// buffer comes back.
    #[inline(always)]
    pub(crate) fn starved(&self) -> bool {
        self.held & STARVED != 0
    }

    /// A side found the pool empty (see [`starved`](Self::starved)).
    #[inline(always)]
    pub(crate) fn set_starved(&mut self) {
        self.held |= STARVED;
    }

    /// Marks the step's call into the shard this thread runs, where the
    /// monitor watches it (`context::mark_call`), until the step is settled:
    /// `false` when the shard was taken and the thread runs none.
    #[inline(always)]
    pub(crate) fn enter(&mut self) -> bool {
        match crate::rt::context::mark_call() {
            crate::rt::context::Marked::Not => true,
            crate::rt::context::Marked::In => {
                self.held |= UNMARK;
                true
            }
            crate::rt::context::Marked::Taken => false,
        }
    }

    /// Whether [`enter`](Self::enter) marked the step's call: its settle ends
    /// the call before it wakes what the step left.
    #[inline(always)]
    pub(crate) fn marked(&self) -> bool {
        self.held & UNMARK != 0
    }

    /// Whether work was left to the driver, so the source must be registered.
    #[inline(always)]
    pub(crate) fn must_register(&self) -> bool {
        self.held & REGISTER != 0
    }

    /// Work was left to the driver (see [`must_register`](Self::must_register)).
    #[inline(always)]
    pub(crate) fn set_must_register(&mut self) {
        self.held |= REGISTER;
    }

    /// The shard whose ring thread must submit what a side wants, in place
    /// of the one asked before.
    #[cfg(ring)]
    pub(crate) fn set_kick(&mut self, home: Option<std::sync::Arc<crate::rt::shard::Shared>>) {
        drop(self.take_kick());
        if let Some(home) = home {
            self.kick.write(home);
            self.held |= KICK;
        }
    }

    /// Takes the shard [`set_kick`](Self::set_kick) asked.
    #[cfg(ring)]
    #[inline(always)]
    pub(crate) fn take_kick(&mut self) -> Option<std::sync::Arc<crate::rt::shard::Shared>> {
        if self.held & KICK == 0 {
            return None;
        }
        self.held &= !KICK;
        // SAFETY: `KICK` said the slot holds a value, and it is cleared: the
        // value is read out once.
        Some(unsafe { self.kick.assume_init_read() })
    }

    /// Takes the first waker.
    #[inline(always)]
    fn take_wake(&mut self) -> Option<Waker> {
        if self.held & WAKE == 0 {
            return None;
        }
        self.held &= !WAKE;
        // SAFETY: as in `take_kick`.
        Some(unsafe { self.wake.assume_init_read() })
    }

    /// Takes the first buffer.
    #[inline(always)]
    fn take_drop(&mut self) -> Option<PoolBuf> {
        if self.held & DROP == 0 {
            return None;
        }
        self.held &= !DROP;
        // SAFETY: as in `take_kick`.
        Some(unsafe { self.drop_one.assume_init_read() })
    }

    /// Takes what goes beyond the first waker and buffer.
    #[inline(always)]
    fn take_more(&mut self) -> Option<Box<More>> {
        if self.held & MORE == 0 {
            return None;
        }
        self.held &= !MORE;
        // SAFETY: as in `take_kick`.
        Some(unsafe { self.more.assume_init_read() })
    }

    #[inline]
    pub(crate) fn wake(&mut self, wakers: &mut Wakers) {
        if let Some(w) = wakers.first.take() {
            self.wake_one(w);
        }
        if !wakers.rest.is_empty() {
            self.wake_rest(wakers);
        }
    }

    #[cold]
    #[inline(never)]
    fn wake_rest(&mut self, wakers: &mut Wakers) {
        self.more().wake.append(&mut wakers.rest);
    }

    /// Drops `wakers` without waking them.
    pub(crate) fn forget(&mut self, wakers: &mut Wakers) {
        if !wakers.is_empty() {
            self.more().forget.append(wakers);
        }
    }

    /// Wakes the one task of `waker`.
    #[inline]
    pub(crate) fn wake_one(&mut self, waker: Waker) {
        if self.held & WAKE == 0 {
            self.wake.write(waker);
            self.held |= WAKE;
        } else {
            self.wake_more(waker);
        }
    }

    #[cold]
    #[inline(never)]
    fn wake_more(&mut self, waker: Waker) {
        self.more().wake.push(waker);
    }

    /// Drops `waker` without waking it.
    pub(crate) fn forget_one(&mut self, waker: Waker) {
        self.more().forget.push(waker);
    }

    pub(crate) fn drop_buf(&mut self, buf: PoolBuf) {
        if self.held & DROP == 0 {
            self.drop_one.write(buf);
            self.held |= DROP;
        } else {
            self.more().bufs.push(buf);
        }
    }

    /// Wakes and drops; call with no guard held. Once: it consumes the step,
    /// so its drop finds nothing left to do. A step that left one waker
    /// alone, the usual one that left anything, is woken inline.
    #[inline]
    pub(crate) fn run(mut self) {
        if self.held & SLOTS == WAKE {
            self.wake_alone();
            return;
        }
        if self.held & SLOTS != 0 {
            self.run_held();
        }
        core::mem::forget(self);
    }

    /// Wakes the one waker of a step whose slots hold that alone, and
    /// consumes the step.
    #[inline(always)]
    fn wake_alone(mut self) {
        if let Some(w) = self.take_wake() {
            core::mem::forget(self);
            w.wake();
        }
    }

    /// [`run`](Self::run) of a step that left a value.
    #[inline(never)]
    fn run_held(&mut self) {
        self.flush();
        #[cfg(ring)]
        drop(self.take_kick());
    }

    #[inline]
    fn flush(&mut self) {
        drop(self.take_drop());
        match self.take_more() {
            None => {
                if let Some(w) = self.take_wake() {
                    w.wake();
                }
            }
            Some(more) => self.flush_more(*more),
        }
    }

    /// [`flush`](Self::flush) of a step that left more than one waker or buffer.
    #[inline(never)]
    fn flush_more(&mut self, more: More) {
        let More { wake, forget, bufs } = more;
        drop(bufs);
        drop(forget);
        if let Some(w) = self.take_wake() {
            w.wake();
        }
        wake.into_iter().for_each(Waker::wake);
    }
}

/// The end of a light settle of a marked step (`After::settle_light_owned`): the call's end,
/// then its one waker woken, outside the shard as any step's wake from a task's code is (a
/// waker may run code of its own).
#[inline(never)]
fn unmark_and_wake(wake: Option<Waker>) {
    crate::rt::context::unmark_call();
    if let Some(wake) = wake {
        wake.wake();
    }
}

impl Drop for After {
    fn drop(&mut self) {
        debug_assert!(
            self.held & UNMARK == 0 || std::thread::panicking(),
            "a marked step dropped unsettled"
        );
        // A marked step unwinding (a panic in it, which the task's code may catch) ends its call
        // first, as its settle does: else the poll's word says inside for the rest of the poll,
        // which the monitor then never takes (proofs/tla/handoff, `M_marked_unwind`).
        if self.held & UNMARK != 0 {
            self.held &= !UNMARK;
            crate::rt::context::unmark_unwound();
        }
        if parking() {
            self.park();
        } else {
            self.flush();
        }
        #[cfg(ring)]
        drop(self.take_kick());
    }
}

impl After {
    /// Settles a step that left nothing, or one waker and no flag, the usual cases, inline: it
    /// wakes that waker and consumes the step. `Err` gives back any other, for the caller to
    /// settle as its flags ask and then [`run`](Self::run).
    #[inline(always)]
    pub(crate) fn settle_light(self) -> Result<(), Self> {
        match self.held {
            // Nothing is owned: no waker, no buffer, no allocation, so there is nothing to drop.
            0 => core::mem::forget(self),
            WAKE => self.wake_alone(),
            _ => return Err(self),
        }
        Ok(())
    }

    /// [`settle_light`](Self::settle_light) of an owner's step, which [`enter`](Self::enter) may
    /// have marked: such a step that left nothing, or one waker and no flag, ends the call and
    /// then wakes it; `Err` gives back any other, still marked.
    #[inline(always)]
    pub(crate) fn settle_light_owned(mut self) -> Result<(), Self> {
        match self.held {
            0 => core::mem::forget(self),
            WAKE => self.wake_alone(),
            // A test of the sign past the two above, so theirs stay as an unmarked step's.
            held if held.cast_signed() < 0 && held & !(UNMARK | WAKE) == 0 => {
                let wake = self.take_wake();
                core::mem::forget(self);
                unmark_and_wake(wake);
            }
            _ => return Err(self),
        }
        Ok(())
    }

    #[inline(always)]
    fn is_empty(&self) -> bool {
        self.held & SLOTS == 0
    }

    /// Unwinding: the work goes to the thread's list for [`Unwound`] or the
    /// end of the poll to run, else (the list gone, at thread exit) runs now.
    #[cold]
    fn park(&mut self) {
        if self.is_empty() {
            return;
        }
        let mut work = core::mem::take(self);
        let parked = UNWOUND.try_with(|list| match list.try_borrow_mut() {
            Ok(mut list) => {
                list.push(core::mem::take(&mut work));
                true
            }
            Err(_) => false,
        });
        if parked != Ok(true) {
            work.flush();
            #[cfg(ring)]
            drop(work.take_kick());
        }
    }
}

/// Whether an `After` dropped now is parked: the thread is unwinding with an
/// [`Unwound`] guard alive.
#[cfg(any(ring, test))]
fn parking() -> bool {
    view_live().get() && std::thread::panicking()
}

#[cfg(not(any(ring, test)))]
fn parking() -> bool {
    false
}

#[cfg(any(ring, test))]
thread_local! {
    /// A non-owning view of a source is live on this thread (see [`Unwound`]).
    static VIEW_LIVE: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// This thread's [`VIEW_LIVE`], reached with an inlined access: only its address comes out of
/// `with`, whose closure is then too small not to be inlined.
#[cfg(any(ring, test))]
#[inline(always)]
fn view_live() -> &'static core::cell::Cell<bool> {
    // SAFETY: the cell has no destructor, so it lives as long as the thread; it is a `Cell`,
    // which is not `Sync`, so the reference stays on this thread.
    unsafe { &*VIEW_LIVE.with(core::ptr::from_ref) }
}

thread_local! {
    /// What [`After`]s dropped during unwinding left to run.
    static UNWOUND: core::cell::RefCell<Vec<After>> = const { core::cell::RefCell::new(Vec::new()) };
}

/// Runs what unwinding steps left in this thread's list.
#[cfg(any(ring, test))]
pub(crate) fn run_unwound() {
    let Ok(work) = UNWOUND.try_with(|list| core::mem::take(&mut *list.borrow_mut())) else {
        return;
    };
    work.into_iter().for_each(After::run);
}

/// Declared before a non-owning view of a source, so that it drops after it:
/// while it lives, an `After` dropped by unwinding is parked, and when it
/// drops by unwinding it runs what was parked.
#[cfg(any(ring, test))]
pub(crate) struct Unwound {
    outer: bool,
}

#[cfg(any(ring, test))]
impl Unwound {
    pub(crate) fn enter() -> Self {
        Self {
            outer: view_live().replace(true),
        }
    }
}

#[cfg(any(ring, test))]
impl Drop for Unwound {
    fn drop(&mut self) {
        view_live().set(self.outer);
        if std::thread::panicking() {
            run_unwound();
        }
    }
}

/// The wakers of the tasks waiting on a side. A waker of a task already
/// registered is not added again, so the list holds one waker per task.
#[derive(Default)]
pub(crate) struct Wakers {
    first: Option<Waker>,
    rest: Vec<Waker>,
}

impl Wakers {
    #[inline(always)]
    pub(crate) fn register(&mut self, waker: &Waker) {
        let known = self.first.as_ref().is_some_and(|w| w.will_wake(waker))
            || self.rest.iter().any(|w| w.will_wake(waker));
        if known {
            return;
        }
        self.push(waker.clone());
    }

    /// Adds `waker`, known or not.
    #[inline(always)]
    fn push(&mut self, waker: Waker) {
        if self.first.is_none() {
            self.first = Some(waker);
        } else {
            self.push_rest(waker);
        }
    }

    #[cold]
    #[inline(never)]
    fn push_rest(&mut self, waker: Waker) {
        self.rest.push(waker);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.first.is_none()
    }

    /// How many tasks wait.
    #[cfg(any(readiness, test))]
    pub(crate) fn len(&self) -> usize {
        usize::from(self.first.is_some()) + self.rest.len()
    }

    /// Moves every waker of `other` here.
    fn append(&mut self, other: &mut Wakers) {
        if let Some(w) = other.first.take() {
            self.push(w);
        }
        self.rest.append(&mut other.rest);
    }
}

#[cfg(all(test, not(loom)))]
mod tests;
