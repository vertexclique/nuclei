//! The monitor: one thread per runtime that hands a worker's shard over when
//! the worker stays in one poll past the threshold.
//!
//! While a worker is out of its park the monitor ticks every quarter of the
//! threshold and reads each worker's word: a read equal to the last while
//! the worker is outside its shard extends a streak, and the fourth in a row
//! (the poll ran at least four ticks and under five) takes the shard. With
//! every worker parked it sleeps, and a worker that ends its park wakes it:
//! the monitor says it sleeps and then looks at the parks again, the worker
//! ends its park and then looks at the monitor, with a sequentially
//! consistent fence on both sides.
//!
//! The take (proofs/tla/handoff): the claim pending, the heavy fence, the
//! word read again; still the same, the runner is outside and comes back
//! only through its claim, so the claim goes from pending to taken, unless
//! the runner cancelled it first. The shard is then the monitor's: the
//! runnables in its own queues go back to their schedules, its old runner is
//! let off its core, and the shard goes, with a fresh slot, to a spare
//! thread (one is started when none waits; if that fails the shard waits
//! for the next tick, no worse off than with no handoff).

use core::pin::pin;
use std::sync::{Arc, OnceLock, Weak};
use std::thread::Thread;
use std::time::Duration;

use kovan_queue::seg_queue::SegQueue;

use super::life::Life;
use super::slot::{self, Slot};
use crate::rt::runtime::Inner;
use crate::rt::shard::{Shard, Shared};
use crate::sync::internal::{AtomicBool, AtomicPtr, Ordering, fence};

/// Reads of one word in a row that make a blocked poll.
const STREAK: u32 = 4;

/// A shard on its way to another thread.
pub(crate) struct Owned(pub(crate) Arc<Shard>);

// SAFETY: a shard goes to another thread only at a hand-over of the
// protocol, once its runner touches it no more (the monitor took it, or a
// worker thread has not run it yet), and only its runner touches its cells;
// its last drop runs where its last holder lets go, after every runner is
// done with it.
unsafe impl Send for Owned {}

/// A shard the monitor may take, without keeping it alive.
struct Held(Weak<Shard>);

// SAFETY: upgraded only by the monitor, which touches the shard only after
// its take succeeded (the shard is then its own); see `Owned`.
unsafe impl Send for Held {}
// SAFETY: as above.
unsafe impl Sync for Held {}

/// A thread running a worker shard: its slot and its life.
#[derive(Clone)]
pub(crate) struct Runner {
    pub(crate) slot: Arc<Slot>,
    pub(crate) life: Arc<Life>,
}

/// A worker place the monitor watches.
struct Place {
    shared: Arc<Shared>,
    shard: Held,
    first: Runner,
}

/// A shard handed to a thread: the place it fills, and the slot its new
/// runner runs it with.
pub(crate) struct Gift {
    pub(crate) place: usize,
    pub(crate) shard: Owned,
    pub(crate) slot: Arc<Slot>,
}

/// A thread waiting for a shard to run.
pub(crate) struct Spare {
    thread: Thread,
    life: Arc<Life>,
    gift: AtomicPtr<Gift>,
}

impl Spare {
    /// The calling thread, as a spare of its life.
    pub(crate) fn here(life: Arc<Life>) -> Arc<Self> {
        Arc::new(Self {
            thread: std::thread::current(),
            life,
            gift: AtomicPtr::new(core::ptr::null_mut()),
        })
    }

    fn take_gift(&self) -> Option<Gift> {
        let gift = self.gift.swap(core::ptr::null_mut(), Ordering::Acquire);
        // SAFETY: a gift is a box the monitor gave up to this spare alone.
        (!gift.is_null()).then(|| *unsafe { Box::from_raw(gift) })
    }
}

/// A runtime's watch over its workers.
pub(crate) struct Monitor {
    /// A poll this long is a blocked one.
    after: Duration,
    /// The runners' fences are full ones: the process has no heavy fence.
    full_fence: bool,
    places: Box<[OnceLock<Place>]>,
    spares: SegQueue<Arc<Spare>>,
    /// The monitor sleeps: every worker parked.
    asleep: AtomicBool,
    thread: OnceLock<Thread>,
    closed: AtomicBool,
    /// Wakes the threads waiting for their bound tasks once closed.
    closing: crate::sync::Notify,
    /// The cores of the process, which a blocked thread may run on.
    cores: Vec<usize>,
}

