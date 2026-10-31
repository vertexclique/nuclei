//! What a step under a side's guard leaves for once the guard is let go
//! ([`After`]): the wakes and drops that may run code of their own, parked
//! while a non-owning view of a source is live and the thread unwinds
//! ([`Unwound`]); and the wakers of the tasks waiting on a side ([`Wakers`]).

use core::task::Waker;

use crate::buf::PoolBuf;

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
