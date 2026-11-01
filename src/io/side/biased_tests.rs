//! The side biased to its owner: the owner holds without the word, a foreign
//! take switches the side for good or leaves its work to the owner, and no
//! work left to the owner is lost. `proofs/tla/side_biased/` checks the
//! protocol; these run it on real threads. Where the process has no heavy
//! fence a side starts shared and the tests skip.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::task::{Poll, Wake, Waker};
use std::time::{Duration, Instant};

use super::{After, Hooks, Side};

#[derive(Default)]
struct State {
    steps: u64,
    seen: u8,
    requests: u64,
}

/// A thread's hooks: `who` is its owner id (0 for a foreign thread).
struct Probe {
    who: usize,
}

impl Hooks<State> for Probe {
    fn serve(&self, _: &mut State, _: &mut After) {}

    fn deferred(&self, st: &mut State, bits: u8, _: &mut After) {
        st.seen |= bits;
    }

    fn requests(&self, st: &mut State, _: &mut After) {
        st.requests += 1;
    }

    fn owner(&self) -> usize {
        self.who
    }
}

const OWNER: Probe = Probe { who: 1 };
const FOREIGN: Probe = Probe { who: 0 };

fn biased() -> Option<Side<State>> {
    crate::rt::handoff::fence::available().then(|| Side::biased(State::default()))
}

struct Counted(AtomicUsize);

