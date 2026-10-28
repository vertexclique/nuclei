//! [`JoinSet`]: a set of tasks whose outputs come back in the order the tasks
//! finish.
//!
//! Each slot of the set has a waker of its own, made once with the slot and
//! kept for every member it holds: woken, it queues itself (at most once,
//! its `queued` flag) in the set's ready queue, linked through the waker
//! itself (the shard inbox's queue, proofs/tla/inbox), and wakes the set's
//! awaiter. `join_next` polls only members that were woken, never the whole
//! set, and a spawn into a set that has a free slot allocates nothing for
//! the set. A slot reused by a later member may get a stale wake from the
//! member before it, which costs one spurious poll and nothing else. A
//! waker holds the set weakly: one that wakes after the set is gone does
//! nothing, and nothing queued keeps the set alive.

use core::future::Future;
use core::pin::Pin;
use core::ptr::NonNull;
use core::task::{Context, Poll, Waker};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::task::Wake;

use crate::rt::inbox::{Inbox, Link, Linked, Pop};
use crate::sync::atomic_waker::AtomicWaker;
use crate::task::{AbortHandle, Id, Join, JoinError, Task};

/// What the slots' wakers share with the set.
struct Shared {
    /// Wakers of woken members, in wake order.
    ready: Inbox<Queued>,
    /// The task awaiting `join_next`.
    awaiter: AtomicWaker,
}

/// A slot's waker: queues itself once per wake.
struct SlotWake {
    link: Link,
    shared: Weak<Shared>,
    slot: usize,
    queued: AtomicBool,
}

impl Wake for SlotWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if !self.queued.swap(true, Ordering::AcqRel)
            && let Some(shared) = self.shared.upgrade()
        {
            shared.ready.push(Queued(Arc::clone(self)));
            shared.awaiter.wake();
        }
    }
}

/// A slot's waker in the ready queue, with the reference that keeps it.
struct Queued(Arc<SlotWake>);

// SAFETY: the link is the waker's own field, kept alive by the reference
// the item holds; a waker is `Send` and `Sync`.
unsafe impl Linked for Queued {
    fn into_link(self) -> NonNull<Link> {
        let wake = Arc::into_raw(self.0);
        // SAFETY: `wake` points at a live waker, whose link is in it.
        unsafe { NonNull::new_unchecked(core::ptr::addr_of!((*wake).link).cast_mut()) }
    }

    unsafe fn from_link(link: NonNull<Link>) -> Self {
        let offset = core::mem::offset_of!(SlotWake, link);
        // SAFETY: the link is a waker's `link` field, given up by
        // `into_link` with its reference.
        Self(unsafe { Arc::from_raw(link.as_ptr().cast::<u8>().sub(offset).cast::<SlotWake>()) })
    }
}

/// A place in the set: its waker, kept, and the member it holds.
struct Slot<T> {
    wake: Arc<SlotWake>,
    waker: Waker,
    member: Option<Member<T>>,
}

struct Member<T> {
    join: Join<T>,
    abort: AbortHandle,
}

/// A set of spawned tasks, joined in the order they finish.
///
/// Dropping the set cancels every task still in it, as dropping their
/// [`Task`] handles would; [`detach_all`](Self::detach_all) lets them run on.
///
/// ```
/// # nuclei::block_on(async {
/// let mut set = nuclei::task::JoinSet::new();
/// for i in 0..4u64 {
///     set.spawn(async move { i * 10 });
/// }
/// let mut sum = 0;
/// while let Some(out) = set.join_next().await {
///     sum += out.unwrap();
/// }
/// assert_eq!(sum, 60);
/// # });
/// ```
pub struct JoinSet<T> {
    slots: Vec<Slot<T>>,
    free: Vec<usize>,
    len: usize,
    shared: Arc<Shared>,
}

