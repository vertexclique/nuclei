//! A reusable barrier: every group of `n` waits completes together.
//!
//! The barrier word packs the current generation's arrivals (low 32 bits)
//! with the generation (high 32 bits) in one `AtomicU64`, so an arrival
//! learns its generation and whether it completes it in one
//! compare-and-swap. On 64-bit targets that is a word; on armv7 and wasm32
//! it is their native 64-bit atomic, never an emulated wider one. Waiters
//! carry their generation; a drain pass grants every waiter of a finished
//! one. A waiter would miss its generation only if 2^32 generations
//! finished while one drain pass stood still.

#![allow(unsafe_code)]

use core::fmt;
use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use super::internal::{AtomicU64, Ordering, UnsafeCell};
use super::list::List;
use super::queue::{Arrivals, Policy, QUEUED, Step, WaitQueue};
use super::waiter::{Cancel, Wait, WakeList, release_queued};

const COUNT_MASK: u64 = u32::MAX as u64;
const ONE_GENERATION: u64 = 1 << 32;

fn generation(word: u64) -> u32 {
    (word >> 32) as u32
}

struct Waiting {
    list: List,
    /// The generation every waiter in `list` belongs to.
    generation: u32,
}

/// A barrier that releases its waiters in groups of `n`.
///
/// The `n`-th [`wait`](Self::wait) of a generation completes at once as
/// its leader and wakes the other `n - 1`; the barrier then starts the
/// next generation. A wait dropped before its generation completes stays
/// counted: the generation completes once `n` parties have arrived, and
/// the dropped one is simply not woken. (Taking the arrival back would
/// race with the arrival that completes the generation.)
///
/// ```
/// use nuclei::sync::Barrier;
/// use std::future::Future;
/// use std::pin::pin;
/// use std::task::{Context, Waker};
///
/// let barrier = Barrier::new(2);
/// let mut cx = Context::from_waker(Waker::noop());
/// let mut first = pin!(barrier.wait());
/// assert!(first.as_mut().poll(&mut cx).is_pending());
/// let mut second = pin!(barrier.wait());
/// assert!(second.as_mut().poll(&mut cx).is_ready());
/// assert!(first.poll(&mut cx).is_ready());
/// ```
pub struct Barrier {
    n: u32,
    word: AtomicU64,
    queue: WaitQueue,
    waiting: UnsafeCell<Waiting>,
}

// SAFETY: the waiting list is touched only by the drain role's holder.
unsafe impl Send for Barrier {}
// SAFETY: as above.
unsafe impl Sync for Barrier {}

impl Barrier {
    loom_const_fn! {
        /// A barrier for groups of `n` waits; `0` acts as `1`.
        ///
        /// # Panics
        ///
        /// When `n` exceeds `u32::MAX`.
        pub fn new(n: usize) -> Self {
            assert!(n <= u32::MAX as usize, "a barrier holds at most u32::MAX participants");
            Self {
                n: if n == 0 { 1 } else { n as u32 },
                word: AtomicU64::new(0),
                queue: WaitQueue::new(0),
                waiting: UnsafeCell::new(Waiting { list: List::new(), generation: 0 }),
            }
        }
    }

    /// A future that completes when this wait's generation is complete.
    pub fn wait(&self) -> BarrierWait<'_> {
        BarrierWait {
            barrier: self,
            generation: None,
            wait: Wait::new(),
        }
    }

    /// Counts an arrival: `Ok` when it completes its generation, else the
    /// generation it waits in.
    fn arrive(&self) -> Result<(), u32> {
        let mut word = self.word.load(Ordering::Acquire);
        loop {
            let leader = (word & COUNT_MASK) + 1 == u64::from(self.n);
            let next = if leader {
                (word & !COUNT_MASK).wrapping_add(ONE_GENERATION)
            } else {
                word + 1
            };
            match self
                .word
                .compare_exchange_weak(word, next, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) if leader => return Ok(()),
                Ok(_) => return Err(generation(word)),
                Err(actual) => word = actual,
            }
        }
    }
}

impl Policy for Barrier {
    fn queue(&self) -> &WaitQueue {
        &self.queue
    }

