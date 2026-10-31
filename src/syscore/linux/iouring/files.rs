//! A ring's files, as the ring's own thread sees it: which slots a source
//! holds, which are free, and how many entries still name each slot.
//!
//! A slot is handed to a source at its first operation on the ring and
//! released when the source goes. It is not given to another source until no
//! entry names it any more, submitted or still queued: an entry queued before
//! the release and submitted after it would otherwise reach the next source's
//! file.
//!
//! The first slots are the kernel's fixed-file table, which entries name by
//! index. Past it, a slot names the source's own descriptor, which entries
//! name by number: the ring makes no copy of it, so a process with no
//! descriptor to spare registers its sources all the same. The number stays
//! the source's while an entry that names it is queued: the source's object
//! closes only once its entries reached the kernel (which holds the file of
//! each from then on), as the ring's detach sees to (`ShardRing::detach`).
//!
//! A source that goes is taken out of the table at once (the kernel then
//! closes the file once its last request let go of it), while the slot stays
//! reserved until its entries completed.
//!
//! Before Linux 6.13 a file taken out of the kernel's table is let go only
//! once every request issued while it was in the table completed, those on
//! other files too, so one idle receive anywhere on the ring keeps a closed
//! socket open: its peer never sees the end. Where [`release_at_once`] says
//! the kernel behaves so, rings get no table, and every source's entries name
//! its own descriptor, each request holding the file itself.

use std::os::fd::{AsRawFd, OwnedFd, RawFd};

/// What one slot is doing.
#[derive(Default)]
struct Slot {
    /// A source holds it.
    held: bool,
    /// Entries that name it and have not completed.
    entries: u32,
    /// Past the kernel's table: the source's own descriptor, which entries
    /// name by number.
    own: Option<RawFd>,
}

/// A ring's file slots.
pub(crate) struct Files {
    /// The kernel's table, grown as slots are first taken, up to `fixed`.
    table: Vec<Slot>,
    /// The slots past it, by their number less `fixed`.
    copies: Vec<Slot>,
    /// The kernel's table has the slots below this.
    fixed: u32,
    /// Free slots of the kernel's table, those taken before.
    free: Vec<u32>,
    /// Free slots past it.
    spare: Vec<u32>,
}

impl Files {
    /// The slots of a kernel table of `fixed` files, all free.
    pub(crate) fn new(fixed: u32) -> Self {
        Self {
            table: Vec::new(),
            copies: Vec::new(),
            fixed,
            free: Vec::new(),
            spare: Vec::new(),
        }
    }

    fn slot(&self, slot: u32) -> Option<&Slot> {
        match slot.checked_sub(self.fixed) {
            None => self.table.get(slot as usize),
            Some(copy) => self.copies.get(copy as usize),
        }
    }

    fn slot_mut(&mut self, slot: u32) -> Option<&mut Slot> {
        match slot.checked_sub(self.fixed) {
            None => self.table.get_mut(slot as usize),
            Some(copy) => self.copies.get_mut(copy as usize),
        }
    }

    /// A free slot of the kernel's table, or `None` when every one is held
    /// or still named.
    pub(crate) fn take(&mut self) -> Option<u32> {
        let slot = match self.free.pop() {
            Some(slot) => slot,
            None if self.table.len() < self.fixed as usize => {
                self.table.push(Slot::default());
                (self.table.len() - 1) as u32
            }
            None => return None,
        };
        self.table[slot as usize].held = true;
        Some(slot)
    }

    /// A slot past the kernel's table, naming the source's own descriptor
    /// `fd`.
    pub(crate) fn take_own(&mut self, fd: RawFd) -> u32 {
        let slot = self.spare.pop().unwrap_or_else(|| {
            self.copies.push(Slot::default());
            self.fixed + (self.copies.len() - 1) as u32
        });
        if let Some(s) = self.slot_mut(slot) {
            s.held = true;
            s.own = Some(fd);
        }
        slot
    }

