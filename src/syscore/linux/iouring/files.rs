//! A ring's files, as the ring's own thread sees it: which slots hold a
//! file, which are free, and how many entries still name each slot.
//!
//! A slot is handed to a source at its first operation on the ring and
//! released when the source closes. It is not given to another source, and
//! the file it holds is not let go, until no entry names it any more,
//! submitted or still queued: an entry queued before the release and
//! submitted after it would otherwise reach the next source's file.
//!
//! The first slots are the kernel's fixed-file table, which entries name by
//! index. Past it, a slot holds the ring's own copy of the source's
//! descriptor, which entries name by number: the number stays the copy's
//! for as long as an entry names it, so it never reaches another file
//! either. A full table never turns a source away.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};

/// What one slot is doing.
#[derive(Default)]
struct Slot {
    /// A source holds it.
    held: bool,
    /// Entries that name it and have not completed.
    entries: u32,
    /// Past the kernel's table: the ring's copy of the source's descriptor.
    copy: Option<OwnedFd>,
}

/// A ring's file slots.
pub(crate) struct Files {
    slots: Vec<Slot>,
    /// The kernel's table has the slots below this.
    fixed: u32,
    /// Free slots of the kernel's table.
    free: Vec<u32>,
    /// Free slots past it.
    spare: Vec<u32>,
}

impl Files {
    /// The slots of a kernel table of `fixed` files, all free.
    pub(crate) fn new(fixed: u32) -> Self {
        Self {
            slots: (0..fixed).map(|_| Slot::default()).collect(),
            fixed,
            free: (0..fixed).rev().collect(),
            spare: Vec::new(),
        }
    }

    /// A free slot of the kernel's table, or `None` when every one is held
    /// or still named.
    pub(crate) fn take(&mut self) -> Option<u32> {
        let slot = self.free.pop()?;
        self.slots[slot as usize].held = true;
        Some(slot)
    }

    /// A slot past the kernel's table, holding `copy`.
    pub(crate) fn take_copy(&mut self, copy: OwnedFd) -> u32 {
        let slot = self.spare.pop().unwrap_or_else(|| {
            self.slots.push(Slot::default());
            (self.slots.len() - 1) as u32
        });
        let s = &mut self.slots[slot as usize];
        s.held = true;
        s.copy = Some(copy);
        slot
    }

    /// Whether `slot` is one of the kernel's table.
    pub(crate) fn is_fixed(&self, slot: u32) -> bool {
        slot < self.fixed
    }

    /// The descriptor entries name `slot` by: `None` for the kernel's
    /// table, whose entries name the index.
    pub(crate) fn copy_of(&self, slot: u32) -> Option<RawFd> {
        self.slots
            .get(slot as usize)
            .and_then(|s| s.copy.as_ref())
            .map(AsRawFd::as_raw_fd)
    }

    /// An entry naming `slot` was queued.
    pub(crate) fn named(&mut self, slot: u32) {
        if let Some(s) = self.slots.get_mut(slot as usize) {
            s.entries += 1;
        }
    }

    /// An entry naming `slot` completed. Returns whether the slot is free
    /// now: a slot of the kernel's table then still holds its file, which
    /// the caller lets go.
    pub(crate) fn done(&mut self, slot: u32) -> bool {
        match self.slots.get_mut(slot as usize) {
            // A slot no entry names is never freed by one (it would go back
            // to its list twice).
            Some(s) if s.entries > 0 => {
                s.entries -= 1;
                !s.held && s.entries == 0 && self.free_slot(slot)
            }
            _ => false,
        }
    }

    /// The source of `slot` closed: the slot is free once no entry names
    /// it. Returns whether it is free now, as [`done`](Self::done).
    pub(crate) fn release(&mut self, slot: u32) -> bool {
        match self.slots.get_mut(slot as usize) {
            Some(s) if s.held => {
                s.held = false;
                s.entries == 0 && self.free_slot(slot)
            }
            _ => false,
        }
    }

    /// Frees `slot`, neither held nor named: a copy closes here.
    fn free_slot(&mut self, slot: u32) -> bool {
        if self.is_fixed(slot) {
            self.free.push(slot);
        } else {
            self.slots[slot as usize].copy = None;
            self.spare.push(slot);
        }
        true
    }

    /// Slots held or still named.
    #[cfg(test)]
    pub(crate) fn in_use(&self) -> usize {
        self.slots.len() - self.free.len() - self.spare.len()
    }

    /// Slots past the kernel's table held or still named.
    #[cfg(test)]
    pub(crate) fn copies_in_use(&self) -> usize {
        self.slots.len() - self.fixed as usize - self.spare.len()
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::{AsRawFd, OwnedFd};

    use super::Files;

    fn descriptor() -> OwnedFd {
        std::fs::File::open("/dev/null").unwrap().into()
    }

    #[test]
    fn a_released_slot_waits_for_its_last_entry() {
        let mut files = Files::new(2);
        let a = files.take().unwrap();
        files.named(a);
        files.named(a);
        assert!(!files.release(a));
        assert!(!files.done(a));
        // One entry still names the slot: it is not handed out.
        let b = files.take().unwrap();
        assert_ne!(a, b);
        assert!(files.take().is_none(), "a still-named slot was handed out");
        assert!(files.done(a), "the last entry frees the slot");
        assert_eq!(files.take(), Some(a));
    }

    #[test]
    fn a_slot_with_no_entries_is_free_at_its_release() {
        let mut files = Files::new(1);
        let a = files.take().unwrap();
        assert_eq!(files.in_use(), 1);
        assert!(files.release(a));
        assert_eq!(files.in_use(), 0);
        assert_eq!(files.take(), Some(a));
    }

    /// Past a full table a source gets a slot holding a copy, which entries
    /// name by number and which closes only once no entry names it.
    #[test]
    fn past_a_full_table_a_copy_is_named_until_its_last_entry() {
        let mut files = Files::new(1);
        let fixed = files.take().unwrap();
        assert!(files.take().is_none());
        assert_eq!(files.copy_of(fixed), None);
        let copy = descriptor();
        let number = copy.as_raw_fd();
        let a = files.take_copy(copy);
        assert!(!files.is_fixed(a));
        assert_eq!(files.copy_of(a), Some(number));
        files.named(a);
        assert!(!files.release(a));
        // Still named: the copy stays open, so its number is still its own.
        // SAFETY: a plain fcntl that only asks whether `number` is open.
        assert!(unsafe { libc::fcntl(number, libc::F_GETFD) } >= 0);
        assert!(files.done(a));
        assert_eq!(files.copy_of(a), None);
        assert_eq!(files.in_use(), 1);
        // The freed slot past the table is the next copy's.
        let b = files.take_copy(descriptor());
        assert_eq!(a, b);
        assert!(files.release(b));
    }
}