impl<T> Default for JoinSet<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> JoinSet<T> {
    /// An empty set.
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            len: 0,
            shared: Arc::new(Shared {
                ready: Inbox::new(),
                awaiter: AtomicWaker::new(),
            }),
        }
    }

    /// The tasks in the set, finished and not joined ones included.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether the set holds no task.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Takes `task` into the set and answers a handle that cancels it.
    pub fn insert(&mut self, task: Task<T>) -> AbortHandle {
        let abort = task.abort_handle();
        let member = Member {
            join: task.join(),
            abort: abort.clone(),
        };
        let slot = match self.free.pop() {
            Some(slot) => slot,
            None => {
                let slot = self.slots.len();
                let wake = Arc::new(SlotWake {
                    link: Link::new(),
                    shared: Arc::downgrade(&self.shared),
                    slot,
                    queued: AtomicBool::new(false),
                });
                self.slots.push(Slot {
                    waker: Waker::from(Arc::clone(&wake)),
                    wake,
                    member: None,
                });
                slot
            }
        };
        let place = &mut self.slots[slot];
        place.member = Some(member);
        self.len += 1;
        // Queued at once, unless a stale wake queued the slot already: the
        // next `join_next` polls the new member.
        if !place.wake.queued.swap(true, Ordering::AcqRel) {
            self.shared.ready.push(Queued(Arc::clone(&place.wake)));
        }
        abort
    }

    /// Spawns `future` as [`spawn`](crate::task::spawn) does, into the set.
    pub fn spawn<F>(&mut self, future: F) -> AbortHandle
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        self.insert(crate::task::spawn(future))
    }

    /// Spawns `future` as [`spawn_local`](crate::task::spawn_local) does,
    /// into the set.
    pub fn spawn_local<F>(&mut self, future: F) -> AbortHandle
    where
        F: Future<Output = T> + 'static,
        T: 'static,
    {
        self.insert(crate::task::spawn_local(future))
    }

    /// Runs `f` on the blocking pool, into the set.
    pub fn spawn_blocking<F>(&mut self, f: F) -> AbortHandle
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.insert(crate::task::spawn_blocking(f))
    }

    /// Spawns `future` on the runtime `handle` refers to, into the set.
    pub fn spawn_on<F>(&mut self, future: F, handle: &crate::runtime::RuntimeHandle) -> AbortHandle
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        self.insert(handle.spawn(future))
    }

    /// The output of the next task to finish, `None` when the set is empty.
    /// A task that was cancelled or panicked gives its [`JoinError`].
    pub async fn join_next(&mut self) -> Option<Result<T, JoinError>> {
        core::future::poll_fn(|cx| self.poll_join_next(cx))
            .await
            .map(|out| out.map(|(_, value)| value))
    }

    /// [`join_next`](Self::join_next), with the id of the task.
    pub async fn join_next_with_id(&mut self) -> Option<Result<(Id, T), JoinError>> {
        core::future::poll_fn(|cx| self.poll_join_next(cx)).await
    }

    /// The output of a task that has finished, without waiting: `None` when
    /// none has (the set may still hold tasks).
    pub fn try_join_next(&mut self) -> Option<Result<T, JoinError>> {
        let mut cx = Context::from_waker(Waker::noop());
        match self.poll_join_next(&mut cx) {
            Poll::Ready(Some(out)) => Some(out.map(|(_, value)| value)),
            _ => None,
        }
    }

    /// Polls for the next finished task.
    pub fn poll_join_next(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<(Id, T), JoinError>>> {
        if self.len == 0 {
            return Poll::Ready(None);
        }
        self.shared.awaiter.register(cx.waker());
        // At most one look at each member per poll: a member that keeps
        // waking itself cannot hold the caller here.
        for _ in 0..self.len {
            // SAFETY: the set alone takes from its queue (`&mut self`).
            let wake = match unsafe { self.shared.ready.pop() } {
                Pop::Item(Queued(wake)) => wake,
                // A wake between its two steps wakes the awaiter after it.
                Pop::Empty | Pop::Busy => return Poll::Pending,
            };
            wake.queued.store(false, Ordering::Release);
            let slot = wake.slot;
            // A stale wake of an earlier set's slot (after `detach_all`).
            let Some(place) = self
                .slots
                .get_mut(slot)
                .filter(|p| Arc::ptr_eq(&p.wake, &wake))
            else {
                continue;
            };
            let Some(member) = place.member.as_mut() else {
                continue;
            };
            let mut member_cx = Context::from_waker(&place.waker);
            if let Poll::Ready(out) = Pin::new(&mut member.join).poll(&mut member_cx) {
                let id = member.abort.id();
                place.member = None;
                self.free.push(slot);
                self.len -= 1;
                return Poll::Ready(Some(out.map(|value| (id, value))));
            }
        }
        // Every look found an unfinished member that was woken again: give
        // other tasks a turn and come back.
        cx.waker().wake_by_ref();
        Poll::Pending
    }

    /// Every task's output, in finishing order.
    ///
    /// # Panics
    ///
    /// When a task panicked (its panic is resumed) or was cancelled.
    #[expect(clippy::panic, reason = "documented under `# Panics`")]
    pub async fn join_all(mut self) -> Vec<T> {
        let mut outputs = Vec::with_capacity(self.len);
        while let Some(out) = self.join_next().await {
            match out {
                Ok(value) => outputs.push(value),
                Err(error) => match error.try_into_panic() {
                    Ok(panic) => std::panic::resume_unwind(panic),
                    Err(error) => panic!("`JoinSet::join_all`: {error}"),
                },
            }
        }
        outputs
    }

    /// Cancels every task in the set; they stay in it, and `join_next` gives
    /// each one's cancellation (or its output, if it finished first).
    pub fn abort_all(&mut self) {
        for member in self.slots.iter().filter_map(|p| p.member.as_ref()) {
            member.abort.abort();
        }
    }

    /// Takes every task out of the set and lets each run to its end. The
    /// slots go with them, so a detached task's later wake meets none.
    pub fn detach_all(&mut self) {
        for place in self.slots.drain(..) {
            if let Some(member) = place.member {
                member.join.detach();
            }
        }
        self.free.clear();
        self.len = 0;
        // SAFETY: the set alone takes from its queue (`&mut self`).
        unsafe { self.shared.ready.drain(drop) };
    }

    /// Cancels every task and waits until each one is out of the set.
    pub async fn shutdown(&mut self) {
        self.abort_all();
        while self.join_next().await.is_some() {}
    }
}

impl<T, F> FromIterator<F> for JoinSet<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    fn from_iter<I: IntoIterator<Item = F>>(iter: I) -> Self {
        let mut set = Self::new();
        for future in iter {
            set.spawn(future);
        }
        set
    }
}

impl<T, F> Extend<F> for JoinSet<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    fn extend<I: IntoIterator<Item = F>>(&mut self, iter: I) {
        for future in iter {
            self.spawn(future);
        }
    }
}

impl<T> fmt::Debug for JoinSet<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("JoinSet").field("len", &self.len).finish()
    }
}
