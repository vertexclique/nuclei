//! A shard's timers: a hierarchical timing wheel.
//!
//! Six levels of 64 slots, at one millisecond per slot on level 0 and 64
//! times wider on each level up: level `l` slot `s` covers the
//! milliseconds `[s * 64^l, (s + 1) * 64^l)` of the current `64^(l+1)`
//! window. A timer goes to the lowest level whose window holds both the
//! wheel's time and the timer's deadline (the highest bit in which the two
//! differ picks it), in the slot its deadline falls in. Each level keeps a
//! bitmap of its occupied slots, so the next slot to look at is a bit scan.
//! When the wheel's time reaches a slot: on level 0 its timers fire; above,
//! its timers are inserted again, which puts each on a lower level (its
//! deadline now shares a smaller window with the wheel's time), until it
//! fires on level 0 exactly at its deadline.
//!
//! Timers live in a slab of entries linked into their slot's list by index,
//! so arming and cancelling are O(1) and allocate nothing once the slab has
//! grown to the most timers alive at once. A [`TimerKey`] carries the
//! entry's generation: a key whose timer is gone never reaches the entry
//! that reuses its place.
//!
//! The wheel is shard-local and knows only milliseconds since its own
//! start; the time module converts instants and rounds deadlines up, so a
//! timer never fires early. `proofs/lean/Nuclei/Wheel.lean` proves the
//! placement and cascade arithmetic this rests on, for the window and slot
//! definition `level_by_windows` gives; a property test checks the fast
//! `level_for` against it.

use core::task::Waker;

/// Bits of a slot index.
const SLOT_BITS: u32 = 6;
/// Slots per level.
const SLOTS: usize = 1 << SLOT_BITS;
/// Levels of the wheel.
const LEVELS: usize = 6;
/// How far ahead the wheel places a timer exactly: `64^6` milliseconds,
/// about 795 days. A deadline further away is placed on the top level at
/// that distance and placed again when its slot comes.
const SPAN: u64 = 1 << (SLOT_BITS as u64 * LEVELS as u64);
/// The end of a slot list, and of the free list.
const NIL: usize = usize::MAX;

/// A timer in the wheel. Stale once its timer is removed: every call with
/// it then does nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TimerKey {
    index: usize,
    generation: u32,
}

/// Where an entry is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
    /// On the free list.
    Free,
    /// Linked into a slot.
    Armed { level: u8, slot: u8 },
    /// Its deadline passed: its waker was taken and woken.
    Fired,
}

struct Entry {
    /// The deadline, in milliseconds of the wheel.
    when: u64,
    waker: Option<Waker>,
    /// The neighbours in the slot list (or, for a free entry, `next` is the
    /// next free one).
    prev: usize,
    next: usize,
    generation: u32,
    place: Place,
}

/// One level: the head of each slot's list and which slots hold any.
struct Level {
    occupied: u64,
    heads: [usize; SLOTS],
}

impl Level {
    const fn new() -> Self {
        Self {
            occupied: 0,
            heads: [NIL; SLOTS],
        }
    }
}

/// A shard's timing wheel.
pub(crate) struct Wheel {
    /// The wheel's time: every slot before it has been processed.
    elapsed: u64,
    levels: [Level; LEVELS],
    entries: Vec<Entry>,
    free: usize,
    /// Timers armed.
    armed: usize,
}

impl Default for Wheel {
    fn default() -> Self {
        Self::new()
    }
}

/// The level a deadline `when` goes to while the wheel's time is
/// `elapsed`: the level of the highest bit in which the two differ (level 0
/// when they share every bit above the slot bits).
#[inline]
fn level_for(elapsed: u64, when: u64) -> usize {
    let masked = ((elapsed ^ when) | (SLOTS as u64 - 1)).min(SPAN - 1);
    let significant = 63 - masked.leading_zeros();
    (significant / SLOT_BITS) as usize
}

/// The slot of `when` on `level`.
#[inline]
fn slot_for(when: u64, level: usize) -> usize {
    ((when >> (level as u32 * SLOT_BITS)) as usize) & (SLOTS - 1)
}

/// `level_for` by its definition: the lowest level whose window (the
/// `64^(level+1)` milliseconds around it) holds both times, or the top
/// level. What the Lean proof and the property test are written against.
#[cfg(test)]
fn level_by_windows(elapsed: u64, when: u64) -> usize {
    (0..LEVELS)
        .find(|&l| {
            let width = SLOT_BITS * (l as u32 + 1);
            elapsed >> width == when >> width
        })
        .unwrap_or(LEVELS - 1)
}

