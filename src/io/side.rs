//! The guard of one direction of a source, shared by the source's polls and
//! its shard's driver.
//!
//! # Progress
//!
//! No thread ever waits for another. Taking a side is one `fetch_or`; a
//! thread that finds it held leaves what it came for to the holder and goes
//! on, and the holder, as it lets go, does that work itself. What can be
//! left: the driver's part for an edge (`READY`), the wake of a poll that
//! found the side held (`WAITING`), side-local work such as waking readers a
//! returned buffer unstarves (`DEFER`), and requests that need both sides of
//! the source (`REQ`). Letting go is one compare-and-swap from the exact
//! word the holder took to zero; when others left flags meanwhile it fails,
//! and the holder takes the flags with one swap (still holding), does the
//! work, and tries again. So an edge is never lost, no poll spins, and the
//! driver never stalls behind a poll. `proofs/tla/side/` checks the
//! protocol and the loom models below check this code.
//!
//! A poll that finds the side held stores its waker in the side's contended
//! set, then raises `WAITING`; if that showed the side free again, it takes
//! the side itself (one more `fetch_or`), else it returns `Pending` and the
//! holder wakes it.
//!
//! # Biased to its owner
//!
//! A side made with [`Side::biased`], where the process has a heavy fence
//! ([`fence`]), starts biased to its owner, the thread running the source's
//! home shard. The owner takes the side with a plain store of `own`, the
//! compiler's fence and a plain load of `intent`: zero, it holds, with no
//! locked instruction; one, it undoes `own` and takes through the word as
//! above. It lets go with a plain store of `own := 0` and a plain load of
//! `intent`; only when that is one does it read the word with a locked
//! instruction, and finding work left there takes the side again through the
//! word and does it. Any other thread (a foreign take) raises `intent`, runs
//! the heavy fence and reads `own`: zero, the side goes to shared mode for
//! good and the thread takes through the word; one, it leaves its work for
//! the owner exactly as it would for a holder of the word. In shared mode
//! only the word is used. `proofs/tla/side_biased/` checks this protocol on
//! a machine with store buffers; where there is no heavy fence, and under
//! loom, a side starts shared and the protocol is the one above.
//!
//! Nothing that may run code of its own happens under the guard: wakers
//! are woken and buffers dropped (a dropped buffer may wake a source that
//! waits for one) only after the guard is let go, through [`After`].
//!
//! A step that unwinds while a non-owning view of a source is live (a ring
//! arrival, `ShardRing::arrived`) must not run those wakes either, for a
//! waker that re-enters the runtime could free the source under the view.
//! While [`Unwound`], declared before the view, is alive, an [`After`]
//! dropped during unwinding moves its work to a thread-local list instead,
//! and the guard runs it once the view is gone. Anywhere else an unwinding
//! step wakes at once, as always.

use core::sync::atomic::compiler_fence;
use core::task::{Poll, Waker};

use crate::buf::PoolBuf;
use crate::rt::handoff::fence;
use crate::sync::atomic_waker::{AtomicWaker, Registered};
use crate::sync::internal::fence as full_fence;
use crate::sync::internal::{AtomicBool, AtomicPtr, AtomicU8, AtomicUsize, Ordering, UnsafeCell};

/// Someone holds the side.
const BUSY: u8 = 1;
/// An edge came while the side was held: the holder does the driver's part.
const READY: u8 = 2;
/// A poll found the side held and waits in the contended set.
const WAITING: u8 = 4;
/// Side-local work waits in the deferred word.
const DEFER: u8 = 8;
/// The source has requests that need both of its sides.
const REQ: u8 = 16;

/// How often each path of the biased protocol ran, for the tests that must
/// show a race window was met: foreign calls that left work to the owner,
/// owner releases that found work and took the side again, and owner takes
/// that fell back to the word.
#[cfg(test)]
pub(crate) static WITNESS: [core::sync::atomic::AtomicUsize; 3] = [
    core::sync::atomic::AtomicUsize::new(0),
    core::sync::atomic::AtomicUsize::new(0),
    core::sync::atomic::AtomicUsize::new(0),
];

