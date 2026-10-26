//! [`JoinSet`]: a set of tasks whose outputs come back in the order the tasks
//! finish.
//!
//! Each member is polled with a waker of its own, which queues the member's
//! slot when its task wakes it and wakes the set's awaiter: `join_next` polls
//! only members that were woken, never the whole set. A slot reused by a later
//! member may get a stale wake from the member before it, which costs one
//! spurious poll and nothing else.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, Waker};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Wake;

use crate::sync::atomic_waker::AtomicWaker;
use crate::task::{AbortHandle, Id, Join, JoinError, Task};

/// What the members' wakers share with the set.
struct Shared {
    /// Slots of woken members, in wake order.
    ready: kovan_queue::seg_queue::SegQueue<usize>,
    /// The task awaiting `join_next`.
    awaiter: AtomicWaker,
}

/// A member's waker: queues its slot once per wake.
struct MemberWake {
    shared: Arc<Shared>,
    slot: usize,
    queued: AtomicBool,
}

impl Wake for MemberWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if !self.queued.swap(true, Ordering::AcqRel) {
            self.shared.ready.push(self.slot);
            self.shared.awaiter.wake();
        }
    }
}

struct Member<T> {
    join: Join<T>,
    wake: Arc<MemberWake>,
    waker: Waker,
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
    members: Vec<Option<Member<T>>>,
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
            members: Vec::new(),
            free: Vec::new(),
            len: 0,
            shared: Arc::new(Shared {
                ready: kovan_queue::seg_queue::SegQueue::new(),
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
        let slot = self.free.pop().unwrap_or(self.members.len());
        let wake = Arc::new(MemberWake {
            shared: Arc::clone(&self.shared),
            slot,
            // Queued at once: the first `join_next` polls the new member.
            queued: AtomicBool::new(true),
        });
        let member = Member {
            join: task.join(),
            waker: Waker::from(Arc::clone(&wake)),
            wake,
            abort: abort.clone(),
        };
        if slot == self.members.len() {
            self.members.push(Some(member));
        } else {
            self.members[slot] = Some(member);
        }
        self.len += 1;
        self.shared.ready.push(slot);
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
            let Some(slot) = self.shared.ready.pop() else {
                return Poll::Pending;
            };
            let Some(member) = self.members.get_mut(slot).and_then(Option::as_mut) else {
                continue;
            };
            member.wake.queued.store(false, Ordering::Release);
            let mut member_cx = Context::from_waker(&member.waker);
            if let Poll::Ready(out) = Pin::new(&mut member.join).poll(&mut member_cx) {
                let id = member.abort.id();
                self.members[slot] = None;
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
        for member in self.members.iter().flatten() {
            member.abort.abort();
        }
    }

    /// Takes every task out of the set and lets each run to its end.
    pub fn detach_all(&mut self) {
        for member in self.members.drain(..).flatten() {
            member.join.detach();
        }
        self.free.clear();
        self.len = 0;
        while self.shared.ready.pop().is_some() {}
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
