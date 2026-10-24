//! The IO sources a shard's poller watches, by token.
//!
//! A token is a slot index and the slot's generation, and is what the
//! kernel hands back with each event (epoll data, kevent udata): never a
//! pointer. A slot's generation moves on when its registration goes, so an
//! event still in flight for a registration that is gone, or for a slot
//! reused since, finds nothing. Tokens fit a pointer-sized word, since that
//! is all kqueue carries.

use std::io;
use std::sync::Arc;

use crate::io::Source;

/// Bits of a token that index the slot; the rest carry the generation.
const INDEX_BITS: u32 = if usize::BITS >= 64 { 32 } else { 20 };
const INDEX_MASK: u64 = (1 << INDEX_BITS) - 1;
/// Generations wrap within what a token holds above the index.
const GEN_MASK: u64 = (usize::MAX as u64) >> INDEX_BITS;

struct Slot {
    generation: u64,
    source: Option<Arc<Source>>,
}

/// The shard's registrations.
#[derive(Default)]
pub(crate) struct Sources {
    slots: Vec<Slot>,
    free: Vec<u32>,
}

fn token(index: u32, generation: u64) -> u64 {
    (generation << INDEX_BITS) | u64::from(index)
}

impl Sources {
    /// Stores `source` and returns its token.
    pub(crate) fn insert(&mut self, source: Arc<Source>) -> io::Result<u64> {
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            slot.source = Some(source);
            return Ok(token(index, slot.generation));
        }
        let index = self.slots.len();
        // The last index is never handed out, so no token is all ones (the
        // pollers' own wake and "no registration" use that).
        if index as u64 >= INDEX_MASK {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "the shard watches as many sources as a token can name",
            ));
        }
        self.slots.push(Slot {
            generation: 0,
            source: Some(source),
        });
        Ok(token(index as u32, 0))
    }

    /// The source registered under `token`, if that registration is still
    /// in place.
    pub(crate) fn get(&self, token: u64) -> Option<&Arc<Source>> {
        let slot = self.slots.get((token & INDEX_MASK) as usize)?;
        if slot.generation != token >> INDEX_BITS {
            return None;
        }
        slot.source.as_ref()
    }

    /// Ends the registration under `token`; a stale token does nothing.
    pub(crate) fn remove(&mut self, token: u64) -> Option<Arc<Source>> {
        let index = (token & INDEX_MASK) as usize;
        let slot = self.slots.get_mut(index)?;
        if slot.generation != token >> INDEX_BITS {
            return None;
        }
        let source = slot.source.take()?;
        slot.generation = (slot.generation + 1) & GEN_MASK;
        self.free.push(index as u32);
        Some(source)
    }

    /// Takes every registered source out.
    pub(crate) fn drain(&mut self) -> impl Iterator<Item = Arc<Source>> + '_ {
        self.free.clear();
        self.slots.drain(..).filter_map(|slot| slot.source)
    }

    /// How many sources are registered.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.slots.len() - self.free.len()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::{INDEX_BITS, Sources};
    use crate::io::Source;
    use crate::io::source::Kind;

    fn source() -> std::sync::Arc<Source> {
        Source::new(-1, Kind::Bytes)
    }

    #[test]
    fn a_removed_registration_is_not_found_by_its_old_token() {
        let mut s = Sources::default();
        let a = s.insert(source()).unwrap();
        assert!(s.get(a).is_some());
        assert!(s.remove(a).is_some());
        assert!(s.get(a).is_none());
        // The slot is reused under a new generation.
        let b = s.insert(source()).unwrap();
        assert_eq!(a & ((1 << INDEX_BITS) - 1), b & ((1 << INDEX_BITS) - 1));
        assert_ne!(a, b);
        assert!(s.get(a).is_none(), "the old token names nothing");
        assert!(s.remove(a).is_none(), "nor removes the new registration");
        assert!(s.get(b).is_some());
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn tokens_fit_a_pointer_and_are_never_all_ones() {
        let mut s = Sources::default();
        for _ in 0..1000 {
            let t = s.insert(source()).unwrap();
            assert!(t <= usize::MAX as u64);
            assert_ne!(t, u64::MAX);
            s.remove(t);
        }
    }
}