#[cfg(test)]
fn witness(path: usize) {
    WITNESS[path].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// `intent` bit: a foreign call's heavy fence failed.
const UNFENCED: u8 = 2;

/// Slots of a side's contended set: polls of more tasks than this at one
/// moment share the last slot.
const SIDE_SLOTS: usize = 4;

/// One direction of a source: its state, behind the guard.
pub(crate) struct Side<S> {
    state: AtomicU8,
    /// Side-local work left for the holder; its bits are the user's.
    deferred: AtomicU8,
    /// Wakers of polls that found the side held, made on the side's first
    /// contention.
    contended: AtomicPtr<WakerSet<SIDE_SLOTS>>,
    /// One while the owner holds the side biased.
    own: AtomicU8,
    /// One once a foreign thread came, for good.
    intent: AtomicU8,
    /// Shared mode: every take goes through `state`. Set for good.
    shared: AtomicBool,
    /// The shard the owner runs (its address), claimed by the first owner
    /// take; a take for another shard is a foreign one.
    owner: AtomicUsize,
    inner: UnsafeCell<S>,
}

// SAFETY: the contents are reached only by the holder of the guard.
unsafe impl<S: Send> Send for Side<S> {}
// SAFETY: as for `Send`: one holder at a time.
unsafe impl<S: Send> Sync for Side<S> {}

/// What a side's holder does for the work others left it, and as it takes
/// and lets go of the side.
pub(crate) trait Hooks<S> {
    /// Just after the side is taken.
    fn taken(&self, _st: &mut S, _after: &mut After) {}
    /// The driver's part, for an edge that came while the side was held.
    fn serve(&self, st: &mut S, after: &mut After);
    /// Side-local work others left: the deferred word's bits.
    fn deferred(&self, _st: &mut S, _bits: u8, _after: &mut After) {}
    /// The source's requests may be pending (each holder looks).
    fn requests(&self, _st: &mut S, _after: &mut After) {}
    /// The side was let go; `saw_requests`: the holder saw `REQ` meanwhile.
    fn released(&self, _saw_requests: bool, _after: &mut After) {}
    /// Just before each attempt to let go.
    fn releasing(&self, _st: &mut S) {}
    /// The address of the shard the calling thread runs, inside it, when the
    /// source is homed on that shard, else 0: the caller is then the owner
    /// of a biased side.
    fn owner(&self) -> usize {
        0
    }
    /// This caller just claimed the side as its owner.
    fn claimed(&self) {}
    /// Asks the owner's thread to take and let go of the side, so that work
    /// left for it is served.
    fn nudge(&self) {}
}

/// A held side. Let go through [`release`](Held::release), which does what
/// others left; a drop (a panic under the guard) only wakes the contended.
pub(crate) struct Held<'a, S> {
    side: &'a Side<S>,
    /// The word the holder took: what letting go swaps back to zero.
    expect: u8,
    /// The driver took the side with its own `READY`.
    own_ready: bool,
    /// The owner holds it biased: `own` is up, the word is not.
    biased: bool,
    /// Flags of the last take of the word still to be done (see `Held::release_word`).
    rest: u8,
    /// The holder saw `REQ` as it let go.
    saw_requests: bool,
}

/// The hooks of a caller that is nobody's owner and asks for nothing.
#[cfg(test)]
struct Nobody;

#[cfg(test)]
impl<S> Hooks<S> for Nobody {
    fn serve(&self, _: &mut S, _: &mut After) {}
}

/// What a take found.
enum Take<'a, S> {
    /// The side is held now.
    Held(Held<'a, S>),
    /// The word is held by another: the call's flags are left for its holder.
    Busy,
    /// A foreign take found the owner holding: the call's flags, if any,
    /// are left for it, and a poll still has its waker to leave.
    Left,
    /// A foreign take whose heavy fence failed: it read nothing the fence
    /// was to order. The call's flags, if any, are left and the owner was
    /// asked to take the side; a poll still has its waker to leave.
    Unserved,
}

/// How a thread takes a side.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Through the word: shared mode.
    Shared,
    /// The owner of a biased side.
    Owner,
    /// Any other thread, or the owner calling again under its own hold.
    Foreign,
}

impl Role {
    /// The role of a take after the owner's biased one failed: the owner
    /// then takes through the word.
    fn through_word(self) -> Self {
        match self {
            Role::Owner => Role::Shared,
            other => other,
        }
    }
}

impl<S> Side<S> {
    /// A side that takes through its word only.
    pub(crate) fn new(inner: S) -> Self {
        Self::with_mode(inner, true)
    }

    /// A side biased to its owner where the process has a heavy fence, else
    /// as [`new`](Self::new).
    pub(crate) fn biased(inner: S) -> Self {
        Self::with_mode(inner, !fence::available())
    }

