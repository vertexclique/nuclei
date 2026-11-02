//! What a thread knows about the runtime it runs: the shard it is running
//! now, its own shard (a thread that is not a worker gets one when it first
//! needs it), the runtime `spawn` places tasks on, and the state of the
//! thread as its shard's runner.
//!
//! # The runner
//!
//! A thread runs its shard's own code (the loop, or a call into the runtime
//! from a task) *inside* the shard, and a task's code (or a `block_on`
//! future's) *outside* it; only inside does it touch the shard's cells. A
//! thread that runs a worker shard the monitor watches has a slot
//! (`rt::handoff`): each poll it starts leaves the shard in a new epoch, and
//! each poll's end, and each call into the runtime from the task's code (a
//! bracket, in [`current`], whichever path of the runtime makes it), comes
//! back in, unless the monitor took the shard meanwhile; a call goes out
//! again as it ends. A thread whose shard was taken is detached from it:
//! from then on it runs no shard, and its calls take the paths of any other
//! thread (proofs/tla/handoff).

use core::cell::{Cell, RefCell};
use std::io;
use std::rc::Rc;
use std::sync::Arc;

use super::handoff::home::Home;
use super::handoff::slot::{Back, Slot};
use super::runtime::Inner;
use super::shard::{Limits, Shard, Shared};
use super::task::{Runnable, ScheduleInfo};

/// The nesting a thread's own shard allows unless its runtime says
/// otherwise.
pub(crate) const DEFAULT_MAX_NESTING: usize = 256;

/// What this thread runs now, as only this thread sees it. No destructor: it
/// is there for the whole life of the thread.
pub(crate) struct Here {
    /// The shard this thread runs: a worker's, or the thread's own while it
    /// runs `block_on`.
    current: Cell<*const Shard>,
    /// A task's poll (or a `block_on` future's) is on this thread's stack: a
    /// wake it makes goes to the LIFO slot.
    in_task: Cell<bool>,
    /// The thread's slot while it runs a task's code (or a `block_on`
    /// future's) on a shard the monitor watches, with no call into the
    /// runtime under way: such a call is a bracket. Null inside the shard, in
    /// a call, and on a shard nobody watches, whose calls need none.
    out: Cell<*const Slot>,
    /// The nesting of `block_on` on this thread.
    depth: Cell<usize>,
    /// The thread's slot while it runs a worker shard the monitor watches,
    /// null otherwise; kept alive by the loop that entered the shard.
    slot: Cell<*const Slot>,
    /// The epoch of the last poll begun.
    epochs: Cell<u64>,
    /// The word of the poll a call marked by [`mark_call`] found, which [`unmark_call`] puts
    /// back.
    saved: Cell<u64>,
    /// The address of the `Shared` of the shard this thread was detached
    /// from, compared, never followed; 0 for none.
    detached: Cell<usize>,
    /// The home of this thread's bound tasks, kept alive by `HOME`; null
    /// for none yet.
    home: Cell<*const Home>,
    /// The id of the task this thread is polling, if any (`task::current_id`).
    pub(crate) task: Cell<Option<super::task::Id>>,
    /// The budget left in this thread's poll; `coop::UNBUDGETED` outside a
    /// poll, or in an unconstrained one (`coop`).
    pub(crate) budget: Cell<u64>,
    /// The runtime `RUNTIME` holds, null for none: read in place, never
    /// followed once `RUNTIME` lets go of it.
    runtime: Cell<*const Inner>,
    /// The shard `OWN` holds, null for none yet or once it went.
    own: Cell<*const Shard>,
    /// The shard this thread lets go of as its runner (`letting_go`), null for none.
    letting_go: Cell<*const Arc<Shared>>,
}

