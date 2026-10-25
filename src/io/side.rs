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
//! Nothing that may run code of its own happens under the guard: wakers
//! are woken and buffers dropped (a dropped buffer may wake a source that
//! waits for one) only after the guard is let go, through [`After`].

use core::task::{Poll, Waker};

use crate::buf::PoolBuf;
use crate::sync::atomic_waker::{AtomicWaker, Registered};
use crate::sync::internal::{AtomicPtr, AtomicU8, AtomicUsize, Ordering, UnsafeCell};

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
}

/// A held side. Let go through [`release`](Held::release), which does what
/// others left; a drop (a panic under the guard) only wakes the contended.
pub(crate) struct Held<'a, S> {
    side: &'a Side<S>,
    /// The word the holder took: what letting go swaps back to zero.
    expect: u8,
    /// The driver took the side with its own `READY`.
    own_ready: bool,
}

impl<S> Side<S> {
    pub(crate) fn new(inner: S) -> Self {
        Self {
            state: AtomicU8::new(0),
            deferred: AtomicU8::new(0),
            contended: AtomicPtr::new(core::ptr::null_mut()),
            inner: UnsafeCell::new(inner),
        }
    }

    /// The side, if it was free, taken with one `fetch_or` that also raises
    /// `extra`; if it was held, `extra` is left for its holder.
    fn take(&self, extra: u8) -> Option<Held<'_, S>> {
        // AcqRel: acquires what the last holder published, and publishes
        // what this thread left (its deferred bits, its contended waker)
        // to the holder.
        let prev = self.state.fetch_or(BUSY | extra, Ordering::AcqRel);
        // Built only on success: a `Held` dropped unused would let go of a
        // side another thread holds.
        (prev & BUSY == 0).then(|| Held {
            side: self,
            expect: BUSY | (extra & READY),
            own_ready: extra & READY != 0,
        })
    }

    /// The side, if it is free: a test's look.
    #[cfg(test)]
    pub(crate) fn try_hold(&self) -> Option<Held<'_, S>> {
        self.take(0)
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
    pub(crate) fn take_for_request(&self) -> Option<Held<'_, S>> {
        self.take(REQ)
    }

    /// Runs one step of a poll under the guard. A poll that finds the side
    /// held registers `waker` and returns `Pending`: the holder wakes it as
    /// it lets go. When an edge came while the step ran, the driver's part
    /// runs here, then the step again if it was pending.
    pub(crate) fn poll<R>(
        &self,
        after: &mut After,
        waker: &Waker,
        hooks: &impl Hooks<S>,
        mut step: impl FnMut(&mut S, &mut After) -> Poll<R>,
    ) -> Poll<R> {
        let Some(mut held) = self.take(0).or_else(|| self.contend(waker)) else {
            return Poll::Pending;
        };
        hooks.taken(held.get(), after);
        let mut out = step(held.get(), after);
        held.release_with(after, hooks, |st, after| {
            if out.is_pending() {
                out = step(st, after);
            }
        });
        out
    }

    /// For a poll that found the side held: its waker goes to the contended
    /// set for the holder to wake, or, if the side came free meanwhile, the
    /// side is taken after all.
    fn contend(&self, waker: &Waker) -> Option<Held<'_, S>> {
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
        self.take(0)
    }

    /// The driver's part for an edge, after side-local work `defer` (say,
    /// urgent data came): done now if the side is free, else by its holder
    /// as it lets go.
    pub(crate) fn drive(&self, after: &mut After, hooks: &impl Hooks<S>, defer: u8) {
        if defer != 0 {
            // Release: the holder that takes DEFER reads the bits.
            self.deferred.fetch_or(defer, Ordering::Release);
        }
        let extra = if defer != 0 { READY | DEFER } else { READY };
        let Some(mut held) = self.take(extra) else {
            return;
        };
        hooks.taken(held.get(), after);
        held.run_deferred(hooks, after);
        hooks.serve(held.get(), after);
        held.release(after, hooks);
    }

    /// Leaves side-local work `bits` for whoever holds the side: done now if
    /// the side is free, else by its holder as it lets go.
    pub(crate) fn defer(&self, bits: u8, after: &mut After, hooks: &impl Hooks<S>) {
        // Release: the holder that takes DEFER reads the bits.
        self.deferred.fetch_or(bits, Ordering::Release);
        let Some(mut held) = self.take(DEFER) else {
            return;
        };
        hooks.taken(held.get(), after);
        held.run_deferred(hooks, after);
        held.release(after, hooks);
    }

    /// The contents, through exclusive access: nobody else can hold it.
    pub(crate) fn get_mut(&mut self) -> &mut S {
        self.inner.with_mut(|p| {
            // SAFETY: `&mut self` excludes every holder.
            unsafe { &mut *p }
        })
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

    fn run_deferred(&mut self, hooks: &impl Hooks<S>, after: &mut After) {
        // Acquire: pairs with the Release of whoever left the bits.
        let bits = self.side.deferred.swap(0, Ordering::Acquire);
        if bits != 0 {
            hooks.deferred(self.get(), bits, after);
        }
    }

    /// Lets go of the side, first doing what others left for its holder.
    pub(crate) fn release(self, after: &mut After, hooks: &impl Hooks<S>) {
        self.release_with(after, hooks, |_, _| {});
    }

    /// As [`release`](Self::release); `again` runs after each driver's part
    /// (a poll's step, while it waits).
    fn release_with(
        mut self,
        after: &mut After,
        hooks: &impl Hooks<S>,
        mut again: impl FnMut(&mut S, &mut After),
    ) {
        let mut saw_requests = false;
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
                self.run_deferred(hooks, after);
            }
            if flags & READY != 0 {
                hooks.serve(self.get(), after);
                again(self.get(), after);
            }
            if flags & WAITING != 0 {
                self.side.wake_contended(after);
            }
            if flags & REQ != 0 {
                saw_requests = true;
                hooks.requests(self.get(), after);
            }
        }
        core::mem::forget(self);
        hooks.released(saw_requests, after);
    }
}