    fn with_mode(inner: S, shared: bool) -> Self {
        Self {
            state: AtomicU8::new(0),
            deferred: AtomicU8::new(0),
            contended: AtomicPtr::new(core::ptr::null_mut()),
            own: AtomicU8::new(0),
            intent: AtomicU8::new(0),
            shared: AtomicBool::new(shared),
            owner: AtomicUsize::new(0),
            inner: UnsafeCell::new(inner),
        }
    }

    /// How the caller, whose [`Hooks::owner`] is `who`, takes the side.
    #[inline(always)]
    fn role(&self, hooks: &impl Hooks<S>) -> Role {
        // Acquire: a side the owner switched shows its `own := 0` first.
        if self.shared.load(Ordering::Acquire) {
            return Role::Shared;
        }
        let who = hooks.owner();
        let owns =
            who != 0 && (self.owner.load(Ordering::Relaxed) == who || self.claim(who, hooks));
        // Own is up only under the owner's own hold: a call under it is
        // foreign to itself, and leaves its work for the hold's release.
        if owns && self.own.load(Ordering::Relaxed) == 0 {
            Role::Owner
        } else {
            Role::Foreign
        }
    }

    /// Whether `who`, which is not the side's owner, claims the side as its owner now: only
    /// while no shard has.
    #[cold]
    #[inline(never)]
    fn claim(&self, who: usize, hooks: &impl Hooks<S>) -> bool {
        let claimed = self.owner.load(Ordering::Relaxed) == 0
            && self
                .owner
                .compare_exchange(0, who, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok();
        if claimed {
            hooks.claimed();
        }
        claimed
    }

    /// The owner's take: `own := 1`, the compiler's fence, then `intent` and
    /// the mode with plain loads. Held if no foreign thread came; else
    /// `own := 0` and the caller takes through the word.
    #[inline(always)]
    fn try_biased(&self) -> Option<Held<'_, S>> {
        self.own.store(1, Ordering::Relaxed);
        compiler_fence(Ordering::SeqCst);
        if self.intent.load(Ordering::Relaxed) == 0 && !self.shared.load(Ordering::Relaxed) {
            return Some(Held {
                side: self,
                expect: BUSY,
                own_ready: false,
                biased: true,
                rest: 0,
                saw_requests: false,
            });
        }
        self.biased_refused();
        None
    }

    /// The owner's biased take found a foreign thread came: `own := 0`.
    #[cold]
    #[inline(never)]
    fn biased_refused(&self) {
        self.own.store(0, Ordering::Relaxed);
        self.switch_if_unfenced();
        #[cfg(test)]
        witness(2);
    }

