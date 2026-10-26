//! The per-thread random numbers behind the runtime's choices: where a task
//! spawned from outside the workers goes, and which branch of a fair
//! `select!` is polled first.
//!
//! Each thread has its own xorshift generator, seeded from its address and
//! the clock; nothing depends on its quality but balance. A runtime built
//! with an [`RngSeed`] seeds the generator of each of its workers, and of
//! each thread while it is in the runtime's `block_on`, from that seed
//! instead, so each such thread draws the same numbers run after run.

use core::cell::Cell;

thread_local! {
    static STATE: Cell<u64> = Cell::new(fresh());
}

/// A state distinct per thread and per call: the address of a local and
/// the clock.
fn fresh() -> u64 {
    let local = 0u8;
    let addr = &local as *const u8 as u64;
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64);
    (addr ^ time.rotate_left(32)) | 1
}

/// The splitmix64 finalizer: spreads every bit of `z` over the word.
fn mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// The next number of this thread's generator.
fn next() -> u64 {
    STATE
        .try_with(|s| {
            let mut x = s.get();
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            s.set(x);
            x
        })
        // A thread on its way out has no generator left: any number serves.
        .unwrap_or_else(|_| fresh())
}

/// A random number below `n`.
pub(crate) fn random_below(n: usize) -> usize {
    // Multiply-shift: unbiased enough for placement, no division.
    ((u128::from(next()) * n as u128) >> 64) as usize
}

/// A seed for a runtime's random choices: where a task spawned from outside
/// its workers goes, and which branch of a fair
/// [`select!`](crate::select) is polled first.
///
/// A runtime built with [`Builder::rng_seed`](crate::runtime::Builder::rng_seed)
/// seeds each of its workers, and each thread in its `block_on`, from the
/// seed: run after run, such a thread draws the same numbers, so its
/// choices repeat as long as what it chooses among is the same. The threads
/// still interleave as the system runs them.
///
/// ```
/// use nuclei::runtime::{RngSeed, Runtime};
///
/// let seed = RngSeed::from_bytes(b"a run to repeat");
/// assert_eq!(seed, RngSeed::from_bytes(b"a run to repeat"));
/// let rt = Runtime::builder().rng_seed(seed).build().unwrap();
/// # drop(rt);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RngSeed(u64);

impl RngSeed {
    /// A seed different each time.
    pub fn new() -> Self {
        Self(mix(next()))
    }

    /// The seed of `bytes`: the same bytes give the same seed on every
    /// system.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        // FNV-1a: fixed, so a seed never depends on the toolchain.
        let hash = bytes.iter().fold(0xCBF2_9CE4_8422_2325u64, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01B3)
        });
        Self(mix(hash))
    }
}

impl Default for RngSeed {
    fn default() -> Self {
        Self::new()
    }
}

/// This thread's generator set from a seed until the guard drops, then
/// back to where it was.
pub(crate) struct Reseeded(Option<u64>);

impl Reseeded {
    /// Seeds this thread's generator from `seed`, for the runtime's thread
    /// `thread` (0 for a thread in its `block_on`, a worker's index plus one).
    pub(crate) fn new(seed: RngSeed, thread: u64) -> Self {
        let state = mix(seed.0 ^ mix(thread)) | 1;
        Self(STATE.try_with(|s| s.replace(state)).ok())
    }
}

impl Drop for Reseeded {
    fn drop(&mut self) {
        if let Some(previous) = self.0 {
            let _ = STATE.try_with(|s| s.set(previous));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_seed_draws_the_same_numbers_each_time_and_is_undone() {
        let before: Vec<u64> = {
            let _seeded = Reseeded::new(RngSeed::from_bytes(b"x"), 3);
            (0..8).map(|_| next()).collect()
        };
        let again: Vec<u64> = {
            let _seeded = Reseeded::new(RngSeed::from_bytes(b"x"), 3);
            (0..8).map(|_| next()).collect()
        };
        assert_eq!(before, again);
        let other_thread: Vec<u64> = {
            let _seeded = Reseeded::new(RngSeed::from_bytes(b"x"), 4);
            (0..8).map(|_| next()).collect()
        };
        assert_ne!(before, other_thread);
        let other_seed: Vec<u64> = {
            let _seeded = Reseeded::new(RngSeed::from_bytes(b"y"), 3);
            (0..8).map(|_| next()).collect()
        };
        assert_ne!(before, other_seed);
        // Undone: the thread's own generator goes on, not the seed's.
        let after: Vec<u64> = (0..8).map(|_| next()).collect();
        assert_ne!(before, after);
    }

    #[test]
    fn numbers_stay_below_the_bound() {
        for n in [1, 2, 3, 7, 64, 1000] {
            assert!((0..1000).all(|_| random_below(n) < n));
        }
        assert_ne!(RngSeed::new(), RngSeed::new());
    }
}
