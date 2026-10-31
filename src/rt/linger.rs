//! Lingers: the work a handle leaves as it drops, which must be done whatever
//! happens to the handle and to the runtime it ran on: the bytes a socket's
//! write-behind holds, a file's writes behind, a gone shard's sources that
//! still hold bytes. Each is bounded by its own deadline (the runtime's
//! [`write_behind_linger`](crate::runtime::Builder::write_behind_linger), see
//! [`time`]) and done by the runtime the thread that hands it is inside (or
//! the process default runtime, if it runs):
//!
//! - while one of its workers takes tasks, as a task there;
//! - with none, on the shard of the thread that hands it, which runs it at
//!   its next `block_on`;
//! - once the runtime's drop began, in the runtime's list, which the drop
//!   runs on its own thread before it returns.
//!
//! A linger's task dropped before it ended (its shard stopped as the runtime
//! drops) hands what it had left back, so it goes on where it can. A task
//! that waits on a completion as its shard goes (a file's write a ring's
//! keeper or the blocking pool finishes) is dropped only once that
//! completion wakes it, on the completing thread, inside its runtime still
//! (`sched::Counted`): the linger's task hands its work back then, and the
//! task that holds a handle hands that handle's linger. So the runtime
//! counts every linger handed to it until it ended, and its drop waits,
//! running its list meanwhile and bounded by the linger's time, until none
//! is left, no file write its workers issued is still to wake its task
//! (`fs::call::WriteCall`), and no task woken on a worker's stopped shard is
//! still to be dropped. One handed where no runtime runs, or after a
//! runtime's drop is over, runs on a thread of its own with a runtime of its
//! own: no runtime is started behind the user's back
//! (proofs/tla/going/Linger.tla).

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::sync::{Arc, Weak};

use kovan_queue::seg_queue::SegQueue;

use super::runtime::Inner;
use crate::sync::atomic_waker::AtomicWaker;
use crate::sync::internal::{AtomicBool, AtomicUsize, Ordering, fence};

/// How long a linger may take: the `write_behind_linger` of the runtime this
/// thread is inside (where the linger runs), or the default.
pub(crate) fn time() -> std::time::Duration {
    super::runtime::io_knobs::current().linger
}

/// A linger's work, boxed so it moves between threads and runtimes.
pub(crate) type Work = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Hands `work` to the runtime this thread is inside, or to the default
/// runtime if it runs; with neither, to a thread of its own.
pub(crate) fn hand<F>(work: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    let work: Work = Box::pin(work);
    match super::runtime::running_inner() {
        Some(inner) => inner.take_linger(work),
        None => alone(work),
    }
}

/// A runtime's lingers: the count of those not ended, and the list of those
/// no worker of it takes.
pub(crate) struct Lingering {
    list: SegQueue<Work>,
    /// Lingers handed to the runtime that have not ended: running as tasks,
    /// held by a task that waits to hand them back, or in the list.
    active: AtomicUsize,
    /// The drop's run, woken as a linger comes to the list or ends.
    waker: AtomicWaker,
    /// The runtime's drop began: lingers wait in the list from now on.
    closing: AtomicBool,
    /// The drop is over: one handed later runs alone.
    over: AtomicBool,
}

impl Lingering {
    pub(crate) fn new() -> Self {
        Self {
            list: SegQueue::new(),
            active: AtomicUsize::new(0),
            waker: AtomicWaker::new(),
            closing: AtomicBool::new(false),
            over: AtomicBool::new(false),
        }
    }

    /// Lingers handed to the runtime that have not ended.
    pub(crate) fn active(&self) -> usize {
        self.active.load(Ordering::SeqCst)
    }

    /// A linger of the runtime ended (or went to a thread of its own).
    fn ended(&self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
        self.waker.wake();
    }

    /// Wakes the drop's run to look again (a worker's shard settled).
    pub(crate) fn nudge(&self) {
        self.waker.wake();
    }

    /// The runtime's drop begins: its workers take no linger from now on.
    pub(crate) fn close(&self) {
        self.closing.store(true, Ordering::SeqCst);
    }

    pub(crate) fn is_closing(&self) -> bool {
        self.closing.load(Ordering::SeqCst)
    }

    /// The drop's run is over (or was given up): lingers go alone.
    pub(crate) fn is_over(&self) -> bool {
        self.over.load(Ordering::SeqCst)
    }

    /// Puts `work` in the list; one that finds the drop's run over takes it
    /// back out and runs it alone, as does the drop's last look.
    pub(crate) fn push(&self, work: Work) {
        self.list.push(work);
        self.waker.wake();
        // Pairs with the fence of `end`: either this sees the run over, or
        // the drop's last look finds the work.
        fence(Ordering::SeqCst);
        if self.over.load(Ordering::SeqCst) {
            self.run_rest_alone();
        }
    }