    /// The side, if it was free, taken with one `fetch_or` that also raises
    /// `extra`; if it was held, `extra` is left for its holder.
    fn take_word(&self, extra: u8) -> Take<'_, S> {
        // AcqRel: acquires what the last holder published, and publishes
        // what this thread left (its deferred bits, its contended waker)
        // to the holder.
        let prev = self.state.fetch_or(BUSY | extra, Ordering::AcqRel);
        // Built only on success: a `Held` dropped unused would let go of a
        // side another thread holds.
        if prev & BUSY == 0 {
            Take::Held(Held {
                side: self,
                expect: BUSY | (extra & READY),
                own_ready: extra & READY != 0,
                biased: false,
                rest: 0,
                saw_requests: false,
            })
        } else {
            Take::Busy
        }
    }

    /// A foreign take of a biased side: `intent` up, the heavy fence, then
    /// `own`. Zero, the side goes to shared mode and is taken through the
    /// word; one, `extra` is left for the owner.
    fn take_foreign(&self, extra: u8, hooks: &impl Hooks<S>) -> Take<'_, S> {
        self.intent.fetch_or(1, Ordering::SeqCst);
        if !fence::heavy_retried() {
            return self.unserved(extra, hooks);
        }
        if self.own.load(Ordering::Acquire) == 0 {
            self.shared.store(true, Ordering::Relaxed);
            return self.take_word(extra);
        }
        if extra != 0 {
            let prev = self.state.fetch_or(extra, Ordering::AcqRel);
            if prev & BUSY == 0 && self.own.load(Ordering::Acquire) == 0 {
                self.shared.store(true, Ordering::Relaxed);
                return self.take_word(extra);
            }
        }
        #[cfg(test)]
        witness(0);
        Take::Left
    }

    /// A foreign call whose heavy fence failed (`intent` is up): `own` is not
    /// read, since only the fence orders it. With no owner claimed, none can
    /// hold biased (a later claimant sees `intent`), so the side goes shared
    /// and is taken through the word. Otherwise the call's flag is left and
    /// the owner, which sees `intent` at its next release, is asked to take
    /// the side and let go of it. A claimant whose notice is not in yet has
    /// not held either, and takes through the word after it sees `intent`.
    fn unserved(&self, extra: u8, hooks: &impl Hooks<S>) -> Take<'_, S> {
        full_fence(Ordering::SeqCst);
        if self.owner.load(Ordering::SeqCst) == 0 {
            self.shared.store(true, Ordering::Relaxed);
            return self.take_word(extra);
        }
        self.intent.fetch_or(UNFENCED, Ordering::SeqCst);
        if extra != 0 {
            self.state.fetch_or(extra, Ordering::AcqRel);
            hooks.nudge();
        }
        Take::Unserved
    }

    /// The side, taken as `role` does, raising `extra` as the word's take
    /// does.
    #[inline(always)]
    fn take_as(&self, role: Role, extra: u8, hooks: &impl Hooks<S>) -> Take<'_, S> {
        match role {
            Role::Shared => self.take_word(extra),
            Role::Owner => match self.try_biased() {
                Some(held) => Take::Held(held),
                None => self.take_word(extra),
            },
            Role::Foreign => self.take_foreign(extra, hooks),
        }
    }

    #[inline]
    fn take(&self, extra: u8, hooks: &impl Hooks<S>) -> Option<Held<'_, S>> {
        match self.take_as(self.role(hooks), extra, hooks) {
            Take::Held(held) => Some(held),
            Take::Busy | Take::Left | Take::Unserved => None,
        }
    }

    /// The side, if it is free, taken as a thread that is not its owner.
    #[cfg(test)]
    pub(crate) fn try_hold(&self) -> Option<Held<'_, S>> {
        self.take(0, &Nobody)
    }

    /// The side, if it is free, taken as `hooks`' thread takes it.
    #[cfg(completion)]
    #[inline]
    pub(crate) fn try_hold_for(&self, hooks: &impl Hooks<S>) -> Option<Held<'_, S>> {
        self.take(0, hooks)
    }

    /// Whether the side went to shared mode.
    #[cfg(test)]
    pub(crate) fn is_shared(&self) -> bool {
        self.shared.load(Ordering::SeqCst)
    }

    /// The guard's word and the deferred bits, for a test that failed.
    #[cfg(test)]
    pub(crate) fn words(&self) -> (u8, u8) {
        (
            self.state.load(Ordering::SeqCst),
            self.deferred.load(Ordering::SeqCst),
        )
    }

    /// For a request that needs this side and another: the side, if it was
    /// free; if it was held, its holder sees `REQ` and looks at the requests.
    pub(crate) fn take_for_request(&self, hooks: &impl Hooks<S>) -> Option<Held<'_, S>> {
        self.take(REQ, hooks)
    }

    /// Runs one step of a poll under the guard. A poll that finds the side
    /// held registers `waker` and returns `Pending`: the holder wakes it as
    /// it lets go. When an edge came while the step ran, the driver's part
    /// runs here, then the step again if it was pending.
    #[inline(always)]
    pub(crate) fn poll<R>(
        &self,
        after: &mut After,
        waker: &Waker,
        hooks: &impl Hooks<S>,
        mut step: impl FnMut(&mut S, &mut After) -> Poll<R>,
    ) -> Poll<R> {
        let mut held = match self.take_as(self.role(hooks), 0, hooks) {
            Take::Held(held) => held,
            missed => match self.poll_missed(missed, waker, hooks) {
                Some(held) => held,
                None => return Poll::Pending,
            },
        };
        hooks.taken(held.get(), after);
        // One call of `step`, so it is inlined here: it runs first, and again after each
        // driver's part while it is pending.
        let mut out = Poll::Pending;
        loop {
            if out.is_pending() {
                out = step(held.get(), after);
            }
            match held.release_or_again(after, hooks) {
                Ok(()) => return out,
                Err(again) => held = again,
            }
        }
    }

    /// A poll's take that did not hold the side: its waker is left as `take` asks, or the side
    /// is taken after all.
    #[cold]
    #[inline(never)]
    fn poll_missed<'a>(
        &'a self,
        take: Take<'a, S>,
        waker: &Waker,
        hooks: &impl Hooks<S>,
    ) -> Option<Held<'a, S>> {
        match take {
            Take::Held(held) => Some(held),
            Take::Busy => self.contend(waker, false),
            Take::Left => self.contend(waker, true),
            Take::Unserved => {
                self.leave_waker(waker, hooks);
                None
            }
        }
    }

    /// For a poll that found the side held: its waker goes to the contended
    /// set for the holder to wake, or, if the side came free meanwhile, the
    /// side is taken after all. `left`: the poll found a biased owner
    /// holding, so the side is free only if `own` says so on a second look.
    fn contend(&self, waker: &Waker, left: bool) -> Option<Held<'_, S>> {
        if !self.contended().register(waker) {
            // The holder is taking the contended wakers right now, so it is
            // about to let go, and `waker` was woken already: this poll ends
            // and the next one finds the side free, with no flag that would
            // send the holder round again.
            return None;
        }
        // AcqRel: publishes the registration to the holder that takes the
        // flag, and acquires the release of a holder that let go first.
        if self.state.fetch_or(WAITING, Ordering::AcqRel) & BUSY != 0 {
            return None;
        }
        if left {
            if self.own.load(Ordering::Acquire) != 0 {
                return None;
            }
            self.shared.store(true, Ordering::Relaxed);
        }
        match self.take_word(0) {
            Take::Held(held) => Some(held),
            Take::Busy | Take::Left | Take::Unserved => None,
        }
    }

    /// A poll whose foreign take got no fence: its waker and the waiting bit
    /// are left, then the owner is asked to take the side and wake it.
    fn leave_waker(&self, waker: &Waker, hooks: &impl Hooks<S>) {
        // A false register woke the waker already: the poll comes again.
        if self.contended().register(waker) {
            self.state.fetch_or(WAITING, Ordering::AcqRel);
            hooks.nudge();
        }
    }

    /// The driver's part for an edge, after side-local work `defer` (say,
    /// urgent data came): done now if the side is free, else by its holder
    /// as it lets go.
    #[cfg(readiness)]
    #[inline]
    pub(crate) fn drive(&self, after: &mut After, hooks: &impl Hooks<S>, defer: u8) {
        let role = self.role(hooks);
        let (mut held, bits) = match self.hold_biased(role) {
            // Held biased: the bits go straight to the hook.
            Some(held) => (held, defer),
            None => {
                if defer != 0 {
                    // Release: the holder that takes DEFER reads the bits.
                    self.deferred.fetch_or(defer, Ordering::Release);
                }
                let extra = if defer != 0 { READY | DEFER } else { READY };
                match self.take_as(role.through_word(), extra, hooks) {
                    Take::Held(held) => (held, 0),
                    Take::Busy | Take::Left | Take::Unserved => return,
                }
            }
        };
        hooks.taken(held.get(), after);
        held.run_deferred(bits, hooks, after);
        hooks.serve(held.get(), after);
        held.release(after, hooks);
    }

    /// Leaves side-local work `bits` for whoever holds the side: done now if
    /// the side is free, else by its holder as it lets go.
    pub(crate) fn defer(&self, bits: u8, after: &mut After, hooks: &impl Hooks<S>) {
        let role = self.role(hooks);
        let (mut held, direct) = match self.hold_biased(role) {
            // Held biased: the bits go straight to the hook, with no trip
            // through the deferred word.
            Some(held) => (held, bits),
            None => {
                // Release: the holder that takes DEFER reads the bits.
                self.deferred.fetch_or(bits, Ordering::Release);
                match self.take_as(role.through_word(), DEFER, hooks) {
                    Take::Held(held) => (held, 0),
                    Take::Busy | Take::Left | Take::Unserved => return,
                }
            }
        };
        hooks.taken(held.get(), after);
        held.run_deferred(direct, hooks, after);
        held.release(after, hooks);
    }

    /// The biased hold, if `role` is the owner's and no foreign thread came.
    fn hold_biased(&self, role: Role) -> Option<Held<'_, S>> {
        if role == Role::Owner {
            self.try_biased()
        } else {
            None
        }
    }

    /// The contents, through exclusive access: nobody else can hold it.
    pub(crate) fn get_mut(&mut self) -> &mut S {
        self.inner.with_mut(|p| {
            // SAFETY: `&mut self` excludes every holder.
            unsafe { &mut *p }
        })
    }

    /// The owner, holding nothing (`own` is down), switches a side to shared
    /// mode when a foreign thread left work with no fence (`intent` bit 2):
    /// that thread can never take a biased side, and needs the word.
    fn switch_if_unfenced(&self) {
        if self.intent.load(Ordering::Relaxed) & UNFENCED != 0 {
            // Release: after `own := 0`, for whoever reads the mode.
            self.shared.store(true, Ordering::Release);
        }
    }

    /// The owner lets go of its biased hold: `own := 0`, then a plain look
    /// at `intent`. Only when a foreign thread came is the word read, with a
    /// locked instruction, which puts `own := 0` ahead of the read. True when
    /// work was left there and the side was free: the caller holds it through
    /// the word then, and does that work.
    #[inline(always)]
    fn release_biased(&self) -> bool {
        // Release: the next holder sees what this one did.
        self.own.store(0, Ordering::Release);
        compiler_fence(Ordering::SeqCst);
        if self.intent.load(Ordering::Relaxed) == 0 {
            return false;
        }
        self.release_biased_came()
    }

    /// [`release_biased`](Self::release_biased) once a foreign thread came.
    #[cold]
    #[inline(never)]
    fn release_biased_came(&self) -> bool {
        self.switch_if_unfenced();
        let left = self.state.fetch_or(0, Ordering::SeqCst);
        if left & (WAITING | READY | DEFER | REQ) == 0 {
            return false;
        }
        let held = self.state.fetch_or(BUSY, Ordering::AcqRel) & BUSY == 0;
        #[cfg(test)]
        witness(1);
        held
    }

    /// The contended set, made on first use.
    fn contended(&self) -> &WakerSet<SIDE_SLOTS> {
        let mut set = self.contended.load(Ordering::Acquire);
        if set.is_null() {
            let made = Box::into_raw(Box::new(WakerSet::new()));
            set = match self.contended.compare_exchange(
                core::ptr::null_mut(),
                made,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => made,
                Err(theirs) => {
                    // SAFETY: `made` was never published.
                    drop(unsafe { Box::from_raw(made) });
                    theirs
                }
            };
        }
        // SAFETY: once published, the set lives as long as the side.
        unsafe { &*set }
    }

    /// Wakes every poll waiting in the contended set.
    fn wake_contended(&self, after: &mut After) {
        let set = self.contended.load(Ordering::Acquire);
        // SAFETY: as in `contended`.
        if let Some(set) = unsafe { set.as_ref() } {
            set.take_all(after);
        }
    }
}