impl Monitor {
    /// The watch of a runtime with room for `places` workers, a poll of
    /// `after` a blocked one.
    pub(crate) fn new(after: Duration, places: usize, cores: Vec<usize>) -> Self {
        Self {
            after,
            full_fence: !super::fence::available(),
            places: (0..places).map(|_| OnceLock::new()).collect(),
            spares: SegQueue::new(),
            asleep: AtomicBool::new(false),
            thread: OnceLock::new(),
            closed: AtomicBool::new(false),
            closing: crate::sync::Notify::new(),
            cores,
        }
    }

    /// A slot for a new runner.
    pub(crate) fn slot(&self) -> Arc<Slot> {
        Arc::new(Slot::new(self.full_fence))
    }

    /// Worker place `index` started, its shard run by `first`.
    pub(crate) fn started(&self, index: usize, shard: &Arc<Shard>, first: Runner) {
        if let Some(place) = self.places.get(index) {
            let _ = place.set(Place {
                shared: Arc::clone(&shard.shared),
                shard: Held(Arc::downgrade(shard)),
                first,
            });
        }
        self.worker_woke();
    }

    /// A worker's park ended, or a worker started: an asleep monitor wakes.
    pub(crate) fn worker_woke(&self) {
        fence(Ordering::SeqCst);
        if self.asleep.load(Ordering::SeqCst)
            && let Some(thread) = self.thread.get()
        {
            thread.unpark();
        }
    }