    /// Whether `slot` is one of the kernel's table.
    pub(crate) fn is_fixed(&self, slot: u32) -> bool {
        slot < self.fixed
    }

    /// The descriptor entries name `slot` by: `None` for the kernel's
    /// table, whose entries name the index.
    pub(crate) fn number_of(&self, slot: u32) -> Option<RawFd> {
        self.slot(slot).and_then(|s| s.own)
    }

    /// Whether an entry still names `slot`.
    pub(crate) fn is_named(&self, slot: u32) -> bool {
        self.slot(slot).is_some_and(|s| s.entries > 0)
    }

    /// An entry naming `slot` was queued.
    #[cfg(test)]
    pub(crate) fn named(&mut self, slot: u32) {
        self.name(slot);
    }

    /// An entry naming `slot` was queued: returns [`number_of`](Self::number_of) the slot, in one
    /// lookup.
    #[inline]
    pub(crate) fn name(&mut self, slot: u32) -> Option<RawFd> {
        let s = self.slot_mut(slot)?;
        s.entries += 1;
        s.own
    }

    /// An entry naming `slot` completed. Returns whether the slot is free
    /// now: it can be handed out again (its file left the table as its source
    /// went).
    pub(crate) fn done(&mut self, slot: u32) -> bool {
        match self.slot_mut(slot) {
            // A slot no entry names is never freed by one (it would go back
            // to its list twice).
            Some(s) if s.entries > 0 => {
                s.entries -= 1;
                !s.held && s.entries == 0 && self.free_slot(slot)
            }
            _ => false,
        }
    }

    /// The source of `slot` went: the slot is free once no entry names it,
    /// and reserved until then. Returns whether it is free now, as
    /// [`done`](Self::done). The caller takes a file of the kernel's table
    /// out of it now, whatever entries still name the slot.
    pub(crate) fn release(&mut self, slot: u32) -> bool {
        match self.slot_mut(slot) {
            Some(s) if s.held => {
                s.held = false;
                s.entries == 0 && self.free_slot(slot)
            }
            _ => false,
        }
    }

    /// Frees `slot`, neither held nor named: it names no descriptor now.
    fn free_slot(&mut self, slot: u32) -> bool {
        if self.is_fixed(slot) {
            self.free.push(slot);
        } else {
            if let Some(s) = self.slot_mut(slot) {
                s.own = None;
            }
            self.spare.push(slot);
        }
        true
    }

    /// Slots held or still named.
    #[cfg(test)]
    pub(crate) fn in_use(&self) -> usize {
        self.table.len() + self.copies.len() - self.free.len() - self.spare.len()
    }

    /// Slots past the kernel's table held or still named.
    #[cfg(test)]
    pub(crate) fn own_in_use(&self) -> usize {
        self.copies.len() - self.spare.len()
    }
}

/// Whether a file taken out of a ring's fixed-file table is let go at once
/// while other requests are out (Linux 6.13 on), found once per process by
/// trying it: a pipe's write end in the table, a read out on another fixed
/// file, the write end taken out and closed here; the pipe's read end sees
/// the hang-up only if the kernel let go of its file. Anything that goes
/// wrong in the trial says no, which costs only the table's speed.
pub(crate) fn release_at_once() -> bool {
    static AT_ONCE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AT_ONCE.get_or_init(|| trial().unwrap_or(false))
}