impl<S> Drop for Side<S> {
    fn drop(&mut self) {
        let set = crate::sync::internal::take_ptr(&mut self.contended);
        if !set.is_null() {
            // SAFETY: made by `Box::into_raw` and owned by the side alone.
            drop(unsafe { Box::from_raw(set) });
        }
    }
}

impl<S> Held<'_, S> {
    pub(crate) fn get(&mut self) -> &mut S {
        self.side.inner.with_mut(|p| {
            // SAFETY: BUSY is ours: nobody else reaches the contents.
            unsafe { &mut *p }
        })
    }

    /// Runs the side-local work left in the deferred word, and `direct`, the
    /// bits of the holder's own call.
    fn run_deferred(&mut self, direct: u8, hooks: &impl Hooks<S>, after: &mut After) {
        let mut bits = direct;
        // A plain look first: a locked swap only when somebody left bits.
        if self.side.deferred.load(Ordering::Relaxed) != 0 {
            // Acquire: pairs with the Release of whoever left the bits.
            bits |= self.side.deferred.swap(0, Ordering::Acquire);
        }
        if bits != 0 {
            hooks.deferred(self.get(), bits, after);
        }
    }

    /// Lets go of the side, first doing what others left for its holder.
    #[inline]
    pub(crate) fn release(mut self, after: &mut After, hooks: &impl Hooks<S>) {
        while let Err(again) = self.release_or_again(after, hooks) {
            self = again;
        }
    }

    /// Lets go of the side, first doing what others left for its holder; `Err` gives the side
    /// back, still held, just after the driver's part ran for an edge that came meanwhile, for
    /// the caller to run its step again (a poll's, while it waits) before it lets go again. The
    /// owner's biased release, which finds nothing left, is the inlined part.
    #[inline(always)]
    fn release_or_again(mut self, after: &mut After, hooks: &impl Hooks<S>) -> Result<(), Self> {
        if self.biased {
            hooks.releasing(self.get());
            if !self.side.release_biased() {
                core::mem::forget(self);
                hooks.released(false, after);
                return Ok(());
            }
            // Work was left: the side is held through the word now, and
            // done as a holder of the word does it.
            self.biased = false;
            self.expect = BUSY;
        }
        self.release_word(after, hooks)
    }

    /// The release of a side held through the word. An edge's driver's part gives the side
    /// back to the caller at once (see [`release_or_again`](Self::release_or_again)); the rest
    /// of the flags taken with it are done as the release goes on.
    #[inline(never)]
    fn release_word(mut self, after: &mut After, hooks: &impl Hooks<S>) -> Result<(), Self> {
        self.finish_flags(after, hooks);
        loop {
            hooks.releasing(self.get());
            // Release: publishes the contents to the next holder.
            if self
                .side
                .state
                .compare_exchange(self.expect, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                break;
            }
            // Flags others left meanwhile: taken, the side still held.
            let mut flags = self.side.state.swap(BUSY, Ordering::AcqRel) & !BUSY;
            if core::mem::take(&mut self.own_ready) {
                // Only the driver raises READY, and it is the holder.
                flags &= !READY;
            }
            self.expect = BUSY;
            if flags & DEFER != 0 {
                self.run_deferred(0, hooks, after);
            }
            self.rest = flags & (WAITING | REQ);
            if flags & READY != 0 {
                hooks.serve(self.get(), after);
                return Err(self);
            }
            self.finish_flags(after, hooks);
        }
        let saw_requests = self.saw_requests;
        core::mem::forget(self);
        hooks.released(saw_requests, after);
        Ok(())
    }

    /// The flags of the last take of the word that come after the driver's part: the contended
    /// polls are woken, and the source's requests looked at.
    fn finish_flags(&mut self, after: &mut After, hooks: &impl Hooks<S>) {
        let flags = core::mem::take(&mut self.rest);
        if flags & WAITING != 0 {
            self.side.wake_contended(after);
        }
        if flags & REQ != 0 {
            self.saw_requests = true;
            hooks.requests(self.get(), after);
        }
    }
}