    /// The drop is over: what is left, and what comes after, runs alone.
    pub(crate) fn end(&self) {
        self.over.store(true, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        self.run_rest_alone();
    }

    fn run_rest_alone(&self) {
        while let Ok(work) = self.list.try_pop() {
            self.ended();
            alone(work);
        }
    }
}

/// The lingers of a runtime's list, run together until the runtime's are
/// all done ([`Inner::lingers_done`]).
pub(crate) struct RunAll<'a> {
    inner: &'a Inner,
    running: Vec<Work>,
}

impl Future for RunAll<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        let lingering = this.inner.lingering();
        // Before the looks: a linger handed back or ended after them wakes it.
        lingering.waker.register(cx.waker());
        loop {
            while let Ok(work) = lingering.list.try_pop() {
                this.running.push(work);
            }
            this.running.retain_mut(|work| {
                let ended = work.as_mut().poll(cx).is_ready();
                if ended {
                    lingering.ended();
                }
                !ended
            });
            // A linger that ended may have handed another (a drop in it).
            if lingering.list.is_empty() {
                break;
            }
        }
        if this.inner.lingers_done() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

impl Inner {
    /// Whether nothing is left that may hand the runtime a linger, nor a
    /// linger it took not ended: each worker's shard settled (looked at
    /// first: a task its look waits for to drop hands its linger before),
    /// and no linger active.
    pub(crate) fn lingers_done(&self) -> bool {
        self.workers.shards().all(|shard| shard.settled()) && self.lingering().active() == 0
    }

    /// Runs the runtime's list until [`lingers_done`](Self::lingers_done):
    /// a future the drop runs on its own thread.
    pub(crate) fn run_lingers(&self) -> RunAll<'_> {
        RunAll {
            inner: self,
            running: Vec::new(),
        }
    }

    /// Takes a new linger, counted until it ends: as a task of a worker
    /// that takes tasks, else on this thread's shard, until the runtime's
    /// drop began; from then on into the list the drop runs.
    pub(crate) fn take_linger(self: &Arc<Self>, work: Work) {
        self.lingering().active.fetch_add(1, Ordering::SeqCst);
        self.route_linger(work, true);
    }

    /// Where a linger of this runtime goes now (`here`: this thread's shard
    /// may take it).
    fn route_linger(self: &Arc<Self>, work: Work, here: bool) {
        let lingering = self.lingering();
        if !lingering.is_closing() {
            let guarded = Guarded {
                work: Some(work),
                owner: Arc::downgrade(self),
            };
            match super::runtime::spawn_linger(self, guarded, here) {
                Ok(()) => return,
                Err(guarded) => return guarded.into_list(),
            }
        }
        lingering.push(work);
    }
}

/// A linger run as a task: dropped before it ended, it hands what it left
/// back to its runtime (or to a thread of its own, the runtime gone).
pub(crate) struct Guarded {
    work: Option<Work>,
    owner: Weak<Inner>,
}

impl Guarded {
    /// What is left goes to the runtime's list, or alone with no runtime.
    fn into_list(mut self) {
        let Some(work) = self.work.take() else {
            return;
        };
        match self.owner.upgrade() {
            Some(inner) => inner.lingering().push(work),
            None => alone(work),
        }
    }
}

impl Future for Guarded {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let this = self.get_mut();
        let Some(work) = this.work.as_mut() else {
            return Poll::Ready(());
        };
        let poll = work.as_mut().poll(cx);
        if poll.is_ready() {
            this.work = None;
            if let Some(inner) = this.owner.upgrade() {
                inner.lingering().ended();
            }
        }
        poll
    }
}

impl Drop for Guarded {
    fn drop(&mut self) {
        let Some(work) = self.work.take() else {
            return;
        };
        // Never on this thread's shard: it may be the one going.
        match self.owner.upgrade() {
            Some(inner) => inner.route_linger(work, false),
            None => alone(work),
        }
    }
}

/// Runs `work` on a thread of its own, with a runtime of its own (no worker,
/// its blocking pool for a file's calls); where no thread can start, on this
/// one, as a last resort said once.
// vertexia: a thread per linger handed where no runtime runs; one thread for
// them all if that is ever common.
pub(crate) fn alone(work: Work) {
    let (give, take) = crate::sync::oneshot::channel::<Work>();
    let started = std::thread::Builder::new()
        .name("nuclei-linger".into())
        .spawn(move || {
            if let Ok(work) = take.blocking_recv() {
                run_with_own_runtime(work);
            }
        });
    let unsent = match started {
        Ok(_) => give.send(work).err(),
        Err(err) => {
            log::warn!(
                "no thread could start to deliver the bytes a dropped handle left ({err}): they are delivered on the thread that dropped it"
            );
            Some(work)
        }
    };
    if let Some(work) = unsent {
        run_with_own_runtime(work);
    }
}

fn run_with_own_runtime(work: Work) {
    match crate::Runtime::builder().worker_threads(0).build() {
        Ok(rt) => rt.block_on(work),
        Err(err) => log::error!(
            "the bytes a dropped handle left were not delivered: no runtime could start for them ({err})"
        ),
    }
}