impl<S> Drop for Held<'_, S> {
    /// Only a panic under the guard gets here: the side goes free, and the
    /// polls waiting on it are woken to look again.
    fn drop(&mut self) {
        let flags = self.side.state.swap(0, Ordering::AcqRel);
        if flags & WAITING != 0 {
            let mut after = After::default();
            self.side.wake_contended(&mut after);
            after.run();
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
pub(crate) fn whole_too_large(len: usize) -> Option<std::io::Error> {
    crate::buf::with_pool(|pool| {
        pool.filter(|p| !p.could_hold_whole(len)).map(|p| {
            std::io::Error::new(
                std::io::ErrorKind::OutOfMemory,
                format!(
                    "a datagram of {len} bytes needs more IO buffer memory than the runtime allows ({} bytes); raise `Builder::io_buffer_cap`",
                    p.cap()
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
    wake: Wakers,
    forget: Wakers,
    /// Buffers to drop: almost every step drops one at most.
    drop_one: Option<PoolBuf>,
    drop_more: Vec<PoolBuf>,
    /// A side found the pool empty and must hear when a buffer comes back.
    pub(crate) starved: bool,
    /// Work was left to the driver: the source must be registered.
    pub(crate) register: bool,
    /// A side of a ring source wants an operation the ring's thread, the
    /// one of this shard, must submit.
    #[cfg(ring)]
    pub(crate) kick: Option<std::sync::Arc<crate::rt::shard::Shared>>,
}

impl After {
    pub(crate) fn wake(&mut self, wakers: &mut Wakers) {
        self.wake.append(wakers);
    }

    /// Drops `wakers` without waking them.
    pub(crate) fn forget(&mut self, wakers: &mut Wakers) {
        self.forget.append(wakers);
    }

    /// Wakes the one task of `waker`.
    pub(crate) fn wake_one(&mut self, waker: Waker) {
        self.wake.push(waker);
    }

    /// Drops `waker` without waking it.
    pub(crate) fn forget_one(&mut self, waker: Waker) {
        self.forget.push(waker);
    }

    pub(crate) fn drop_buf(&mut self, buf: PoolBuf) {
        match self.drop_one {
            None => self.drop_one = Some(buf),
            Some(_) => self.drop_more.push(buf),
        }
    }

    /// Wakes and drops; call with no guard held.
    pub(crate) fn run(&mut self) {
        drop(self.drop_one.take());
        if !self.drop_more.is_empty() {
            self.drop_more.clear();
        }
        if !self.forget.is_empty() {
            self.forget = Wakers::default();
        }
        self.wake.wake_all();
    }
}

impl Drop for After {
    fn drop(&mut self) {
        self.run();
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
    pub(crate) fn register(&mut self, waker: &Waker) {
        let known = self
            .first
            .iter()
            .chain(self.rest.iter())
            .any(|w| w.will_wake(waker));
        if known {
            return;
        }
        self.push(waker.clone());
    }

    /// Adds `waker`, known or not.
    fn push(&mut self, waker: Waker) {
        if self.first.is_none() {
            self.first = Some(waker);
        } else {
            self.rest.push(waker);
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.first.is_none()
    }

    /// How many tasks wait.
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

    fn wake_all(&mut self) {
        if let Some(w) = self.first.take() {
            w.wake();
        }
        if !self.rest.is_empty() {
            for w in self.rest.drain(..) {
                w.wake();
            }
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests;

#[cfg(all(test, loom))]
mod loom_tests;