impl<S> Drop for Held<'_, S> {
    /// Only a panic under the guard gets here: the side goes free, and the
    /// polls waiting on it are woken to look again.
    fn drop(&mut self) {
        if self.biased {
            // Own goes down; work others left stays in the word for the next
            // holder, and the polls waiting are woken to look again.
            self.side.own.store(0, Ordering::Relaxed);
            compiler_fence(Ordering::SeqCst);
            if self.side.intent.load(Ordering::Relaxed) != 0
                && self.side.state.fetch_or(0, Ordering::SeqCst) & WAITING != 0
            {
                let mut after = After::default();
                self.side.wake_contended(&mut after);
            }
            return;
        }
        let flags = self.side.state.swap(0, Ordering::AcqRel);
        if flags & WAITING != 0 {
            // Dropped, not run: during unwinding the wakes are parked.
            let mut after = After::default();
            self.side.wake_contended(&mut after);
        }
    }
}

/// Wakers of tasks waiting for one thing, registered and taken without a
/// lock, every operation in a bounded number of steps.
///
/// A task claims a slot with its tag (its waker's data pointer) in one
/// compare-and-swap, and registers there again on later polls. When every
/// slot holds another task, the last slot is shared: a registration there
/// wakes the waker it displaces, so no task loses its wake, at the cost of
/// a spurious one.
pub(crate) struct WakerSet<const N: usize> {
    tags: [AtomicUsize; N],
    wakers: [AtomicWaker; N],
}