    fn pass(&self, arrivals: Arrivals, woken: &mut WakeList<'_>, _wake_front: bool) -> usize {
        let pool = &self.queue;
        let current = generation(self.word.load(Ordering::Acquire));
        // SAFETY: only the drain role's holder runs a pass.
        self.waiting.with_mut(|waiting| unsafe {
            let waiting = &mut *waiting;
            if waiting.generation != current {
                while let Some(node) = waiting.list.pop_front() {
                    woken.grant(node);
                }
                waiting.generation = current;
            }
            for node in arrivals {
                if node.as_ref().is_cancelled() {
                    release_queued(node, pool);
                } else if node.as_ref().payload() as u32 != current {
                    woken.grant(node);
                } else {
                    waiting.list.push_back(node);
                }
            }
            waiting.list.tidy(&self.queue);
            if waiting.list.front(pool).is_none() {
                QUEUED
            } else {
                0
            }
        })
    }
}

impl Drop for Barrier {
    fn drop(&mut self) {
        let pool = &self.queue;
        // SAFETY: `&mut self` rules out a concurrent pass.
        self.waiting
            .with_mut(|waiting| unsafe { (*waiting).list.clear(pool) });
    }
}

impl fmt::Debug for Barrier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Barrier").field("n", &self.n).finish()
    }
}

/// What a completed [`Barrier::wait`] reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BarrierWaitResult {
    leader: bool,
}

impl BarrierWaitResult {
    /// True for exactly one wait per generation: the one that completed
    /// it.
    pub fn is_leader(&self) -> bool {
        self.leader
    }
}

/// The future [`Barrier::wait`] returns.
#[must_use = "futures do nothing unless polled"]
pub struct BarrierWait<'a> {
    barrier: &'a Barrier,
    /// The generation this wait arrived in, once it has.
    generation: Option<u32>,
    wait: Wait,
}

impl Future for BarrierWait<'_> {
    type Output = BarrierWaitResult;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<BarrierWaitResult> {
        let this = self.get_mut();
        let barrier = this.barrier;
        if this.generation.is_none() {
            match barrier.arrive() {
                Ok(()) => {
                    barrier.transition(|state| {
                        if state & QUEUED != 0 {
                            Step::Drain(state)
                        } else {
                            Step::Set(state)
                        }
                    });
                    return Poll::Ready(BarrierWaitResult { leader: true });
                }
                Err(generation) => {
                    this.generation = Some(generation);
                    barrier.enqueue(&mut this.wait, generation as usize, cx.waker());
                    return this
                        .wait
                        .poll(&barrier.queue, None)
                        .map(|_| BarrierWaitResult { leader: false });
                }
            }
        }
        this.wait
            .poll(&barrier.queue, Some(cx.waker()))
            .map(|_| BarrierWaitResult { leader: false })
    }
}

impl Drop for BarrierWait<'_> {
    fn drop(&mut self) {
        // The arrival stays counted (see `Barrier`); only the node goes.
        if let Cancel::Withdrawn(_) = self.wait.cancel(&self.barrier.queue) {
            self.barrier.withdrawn(false);
        }
    }
}

impl fmt::Debug for BarrierWait<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BarrierWait")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::sync::test_support::{Polled, block_on};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as StdOrdering};

    #[test]
    fn each_generation_releases_together_with_one_leader() {
        let barrier = Barrier::new(3);
        for _ in 0..3 {
            let mut a = Polled::new(barrier.wait());
            a.pending();
            let mut b = Polled::new(barrier.wait());
            b.pending();
            let leader = Polled::new(barrier.wait()).ready();
            assert!(leader.is_leader());
            assert_eq!((a.wakes(), b.wakes()), (1, 1));
            assert!(!a.ready().is_leader());
            assert!(!b.ready().is_leader());
        }
    }

    #[test]
    fn a_dropped_wait_stays_counted() {
        let barrier = Barrier::new(2);
        Polled::new(barrier.wait()).pending();
        let leader = Polled::new(barrier.wait()).ready();
        assert!(leader.is_leader(), "the dropped arrival completed the pair");
        let mut next = Polled::new(barrier.wait());
        next.pending();
        assert!(Polled::new(barrier.wait()).ready().is_leader());
        assert!(!next.ready().is_leader());
    }

    #[test]
    fn threads_meet_at_every_generation() {
        const ROUNDS: usize = if cfg!(miri) { 3 } else { 200 };
        let barrier = Arc::new(Barrier::new(4));
        let leaders = Arc::new(AtomicUsize::new(0));
        let threads: Vec<_> = (0..4)
            .map(|_| {
                let (barrier, leaders) = (Arc::clone(&barrier), Arc::clone(&leaders));
                std::thread::spawn(move || {
                    for _ in 0..ROUNDS {
                        if block_on(barrier.wait()).is_leader() {
                            leaders.fetch_add(1, StdOrdering::SeqCst);
                        }
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("worker");
        }
        assert_eq!(leaders.load(StdOrdering::SeqCst), ROUNDS);
    }
}
