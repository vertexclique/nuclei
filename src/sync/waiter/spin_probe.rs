//! Loom-only instruments of the spin on the waiter's node: what the
//! models in `loom_model.rs` count to show they reach each branch of the
//! spin, and the one guard their calibration takes away. Loom runs a model's
//! threads as coroutines on the test's own thread, so these are per test.

use core::cell::Cell;
use std::thread::LocalKey;

std::thread_local! {
    /// Spins on a node that began.
    pub(in super::super) static SPINS: Cell<usize> = const { Cell::new(0) };
    /// Spins that saw a mark land on the unarmed node.
    pub(in super::super) static MARKED_WHILE_SPINNING: Cell<usize> = const { Cell::new(0) };
    /// Spins that ran out, after which only the look found a mark.
    pub(in super::super) static MARKED_AFTER_LAST_READ: Cell<usize> = const { Cell::new(0) };
    /// The calibration's switch: a spin that runs out arms the waker
    /// without the look's reading of the marks.
    pub(in super::super) static SKIP_LOOK: Cell<bool> = const { Cell::new(false) };
}

/// Adds one to `counter`.
pub(in super::super) fn count(counter: &'static LocalKey<Cell<usize>>) {
    counter.with(|c| c.set(c.get() + 1));
}

/// Reads and resets the three counts, in the order above.
pub(in super::super) fn take() -> [usize; 3] {
    [&SPINS, &MARKED_WHILE_SPINNING, &MARKED_AFTER_LAST_READ].map(|c| c.with(|c| c.replace(0)))
}