impl Here {
    const fn new() -> Self {
        Self {
            current: Cell::new(core::ptr::null()),
            in_task: Cell::new(false),
            out: Cell::new(core::ptr::null()),
            depth: Cell::new(0),
            slot: Cell::new(core::ptr::null()),
            epochs: Cell::new(0),
            saved: Cell::new(0),
            detached: Cell::new(0),
            home: Cell::new(core::ptr::null()),
            task: Cell::new(None),
            budget: Cell::new(crate::coop::UNBUDGETED),
            runtime: Cell::new(core::ptr::null()),
            own: Cell::new(core::ptr::null()),
            letting_go: Cell::new(core::ptr::null()),
        }
    }

    /// The thread found its shard taken: it runs no shard from now on.
    #[cold]
    fn detach(&self) {
        // SAFETY: `current` points at a shard while its `Entered` lives.
        if let Some(shard) = unsafe { self.current.get().as_ref() } {
            self.detached.set(shard.shared_addr());
        }
        self.current.set(core::ptr::null());
        self.slot.set(core::ptr::null());
        self.out.set(core::ptr::null());
    }
}

thread_local! {
    static HERE: Here = const { Here::new() };
    /// This thread's own shard, made on first need, dropped (with every task
    /// still queued on it) when the thread exits.
    static OWN: OwnCell = const { OwnCell(RefCell::new(None)) };
    /// The runtime `spawn` uses here: the worker's, or the one whose
    /// `block_on` (or `enter`) this thread is inside.
    static RUNTIME: RuntimeCell = const { RuntimeCell(Cell::new(None)) };
    /// The home of this thread's bound tasks (see `Here::home`).
    static HOME: HomeCell = const { HomeCell(RefCell::new(None)) };
}

/// The thread's runtime, which `Here::runtime` points at; it points at none
/// once the cell goes.
struct RuntimeCell(Cell<Option<Arc<Inner>>>);

impl RuntimeCell {
    /// Sets `runtime`, mirrored in `Here::runtime`, and answers the one before.
    fn replace(&self, runtime: Option<Arc<Inner>>) -> Option<Arc<Inner>> {
        local()
            .runtime
            .set(runtime.as_ref().map_or(core::ptr::null(), Arc::as_ptr));
        self.0.replace(runtime)
    }
}

impl Drop for RuntimeCell {
    fn drop(&mut self) {
        local().runtime.set(core::ptr::null());
    }
}

/// The thread's own shard, which `Here::own` points at; it points at none
/// once the cell goes.
struct OwnCell(RefCell<Option<Rc<Shard>>>);

impl Drop for OwnCell {
    fn drop(&mut self) {
        local().own.set(core::ptr::null());
    }
}

/// The thread's home; `Here::home` points at it no more once it goes.
struct HomeCell(RefCell<Option<Arc<Home>>>);

impl Drop for HomeCell {
    fn drop(&mut self) {
        local().home.set(core::ptr::null());
    }
}

/// This thread's [`Here`]: one access of the thread-local, whose work then
/// runs in the caller.
#[inline(always)]
pub(crate) fn local() -> &'static Here {
    // SAFETY: `HERE` has a constant initializer and no destructor, so it is
    // there for the whole life of the thread; `Here` is not `Sync`, so the
    // reference never leaves it.
    HERE.with(|here| unsafe { &*core::ptr::from_ref(here) })
}

/// A call into the runtime from a task's code on a shard the monitor
/// watches: inside the shard until it drops, then out again with the poll's
/// word.
struct Bracket {
    slot: &'static Slot,
    word: u64,
}

impl Bracket {
    /// Comes back into the shard from `here`, whose task's code runs outside
    /// it with the slot `slot`: `None` when the shard was taken, and the
    /// thread is detached from it.
    #[inline(always)]
    fn take(here: &'static Here, slot: *const Slot) -> Option<Self> {
        // SAFETY: the slot outlives the loop that set it (see `Here::slot`),
        // and so every call its polls make.
        let slot = unsafe { &*slot };
        match slot.mark() {
            (word, Back::In) => {
                here.out.set(core::ptr::null());
                Some(Self { slot, word })
            }
            (_, Back::Taken) => {
                here.detach();
                None
            }
        }
    }
}

