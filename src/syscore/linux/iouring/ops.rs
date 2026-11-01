//! The operations a ring has in flight, found by their user data.
//!
//! An operation's user data is its slot's index and the slot's generation,
//! never a pointer. The slot holds everything the operation's entry points
//! at (buffers, addresses, message headers) and is freed only by the
//! operation's own final completion, so that memory lives as long as the
//! kernel may use it. A completion whose user data names a slot of another
//! generation (one whose operation is gone and whose slot was reused)
//! finds nothing.
//!
//! The slab holds at most its capacity, the ring's completion queue size:
//! the operations in flight never have more completions to post than the
//! queue holds, and an operation that finds the slab full waits for room.

/// Bits of the user data that index the slot; the rest carry the generation.
const INDEX_BITS: u32 = 32;
const INDEX_MASK: u64 = (1 << INDEX_BITS) - 1;

/// The user data no operation of the slab ever has: the ring's own wake.
pub(crate) const NOT_AN_OP: u64 = u64::MAX;

struct Slot<T> {
    generation: u32,
    op: Option<T>,
}

/// A ring's operations in flight.
pub(crate) struct Ops<T> {
    slots: Vec<Slot<T>>,
    free: Vec<u32>,
    capacity: usize,
}

fn user_data(index: u32, generation: u32) -> u64 {
    (u64::from(generation) << INDEX_BITS) | u64::from(index)
}

impl<T> Ops<T> {
    /// A slab for at most `capacity` operations in flight (fewer than
    /// 2^32 - 1, so no index is all ones).
    pub(crate) fn new(capacity: u32) -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            capacity: capacity.min(u32::MAX - 1) as usize,
        }
    }

    /// Keeps `op` and returns the user data its entry carries, or gives `op`
    /// back when the slab is full.
    #[inline]
    pub(crate) fn insert(&mut self, op: T) -> Result<u64, T> {
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            slot.op = Some(op);
            return Ok(user_data(index, slot.generation));
        }
        if self.slots.len() >= self.capacity {
            return Err(op);
        }
        let index = self.slots.len() as u32;
        self.slots.push(Slot {
            generation: 0,
            op: Some(op),
        });
        Ok(user_data(index, 0))
    }

    /// The operation of `user_data`, if it is still in flight.
    pub(crate) fn get_mut(&mut self, user_data: u64) -> Option<&mut T> {
        let slot = self.slots.get_mut((user_data & INDEX_MASK) as usize)?;
        if u64::from(slot.generation) != user_data >> INDEX_BITS {
            return None;
        }
        slot.op.as_mut()
    }

    /// Takes the operation of `user_data` out, freeing its slot for another
    /// generation; a stale user data takes nothing.
    pub(crate) fn remove(&mut self, user_data: u64) -> Option<T> {
        self.remove_if(user_data, |_| true)
    }

    /// As [`remove`](Self::remove), but only an operation `take` says
    /// to take: one lookup for the look and the take.
    #[inline]
    pub(crate) fn remove_if(&mut self, user_data: u64, take: impl FnOnce(&T) -> bool) -> Option<T> {
        let index = (user_data & INDEX_MASK) as usize;
        let slot = self.slots.get_mut(index)?;
        if u64::from(slot.generation) != user_data >> INDEX_BITS
            || !slot.op.as_ref().is_some_and(take)
        {
            return None;
        }
        let op = slot.op.take()?;
        slot.generation = slot.generation.wrapping_add(1);
        self.free.push(index as u32);
        Some(op)
    }

    /// How many operations are in flight.
    pub(crate) fn len(&self) -> usize {
        self.slots.len() - self.free.len()
    }

    /// The user data of every operation in flight.
    pub(crate) fn keys(&self) -> impl Iterator<Item = u64> + '_ {
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| slot.op.is_some())
            .map(|(index, slot)| user_data(index as u32, slot.generation))
    }

    /// The operations in flight, for a test.
    #[cfg(test)]
    pub(crate) fn iter(&self) -> impl Iterator<Item = &T> {
        self.slots.iter().filter_map(|slot| slot.op.as_ref())
    }

    /// Takes every operation out: what a ring that could not drain leaves.
    pub(crate) fn drain(&mut self) -> impl Iterator<Item = T> + '_ {
        self.free.clear();
        self.slots.drain(..).filter_map(|slot| slot.op)
    }
}

#[cfg(test)]
mod tests {
    use super::{INDEX_MASK, NOT_AN_OP, Ops};

    #[test]
    fn a_completion_for_a_reused_slot_finds_nothing() {
        let mut ops = Ops::new(8);
        let a = ops.insert("first").unwrap();
        assert_eq!(ops.remove(a), Some("first"));
        let b = ops.insert("second").unwrap();
        assert_eq!(a & INDEX_MASK, b & INDEX_MASK, "the slot is reused");
        assert_ne!(a, b);
        assert!(ops.get_mut(a).is_none(), "the old user data names nothing");
        assert!(ops.remove(a).is_none(), "nor removes the new operation");
        assert_eq!(ops.remove(b), Some("second"));
        assert_eq!(ops.len(), 0);
    }

    #[test]
    fn no_operation_has_the_wakes_user_data() {
        let mut ops = Ops::new(10_000);
        for i in 0..10_000u32 {
            let u = ops.insert(i).unwrap();
            assert_ne!(u, NOT_AN_OP);
            if i % 3 == 0 {
                ops.remove(u);
            }
        }
    }

    #[test]
    fn a_full_slab_gives_the_operation_back_until_one_completes() {
        let mut ops = Ops::new(2);
        let a = ops.insert(1).unwrap();
        ops.insert(2).unwrap();
        assert_eq!(ops.insert(3), Err(3));
        ops.remove(a);
        assert!(ops.insert(3).is_ok());
    }
}