impl Wake for Counted {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn counted() -> (Arc<Counted>, Waker) {
    let count = Arc::new(Counted(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&count));
    (count, waker)
}

fn step(st: &mut State, _: &mut After) -> Poll<()> {
    st.steps += 1;
    Poll::Ready(())
}

/// Polls `side` as `hooks`' thread, running the wakes it left.
fn poll(side: &Side<State>, waker: &Waker, hooks: &impl Hooks<State>) -> Poll<()> {
    let mut after = After::default();
    let polled = side.poll(&mut after, waker, hooks, step);
    after.run();
    polled
}

fn defer(side: &Side<State>, bits: u8, hooks: &impl Hooks<State>) {
    let mut after = After::default();
    side.defer(bits, &mut after, hooks);
    after.run();
}

/// A request that needs the side: done by whoever holds it, here or at the
/// holder's release.
fn request(side: &Side<State>, hooks: &impl Hooks<State>) {
    let mut after = After::default();
    if let Some(held) = side.take_for_request(hooks) {
        held.release(&mut after, hooks);
    }
    after.run();
}

fn steps(side: &mut Side<State>) -> u64 {
    side.get_mut().steps
}

#[test]
fn the_owner_holds_a_biased_side_without_touching_the_word() {
    let Some(mut side) = biased() else { return };
    let (_, waker) = counted();
    let mut after = After::default();
    let polled = side.poll(&mut after, &waker, &OWNER, |st, _| {
        assert_eq!(side.words().0, 0, "the word was taken");
        st.steps += 1;
        Poll::Ready(())
    });
    after.run();
    assert!(polled.is_ready());
    assert!(!side.is_shared());
    assert_eq!(side.words(), (0, 0));
    defer(&side, 4, &OWNER);
    assert_eq!(side.words(), (0, 0), "a defer of the owner left a flag");
    assert_eq!(side.get_mut().seen, 4, "the owner's defer was not done");
    assert!(!side.is_shared());
}

#[test]
fn a_foreign_take_of_a_free_side_switches_it_for_good() {
    let Some(mut side) = biased() else { return };
    let (_, waker) = counted();
    assert!(poll(&side, &waker, &OWNER).is_ready());
    assert!(poll(&side, &waker, &FOREIGN).is_ready());
    assert!(side.is_shared());
    // The owner takes through the word now.
    let mut after = After::default();
    let polled = side.poll(&mut after, &waker, &OWNER, |st, _| {
        assert_eq!(side.words().0, 1, "shared mode did not use the word");
        st.steps += 1;
        Poll::Ready(())
    });
    after.run();
    assert!(polled.is_ready());
    assert!(side.is_shared(), "shared mode went back");
    assert_eq!(steps(&mut side), 3);
}

/// Starts a thread that holds the side as its owner until the test lets it
/// go: `held` says it holds, `go` lets it go.
fn hold_until(
    side: &Arc<Side<State>>,
) -> (
    mpsc::Receiver<()>,
    mpsc::Sender<()>,
    std::thread::JoinHandle<()>,
) {
    let (held_tx, held_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let side = Arc::clone(side);
    let thread = std::thread::spawn(move || {
        let (_, waker) = counted();
        let mut after = After::default();
        let polled = side.poll(&mut after, &waker, &OWNER, |st, _| {
            st.steps += 1;
            held_tx.send(()).unwrap();
            go_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            Poll::Ready(())
        });
        after.run();
        assert!(polled.is_ready());
    });
    (held_rx, go_tx, thread)
}

#[test]
fn a_foreign_poll_while_the_owner_holds_is_woken_at_its_release() {
    let Some(side) = biased() else { return };
    let side = Arc::new(side);
    let (held, go, owner) = hold_until(&side);
    held.recv_timeout(Duration::from_secs(10)).unwrap();
    let (woke, waker) = counted();
    assert!(poll(&side, &waker, &FOREIGN).is_pending());
    assert_eq!(woke.0.load(Ordering::SeqCst), 0);
    go.send(()).unwrap();
    owner.join().unwrap();
    assert_eq!(
        woke.0.load(Ordering::SeqCst),
        1,
        "the waiting poll was not woken"
    );
    assert!(poll(&side, &waker, &FOREIGN).is_ready());
}

#[test]
fn work_a_foreign_thread_left_the_owner_is_done_at_its_release() {
    let Some(side) = biased() else { return };
    let side = Arc::new(side);
    let (held, go, owner) = hold_until(&side);
    held.recv_timeout(Duration::from_secs(10)).unwrap();
    defer(&side, 4, &FOREIGN);
    request(&side, &FOREIGN);
    go.send(()).unwrap();
    owner.join().unwrap();
    let mut side = Arc::into_inner(side).unwrap();
    assert_eq!(side.get_mut().seen, 4, "the deferred work was lost");
    assert_eq!(side.get_mut().requests, 1, "the request was lost");
    assert_eq!(side.words(), (0, 0));
}

#[test]
fn the_owner_calling_again_under_its_own_hold_leaves_its_work_for_its_release() {
    let Some(mut side) = biased() else { return };
    let (_, waker) = counted();
    let mut after = After::default();
    let polled = side.poll(&mut after, &waker, &OWNER, |st, after| {
        defer(&side, 2, &OWNER);
        assert_eq!(st.seen, 0, "the nested call held the side too");
        st.steps += 1;
        let _ = after;
        Poll::Ready(())
    });
    after.run();
    assert!(polled.is_ready());
    assert_eq!(side.get_mut().seen, 2, "the nested call's work was lost");
    assert_eq!(side.words(), (0, 0));
}

struct Parker {
    thread: std::thread::Thread,
    woken: AtomicBool,
}

impl Wake for Parker {
    fn wake(self: Arc<Self>) {
        self.woken.store(true, Ordering::SeqCst);
        self.thread.unpark();
    }
}

thread_local! {
    /// One waker per thread, as one task has one: a contended set tells its
    /// tasks apart by their wakers' addresses.
    static PARKER: Arc<Parker> = Arc::new(Parker {
        thread: std::thread::current(),
        woken: AtomicBool::new(false),
    });
}

/// Polls `side` until its step ran, sleeping until woken between polls: a
/// wake that never comes fails the test.
fn poll_until_done(side: &Side<State>, hooks: &impl Hooks<State>) {
    let parker = PARKER.with(Arc::clone);
    let waker = Waker::from(Arc::clone(&parker));
    let deadline = Instant::now() + Duration::from_secs(20);
    while poll(side, &waker, hooks).is_pending() {
        while !parker.woken.swap(false, Ordering::SeqCst) {
            assert!(
                Instant::now() < deadline,
                "a wake was lost: word {:?}, shared {}, own {}, intent {}",
                side.words(),
                side.is_shared(),
                side.own.load(Ordering::SeqCst),
                side.intent.load(Ordering::SeqCst)
            );
            std::thread::park_timeout(Duration::from_millis(20));
        }
    }
}

/// An owner and two foreign threads call on one side without pause, taking
/// it, leaving work and waking each other: every step runs once, every
/// deferred bit and request is done, no wake is lost and no flag is left.
#[test]
fn an_owner_and_two_foreign_threads_lose_no_step_wake_or_work() {
    let Some(side) = biased() else { return };
    let side = Arc::new(side);
    const ROUNDS: u64 = 20_000;
    let foreign = |bit: u8| {
        let side = Arc::clone(&side);
        std::thread::spawn(move || {
            for round in 0..ROUNDS {
                match round % 3 {
                    0 => poll_until_done(&side, &FOREIGN),
                    1 => {
                        defer(&side, bit, &FOREIGN);
                        poll_until_done(&side, &FOREIGN);
                    }
                    _ => {
                        request(&side, &FOREIGN);
                        poll_until_done(&side, &FOREIGN);
                    }
                }
            }
        })
    };
    let a = foreign(1);
    let b = foreign(2);
    for round in 0..ROUNDS {
        if round % 2 == 0 {
            poll_until_done(&side, &OWNER);
        } else {
            defer(&side, 4, &OWNER);
            poll_until_done(&side, &OWNER);
        }
        if round % 64 == 0 {
            std::thread::yield_now();
        }
    }
    a.join().unwrap();
    b.join().unwrap();
    let mut side = Arc::into_inner(side).unwrap();
    assert_eq!(
        steps(&mut side),
        3 * ROUNDS,
        "a step ran twice or not at all"
    );
    assert_eq!(side.get_mut().seen, 7, "deferred work was lost");
    assert!(side.get_mut().requests > 0, "requests were lost");
    assert_eq!(side.words(), (0, 0), "a flag was left up");
}

/// Fresh sides, each raced by an owner and two foreign threads arriving as
/// the owner takes and lets go: the window in which a foreign thread's work
/// meets the owner's release. Every side ends with all its steps run, its
/// work done and no flag up.
#[test]
fn a_foreign_thread_arriving_as_the_owner_lets_go_loses_nothing() {
    if biased().is_none() {
        return;
    }
    const SIDES: usize = 3_000;
    const OWNER_OPS: u64 = 6;
    let sides: Arc<Vec<Side<State>>> =
        Arc::new((0..SIDES).map(|_| Side::biased(State::default())).collect());
    let gate = Arc::new(std::sync::Barrier::new(3));
    let foreign = |bit: u8| {
        let (sides, gate) = (Arc::clone(&sides), Arc::clone(&gate));
        std::thread::spawn(move || {
            for side in sides.iter() {
                gate.wait();
                match bit {
                    1 => poll_until_done(side, &FOREIGN),
                    _ => {
                        defer(side, bit, &FOREIGN);
                        request(side, &FOREIGN);
                        poll_until_done(side, &FOREIGN);
                    }
                }
            }
        })
    };
    let (a, b) = (foreign(1), foreign(2));
    for side in sides.iter() {
        gate.wait();
        for _ in 0..OWNER_OPS {
            poll_until_done(side, &OWNER);
            defer(side, 4, &OWNER);
        }
    }
    a.join().unwrap();
    b.join().unwrap();
    let mut sides = Arc::into_inner(sides).unwrap();
    for (i, side) in sides.iter_mut().enumerate() {
        assert_eq!(side.words(), (0, 0), "side {i} kept a flag");
        let st = side.get_mut();
        assert_eq!(st.steps, OWNER_OPS + 2, "side {i} lost or repeated a step");
        assert_eq!(st.seen, 6, "side {i} lost deferred work");
    }
    assert!(
        super::WITNESS[2].load(Ordering::Relaxed) > 0,
        "the owner never met a foreign thread's intent"
    );
}

/// A foreign thread whose heavy fence fails: it asks the owner to touch the
/// side by counting its nudges.
struct Fenceless<'a>(&'a AtomicUsize);

impl Hooks<State> for Fenceless<'_> {
    fn serve(&self, _: &mut State, _: &mut After) {}