impl Drop for Bracket {
    #[inline(always)]
    fn drop(&mut self) {
        // A thread detached in the call (a nested `block_on`'s poll found the
        // shard taken) marks its slot again at its next call, finds it taken
        // for good and runs no shard.
        local().out.set(self.slot);
        self.slot.unmark(self.word);
    }
}

/// Runs `f` with the shard this thread is running, if any. Called from a
/// task's code on a shard the monitor watches, it comes back into the shard
/// first (a bracket), and finds none if the shard was taken. Every path of
/// the runtime that touches the shard it runs comes here, the owner's fast
/// paths too.
#[inline(always)]
pub(crate) fn current<R>(f: impl FnOnce(Option<&Shard>) -> R) -> R {
    let held = current_held();
    f(held.shard())
}

/// The shard this thread is running, as [`current`] finds it, held for as long as the result
/// lives (its bracket, if it made one, ends as it drops). One call of a caller's closure
/// follows, so a step that runs in it is inlined there.
#[inline(always)]
pub(crate) fn current_held() -> Current {
    let here = local();
    let out = here.out.get();
    if out.is_null() {
        return Current {
            shard: here.current.get(),
            _bracket: None,
        };
    }
    bracketed()
}

/// The shard this thread is running, in a bracket held for as long as the result lives, for a
/// caller [`unbracketed`] answered [`Bracketed`]: what [`current_held`] finds there.
#[inline(always)]
pub(crate) fn bracketed() -> Current {
    let here = local();
    let out = here.out.get();
    debug_assert!(!out.is_null(), "a bracket where none is needed");
    let bracket = Bracket::take(here, out);
    Current {
        shard: if bracket.is_some() {
            here.current.get()
        } else {
            core::ptr::null()
        },
        _bracket: bracket,
    }
}

/// The shard this thread runs, if any, when a call into it from here needs no bracket: the
/// thread is inside it, or no monitor watches it. [`Bracketed`] from a task's code on a shard
/// the monitor watches, whose call goes through [`bracketed`] instead; a hot path makes that
/// call out of line, so what its bracket holds costs the path that needs none nothing.
#[inline(always)]
pub(crate) fn unbracketed(here: &Here) -> Result<Option<&Shard>, Bracketed> {
    if here.out.get().is_null() {
        // SAFETY: as in `Current::shard`.
        Ok(unsafe { here.current.get().as_ref() })
    } else {
        Err(Bracketed)
    }
}

/// The shard this thread runs, when a call into it from here needs no bracket: what
/// [`unbracketed`] finds, `None` where it answers [`Bracketed`] too, for a caller whose way
/// for no shard here (out of line) takes the bracket.
#[inline(always)]
pub(crate) fn running_inside(here: &Here) -> Option<&Shard> {
    unbracketed(here).ok().flatten()
}

/// What [`unbracketed`] answers where a call into the shard needs a bracket.
pub(crate) struct Bracketed;

/// The shard this thread runs, if any, for a caller that marks its call itself
/// ([`mark_call`]) before it touches anything of the shard but its fixed parts.
#[inline(always)]
pub(crate) fn running(here: &Here) -> Option<&Shard> {
    // SAFETY: as in `Current::shard`.
    unsafe { here.current.get().as_ref() }
}

/// What [`mark_call`] did.
pub(crate) enum Marked {
    /// No mark was needed: the thread is inside its shard, or nobody watches it.
    Not,
    /// The call is marked, a bracket that [`unmark_call`] ends.
    In,
    /// The shard was taken: the thread is detached from it and runs none.
    Taken,
}