fn trial() -> std::io::Result<bool> {
    use std::os::fd::FromRawFd;

    use super::ring::{Ring, RingConfig, Sqe, sys::IOSQE_FIXED_FILE};

    let pipe = || -> std::io::Result<(OwnedFd, OwnedFd)> {
        let mut fds = [-1; 2];
        // SAFETY: the kernel writes the two descriptors into our array.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: two new descriptors nothing else owns.
        Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
    };
    let (watched_read, watched_write) = pipe()?;
    let (busy_read, busy_write) = pipe()?;
    let mut ring = Ring::new(&RingConfig::new(4))?;
    ring.register_files_sparse(2)?;
    ring.update_file(0, watched_write.as_raw_fd())?;
    ring.update_file(1, busy_read.as_raw_fd())?;
    // A read out on the other fixed file, until the trial writes to it.
    let mut byte = [0u8; 1];
    let read = Sqe::read(1, byte.as_mut_ptr(), 1, u64::MAX)
        .flags(IOSQE_FIXED_FILE)
        .user_data(1);
    ring.push(&read)
        .map_err(|_| std::io::Error::other("no room in a new ring"))?;
    ring.submit()?;
    ring.update_file(0, -1)?;
    drop(watched_write);
    // Deferred work the update left runs at an enter.
    ring.submit()?;
    let mut poll = libc::pollfd {
        fd: watched_read.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one pollfd of our own; no wait.
    let polled = unsafe { libc::poll(&mut poll, 1, 0) };
    let released = polled == 1 && poll.revents & libc::POLLHUP != 0;
    // The read ends, so the ring holds nothing of `byte` as it goes.
    // SAFETY: writes one byte of our own to our own pipe.
    unsafe { libc::write(busy_write.as_raw_fd(), [0u8].as_ptr().cast(), 1) };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        ring.submit_and_wait(1, Some(std::time::Duration::from_millis(100)))?;
        if ring.completions().any(|cqe| cqe.user_data == 1) {
            break;
        }
        if std::time::Instant::now() >= deadline {
            // The read never ended: the ring keeps `byte` for good.
            core::mem::forget(ring);
            return Ok(false);
        }
    }
    Ok(released)
}

#[cfg(test)]
mod tests {
    use std::os::fd::{AsRawFd, OwnedFd};

    use super::Files;

    fn descriptor() -> OwnedFd {
        std::fs::File::open("/dev/null").unwrap().into()
    }

    /// The trial's answer is the kernel's: a file out of the table goes at
    /// once from Linux 6.13, late before.
    #[test]
    fn the_release_trial_answers_as_the_kernel_behaves() {
        // SAFETY: plain data, all zeroes valid; the kernel fills it.
        let mut name: libc::utsname = unsafe { core::mem::zeroed() };
        // SAFETY: a plain syscall filling our own struct.
        assert_eq!(unsafe { libc::uname(&mut name) }, 0);
        // SAFETY: the kernel wrote a NUL-terminated release.
        let release = unsafe { core::ffi::CStr::from_ptr(name.release.as_ptr()) }
            .to_string_lossy()
            .into_owned();
        let mut parts = release.split(|c: char| !c.is_ascii_digit());
        let major: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap();
        let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap();
        if super::super::ring::Ring::new(&super::super::ring::RingConfig::new(4)).is_err() {
            eprintln!("skipping: io_uring unavailable");
            return;
        }
        assert_eq!(
            super::release_at_once(),
            (major, minor) >= (6, 13),
            "kernel {release}"
        );
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

    /// Past a full table a source gets a slot naming its own descriptor,
    /// which entries name by number, reserved until its last entry.
    #[test]
    fn past_a_full_table_a_slot_names_the_sources_own_descriptor_until_its_last_entry() {
        let mut files = Files::new(1);
        let fixed = files.take().unwrap();
        assert!(files.take().is_none());
        assert_eq!(files.number_of(fixed), None);
        let own = descriptor();
        let number = own.as_raw_fd();
        let a = files.take_own(number);
        assert!(!files.is_fixed(a));
        assert_eq!(files.number_of(a), Some(number));
        files.named(a);
        assert!(!files.release(a));
        assert!(files.is_named(a));
        // Reserved while named: a new source gets another slot.
        let other = descriptor();
        let b = files.take_own(other.as_raw_fd());
        assert_ne!(a, b);
        assert!(files.done(a));
        assert_eq!(files.number_of(a), None);
        assert_eq!(files.in_use(), 2);
        // The freed slot past the table is the next source's.
        let c = files.take_own(number);
        assert_eq!(a, c);
        assert!(files.release(c));
        assert!(files.release(b));
        drop((own, other));
    }
}
