//! The monitor: one thread per runtime that hands a worker's shard over when
//! the worker stays in one poll past the threshold.
//!
//! While a worker is out of its park the monitor ticks every quarter of the
//! threshold and reads each worker's word: a read equal to the last while
//! the worker is outside its shard extends a streak, and the fourth in a row
//! (the poll ran at least four ticks and under five) takes the shard. With
//! every worker parked it sleeps, and a worker that ends its park, or backs
//! out of it, wakes it: the monitor says it sleeps and then looks at the
//! parks again, the worker ends its park (or backs out) and then looks at
//! the monitor, with a sequentially consistent fence on both sides
//! (proofs/tla/handoff/Asleep.tla).
//!
//! The take (proofs/tla/handoff): a reference to the shard first (the
//! monitor holds it only weakly, and once the take succeeds its runner may
//! let go of its own at once, `TakeRef.tla`), the claim pending, the heavy
//! fence, the word read again; still the same, the runner is outside and
//! comes back only through its claim, so the claim goes from pending to
//! taken, unless the runner cancelled it first. The shard is then the
//! monitor's: the runnables in its own queues go back to their schedules,
//! its old runner is let off its core, and the shard goes, with a fresh
//! slot, to a spare thread (one is started when none waits; if that fails
//! the shard waits for the next tick, no worse off than with no handoff).

use core::pin::pin;
use std::sync::{Arc, OnceLock, Weak};
use std::thread::Thread;
use std::time::Duration;

use kovan_queue::seg_queue::SegQueue;

use super::bell::Bell;
use super::life::Life;
use super::slot::{self, Slot};
use crate::rt::runtime::Inner;
use crate::rt::shard::{Shard, Shared};
use crate::sync::internal::{AtomicBool, AtomicPtr, AtomicUsize, Ordering, fence};

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

impl Drop for Spare {
    /// A gift handed to a spare that had seen the close and gone: its shard
    /// goes with the spare, never kept for good, inside its runtime (the
    /// spare's thread runs inside it, and the monitor lets go of the one it
    /// pops through `let_go`).
    fn drop(&mut self) {
        drop(self.take_gift());
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
    bell: Bell,
    closed: AtomicBool,
    /// Wakes the threads waiting for their bound tasks once closed.
    closing: crate::sync::Notify,
    /// The cores of the process, which a blocked thread may run on.
    cores: Vec<usize>,
    /// Told of each poll seen past the threshold.
    long_poll: Option<crate::rt::tuning::TaskHook>,
    /// Polls taken from their thread that still run there on no shard a
    /// paused clock looks at: work in flight it waits for, as for a blocking
    /// job (proofs/tla/clock, `M_detached_uncounted`).
    detached: AtomicUsize,
}

impl Monitor {
    /// The watch of a runtime with room for `places` workers, a poll of
    /// `after` a blocked one, each told to `long_poll`.
    pub(crate) fn new(
        after: Duration,
        places: usize,
        cores: Vec<usize>,
        long_poll: Option<crate::rt::tuning::TaskHook>,
    ) -> Self {
        Self {
            after,
            full_fence: !super::fence::available(),
            places: (0..places).map(|_| OnceLock::new()).collect(),
            spares: SegQueue::new(),
            asleep: AtomicBool::new(false),
            bell: Bell::new(),
            closed: AtomicBool::new(false),
            closing: crate::sync::Notify::new(),
            cores,
            long_poll,
            detached: AtomicUsize::new(0),
        }
    }

    /// Polls taken from their thread that run there still, on no counted
    /// shard.
    pub(crate) fn detached(&self) -> usize {
        self.detached.load(Ordering::SeqCst)
    }

    /// A taken poll is over on its thread, or its thread runs it on a shard
    /// a paused clock counts (its own, a guest): in flight no more.
    pub(crate) fn count_out(&self) {
        self.detached.fetch_sub(1, Ordering::SeqCst);
    }