/// Marks a call into the shard this thread runs from a task's code on a shard the monitor
/// watches, as a bracket does, for a caller that keeps the mark with what it settles last (an
/// IO step's `After`) and ends it there with [`unmark_call`]: no guard is held meanwhile, and a
/// thread that needs no mark pays one look at a word. Nothing in the call may mark again or
/// poll (`out` is null meanwhile, as in a bracket). For a caller that found the thread running
/// its shard ([`running`]), so not detached from it: `out` is then its slot.
#[inline(always)]
pub(crate) fn mark_call() -> Marked {
    let here = local();
    if here.out.get().is_null() {
        return Marked::Not;
    }
    debug_assert!(core::ptr::eq(here.out.get(), here.slot.get()));
    // SAFETY: as in `Bracket::take`; the slot is `out`, read again where the path that needs
    // no mark does not keep it.
    let slot = unsafe { &*here.slot.get() };
    match slot.mark() {
        (word, Back::In) => {
            here.saved.set(word);
            here.out.set(core::ptr::null());
            Marked::In
        }
        (_, Back::Taken) => {
            detach_here();
            Marked::Taken
        }
    }
}

/// Pushes `runnable` into the empty LIFO slot of the shard this thread runs, when `ours` says
/// the task is that shard's, from a task's code on a shard the monitor watches (`out` is set),
/// as a leaf: the call marked with no contest of a claim (`Slot::mark_unless_claimed`), and
/// nothing of a bracket's but its mark. `Err` gives `runnable` back, the poll's word put back,
/// when a claim is up or the push is another (the slot holds a task, the task is another
/// shard's, it woke itself), for the full bracket (proofs/tla/handoff, the leaf push).
#[inline(always)]
pub(crate) fn push_lifo_marked<O>(
    runnable: Runnable,
    info: ScheduleInfo,
    ours: O,
) -> Result<(), Runnable>
where
    O: FnOnce(&Shard) -> bool,
{
    let here = local();
    debug_assert!(core::ptr::eq(here.out.get(), here.slot.get()));
    // SAFETY: as in `Bracket::take`.
    let slot = unsafe { &*here.out.get() };
    let Some(word) = slot.mark_unless_claimed() else {
        return Err(runnable);
    };
    let pushed = match running(here) {
        Some(current) if ours(current) => {
            current.push_lifo_alone(runnable, info, here.in_task.get())
        }
        _ => Err(runnable),
    };
    slot.unmark(word);
    pushed
}

/// [`Here::detach`] of this thread, out of line: a caller that reaches it only rarely keeps no
/// pointer to its `Here` for it (its other looks go to the thread's own storage directly).
#[cold]
#[inline(never)]
fn detach_here() {
    local().detach();
}

/// Ends the call [`mark_call`] marked: the poll's word back, outside the shard again.
#[inline(always)]
pub(crate) fn unmark_call() {
    let here = local();
    let slot = here.slot.get();
    here.out.set(slot);
    // SAFETY: as in `Bracket::take`; the call ran no poll, so the thread still runs the shard
    // whose slot this is.
    unsafe { &*slot }.unmark(here.saved.get());
}

/// What [`current_held`] found: the shard, while it lives.
pub(crate) struct Current {
    shard: *const Shard,
    _bracket: Option<Bracket>,
}

impl Current {
    /// The shard this thread is running, if any.
    #[inline(always)]
    pub(crate) fn shard(&self) -> Option<&Shard> {
        // SAFETY: `current` points at a shard only while an `Entered` guard
        // for it lives, and the guard borrows the shard; a taken shard is
        // current no more. The borrow ends with `self`, inside the call that made it.
        unsafe { self.shard.as_ref() }
    }
}

/// Marks the shard of `shared` as the one this thread lets go of, as the only thread that
/// runs it, while `f` runs: as it drops, its sources' registrations and its tasks going. A
/// source's request made meanwhile is the shard's own ([`letting_go_of`]), as one from inside
/// it is.
pub(crate) fn letting_go<R>(shared: &Arc<Shared>, f: impl FnOnce() -> R) -> R {
    /// Puts back what the mark replaced, as `f` returns or unwinds.
    struct Restore(*const Arc<Shared>);
    impl Drop for Restore {
        fn drop(&mut self) {
            local().letting_go.set(self.0);
        }
    }
    let _restore = Restore(local().letting_go.replace(shared));
    f()
}

