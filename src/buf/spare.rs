//! Boxes of small, fixed-size values an operation points the kernel at (a
//! socket address, a message header), kept for reuse by the thread that
//! lets them go.
//!
//! An operation in flight needs such a value at an address that stays put
//! until its completion, while the operation itself may move; so it lives
//! in a box. A [`Spare`] takes its box from the thread's spares and gives
//! it back when it drops, so in steady state an operation allocates
//! nothing for it. Each thread keeps at most [`SPARES`] of a type: the
//! memory held is bounded, and a burst past it frees what it does not keep.

use core::mem::ManuallyDrop;
use core::ops::{Deref, DerefMut};

/// The most spares a thread keeps of one type.
pub(crate) const SPARES: usize = 64;

/// A type whose boxes a thread keeps for reuse.
pub(crate) trait Reused: Sized + 'static {
    /// Runs `f` on this thread's spares of the type; `None` on a thread
    /// that is going (its spares are gone) or inside another such call.
    fn with_spares<R>(f: impl FnOnce(&mut Vec<Box<Self>>) -> R) -> Option<R>;
}

/// Declares the types whose boxes threads keep, each with its own list.
macro_rules! reused {
    ($($ty:ty),* $(,)?) => {$(
        impl $crate::buf::spare::Reused for $ty {
            fn with_spares<R>(f: impl FnOnce(&mut Vec<Box<Self>>) -> R) -> Option<R> {
                thread_local! {
                    static SPARES: core::cell::RefCell<Vec<Box<$ty>>> =
                        const { core::cell::RefCell::new(Vec::new()) };
                }
                SPARES
                    .try_with(|spares| spares.try_borrow_mut().ok().map(|mut s| f(&mut s)))
                    .ok()
                    .flatten()
            }
        }
    )*};
}
pub(crate) use reused;

/// A boxed `T`, its box one of this thread's spares if one was there.
pub(crate) struct Spare<T: Reused>(ManuallyDrop<Box<T>>);

impl<T: Reused> Spare<T> {
    pub(crate) fn new(value: T) -> Spare<T> {
        let boxed = match T::with_spares(Vec::pop).flatten() {
            Some(mut spare) => {
                *spare = value;
                spare
            }
            None => Box::new(value),
        };
        Spare(ManuallyDrop::new(boxed))
    }
}

impl<T: Reused + Clone> Spare<T> {
    /// The value, its box going back to the spares.
    pub(crate) fn into_inner(self) -> T {
        T::clone(&self)
    }
}

impl<T: Reused + Clone> Clone for Spare<T> {
    fn clone(&self) -> Self {
        Self::new(T::clone(self))
    }
}

impl<T: Reused + core::fmt::Debug> core::fmt::Debug for Spare<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        T::fmt(self, f)
    }
}

impl<T: Reused> Deref for Spare<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: Reused> DerefMut for Spare<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

impl<T: Reused> Drop for Spare<T> {
    fn drop(&mut self) {
        // SAFETY: taken once, here; the field is not touched after.
        let mut boxed = Some(unsafe { ManuallyDrop::take(&mut self.0) });
        T::with_spares(|spares| {
            if spares.len() < SPARES
                && let Some(spare) = boxed.take()
            {
                spares.push(spare);
            }
        });
        // A box not kept is freed here.
        drop(boxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct Probe(u64);
    reused!(Probe);

    fn spares() -> usize {
        Probe::with_spares(|s| s.len()).unwrap()
    }

    #[test]
    fn a_dropped_box_is_the_next_ones() {
        Probe::with_spares(Vec::clear);
        let first = Spare::new(Probe(1));
        let at = core::ptr::from_ref::<Probe>(&first) as usize;
        drop(first);
        assert_eq!(spares(), 1);
        let second = Spare::new(Probe(2));
        assert_eq!(
            core::ptr::from_ref::<Probe>(&second) as usize,
            at,
            "the box was reused"
        );
        assert_eq!(*second, Probe(2));
        assert_eq!(spares(), 0);
    }

    #[test]
    fn into_inner_gives_the_value_and_keeps_the_box() {
        Probe::with_spares(Vec::clear);
        assert_eq!(Spare::new(Probe(7)).into_inner(), Probe(7));
        assert_eq!(spares(), 1);
    }

    #[test]
    fn a_thread_keeps_at_most_its_bound() {
        Probe::with_spares(Vec::clear);
        let many: Vec<_> = (0..SPARES as u64 + 10)
            .map(|i| Spare::new(Probe(i)))
            .collect();
        drop(many);
        assert_eq!(spares(), SPARES);
        // Another thread starts with none of them.
        std::thread::spawn(|| assert_eq!(spares(), 0))
            .join()
            .unwrap();
    }
}
