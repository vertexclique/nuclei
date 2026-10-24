//! The guard of one direction of a source, shared by the source's polls and
//! its shard's driver.
//!
//! A side's state word has two bits. `BUSY`: someone holds the side, for one
//! system call or one copy at most, never across an await. `READY`:
//! readiness came while the side was held. The driver never waits for the
//! guard: finding the side held, it sets `READY` and goes on, and the
//! holder, as it lets go, sees `READY` and does the driver's part itself
//! (if the holder let go before `READY` landed, the driver takes the side
//! after all). A poll that finds the side held waits for the holder, which
//! is only ever doing one bounded step. So an edge is never lost and the
//! driver never stalls behind a poll. `proofs/tla/side/` checks the
//! protocol and the loom models below check this code.
//!
//! Nothing that may run code of its own happens under the guard: wakers
//! are woken and buffers dropped (a dropped buffer may wake a source that
//! waits for one) only after the guard is let go, through [`After`].

use core::task::Waker;

use crate::buf::PoolBuf;
use crate::sync::internal::{AtomicU8, Ordering, UnsafeCell};

/// Someone holds the side.
const BUSY: u8 = 1;
/// Readiness came while the side was held.
const READY: u8 = 2;

/// One direction of a source: its state, behind the guard.
pub(crate) struct Side<S> {
    state: AtomicU8,
    inner: UnsafeCell<S>,
}

// SAFETY: the contents are reached only by the holder of the guard.
unsafe impl<S: Send> Send for Side<S> {}
// SAFETY: as for `Send`: one holder at a time.
unsafe impl<S: Send> Sync for Side<S> {}

/// A held side. Dropping it lets go without looking at `READY`; the paths
/// that must look use [`release`](Held::release).
pub(crate) struct Held<'a, S> {
    side: &'a Side<S>,
}

impl<S> Side<S> {
    pub(crate) fn new(inner: S) -> Self {
        Self {
            state: AtomicU8::new(0),
            inner: UnsafeCell::new(inner),
        }
    }

    /// Takes the side, unless someone holds it. A `READY` left on a free
    /// side (the holder let go as the driver flagged it) is taken with it:
    /// the new holder is about to look at the descriptor anyway.
    fn try_hold(&self) -> Option<Held<'_, S>> {
        let mut state = self.state.load(Ordering::Relaxed);
        loop {
            if state & BUSY != 0 {
                return None;
            }
            // Acquire: the holder before us published the contents with its
            // Release.
            match self.state.compare_exchange_weak(
                state,
                BUSY,
                Ordering::Acquire,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(Held { side: self }),
                Err(actual) => state = actual,
            }
        }
    }

    /// Takes the side, waiting for the holder's one step.
    pub(crate) fn hold(&self) -> Held<'_, S> {
        let mut spins = 0u32;
        loop {
            if let Some(held) = self.try_hold() {
                return held;
            }
            #[cfg(loom)]
            loom::thread::yield_now();
            if spins < 64 {
                core::hint::spin_loop();
                spins += 1;
            } else {
                // The holder was preempted mid-step: let it run.
                std::thread::yield_now();
            }
        }
    }

    /// The driver's way in: the side if it is free; if it is held, `READY`
    /// is left for the holder, and `None` returned.
    fn hold_or_flag(&self) -> Option<Held<'_, S>> {
        loop {
            if let Some(held) = self.try_hold() {
                return Some(held);
            }
            // AcqRel: the flag is the holder's to act on, after its step.
            if self.state.fetch_or(READY, Ordering::AcqRel) & BUSY != 0 {
                return None;
            }
            // The holder let go before the flag landed: take the side.
        }
    }

    /// Runs one step of a poll under the guard. When readiness came while
    /// it ran, the driver's part (`serve`, told it serves an edge) runs here
    /// in its place, then the step again if it was pending. Wakes and drops
    /// happen once the guard is let go.
    pub(crate) fn poll<R>(
        &self,
        after: &mut After,
        mut step: impl FnMut(&mut S, &mut After) -> core::task::Poll<R>,
        mut serve: impl FnMut(&mut S, &mut After),
    ) -> core::task::Poll<R> {
        let mut held = self.hold();
        let mut out = step(held.get(), after);
        while held.release() {
            held = self.hold();
            serve(held.get(), after);
            if out.is_pending() {
                out = step(held.get(), after);
            }
        }
        out
    }

    /// Runs `f` under the guard; when readiness came while it ran, the
    /// driver's part (`serve`) runs here before the guard is let go for good.
    pub(crate) fn run<R>(
        &self,
        after: &mut After,
        f: impl FnOnce(&mut S, &mut After) -> R,
        mut serve: impl FnMut(&mut S, &mut After),
    ) -> R {
        let mut held = self.hold();
        let out = f(held.get(), after);
        while held.release() {
            held = self.hold();
            serve(held.get(), after);
        }
        out
    }

    /// The driver's part: `serve` runs under the guard, or, if a poll holds
    /// it, in that poll as it lets go. `edge`: the driver saw readiness;
    /// `serve` is told so, and so is every round after a flag.
    pub(crate) fn drive(
        &self,
        after: &mut After,
        mut serve: impl FnMut(&mut S, bool, &mut After),
        edge: bool,
    ) {
        let mut edge = edge;
        while let Some(mut held) = self.hold_or_flag() {
            serve(held.get(), edge, after);
            if !held.release() {
                return;
            }
            edge = true;
        }
    }

    /// The contents, through exclusive access: nobody else can hold it.
    pub(crate) fn get_mut(&mut self) -> &mut S {
        self.inner.with_mut(|p| {
            // SAFETY: `&mut self` excludes every holder.
            unsafe { &mut *p }
        })
    }
}