/// Runs `f` with the shard this thread lets go of ([`letting_go`]), if any.
pub(crate) fn letting_go_of<R>(f: impl FnOnce(Option<&Arc<Shared>>) -> R) -> R {
    // SAFETY: the mark points at an `Arc` that outlives the `letting_go` call that set it,
    // and is cleared before that call returns; the borrow ends with `f`.
    f(unsafe { local().letting_go.get().as_ref() })
}

/// Runs `f` with the shard this thread runs, if any, which no other thread
/// takes from it until `f` returns, even where a nested `block_on` in `f`
/// polls outside the shard: the slot is pinned meanwhile (a thread nobody
/// watches has none to pin, and its shard is never taken).
pub(crate) fn current_pinned<R>(f: impl FnOnce(Option<&Shard>) -> R) -> R {
    /// Unpins the slot as `f` returns or unwinds, inside the shard again.
    struct Pinned<'a>(&'a Slot);
    impl Drop for Pinned<'_> {
        fn drop(&mut self) {
            self.0.unpin();
        }
    }
    current(|shard| {
        let Some(shard) = shard else {
            return f(None);
        };
        let slot = local().slot.get();
        // SAFETY: the slot outlives the loop that set it (see `Here::slot`),
        // and so the shard this thread is inside for the whole call: the pin
        // keeps the monitor from taking it.
        let _pinned = unsafe { slot.as_ref() }.map(|slot| {
            slot.pin();
            Pinned(slot)
        });
        f(Some(shard))
    })
}

/// Runs `f` with the slot of this thread, while it runs a worker shard the
/// monitor watches.
pub(crate) fn with_slot<R>(f: impl FnOnce(Option<&Slot>) -> R) -> R {
    // SAFETY: the slot outlives the loop that set it (see `Here::slot`).
    f(unsafe { local().slot.get().as_ref() })
}

/// Runs `f` with the shard whose driver this thread may use now: the one
/// it runs, or else its own, which no other thread ever runs.
#[cfg(ring)]
pub(crate) fn here<R>(f: impl FnOnce(Option<&Shard>) -> R) -> R {
    current(|shard| match shard {
        Some(shard) => f(Some(shard)),
        None => {
            let own = own_shard_if_made();
            f(own.as_deref())
        }
    })
}

/// This thread's own shard, if it made one.
#[inline]
pub(crate) fn own_shard_if_made() -> Option<Rc<Shard>> {
    let own = local().own.get();
    if own.is_null() {
        return None;
    }
    // SAFETY: `OWN` holds the shard `Here` names, and the count taken here is
    // the clone's.
    unsafe {
        Rc::increment_strong_count(own);
        Some(Rc::from_raw(own))
    }
}

/// Marks `shard` as the one this thread runs, its own code running, until
/// the guard drops.
pub(crate) fn enter(shard: &Shard) -> Entered<'_> {
    enter_watched(shard, None)
}

/// [`enter`] for a worker shard the monitor watches through `slot`. A thread
/// handed a shard it was once detached from runs it again.
pub(crate) fn enter_watched<'a>(shard: &'a Shard, slot: Option<&'a Slot>) -> Entered<'a> {
    let here = local();
    if slot.is_some() {
        here.detached.set(0);
    }
    enter_with(here, shard, slot)
}

fn enter_with<'a>(here: &Here, shard: &'a Shard, slot: Option<&'a Slot>) -> Entered<'a> {
    Entered {
        previous: here.current.replace(shard),
        slot: here
            .slot
            .replace(slot.map_or(core::ptr::null(), core::ptr::from_ref)),
        out: here.out.replace(core::ptr::null()),
        _shard: core::marker::PhantomData,
    }
}