impl<const N: usize> WakerSet<N> {
    pub(crate) fn new() -> Self {
        Self {
            tags: core::array::from_fn(|_| AtomicUsize::new(0)),
            wakers: core::array::from_fn(|_| AtomicWaker::new()),
        }
    }

    /// Registers `waker` to be woken by the next [`take_all`](Self::take_all).
    ///
    /// The task's own slot first, then a free one; a slot another wake or
    /// registration is using is passed over, and since a take works on one
    /// slot at a time another always serves. Only when every slot holds
    /// another task does the last one take `waker`, waking the one it
    /// displaces. Returns false when `waker` was woken at once instead of
    /// kept: a take met the registration (the taker is waking this set's
    /// tasks now), or the last slot was in use.
    #[inline]
    pub(crate) fn register(&self, waker: &Waker) -> bool {
        // Never zero, the empty slot's tag.
        let tag = waker.data() as usize | 1;
        let own = (0..N).filter(|&i| self.tags[i].load(Ordering::Acquire) == tag);
        let free = (0..N).filter(|&i| {
            self.tags[i]
                .compare_exchange(0, tag, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        });
        for i in own.chain(free) {
            match self.wakers[i].try_register_displacing(waker) {
                Registered::Kept(_) => return true,
                // A take of this slot met the registration and woke it.
                Registered::WokenNow => return false,
                Registered::InUse => {}
            }
        }
        self.wakers[N - 1].register_displacing(waker)
    }

    /// Takes every registered waker, for `after` to wake, and frees the
    /// slots.
    #[inline]
    pub(crate) fn take_all(&self, after: &mut After) {
        for (tag, waker) in self.tags.iter().zip(&self.wakers) {
            if let Some(w) = waker.take() {
                after.wake_one(w);
            }
            // Release: a slot claimed after this sees the waker taken.
            tag.store(0, Ordering::Release);
        }
    }
}

/// The error of a whole datagram larger than any buffer the thread's pool
/// may hold, or `None` when it could get one.
pub(crate) fn whole_too_large(
    known: Option<&crate::buf::Pool>,
    len: usize,
) -> Option<std::io::Error> {
    crate::buf::with_pool_or(known, |pool| {
        pool.filter(|p| !p.could_hold_whole(len)).map(|p| {
            std::io::Error::new(
                std::io::ErrorKind::OutOfMemory,
                format!(
                    "a datagram of {len} bytes needs more IO buffer memory than the runtime allows ({} bytes); raise `Builder::io_buffer_cap`",
                    p.room()
                ),
            )
        })
    })
}

/// What a step leaves for once no guard is held: wakers to wake, wakers and
/// buffers to drop (either may run code that reaches a source), and what
/// the source must do next.
#[derive(Default)]
pub(crate) struct After {
    /// The first waker to wake: almost every step wakes one task at most.
    wake: Option<Waker>,
    /// The first buffer to drop: almost every step drops one at most.
    drop_one: Option<PoolBuf>,
    more: Option<Box<More>>,
    /// A side found the pool empty and must hear when a buffer comes back.
    pub(crate) starved: bool,
    /// Work was left to the driver: the source must be registered.
    pub(crate) register: bool,
    /// A side of a ring source wants an operation the ring's thread, the
    /// one of this shard, must submit.
    #[cfg(ring)]
    pub(crate) kick: Option<std::sync::Arc<crate::rt::shard::Shared>>,
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
        self.more.get_or_insert_default()
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
        match self.wake {
            None => self.wake = Some(waker),
            Some(_) => self.wake_more(waker),
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
        match self.drop_one {
            None => self.drop_one = Some(buf),
            Some(_) => self.more().bufs.push(buf),
        }
    }