    /// A poll is taken from its thread, or its thread's guest shard leaves
    /// with the poll still running: in flight.
    pub(crate) fn count_in(&self) {
        self.detached.fetch_add(1, Ordering::SeqCst);
    }

    /// Tells the long-poll hook of the poll of task `task` (0: a poll that
    /// is no task's, which nothing is told of). A hook that panics is let
    /// go of: the watch goes on.
    fn report_long(&self, task: u64) {
        let (Some(hook), Some(id)) = (
            &self.long_poll,
            core::num::NonZeroU64::new(task).map(crate::rt::task::Id),
        ) else {
            return;
        };
        let meta = crate::rt::tuning::TaskMeta::new(id);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| hook(&meta)));
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
        if self.asleep.load(Ordering::SeqCst) {
            self.bell.ring();
        }
    }

    /// The runtime shuts down: the monitor stops, and so do the spares.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        self.closing.notify_waiters();
        self.bell.ring();
        while let Ok(spare) = self.spares.try_pop() {
            spare.thread.unpark();
        }
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Ready once the runtime shuts down, for a waiter that enables it before it looks at
    /// [`is_closed`](Self::is_closed).
    pub(crate) fn closing(&self) -> crate::sync::Notified<'_> {
        self.closing.notified()
    }

    /// The time between the monitor's reads of a word.
    pub(crate) fn tick(&self) -> Duration {
        (self.after / STREAK).max(Duration::from_micros(100))
    }

    /// The life of the first runner of place `index`.
    #[cfg(test)]
    pub(crate) fn first_life(&self, index: usize) -> Option<Arc<Life>> {
        Some(Arc::clone(&self.places.get(index)?.get()?.first.life))
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
    /// The word of the last poll told to the long-poll hook.
    reported: u64,
}

/// The monitor's thread: watches the workers of the runtime of `runtime`
/// until it shuts down.
pub(crate) fn run(monitor: &Arc<Monitor>, runtime: &Weak<Inner>) {
    monitor.bell.bind();
    let tick = monitor.tick();
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
                monitor.bell.wait();
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
            monitor.bell.wait_for(left);
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
                reported: slot::inside(0),
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
            if watch.streak < STREAK {
                continue;
            }
            // Once per word: read before the take, which gives the runner a
            // fresh slot; told after it, so the hook delays no rescue. The
            // word read again: a poll that ended meanwhile is not told under
            // the task that came after it (proofs/tla/longpoll).
            let long = (watch.reported != word).then(|| {
                watch.reported = word;
                let task = watch.runner.slot.task();
                if watch.runner.slot.word() == word {
                    task
                } else {
                    0
                }
            });
            if waiting.is_none() {
                watch.streak = 0;
                if let Some(gift) = take(monitor, runtime, place, index, watch, word) {
                    place.shared.counters.longest_poll(tick * STREAK);
                    waiting = deliver(&mut watches, gift);
                }
            }
            if let Some(task) = long {
                monitor.report_long(task);
            }
        }
    }
    // A shard that waited for a thread goes here.
    if let Some(gift) = waiting.take() {
        let_go(runtime, gift);
    }
}

/// Drops what holds a shard on the monitor's thread inside the shard's
/// runtime: the drop may be the shard's last, and what the handles of its
/// tasks leave then goes to that runtime's drop.
fn let_go<T>(runtime: &Weak<Inner>, held: T) {
    let inner = runtime.upgrade();
    let _runtime = inner.as_ref().map(crate::rt::context::set_runtime);
    drop(held);
}