/// Restores what this thread ran before [`enter`].
pub(crate) struct Entered<'a> {
    previous: *const Shard,
    slot: *const Slot,
    out: *const Slot,
    _shard: core::marker::PhantomData<&'a Shard>,
}

impl Drop for Entered<'_> {
    fn drop(&mut self) {
        let here = local();
        here.current.set(self.previous);
        here.slot.set(self.slot);
        here.out.set(self.out);
    }
}

/// What a poll found as it ended: whether the thread's shard is still its
/// own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Polled {
    /// The shard is still this thread's.
    In,
    /// The shard was taken during the poll: this thread is detached from it.
    Detached,
}

/// Runs `poll`, a task's poll or a `block_on` future's on the shard of
/// `shared`, outside the shard in a new epoch, then comes back in, whether
/// it returned or unwound. Detached when the shard was taken during the poll
/// (found by its end, or by a call into the runtime from it).
#[inline]
pub(crate) fn poll<R>(shared: &Shared, poll: impl FnOnce() -> R) -> (R, Polled) {
    let here = local();
    if here.slot.get().is_null() {
        return poll_unwatched(here, shared, poll);
    }
    poll_watched(here, shared, poll)
}

/// [`poll`] on a shard no monitor watches: no slot to go out of and come
/// back into, and no call from the poll needs a bracket.
#[inline(always)]
fn poll_unwatched<R>(here: &Here, shared: &Shared, poll: impl FnOnce() -> R) -> (R, Polled) {
    /// Back into the shard, by return or unwind.
    struct Back<'a>(&'a Here, bool);
    impl Drop for Back<'_> {
        #[inline(always)]
        fn drop(&mut self) {
            self.0.in_task.set(self.1);
        }
    }
    let back = Back(here, here.in_task.replace(true));
    let result = poll();
    drop(back);
    (result, polled(here, shared))
}

/// [`poll`] on a shard the monitor watches through this thread's slot. A
/// poll begins inside, its runtime's own code running (the loop,
/// or a call that runs a nested `block_on`), with no bracket under way to
/// put back.
#[inline(never)]
fn poll_watched<R>(here: &'static Here, shared: &Shared, poll: impl FnOnce() -> R) -> (R, Polled) {
    /// The poll's way back into the shard, by return or unwind.
    struct Back {
        here: &'static Here,
        slot: &'static Slot,
        epoch: u64,
        in_task: bool,
    }
    impl Drop for Back {
        #[inline(always)]
        fn drop(&mut self) {
            let here = self.here;
            here.out.set(core::ptr::null());
            if self.slot.come_back(self.epoch) == super::handoff::slot::Back::Taken {
                here.detach();
            }
            here.in_task.set(self.in_task);
        }
    }
    // SAFETY: the slot outlives the loop that set it (see `Here::slot`), and
    // so this poll.
    let slot = unsafe { &*here.slot.get() };
    let epoch = here.epochs.get() + 1;
    here.epochs.set(epoch);
    let back = Back {
        here,
        slot,
        epoch,
        in_task: here.in_task.replace(true),
    };
    here.out.set(slot);
    slot.go_out(epoch);
    let result = poll();
    drop(back);
    (result, polled(here, shared))
}

/// [`poll`] for a task's run, `run`, which never unwinds: each part of it that runs the task's
/// own code (its future, its hooks, its drops) catches a panic or aborts on one
/// (`rt::task::raw`). On a shard the monitor watches, the run then keeps no guard to come back
/// in by: an unwind there would be the runtime's own bug, and aborts.
#[inline(always)]
pub(crate) fn run_task(shared: &Shared, run: impl FnOnce()) -> Polled {
    let here = local();
    if here.slot.get().is_null() {
        return poll_unwatched(here, shared, run).1;
    }
    run_task_watched(here, run);
    polled(here, shared)
}

