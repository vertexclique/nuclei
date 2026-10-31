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
//! bracket, in [`current`]), comes back in, unless the monitor took the
//! shard meanwhile. A thread whose shard was taken is detached from it: from
//! then on it runs no shard, and its calls take the paths of any other
//! thread (proofs/tla/handoff).

use core::cell::{Cell, RefCell};
use std::io;
use std::rc::Rc;
use std::sync::Arc;

use super::handoff::home::Home;
use super::handoff::slot::{Back, Slot};
use super::runtime::Inner;
use super::shard::{Limits, Shard, Shared};
use super::task::Runnable;

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
    /// The thread runs its shard's own code.
    inside: Cell<bool>,
    /// The nesting of `block_on` on this thread.
    depth: Cell<usize>,
    /// The thread's slot while it runs a worker shard the monitor watches,
    /// null otherwise; kept alive by the loop that entered the shard.
    slot: Cell<*const Slot>,
    /// The epoch of the poll in progress, and the last one begun.
    epoch: Cell<u64>,
    epochs: Cell<u64>,
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
}

impl Here {
    const fn new() -> Self {
        Self {
            current: Cell::new(core::ptr::null()),
            in_task: Cell::new(false),
            inside: Cell::new(false),
            depth: Cell::new(0),
            slot: Cell::new(core::ptr::null()),
            epoch: Cell::new(0),
            epochs: Cell::new(0),
            detached: Cell::new(0),
            home: Cell::new(core::ptr::null()),
            task: Cell::new(None),
            budget: Cell::new(crate::coop::UNBUDGETED),
            runtime: Cell::new(core::ptr::null()),
            own: Cell::new(core::ptr::null()),
        }
    }

    /// Comes back into the shard from the poll of `epoch`: whether the shard
    /// is still this thread's (always, for a shard nobody watches).
    #[inline]
    fn come_back(&self, epoch: u64) -> bool {
        // SAFETY: the slot outlives the loop that set it (see `slot`).
        match unsafe { self.slot.get().as_ref() } {
            Some(slot) if slot.come_back(epoch) == Back::Taken => {
                self.detach();
                false
            }
            _ => true,
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

/// A call into the runtime from a task's code: inside the shard until it
/// drops, then out again in the poll's epoch.
struct Bracket<'a>(&'a Here);

impl Drop for Bracket<'_> {
    fn drop(&mut self) {
        let here = self.0;
        here.inside.set(false);
        // SAFETY: as in `Here::come_back`; a thread detached meanwhile has
        // no slot any more.
        if let Some(slot) = unsafe { here.slot.get().as_ref() } {
            slot.go_out(here.epoch.get());
        }
    }
}

/// Runs `f` with the shard this thread is running, if any. Called from a
/// task's code, it comes back into the shard first (a bracket), and finds
/// none if the shard was taken.
#[inline]
pub(crate) fn current<R>(f: impl FnOnce(Option<&Shard>) -> R) -> R {
    let here = local();
    let shard = here.current.get();
    if shard.is_null() {
        return f(None);
    }
    let _bracket = if here.inside.get() || here.slot.get().is_null() {
        None
    } else if here.come_back(here.epoch.get()) {
        here.inside.set(true);
        Some(Bracket(here))
    } else {
        return f(None);
    };
    // SAFETY: `current` points at a shard only while an `Entered` guard
    // for it lives, and the guard borrows the shard; a taken shard is
    // current no more.
    f(unsafe { shard.as_ref() })
}

/// The shard this thread runs, when a call into it from here needs no
/// bracket (the thread is inside it, or no monitor watches it): what
/// [`current`] would hand its closure, without the closure. `None` where
/// [`current`] has to be asked.
#[inline(always)]
pub(crate) fn running_inside(here: &Here) -> Option<&Shard> {
    if here.inside.get() || here.slot.get().is_null() {
        // SAFETY: as in `current`.
        unsafe { here.current.get().as_ref() }
    } else {
        None
    }
}

impl Here {
    /// Whether a task's poll (or a `block_on` future's) is on this thread's
    /// stack ([`in_task`]).
    #[inline(always)]
    pub(crate) fn in_task(&self) -> bool {
        self.in_task.get()
    }
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
        inside: here.inside.replace(true),
        _shard: core::marker::PhantomData,
    }
}

/// Restores what this thread ran before [`enter`].
pub(crate) struct Entered<'a> {
    previous: *const Shard,
    slot: *const Slot,
    inside: bool,
    _shard: core::marker::PhantomData<&'a Shard>,
}

impl Drop for Entered<'_> {
    fn drop(&mut self) {
        let here = local();
        here.current.set(self.previous);
        here.slot.set(self.slot);
        here.inside.set(self.inside);
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
/// back into, and none can be entered during the poll (only a runner's
/// first frame enters one), so the epochs are left as they are.
#[inline(always)]
fn poll_unwatched<R>(here: &Here, shared: &Shared, poll: impl FnOnce() -> R) -> (R, Polled) {
    /// Back into the shard, by return or unwind.
    struct Back<'a>(&'a Here, bool);
    impl Drop for Back<'_> {
        fn drop(&mut self) {
            self.0.in_task.set(self.1);
            self.0.inside.set(true);
        }
    }
    let back = Back(here, here.in_task.replace(true));
    here.inside.set(false);
    let result = poll();
    drop(back);
    let polled = if here.detached.get() == core::ptr::from_ref(shared) as usize {
        Polled::Detached
    } else {
        Polled::In
    };
    (result, polled)
}

/// [`poll`] on a shard the monitor watches through this thread's slot.
#[inline(never)]
fn poll_watched<R>(here: &'static Here, shared: &Shared, poll: impl FnOnce() -> R) -> (R, Polled) {
    /// The poll's way back into the shard, taken once, by return or unwind.
    struct Out<'a> {
        here: &'a Here,
        shared: usize,
        in_task: bool,
        epoch: u64,
        mine: u64,
        done: bool,
    }
    impl Out<'_> {
        fn back(&mut self) -> Polled {
            self.done = true;
            let here = self.here;
            here.come_back(self.mine);
            here.in_task.set(self.in_task);
            here.inside.set(true);
            here.epoch.set(self.epoch);
            if here.detached.get() == self.shared {
                Polled::Detached
            } else {
                Polled::In
            }
        }
    }
    impl Drop for Out<'_> {
        fn drop(&mut self) {
            if !self.done {
                self.back();
            }
        }
    }
    let mine = here.epochs.get() + 1;
    here.epochs.set(mine);
    let mut out = Out {
        here,
        shared: core::ptr::from_ref(shared) as usize,
        in_task: here.in_task.replace(true),
        epoch: here.epoch.replace(mine),
        mine,
        done: false,
    };
    here.inside.set(false);
    // SAFETY: as in `Here::come_back`.
    if let Some(slot) = unsafe { here.slot.get().as_ref() } {
        slot.go_out(mine);
    }
    let result = poll();
    (result, out.back())
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
        let shard = Rc::new(Shard::new(Limits {
            max_nesting: DEFAULT_MAX_NESTING,
            buffer_cap: crate::buf::DEFAULT_CAP,
            backend: super::backend::chosen()?,
        })?);
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
pub(crate) fn home_for(shard: &Shard) -> Arc<Home> {
    HOME.with(|home| home.0.borrow().clone())
        .unwrap_or_else(|| shard.own_home())
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