impl<S> Held<'_, S> {
    pub(crate) fn get(&mut self) -> &mut S {
        self.side.inner.with_mut(|p| {
            // SAFETY: BUSY is ours: nobody else reaches the contents.
            unsafe { &mut *p }
        })
    }

    /// Lets go of the side. Returns whether readiness came while it was
    /// held, so the caller does the driver's part.
    pub(crate) fn release(self) -> bool {
        // Release: publishes the contents to the next holder.
        let state = self.side.state.swap(0, Ordering::AcqRel);
        core::mem::forget(self);
        state & READY != 0
    }
}

impl<S> Drop for Held<'_, S> {
    fn drop(&mut self) {
        self.side.state.store(0, Ordering::Release);
    }
}

/// What a step leaves for once no guard is held: wakers to wake, wakers and
/// buffers to drop (either may run code that reaches a source), and what
/// the source must do next.
#[derive(Default)]
pub(crate) struct After {
    wake: Wakers,
    forget: Wakers,
    drop: Vec<PoolBuf>,
    /// A side found the pool empty and must hear when a buffer comes back.
    pub(crate) starved: bool,
    /// Work was left to the driver: the source must be registered.
    pub(crate) register: bool,
}

impl After {
    pub(crate) fn wake(&mut self, wakers: &mut Wakers) {
        self.wake.append(wakers);
    }

    /// Drops `wakers` without waking them.
    pub(crate) fn forget(&mut self, wakers: &mut Wakers) {
        self.forget.append(wakers);
    }

    pub(crate) fn drop_buf(&mut self, buf: PoolBuf) {
        self.drop.push(buf);
    }

    /// Wakes and drops; call with no guard held.
    pub(crate) fn run(&mut self) {
        self.drop.clear();
        self.forget = Wakers::default();
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
        if self.first.is_none() {
            self.first = Some(waker.clone());
        } else {
            self.rest.push(waker.clone());
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
            if self.first.is_none() {
                self.first = Some(w);
            } else {
                self.rest.push(w);
            }
        }
        self.rest.append(&mut other.rest);
    }

    fn wake_all(&mut self) {
        if let Some(w) = self.first.take() {
            w.wake();
        }
        for w in self.rest.drain(..) {
            w.wake();
        }
    }
}

#[cfg(all(test, loom))]
mod loom_tests {
    use core::task::Poll;

    use loom::sync::Arc;
    use loom::sync::atomic::{AtomicUsize, Ordering};
    use loom::thread;

    use super::{After, Side};

    /// What a side holds in the model: the readiness cache (an edge came
    /// since the last empty read) and what the reader got.
    #[derive(Default)]
    struct Model {
        ready: bool,
        got: usize,
    }

    /// One read attempt on the model's descriptor (`kernel`, the bytes the
    /// kernel holds): takes them all, or clears the cache.
    fn read(m: &mut Model, kernel: &AtomicUsize) -> bool {
        match kernel.swap(0, Ordering::SeqCst) {
            0 => {
                m.ready = false;
                false
            }
            n => {
                m.got += n;
                true
            }
        }
    }

    /// The model: a byte arrives and its edge is reported while a poll
    /// reads. `flag`: the driver uses the protocol; without it, a held side
    /// makes it drop the edge.
    fn model(flag: bool) {
        let side = Arc::new(Side::new(Model {
            ready: true,
            ..Model::default()
        }));
        let kernel = Arc::new(AtomicUsize::new(0));
        let driver = {
            let (side, kernel) = (side.clone(), kernel.clone());
            thread::spawn(move || {
                kernel.fetch_add(1, Ordering::SeqCst);
                let mut after = After::default();
                let serve = |m: &mut Model, edge: bool, _: &mut After| {
                    if edge {
                        m.ready = true;
                    }
                    if m.ready {
                        read(m, &kernel);
                    }
                };
                if flag {
                    side.drive(&mut after, serve, true);
                } else if let Some(mut held) = side.try_hold() {
                    serve(held.get(), true, &mut after);
                }
            })
        };
        let mut after = After::default();
        let _ = side.poll(
            &mut after,
            |m, _| {
                if m.ready && read(m, &kernel) {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            },
            |m, _| {
                m.ready = true;
                read(m, &kernel);
            },
        );
        driver.join().unwrap();
        assert_eq!(
            kernel.load(Ordering::SeqCst),
            0,
            "the byte was left in the kernel"
        );
        assert_eq!(side.hold().get().got, 1);
    }

    /// A poll reads while the data arrives and the driver reports it: the
    /// byte is read, by the poll or by the driver's part, never left in the
    /// kernel with the cache cleared.
    #[test]
    fn an_edge_during_a_poll_is_never_lost() {
        loom::model(|| model(true));
    }

    /// The calibration: a driver that skips a held side loses the edge in
    /// some interleaving.
    #[test]
    #[should_panic(expected = "the byte was left in the kernel")]
    fn calibration_a_driver_that_skips_a_held_side_loses_the_edge() {
        loom::model(|| model(false));
    }
}