/// [`run_task`] on a shard the monitor watches through this thread's slot, from inside it (see
/// [`poll_watched`]).
#[inline(never)]
fn run_task_watched(here: &'static Here, run: impl FnOnce()) {
    // SAFETY: the slot outlives the loop that set it (see `Here::slot`), and
    // so this run.
    let slot = unsafe { &*here.slot.get() };
    let epoch = here.epochs.get() + 1;
    here.epochs.set(epoch);
    let in_task = here.in_task.replace(true);
    here.out.set(slot);
    slot.go_out(epoch);
    super::task::abort_on_panic(run);
    here.out.set(core::ptr::null());
    // The poll's own word back inside: its epoch is not kept across the run, so the run's frame
    // holds one register less. Every call of the poll put its outside word back as it ended, or
    // left it marked when it found the shard taken (proofs/tla/handoff, OutsideWordAtPollEnd).
    debug_assert_eq!(
        slot.word() | 1,
        super::handoff::slot::outside(epoch),
        "a call of the poll left another word"
    );
    if slot.come_back_from_poll() == Back::Taken {
        here.detach();
    }
    here.in_task.set(in_task);
}

/// What a poll on the shard of `shared` found as it ended.
#[inline(always)]
fn polled(here: &Here, shared: &Shared) -> Polled {
    if here.detached.get() == core::ptr::from_ref(shared) as usize {
        Polled::Detached
    } else {
        Polled::In
    }
}

/// Whether a task's poll (or a `block_on` future's) is on this thread's
/// stack.
#[inline]
pub(crate) fn in_task() -> bool {
    local().in_task.get()
}

/// Runs `f` with [`in_task`] set to `in_task`.
pub(crate) fn with_in_task<R>(in_task: bool, f: impl FnOnce() -> R) -> R {
    struct Restore<'a>(&'a Here, bool);
    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            self.0.in_task.set(self.1);
        }
    }
    let here = local();
    let _restore = Restore(here, here.in_task.replace(in_task));
    f()
}

/// Runs `f` one `block_on` deeper on this thread.
///
/// # Panics
///
/// When this thread is already `max` deep.
#[expect(
    clippy::panic,
    reason = "documented under `# Panics` of `block_on` (`Builder::max_nesting`)"
)]
pub(crate) fn deeper<R>(max: usize, f: impl FnOnce() -> R) -> R {
    struct Up<'a>(&'a Here);
    impl Drop for Up<'_> {
        fn drop(&mut self) {
            self.0.depth.set(self.0.depth.get() - 1);
        }
    }
    let here = local();
    let depth = here.depth.get();
    if depth >= max {
        panic!(
            "block_on is nested {max} deep on one thread, the most allowed: each level runs \
             on the thread's stack (raise the limit with `Builder::max_nesting`)"
        );
    }
    here.depth.set(depth + 1);
    let _up = Up(here);
    f()
}

/// This thread's own shard, made now if it has none.
#[inline]
pub(crate) fn own_shard() -> io::Result<Rc<Shard>> {
    match own_shard_if_made() {
        Some(own) => Ok(own),
        None => make_own_shard(),
    }
}

#[inline(never)]
fn make_own_shard() -> io::Result<Rc<Shard>> {
    OWN.with(|own| {
        let own = &own.0;
        if let Some(shard) = own.borrow().as_ref() {
            return Ok(Rc::clone(shard));
        }
        let shard = Shard::new_rc(Limits {
            max_nesting: DEFAULT_MAX_NESTING,
            buffer_cap: crate::buf::DEFAULT_CAP,
            backend: super::backend::chosen()?,
        })?;
        *own.borrow_mut() = Some(Rc::clone(&shard));
        local().own.set(Rc::as_ptr(&shard));
        Ok(shard)
    })
}