    /// Wakes and drops; call with no guard held. Once: it consumes the step,
    /// so its drop finds nothing left to do.
    #[inline]
    pub(crate) fn run(mut self) {
        self.flush();
        #[cfg(ring)]
        drop(self.kick.take());
        core::mem::forget(self);
    }

    #[inline]
    fn flush(&mut self) {
        drop(self.drop_one.take());
        match self.more.take() {
            None => {
                if let Some(w) = self.wake.take() {
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
        if let Some(w) = self.wake.take() {
            w.wake();
        }
        wake.into_iter().for_each(Waker::wake);
    }
}

impl Drop for After {
    fn drop(&mut self) {
        if parking() {
            self.park();
        } else {
            self.flush();
        }
    }
}

impl After {
    /// Consumes a step that left nothing to do, without the work of settling it: `Err` gives
    /// back one that did.
    #[inline(always)]
    pub(crate) fn forget_if_nothing(self) -> Result<(), Self> {
        if self.is_empty() && !self.starved && !self.register {
            // Nothing is owned: no waker, no buffer, no allocation, so there is nothing to drop.
            core::mem::forget(self);
            Ok(())
        } else {
            Err(self)
        }
    }

    #[inline(always)]
    fn is_empty(&self) -> bool {
        self.wake.is_none() && self.drop_one.is_none() && self.more.is_none() && !self.has_kick()
    }

    #[cfg(ring)]
    fn has_kick(&self) -> bool {
        self.kick.is_some()
    }

    #[cfg(not(ring))]
    fn has_kick(&self) -> bool {
        false
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
        }
    }
}

/// Whether an `After` dropped now is parked: the thread is unwinding with an
/// [`Unwound`] guard alive.
#[cfg(any(ring, test))]
fn parking() -> bool {
    VIEW_LIVE.get() && std::thread::panicking()
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
            outer: VIEW_LIVE.replace(true),
        }
    }
}

#[cfg(any(ring, test))]
impl Drop for Unwound {
    fn drop(&mut self) {
        VIEW_LIVE.set(self.outer);
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

#[cfg(all(test, not(loom)))]
mod biased_tests;

#[cfg(all(test, loom))]
mod loom_tests;