    fn nudge(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// A heavy fence that fails leaves a foreign call's work, and its nudge makes
/// the owner serve it: the poll is woken and completes, with no read of
/// `own` and no switch of the side.
#[test]
fn a_foreign_poll_without_a_fence_is_served_by_the_owner_it_nudges() {
    let Some(mut side) = biased() else { return };
    let (_, waker) = counted();
    // The owner took and let go once: it is claimed, and holds nothing.
    assert!(poll(&side, &waker, &OWNER).is_ready());
    crate::rt::handoff::fence::inject_failure(true);
    let nudges = AtomicUsize::new(0);
    let (woke, fwaker) = counted();
    assert!(poll(&side, &fwaker, &Fenceless(&nudges)).is_pending());
    assert_eq!(nudges.load(Ordering::SeqCst), 1, "the owner was not asked");
    assert!(!side.is_shared(), "the side switched without a fence");
    assert_eq!(woke.0.load(Ordering::SeqCst), 0);
    // The owner's thread does what the nudge asks: a take and a release.
    defer(&side, 0, &OWNER);
    assert_eq!(woke.0.load(Ordering::SeqCst), 1, "the poll was not woken");
    crate::rt::handoff::fence::inject_failure(false);
    assert!(poll(&side, &fwaker, &FOREIGN).is_ready());
    assert_eq!(steps(&mut side), 2);
}

/// With no owner claimed a foreign call without a fence takes the side
/// through the word.
#[test]
fn a_foreign_call_without_a_fence_on_an_unclaimed_side_goes_shared() {
    let Some(side) = biased() else { return };
    crate::rt::handoff::fence::inject_failure(true);
    let nudges = AtomicUsize::new(0);
    let (_, waker) = counted();
    assert!(poll(&side, &waker, &Fenceless(&nudges)).is_ready());
    assert!(side.is_shared());
    assert_eq!(nudges.load(Ordering::SeqCst), 0);
    crate::rt::handoff::fence::inject_failure(false);
}

/// After a fence failure, sides made on the thread start shared.
#[test]
fn sides_made_after_a_fence_failure_start_shared() {
    let Some(side) = biased() else { return };
    let (_, waker) = counted();
    assert!(poll(&side, &waker, &OWNER).is_ready());
    assert!(!Side::biased(State::default()).is_shared());
    crate::rt::handoff::fence::inject_failure(true);
    let nudges = AtomicUsize::new(0);
    assert!(poll(&side, &waker, &Fenceless(&nudges)).is_pending());
    assert!(Side::biased(State::default()).is_shared());
    assert!(!crate::rt::handoff::fence::available());
    crate::rt::handoff::fence::inject_failure(false);
    assert!(crate::rt::handoff::fence::available());
}

/// A foreign thread with no fence and an owner calling without pause: never
/// two holders (a counter in the critical section), every foreign poll woken
/// and completed.
#[test]
fn foreign_polls_without_a_fence_never_share_the_side_and_are_all_served() {
    if biased().is_none() {
        return;
    }
    const ROUNDS: u64 = 2_000;
    let side = Arc::new(Side::biased(State::default()));
    let inside = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let nudges = Arc::new(AtomicUsize::new(0));
    let cs = |inside: &AtomicUsize| {
        assert_eq!(inside.fetch_add(1, Ordering::SeqCst), 0, "two holders");
        std::hint::spin_loop();
        inside.fetch_sub(1, Ordering::SeqCst);
    };
    let foreign = {
        let (side, inside, done, nudges) = (
            Arc::clone(&side),
            Arc::clone(&inside),
            Arc::clone(&done),
            Arc::clone(&nudges),
        );
        std::thread::spawn(move || {
            crate::rt::handoff::fence::inject_failure(true);
            let hooks = Fenceless(&nudges);
            let parker = PARKER.with(Arc::clone);
            let waker = Waker::from(Arc::clone(&parker));
            let deadline = Instant::now() + Duration::from_secs(20);
            for _ in 0..ROUNDS {
                let mut after = After::default();
                while side
                    .poll(&mut after, &waker, &hooks, |st, _| {
                        cs(&inside);
                        st.steps += 1;
                        Poll::Ready(())
                    })
                    .is_pending()
                {
                    after.run();
                    after = After::default();
                    while !parker.woken.swap(false, Ordering::SeqCst) {
                        assert!(Instant::now() < deadline, "a wake was lost");
                        std::thread::park_timeout(Duration::from_millis(20));
                    }
                }
                after.run();
            }
            done.store(true, Ordering::SeqCst);
        })
    };
    let (_, waker) = counted();
    let mut handled = 0;
    let mut owner_steps = 0;
    while !done.load(Ordering::SeqCst) {
        let mut after = After::default();
        let polled = side.poll(&mut after, &waker, &OWNER, |st, _| {
            cs(&inside);
            st.steps += 1;
            Poll::Ready(())
        });
        after.run();
        owner_steps += u64::from(polled.is_ready());
        let asked = nudges.load(Ordering::SeqCst);
        if asked > handled {
            handled = asked;
            defer(&side, 0, &OWNER);
        }
    }
    foreign.join().unwrap();
    let mut side = Arc::into_inner(side).unwrap();
    assert_eq!(steps(&mut side), ROUNDS + owner_steps, "a step was lost");
    assert_eq!(side.words(), (0, 0), "a flag was left up");
}