/// The runtime `spawn` uses on this thread, if one is set.
#[inline]
pub(crate) fn runtime() -> Option<Arc<Inner>> {
    let inner = local().runtime.get();
    if inner.is_null() {
        return None;
    }
    // SAFETY: `RUNTIME` holds a strong reference to the runtime `Here`
    // names, so it is live, and the count taken here is the clone's.
    unsafe {
        Arc::increment_strong_count(inner);
        Some(Arc::from_raw(inner))
    }
}

/// The runtime set on this thread, borrowed in place for `f`, which must not
/// set or unset the thread's runtime, nor run code that could (`RUNTIME`
/// holds it meanwhile): no count is taken.
#[inline]
pub(crate) fn with_runtime<R>(f: impl FnOnce(Option<&Inner>) -> R) -> R {
    // SAFETY: `RUNTIME` holds the runtime `Here` names while `f` runs, as
    // `f` leaves the thread's runtime alone.
    f(unsafe { local().runtime.get().as_ref() })
}

/// Whether `inner` is the runtime set on this thread.
#[inline]
pub(crate) fn runtime_is(inner: *const Inner) -> bool {
    core::ptr::eq(local().runtime.get(), inner)
}

/// Makes `inner` the runtime `spawn` uses on this thread, until the guard
/// drops.
pub(crate) fn set_runtime(inner: &Arc<Inner>) -> RuntimeSet {
    let previous = RUNTIME.with(|r| r.replace(Some(Arc::clone(inner))));
    RuntimeSet { previous }
}

/// [`set_runtime`], unless this thread's locals are gone (it is exiting).
pub(crate) fn try_set_runtime(inner: &Arc<Inner>) -> Option<RuntimeSet> {
    let previous = RUNTIME
        .try_with(|r| r.replace(Some(Arc::clone(inner))))
        .ok()?;
    Some(RuntimeSet { previous })
}

/// Restores the runtime that was set before [`set_runtime`].
pub(crate) struct RuntimeSet {
    previous: Option<Arc<Inner>>,
}

impl Drop for RuntimeSet {
    fn drop(&mut self) {
        let previous = self.previous.take();
        let _ = RUNTIME.try_with(|r| drop(r.replace(previous)));
    }
}

/// The home of a bound task spawned now on `shard`, which this thread runs:
/// the thread's own as a worker shard's runner, else the shard's, a thread's
/// own shard.
#[inline]
pub(crate) fn home_for(shard: &Shard) -> Arc<Home> {
    let home = local().home.get();
    if home.is_null() {
        return shard.own_home();
    }
    // SAFETY: `HOME` holds a strong reference to the home `Here` names, so it
    // is live, and the count taken here is the clone's.
    unsafe {
        Arc::increment_strong_count(home);
        Arc::from_raw(home)
    }
}

/// Makes `home` this thread's, for the bound tasks it spawns from now on: a
/// thread starting to run a worker shard. The home it had stays with the
/// tasks it holds.
pub(crate) fn set_home(home: Arc<Home>) {
    set_home_ptr(&home);
    HOME.with(|slot| *slot.0.borrow_mut() = Some(home));
}

fn set_home_ptr(home: &Arc<Home>) {
    local().home.set(Arc::as_ptr(home));
}

/// Whether `home` is this thread's.
#[inline]
pub(crate) fn is_home(home: &Home) -> bool {
    core::ptr::eq(local().home.get(), home)
}

/// The next runnable pushed to this thread's home, if any.
pub(crate) fn pop_home() -> Option<Runnable> {
    // SAFETY: `HOME` keeps the home alive while `home` points at it, and
    // this thread is its thread.
    unsafe { local().home.get().as_ref().and_then(|home| home.pop()) }
}

/// Whether nothing pushed to this thread's home waits, as its taker sees it.
pub(crate) fn home_is_drained() -> bool {
    // SAFETY: as in `pop_home`.
    unsafe {
        local()
            .home
            .get()
            .as_ref()
            .is_none_or(|home| home.is_drained())
    }
}