/// Takes the shard of `place` from its runner, seen at `word` on the last
/// reads; on success, the gift for its next runner.
fn take(
    monitor: &Monitor,
    runtime: &Weak<Inner>,
    place: &Place,
    index: usize,
    watch: &mut Watch,
    word: u64,
) -> Option<Gift> {
    // Held before the claim: once the take below succeeds, the runner may
    // come back at once, find the shard taken and let go of its own
    // reference, the last one but this (proofs/tla/handoff/TakeRef.tla).
    // None left: the shard went, and nothing is claimed.
    let shard = place.shard.0.upgrade()?;
    let old = &watch.runner;
    old.slot.claim();
    if monitor.full_fence {
        fence(Ordering::SeqCst);
    } else if !super::fence::heavy() {
        // The heavy fence failed, which `available` said it would not: no
        // take may rely on it. Counted, so a watch that cannot rescue a
    // The rest of the poll runs on its thread on no shard: in flight, before
    // the shard's next runner can park it idle.
    monitor.count_in();
        // blocked poll shows (proofs/tla/handoff, `F_fail`).
        old.slot.cancel();
        place.shared.counters.handoff_failed();
        let_go(runtime, shard);
        return None;
    }
    // A shard-local value lent to the runner's code (a nested `block_on`
    // in it went outside) keeps the shard where it is.
    if old.slot.word() != word || old.slot.is_pinned() {
        old.slot.cancel();
        let_go(runtime, shard);
        return None;
    }
    if !old.slot.take() {
        // The runner came back first.
        let_go(runtime, shard);
        return None;
    }
    // The shard is the monitor's, its reference held since before the claim.
    // The runner may be back from its poll already: its life's back and this
    // detach add up whichever comes first (proofs/tla/handoff/TakeRef.tla).
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
    // The new runner's words are its own: none is held back as told.
    watch.reported = slot::inside(0);
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
        // A spare that saw the close and left before this pop holds no
        // reference of its own: this one is the last, and the gift goes with
        // it, inside the shard's runtime (proofs/tla/teardown,
        // `LastDropInRuntime`).
        let_go(runtime, spare);
        return Ok((place, life));
    }
    let Some(inner) = runtime.upgrade() else {
        return Err(gift);
    };
    let life = Arc::new(Life::new());
    crate::rt::workers::Workers::spawn_runner(&inner, gift, Arc::clone(&life))?;
    Ok((place, life))
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::rt::shard::Limits;

    #[expect(
        clippy::arc_with_non_send_sync,
        reason = "a worker's shard is shared by count, as `Workers` makes it"
    )]
    fn worker_shard() -> Arc<Shard> {
        Arc::new(
            Shard::new(Limits {
                max_nesting: 8,
                buffer_cap: crate::buf::DEFAULT_CAP,
                backend: crate::rt::backend::chosen().unwrap(),
            })
            .unwrap(),
        )
    }

    /// A monitor with one place, its shard `shard` run by a runner outside
    /// it in the poll of epoch 1; the runner's slot, and its watch as the
    /// fourth equal read leaves it.
    fn watched(shard: &Arc<Shard>) -> (Monitor, Arc<Slot>, Watch) {
        let monitor = Monitor::new(Duration::from_millis(10), 1, Vec::new(), None);
        let slot = monitor.slot();
        let first = Runner {
            slot: Arc::clone(&slot),
            life: Arc::new(Life::new()),
        };
        monitor.started(0, shard, first.clone());
        slot.go_out(1);
        let watch = Watch {
            runner: first,
            last: slot::outside(1),
            streak: STREAK,
            longest: 0,
            reported: slot::inside(0),
        };
        (monitor, slot, watch)
    }

    /// The take of place 0, its runner seen outside in the poll of epoch 1.
    fn take_outside(monitor: &Monitor, watch: &mut Watch) -> Option<Gift> {
        let place = monitor.places[0].get().unwrap();
        take(monitor, &Weak::new(), place, 0, watch, slot::outside(1))
    }

    /// The runner of a shard nobody else holds lets go of it at the moment
    /// the take succeeds: a take that claimed first and held the shard after
    /// found it gone, and left the runner detached from a shard nobody runs,
    /// its queued tasks dropped (the macOS cadence failure). The take holds
    /// the shard before it claims: with no holder left it takes nothing, and
    /// the runner goes back in.
    #[test]
    fn a_take_claims_nothing_of_a_shard_no_one_holds() {
        let shard = worker_shard();
        let (monitor, slot, mut watch) = watched(&shard);
        drop(shard);
        assert!(take_outside(&monitor, &mut watch).is_none());
        assert_eq!(slot.come_back(1), slot::Back::In);
    }

    /// A worker that says it parks, finds work at its last look and backs
    /// out runs on, so it rings the monitor that took that moment for every
    /// worker parked and sleeps, as a park's end does: else the monitor
    /// sleeps while the worker runs, and rescues none of its polls that
    /// block. A monitor awake is not rung.
    #[test]
    fn a_worker_backing_out_of_its_park_rings_the_monitor_asleep() {
        let monitor = Arc::new(Monitor::new(Duration::from_millis(10), 1, Vec::new(), None));
        let shard = Shard::worker(
            Limits {
                max_nesting: 8,
                buffer_cap: crate::buf::DEFAULT_CAP,
                backend: crate::rt::backend::chosen().unwrap(),
            },
            crate::rt::tuning::Tuning::default(),
            1,
            0,
            Some(Arc::clone(&monitor)),
        )
        .unwrap();
        monitor.asleep.store(false, Ordering::SeqCst);
        assert!(
            !shard.begin_park(|| true),
            "work came: the worker backs out"
        );
        assert_eq!(monitor.bell.rings(), 0, "a monitor awake was rung");
        monitor.asleep.store(true, Ordering::SeqCst);
        assert!(
            !shard.begin_park(|| true),
            "work came: the worker backs out"
        );
        assert_eq!(monitor.bell.rings(), 1, "the monitor asleep was not rung");
    }

    /// A call into the shard from the task's code is marked in the slot (the word says inside
    /// the poll) before the runner looks at its claim: a take meanwhile finds the mark after its
    /// fence and lets the shard be. The call's end puts the poll's word back, so the poll, still
    /// running, is taken at the next look; a call after that finds the shard gone.
    #[test]
    fn a_take_lets_a_call_under_way_be_and_takes_the_poll_once_it_ends() {
        let shard = worker_shard();
        let (monitor, slot, mut watch) = watched(&shard);
        let (word, back) = slot.mark();
        assert_eq!(back, slot::Back::In);
        assert_eq!(word, slot::outside(1), "the call read the poll's word");
        assert!(
            take_outside(&monitor, &mut watch).is_none(),
            "the shard was taken while a call was in it"
        );
        slot.unmark(word);
        assert_eq!(slot.word(), slot::outside(1), "the poll is outside again");
        let gift = take_outside(&monitor, &mut watch).expect("the poll, outside again, is taken");
        assert_eq!(
            slot.mark().1,
            slot::Back::Taken,
            "a call after the take went into the shard"
        );
        drop(gift);
    }

    /// A call from the task's code whose mark meets a claim still pending cancels it and goes
    /// in: the take that follows fails, and nothing else is claimed.
    #[test]
    fn a_call_that_meets_a_pending_claim_cancels_it_and_the_take_fails() {
        let shard = worker_shard();
        let (_monitor, slot, _watch) = watched(&shard);
        slot.claim();
        let (word, back) = slot.mark();
        assert_eq!(back, slot::Back::In);
        assert!(
            !slot.take(),
            "the take won over a call that cancelled first"
        );
        slot.unmark(word);
        assert_eq!(slot.come_back(1), slot::Back::In);
    }

    /// A poll the monitor takes while it blocks, through the thread's own context: the call the
    /// task's code makes after the take finds no shard (the thread is detached from it, and the
    /// call takes the paths of any other thread), and the poll ends detached.
    #[test]
    fn a_call_after_the_take_finds_no_shard_and_the_poll_ends_detached() {
        let shard = worker_shard();
        let monitor = Monitor::new(Duration::from_millis(10), 1, Vec::new(), None);
        let slot = monitor.slot();
        let first = Runner {
            slot: Arc::clone(&slot),
            life: Arc::new(Life::new()),
        };
        monitor.started(0, &shard, first.clone());
        let _entered = crate::rt::context::enter_watched(&shard, Some(&slot));
        let (found, polled) = crate::rt::context::poll(&shard.shared, || {
            let before = crate::rt::context::current(|current| current.is_some());
            // The poll blocks; the monitor's look sees it outside and takes the shard.
            let word = slot.word();
            assert!(slot::is_outside(word), "the poll is outside its shard");
            let mut watch = Watch {
                runner: first.clone(),
                last: word,
                streak: STREAK,
                longest: 0,
                reported: slot::inside(0),
            };
            let place = monitor.places[0].get().unwrap();
            let gift = take(&monitor, &Weak::new(), place, 0, &mut watch, word)
                .expect("the blocked poll was taken");
            let after = crate::rt::context::current(|current| current.is_some());
            drop(gift);
            (before, after)
        });
        assert_eq!(
            found,
            (true, false),
            "a call after the take found the shard"
        );
        assert_eq!(polled, crate::rt::context::Polled::Detached);
    }

    /// An IO owner's step marks its call with no guard (`mark_call`), the mark kept with what the
    /// step leaves until its settle ends it (`unmark_call`): a take meanwhile lets the shard be,
    /// and a call inside the step marks nothing more. Once the step ended the poll is outside
    /// again and taken, and a step's mark after that finds the shard taken.
    #[test]
    fn an_owner_step_mark_holds_off_a_take_until_its_settle() {
        use crate::rt::context::{self, Marked};
        let shard = worker_shard();
        let monitor = Monitor::new(Duration::from_millis(10), 1, Vec::new(), None);
        let slot = monitor.slot();
        let first = Runner {
            slot: Arc::clone(&slot),
            life: Arc::new(Life::new()),
        };
        monitor.started(0, &shard, first.clone());
        let _entered = context::enter_watched(&shard, Some(&slot));
        let (_, polled) = context::poll(&shard.shared, || {
            let word = slot.word();
            assert!(matches!(context::mark_call(), Marked::In));
            assert!(
                !slot::is_outside(slot.word()),
                "the step's call is not marked"
            );
            assert!(
                matches!(context::mark_call(), Marked::Not),
                "a call in the step marked"
            );
            let mut watch = Watch {
                runner: first.clone(),
                last: word,
                streak: STREAK,
                longest: 0,
                reported: slot::inside(0),
            };
            let place = monitor.places[0].get().unwrap();
            assert!(
                take(&monitor, &Weak::new(), place, 0, &mut watch, word).is_none(),
                "the shard was taken while a step was marked"
            );
            context::unmark_call();
            assert_eq!(slot.word(), word, "the step's end left the poll inside");
            let gift = take(&monitor, &Weak::new(), place, 0, &mut watch, word)
                .expect("the poll, outside again, was not taken");
            assert!(matches!(context::mark_call(), Marked::Taken));
            drop(gift);
        });
        assert_eq!(polled, context::Polled::Detached);
    }

    /// An owner's straight send finds the socket full, and the shard is taken before the rest of
    /// the same poll; the peer reads meanwhile, and the shard's driver, on whatever thread runs it
    /// now, passes over the write edge that room brings, since the side still says it was only ever
    /// held by its owner and idle. The rest of the poll then goes the way of any other thread, and
    /// must ask the kernel again: told the socket was full, it would block the side on an edge
    /// that already passed, and the byte would wait for good (proofs/tla/sendsnow/TakenFull.tla).
    /// A readiness driver's with biased sides: on a ring a side never blocks on an edge, and with
    /// no heavy fence every side starts shared, whose edges are never passed over.
    #[cfg(unix)]
    #[test]
    fn an_owners_full_send_whose_shard_is_taken_before_the_rest_of_its_poll_asks_again() {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        use std::task::{Context, Poll};

        use crate::rt::context;
        if !super::super::fence::available() {
            eprintln!("skipping: no heavy fence here, so every side starts shared");
            return;
        }
        let shard = worker_shard();
        #[cfg(ring)]
        if shard.shared.is_ring() {
            eprintln!("skipping: a ring's side never waits for a write edge");
            return;
        }
        let monitor = Arc::new(Monitor::new(Duration::from_millis(10), 1, Vec::new(), None));
        let slot = monitor.slot();
        let first = Runner {
            slot: Arc::clone(&slot),
            life: Arc::new(Life::new()),
        };
        monitor.started(0, &shard, first.clone());
        let (a, mut b) = UnixStream::pair().unwrap();
        b.set_nonblocking(true).unwrap();
        let handle = crate::io::Handle::new(a).unwrap();
        let src = Arc::clone(handle.source());
        let fd = handle.get_ref().as_raw_fd();
        let _entered = context::enter_watched(&shard, Some(&slot));
        let mut cx = Context::from_waker(std::task::Waker::noop());
        // A read that waits homes the source here; the owner then writes straight once, and its
        // release says the write side holds nothing and waits for nothing.
        let mut nothing = [0u8; 1];
        let (read, _) = context::poll(&shard.shared, || {
            src.poll_read(&mut cx, &mut nothing, false)
        });
        assert!(read.is_pending(), "{read:?}");
        let (first_write, _) = context::poll(&shard.shared, || src.poll_write(&mut cx, b"x"));
        assert!(matches!(first_write, Poll::Ready(Ok(1))), "{first_write:?}");
        let mut one = [0u8; 1];
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while b.read(&mut one).is_err() {
            assert!(
                std::time::Instant::now() < deadline,
                "the first byte never came"
            );
            std::thread::yield_now();
        }
        // The socket fills, behind the side's back.
        let chunk = [0u8; 4096];
        // SAFETY: a plain send of our own buffer on our own socket.
        while unsafe { libc::send(fd, chunk.as_ptr().cast(), chunk.len(), libc::MSG_DONTWAIT) } > 0
        {
        }
        let gift = std::rc::Rc::new(core::cell::Cell::new(None));
        let (keep, peer, turn_on, taker, old) = (
            std::rc::Rc::clone(&gift),
            b.try_clone().unwrap(),
            Arc::clone(&shard),
            Arc::clone(&monitor),
            Arc::clone(&slot),
        );
        crate::io::source::write::AFTER_FULL_SEND.set(Some(Box::new(move || {
            // The take, between the straight send and the rest of the poll.
            let word = old.word();
            let mut watch = Watch {
                runner: first.clone(),
                last: word,
                streak: STREAK,
                longest: 0,
                reported: slot::inside(0),
            };
            let place = taker.places[0].get().unwrap();
            keep.set(take(&taker, &Weak::new(), place, 0, &mut watch, word));
            // The peer reads all: room comes, and the edge it brings is passed over.
            let mut sink = [0u8; 4096];
            while (&peer).read(&mut sink).is_ok_and(|n| n > 0) {}
            let _ = turn_on.turn(64, Some(Duration::ZERO));
        })));
        let (second, polled) = context::poll(&shard.shared, || src.poll_write(&mut cx, b"y"));
        assert!(gift.take().is_some(), "the shard was not taken in the poll");
        assert_eq!(polled, context::Polled::Detached);
        assert!(matches!(second, Poll::Ready(Ok(1))), "{second:?}");
        assert!(
            matches!(b.read(&mut one), Ok(1)) && one == *b"y",
            "the byte waits behind a side blocked on an edge its driver passed over"
        );
    }

    /// A spare that came after the close looked, saw it and left, its
    /// reference still on the queue: the last pass's hand-off pops it, and the
    /// popped reference is the last, so the gift, and the shard, go with it.
    /// That drop was the monitor's plain one, outside any runtime, and what
    /// the shard's going handed on went elsewhere; it goes through `let_go`,
    /// inside the shard's runtime (proofs/tla/teardown).
    #[test]
    fn a_gift_to_a_spare_that_left_goes_inside_its_runtime() {
        use std::sync::atomic::AtomicU64;
        static DROPPED_IN: AtomicU64 = AtomicU64::new(0);
        struct Marks;
        impl Drop for Marks {
            fn drop(&mut self) {
                let id = crate::rt::context::runtime().map_or(u64::MAX, |inner| inner.id());
                DROPPED_IN.store(id, Ordering::SeqCst);
            }
        }
        static LOCAL: crate::runtime::ShardLocal<Marks> =
            crate::runtime::ShardLocal::__new(|| Marks);
        let rt = crate::runtime::Builder::new()
            .worker_threads(0)
            .build()
            .unwrap();
        let inner = Arc::clone(rt.inner());
        let monitor = Arc::new(Monitor::new(Duration::from_millis(10), 1, Vec::new(), None));
        let shard = Shard::worker(
            Limits {
                max_nesting: 8,
                buffer_cap: crate::buf::DEFAULT_CAP,
                backend: crate::rt::backend::chosen().unwrap(),
            },
            crate::rt::tuning::Tuning::default(),
            inner.id(),
            0,
            Some(Arc::clone(&monitor)),
        )
        .unwrap();
        {
            let _in = crate::rt::context::enter(&shard);
            LOCAL.with(|_| ());
        }
        monitor.close();
        let waits = Arc::clone(&monitor);
        std::thread::spawn(move || {
            assert!(
                waits
                    .wait_for_shard(&Spare::here(Arc::new(Life::new())))
                    .is_none()
            );
        })
        .join()
        .unwrap();
        let gift = Gift {
            place: 0,
            shard: Owned(shard),
            slot: monitor.slot(),
        };
        assert!(give(&monitor, &Arc::downgrade(&inner), gift).is_ok());
        assert_eq!(
            DROPPED_IN.load(Ordering::SeqCst),
            inner.id(),
            "the shard went outside its runtime"
        );
    }

    /// A take that succeeds holds the shard in its gift, so the runner's
    /// letting go of its own reference as it finds the shard taken leaves
    /// the shard to its next runner.
    #[test]
    fn a_taken_shard_outlives_its_runner_letting_go() {
        let shard = worker_shard();
        let (monitor, slot, mut watch) = watched(&shard);
        let gift = take_outside(&monitor, &mut watch).expect("the runner was outside");
        assert_eq!(slot.come_back(1), slot::Back::Taken);
        drop(shard);
        assert_eq!(Arc::strong_count(&gift.shard.0), 1);
    }

    /// A take whose heavy fence fails relies on nothing the fence was to order: it cancels its
    /// claim, takes nothing, and counts the failure on the shard, so a watch that can no longer
    /// rescue a blocked poll is seen in the stats (proofs/tla/handoff, `F_fail`).
    #[test]
    fn a_take_whose_heavy_fence_fails_cancels_its_claim_and_is_counted() {
        let shard = worker_shard();
        let (mut monitor, slot, mut watch) = watched(&shard);
        // The heavy fence's path on every platform: one that has none takes a full fence instead.
        monitor.full_fence = false;
        super::super::fence::inject_failure(true);
        let gift = take_outside(&monitor, &mut watch);
        super::super::fence::inject_failure(false);
        assert!(gift.is_none(), "a take relied on a failed fence");
        assert_eq!(slot.come_back(1), slot::Back::In, "the claim was left up");
        let stats = shard.shared.counters.read(0);
        assert_eq!(stats.handoffs_failed, 1, "the failed take was not counted");
        assert_eq!(stats.handoffs, 0);
        slot.go_out(1);
        let gift = take_outside(&monitor, &mut watch).expect("the poll was not taken");
        let stats = shard.shared.counters.read(0);
        assert_eq!((stats.handoffs, stats.handoffs_failed), (1, 1));
        drop(gift);
    }
}
