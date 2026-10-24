//! loom models of the side's guard: the driver's edge against a poll, and a
//! poll that finds the side held against its holder. Each has a calibration
//! that takes one rule away and must fail.

use core::task::{Poll, Waker};
use std::sync::Arc as StdArc;

use loom::sync::atomic::{AtomicUsize, Ordering};
use loom::sync::{Arc, Notify};
use loom::thread;

use super::{After, Hooks, Side};

/// Wakes the loom thread parked on it.
struct Unpark(Notify);

impl std::task::Wake for Unpark {
    fn wake(self: StdArc<Self>) {
        self.0.notify();
    }

    fn wake_by_ref(self: &StdArc<Self>) {
        self.0.notify();
    }
}

/// What a side holds in the models: the readiness cache (an edge came since
/// the last empty read), what the reader got, and the reader's waker.
#[derive(Default)]
struct Model {
    ready: bool,
    got: usize,
    reader: Option<Waker>,
}

/// One read attempt on the model's descriptor (`kernel`, the bytes it
/// holds): takes them all, or clears the cache.
fn read(m: &mut Model, kernel: &AtomicUsize) {
    match kernel.swap(0, Ordering::SeqCst) {
        0 => m.ready = false,
        n => m.got += n,
    }
}

/// The driver's part: an edge sets the cache, and bytes read wake the
/// reader.
struct Serve<'a> {
    kernel: &'a AtomicUsize,
    /// The calibration's driver part, which forgets the reader's wake.
    wake: bool,
}

impl Hooks<Model> for Serve<'_> {
    fn serve(&self, m: &mut Model, after: &mut After) {
        m.ready = true;
        read(m, self.kernel);
        if m.got > 0
            && self.wake
            && let Some(w) = m.reader.take()
        {
            after.wake_one(w);
        }
    }
}

/// The reader's step: bytes taken, or its waker left for the driver.
fn step(m: &mut Model, kernel: &AtomicUsize, waker: &Waker) -> Poll<()> {
    if m.ready {
        read(m, kernel);
    }
    if m.got > 0 {
        return Poll::Ready(());
    }
    m.reader = Some(waker.clone());
    Poll::Pending
}

/// Polls `side` until the step is done, parking on `park` between polls.
fn poll_until_done(side: &Side<Model>, kernel: &AtomicUsize, hooks: &Serve<'_>) {
    let park = StdArc::new(Unpark(Notify::new()));
    let waker = Waker::from(StdArc::clone(&park));
    loop {
        let mut after = After::default();
        let polled = side.poll(&mut after, &waker, hooks, |m, _| step(m, kernel, &waker));
        after.run();
        if polled.is_ready() {
            return;
        }
        park.0.wait();
    }
}

/// A byte arrives and its edge is reported while the reader polls, the
/// cache starting out "blocked": the driver serves it at once, or the
/// holder serves it as it lets go, and the reader gets the byte. `wake`:
/// whether the driver's part wakes the reader.
fn edge_model(wake: bool) {
    let side = Arc::new(Side::new(Model::default()));
    let kernel = Arc::new(AtomicUsize::new(0));
    let driver = {
        let (side, kernel) = (side.clone(), kernel.clone());
        thread::spawn(move || {
            kernel.fetch_add(1, Ordering::SeqCst);
            let mut after = After::default();
            side.drive(
                &mut after,
                &Serve {
                    kernel: &kernel,
                    wake,
                },
                0,
            );
            after.run();
        })
    };
    poll_until_done(
        &side,
        &kernel,
        &Serve {
            kernel: &kernel,
            wake,
        },
    );
    driver.join().unwrap();
    assert_eq!(
        kernel.load(Ordering::SeqCst),
        0,
        "the byte was left in the kernel"
    );
}

/// An edge that comes while a poll holds the side is never lost, and the
/// reader is always woken: a lost one leaves it parked, which loom reports.
#[test]
fn an_edge_during_a_poll_is_never_lost() {
    loom::model(|| edge_model(true));
}

/// The calibration: a driver's part that reads the byte for the reader but
/// forgets its wake leaves the reader parked in some interleaving.
#[test]
#[should_panic]
fn calibration_a_driver_part_that_forgets_the_wake_strands_the_reader() {
    loom::model(|| edge_model(false));
}

/// What two polls do in the contended model: count their steps.
struct Count<'a>(&'a AtomicUsize);

impl Hooks<usize> for Count<'_> {
    fn serve(&self, _st: &mut usize, _after: &mut After) {}
}

/// Two threads each poll the side once to completion: a poll that finds it
/// held registers and is woken by the holder's release, then runs. `park`:
/// whether a contended poll may park until woken (the calibration polls
/// again only if woken, with a waker that never wakes).
fn contended_model(wakes: bool) {
    let side = Arc::new(Side::new(0usize));
    let steps = Arc::new(AtomicUsize::new(0));
    let other = {
        let (side, steps) = (side.clone(), steps.clone());
        thread::spawn(move || poll_once_to_completion(&side, &steps, true))
    };
    poll_once_to_completion(&side, &steps, wakes);
    other.join().unwrap();
    assert_eq!(steps.load(Ordering::SeqCst), 2, "a poll never ran its step");
    // SAFETY-free look: both threads are done, so the side is free.
    let mut after = After::default();
    let mut held = side.try_hold().expect("the side was left held");
    assert_eq!(*held.get(), 2);
    held.release(&mut after, &Count(&steps));
}

fn poll_once_to_completion(side: &Side<usize>, steps: &AtomicUsize, wakes: bool) {
    let park = StdArc::new(Unpark(Notify::new()));
    let waker = if wakes {
        Waker::from(StdArc::clone(&park))
    } else {
        Waker::noop().clone()
    };
    loop {
        let mut after = After::default();
        let polled = side.poll(&mut after, &waker, &Count(steps), |n, _| {
            *n += 1;
            steps.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(())
        });
        after.run();
        if polled.is_ready() {
            return;
        }
        park.0.wait();
    }
}

/// A poll that finds the side held is woken as the holder lets go, and runs.
#[test]
fn a_contended_poll_is_woken_by_the_holder() {
    loom::model(|| contended_model(true));
}

/// The calibration: a contended poll whose waker wakes nobody stays parked
/// in the interleavings where it found the side held.
#[test]
#[should_panic]
fn calibration_a_contended_poll_that_is_never_woken_stays_parked() {
    loom::model(|| contended_model(false));
}