impl Wheel {
    pub(crate) const fn new() -> Self {
        Self {
            elapsed: 0,
            levels: [const { Level::new() }; LEVELS],
            entries: Vec::new(),
            free: NIL,
            armed: 0,
        }
    }

    /// The wheel's time: every deadline up to it has been processed.
    #[cfg(test)]
    pub(crate) fn elapsed(&self) -> u64 {
        self.elapsed
    }

    /// How many timers are armed.
    pub(crate) fn armed(&self) -> usize {
        self.armed
    }

    /// Takes the waker of every timer, for a wheel whose shard goes: none
    /// fires any more. The timers stay, for their keys to remove.
    pub(crate) fn take_wakers(&mut self) -> Vec<Waker> {
        self.entries
            .iter_mut()
            .filter_map(|entry| entry.waker.take())
            .collect()
    }

    /// Arms a timer for `when`, waking `waker` once the wheel passes it. A
    /// deadline the wheel has passed already is fired at once, without a
    /// wake: the caller sees it fired when it looks.
    pub(crate) fn insert(&mut self, when: u64, waker: &Waker) -> TimerKey {
        let index = self.alloc();
        let entry = &mut self.entries[index];
        entry.when = when;
        if when <= self.elapsed {
            entry.place = Place::Fired;
        } else {
            entry.waker = Some(waker.clone());
            self.link(index);
        }
        TimerKey {
            index,
            generation: self.entries[index].generation,
        }
    }

    /// Whether the timer of `key` has fired. A stale key reads as fired: its
    /// timer is gone, so nothing will fire it again.
    pub(crate) fn is_fired(&self, key: TimerKey) -> bool {
        self.entry(key)
            .is_none_or(|entry| entry.place == Place::Fired)
    }

    /// Replaces the waker of an armed timer, unless it would wake the same
    /// task already.
    pub(crate) fn set_waker(&mut self, key: TimerKey, waker: &Waker) {
        if let Some(index) = self.live(key) {
            let entry = &mut self.entries[index];
            if entry.place != Place::Fired {
                match &entry.waker {
                    Some(old) if old.will_wake(waker) => {}
                    _ => entry.waker = Some(waker.clone()),
                }
            }
        }
    }

    /// Moves the timer of `key` to a new deadline, armed again if it had
    /// fired. A stale key does nothing.
    #[cfg(test)]
    pub(crate) fn reset(&mut self, key: TimerKey, when: u64, waker: &Waker) {
        let Some(index) = self.live(key) else { return };
        if let Place::Armed { .. } = self.entries[index].place {
            self.unlink(index);
        }
        let entry = &mut self.entries[index];
        entry.when = when;
        if when <= self.elapsed {
            entry.place = Place::Fired;
            entry.waker = None;
        } else {
            if !entry.waker.as_ref().is_some_and(|w| w.will_wake(waker)) {
                entry.waker = Some(waker.clone());
            }
            self.link(index);
        }
    }

    /// Removes the timer of `key`, armed or fired; its key goes stale. A
    /// stale key does nothing.
    pub(crate) fn remove(&mut self, key: TimerKey) {
        let Some(index) = self.live(key) else { return };
        if let Place::Armed { .. } = self.entries[index].place {
            self.unlink(index);
        }
        let entry = &mut self.entries[index];
        entry.waker = None;
        entry.place = Place::Free;
        entry.generation = entry.generation.wrapping_add(1);
        entry.next = self.free;
        self.free = index;
    }

    /// The earliest time the wheel has work at: a level-0 slot whose timers
    /// fire then, or a higher slot whose timers move down then. `None` with
    /// no timer armed. What a shard's park waits for at most.
    pub(crate) fn next_deadline(&self) -> Option<u64> {
        self.next_slot().map(|(_, _, deadline)| deadline)
    }

