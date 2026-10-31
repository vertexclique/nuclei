//! The values of the `shard_local!` statics on one shard: one per key, made
//! on its first use there and dropped with the shard. They live in the
//! shard's own part, so they move with it when it goes to another thread
//! (`rt::handoff`), and only the thread running the shard reaches them.

use core::any::Any;
use core::cell::RefCell;

/// A key's place on a shard.
enum Place {
    Empty,
    /// Its value is being made: the maker used the key itself.
    Making,
    /// Boxed: a value never moves while the shard lives, however many keys
    /// come after it.
    Made(Box<dyn Any + Send>),
}

/// A shard's shard-local values, by key.
#[derive(Default)]
pub(crate) struct Locals(RefCell<Vec<Place>>);

/// Why a key has no value to lend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Missing {
    /// Its value is being made, by a maker that used the key.
    Making,
}

impl Locals {
    /// The value of `key`, made with `init` on its first use here.
    pub(crate) fn get_or_init<T>(&self, key: usize, init: fn() -> T) -> Result<&T, Missing>
    where
        T: Send + 'static,
    {
        {
            let mut places = self.0.borrow_mut();
            if places.len() <= key {
                places.resize_with(key + 1, || Place::Empty);
            }
            match &places[key] {
                // SAFETY: see `lend`.
                Place::Made(value) => return Ok(unsafe { &*Self::lend(value) }),
                Place::Making => return Err(Missing::Making),
                Place::Empty => places[key] = Place::Making,
            }
        }
        // Back to empty if `init` unwinds, so a later use tries again.
        struct Unmade<'a>(&'a Locals, usize, bool);
        impl Drop for Unmade<'_> {
            fn drop(&mut self) {
                if !self.2 {
                    self.0.0.borrow_mut()[self.1] = Place::Empty;
                }
            }
        }
        let mut unmade = Unmade(self, key, false);
        // The borrow is let go of: `init` may use other keys.
        let value: Box<dyn Any + Send> = Box::new(init());
        let mut places = self.0.borrow_mut();
        places[key] = Place::Made(value);
        unmade.2 = true;
        match &places[key] {
            // SAFETY: see `lend`.
            Place::Made(value) => Ok(unsafe { &*Self::lend(value) }),
            Place::Empty | Place::Making => Err(Missing::Making),
        }
    }

    /// The value in `value`'s box, which may be lent for as long as the
    /// shard lives: the box is never dropped or moved meanwhile, as places
    /// are only ever added, and `take` needs the shard by `&mut`.
    fn lend<T: 'static>(value: &Box<dyn Any + Send>) -> *const T {
        match value.downcast_ref::<T>() {
            Some(value) => value,
            // A key is one static's, of one type.
            None => unreachable_type(),
        }
    }

    /// Takes every value out, to be dropped as the shard goes.
    pub(crate) fn take(&mut self) -> Vec<Box<dyn Any + Send>> {
        core::mem::take(self.0.get_mut())
            .into_iter()
            .filter_map(|place| match place {
                Place::Made(value) => Some(value),
                Place::Empty | Place::Making => None,
            })
            .collect()
    }
}

#[cold]
#[expect(
    clippy::panic,
    reason = "a key belongs to one static of one type: another type is a bug of this module"
)]
fn unreachable_type() -> ! {
    panic!("a shard-local key held a value of another type")
}