    /// The runtime shuts down: the monitor stops, and so do the spares.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.closing.notify_waiters();
        if let Some(thread) = self.thread.get() {
            thread.unpark();
        }
        while let Ok(spare) = self.spares.try_pop() {
            spare.thread.unpark();
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Waits, as spare `me`, for a shard to run: `None` once the runtime
    /// shuts down.
    pub(crate) fn wait_for_shard(&self, me: &Arc<Spare>) -> Option<Gift> {
        self.spares.push(Arc::clone(me));
        loop {
            if let Some(gift) = me.take_gift() {
                return Some(gift);
            }
            if self.is_closed() {
                // A gift handed over just before the close is run, then the
                // spare goes.
                return me.take_gift();
            }
            std::thread::park();
        }
    }

    /// Ready once the runtime shuts down, or once `home` has no live task:
    /// what a thread detached from its shard waits for on its own shard.
    pub(crate) async fn home_done(&self, home: &super::home::Home) {
        let mut closing = pin!(self.closing.notified());
        closing.as_mut().enable();
        if self.is_closed() {
            return;
        }
        core::future::poll_fn(|cx| {
            if home.poll_drained(cx).is_ready() {
                return core::task::Poll::Ready(());
            }
            closing.as_mut().poll(cx)
        })
        .await;
    }

    /// Whether every started worker is parked.
    fn all_parked(&self) -> bool {
        self.places
            .iter()
            .filter_map(OnceLock::get)
            .all(|place| place.shared.is_parked() || place.shared.is_stopped())
    }
}

/// What the monitor knows of one place.
struct Watch {
    runner: Runner,
    last: u64,
    streak: u32,
    longest: u32,
}

/// The monitor's thread: watches the workers of the runtime of `runtime`
/// until it shuts down.
pub(crate) fn run(monitor: &Arc<Monitor>, runtime: &Weak<Inner>) {
    let _ = monitor.thread.set(std::thread::current());
    let tick = (monitor.after / STREAK).max(Duration::from_micros(100));
    let mut watches: Vec<Option<Watch>> = (0..monitor.places.len()).map(|_| None).collect();
    let mut waiting: Option<Gift> = None;
    // Hands a gift on, the place's watch following its new runner.
    let deliver = |watches: &mut Vec<Option<Watch>>, gift: Gift| match give(monitor, runtime, gift)
    {
        Ok((place, life)) => {
            if let Some(watch) = watches[place].as_mut() {
                watch.runner.life = life;
            }
            None
        }
        Err(gift) => Some(gift),
    };
    while !monitor.is_closed() {
        if let Some(gift) = waiting.take() {
            waiting = deliver(&mut watches, gift);
        }
        if waiting.is_none() && monitor.all_parked() {
            monitor.asleep.store(true, Ordering::SeqCst);
            fence(Ordering::SeqCst);
            if monitor.all_parked() && !monitor.is_closed() {
                std::thread::park();
            }
            monitor.asleep.store(false, Ordering::SeqCst);
            watches
                .iter_mut()
                .flatten()
                .for_each(|watch| watch.streak = 0);
            continue;
        }
        // A tick at least: a park may end early (spuriously, or woken), and
        // reads closer than a tick would take a shorter poll for a long one
        // (proofs/lean, `Handoff`: reads a tick apart or more).
        let next = std::time::Instant::now() + tick;
        while let Some(left) = next.checked_duration_since(std::time::Instant::now())
            && !left.is_zero()
            && !monitor.is_closed()
        {
            std::thread::park_timeout(left);
        }
        for (index, place) in monitor.places.iter().enumerate() {
            let Some(place) = place.get() else {
                continue;
            };
            let watch = watches[index].get_or_insert_with(|| Watch {
                runner: place.first.clone(),
                last: slot::inside(0),
                streak: 0,
                longest: 0,
            });
            let word = watch.runner.slot.word();
            if slot::is_outside(word) && word == watch.last {
                watch.streak += 1;
            } else {
                if watch.streak > watch.longest {
                    watch.longest = watch.streak;
                    place.shared.counters.longest_poll(tick * watch.streak);
                }
                watch.streak = 0;
            }
            watch.last = word;
            if watch.streak < STREAK || waiting.is_some() {
                continue;
            }
            watch.streak = 0;
            if let Some(gift) = take(monitor, place, index, watch, word) {
                place.shared.counters.longest_poll(tick * STREAK);
                waiting = deliver(&mut watches, gift);
            }
        }
    }
}

/// Takes the shard of `place` from its runner, seen at `word` on the last
/// reads; on success, the gift for its next runner.
fn take(
    monitor: &Monitor,
    place: &Place,
    index: usize,
    watch: &mut Watch,
    word: u64,
) -> Option<Gift> {
    let old = &watch.runner;
    old.slot.claim();
    if monitor.full_fence {
        fence(Ordering::SeqCst);
    } else if !super::fence::heavy() {
        // The heavy fence failed, which `available` said it would not: no
        // take may rely on it.
        old.slot.cancel();
        return None;
    }
    if old.slot.word() != word {
        old.slot.cancel();
        return None;
    }
    if !old.slot.take() {
        // The runner came back first.
        return None;
    }
    // The shard is the monitor's. The blocked runner still holds it.
    let shard = place.shard.0.upgrade()?;
    old.life.detach();
    old.life.unpin(&monitor.cores);
    shard.hand_over_queues();
    shard.shared.counters.handed_off();
    let slot = monitor.slot();
    watch.runner = Runner {
        slot: Arc::clone(&slot),
        // The thread that takes it, set by `give`.
        life: Arc::clone(&old.life),
    };
    watch.last = slot::inside(0);
    Some(Gift {
        place: index,
        shard: Owned(shard),
        slot,
    })
}

/// Hands `gift` to a spare, or to a thread started for it: the place and
/// the life of the thread that took it, or the gift back if none could.
fn give(monitor: &Monitor, runtime: &Weak<Inner>, gift: Gift) -> Result<(usize, Arc<Life>), Gift> {
    let place = gift.place;
    if let Ok(spare) = monitor.spares.try_pop() {
        let life = Arc::clone(&spare.life);
        spare
            .gift
            .store(Box::into_raw(Box::new(gift)), Ordering::Release);
        spare.thread.unpark();
        return Ok((place, life));
    }
    let Some(inner) = runtime.upgrade() else {
        return Err(gift);
    };
    let life = Arc::new(Life::new());
    crate::rt::workers::Workers::spawn_runner(&inner, gift, Arc::clone(&life))?;
    Ok((place, life))
}