    /// Processes every slot up to `now`: timers whose deadline passed fire,
    /// their wakers pushed to `fired` (woken by the caller once it no longer
    /// holds the wheel); timers of higher slots move down.
    pub(crate) fn advance(&mut self, now: u64, fired: &mut Vec<Waker>) {
        while let Some((level, slot, deadline)) = self.next_slot() {
            if deadline > now {
                break;
            }
            self.elapsed = deadline;
            let mut index = self.levels[level].heads[slot];
            self.levels[level].heads[slot] = NIL;
            self.levels[level].occupied &= !(1 << slot);
            while index != NIL {
                let entry = &mut self.entries[index];
                let next = entry.next;
                if entry.when <= now {
                    entry.place = Place::Fired;
                    self.armed -= 1;
                    if let Some(waker) = entry.waker.take() {
                        fired.push(waker);
                    }
                } else {
                    // Not due yet: placed again, on a lower level (or, for a
                    // deadline beyond the wheel's span, on the top level).
                    self.armed -= 1;
                    self.link(index);
                }
                index = next;
            }
        }
        self.elapsed = self.elapsed.max(now);
    }

    /// The next occupied slot at or after the wheel's time, lowest deadline
    /// first: `(level, slot, deadline)`.
    fn next_slot(&self) -> Option<(usize, usize, u64)> {
        let mut best: Option<(usize, usize, u64)> = None;
        for (level, lv) in self.levels.iter().enumerate() {
            if lv.occupied == 0 {
                continue;
            }
            let shift = level as u32 * SLOT_BITS;
            let now_slot = slot_for(self.elapsed, level);
            let range = 1u64 << (shift + SLOT_BITS);
            let window = self.elapsed & !(range - 1);
            // The first occupied slot after the wheel's position, in this
            // window. A timer is never placed at or behind the position,
            // except on the top level, where a deadline in the next window (or
            // beyond the span) goes: those slots come after every slot of
            // this window, in their own order.
            let after = if now_slot + 1 < SLOTS {
                lv.occupied & (u64::MAX << (now_slot + 1))
            } else {
                0
            };
            let (slot, deadline) = if after != 0 {
                let slot = after.trailing_zeros() as usize;
                (slot, window + ((slot as u64) << shift))
            } else {
                debug_assert_eq!(
                    level,
                    LEVELS - 1,
                    "a timer behind the wheel on level {level}"
                );
                let slot = lv.occupied.trailing_zeros() as usize;
                (slot, window + range + ((slot as u64) << shift))
            };
            if best.is_none_or(|(_, _, d)| deadline < d) {
                best = Some((level, slot, deadline));
            }
        }
        best
    }

    /// Links an entry into the slot its deadline belongs to now.
    fn link(&mut self, index: usize) {
        let when = self.entries[index].when;
        // A deadline beyond the span is placed at the span's edge.
        let placed = when.min(self.elapsed + SPAN - 1);
        let level = level_for(self.elapsed, placed);
        let slot = slot_for(placed, level);
        let lv = &mut self.levels[level];
        let head = lv.heads[slot];
        lv.heads[slot] = index;
        lv.occupied |= 1 << slot;
        if head != NIL {
            self.entries[head].prev = index;
        }
        let entry = &mut self.entries[index];
        entry.prev = NIL;
        entry.next = head;
        entry.place = Place::Armed {
            level: level as u8,
            slot: slot as u8,
        };
        self.armed += 1;
    }

    /// Unlinks an armed entry from its slot.
    fn unlink(&mut self, index: usize) {
        let (prev, next, level, slot) = {
            let entry = &self.entries[index];
            let Place::Armed { level, slot } = entry.place else {
                return;
            };
            (entry.prev, entry.next, level as usize, slot as usize)
        };
        if prev == NIL {
            self.levels[level].heads[slot] = next;
            if next == NIL {
                self.levels[level].occupied &= !(1 << slot);
            }
        } else {
            self.entries[prev].next = next;
        }
        if next != NIL {
            self.entries[next].prev = prev;
        }
        self.armed -= 1;
    }

    /// A free entry, from the free list or a new one.
    fn alloc(&mut self) -> usize {
        if self.free != NIL {
            let index = self.free;
            self.free = self.entries[index].next;
            return index;
        }
        let index = self.entries.len();
        self.entries.push(Entry {
            when: 0,
            waker: None,
            prev: NIL,
            next: NIL,
            generation: 0,
            place: Place::Free,
        });
        index
    }

    /// The entry of a key that is not stale.
    fn entry(&self, key: TimerKey) -> Option<&Entry> {
        self.entries
            .get(key.index)
            .filter(|e| e.generation == key.generation && e.place != Place::Free)
    }

    /// The index of a key that is not stale.
    fn live(&self, key: TimerKey) -> Option<usize> {
        self.entry(key).map(|_| key.index)
    }
}

#[cfg(test)]
mod tests;
