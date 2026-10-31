//! A paused clock that moves by itself, and the calls that pause, resume and
//! advance a runtime's clock.
//!
//! A paused clock jumps to the earliest armed timer when the runtime has
//! nothing else to do: every shard of it (its workers, and the shards of the
//! threads inside its `block_on`) is parked with nothing pushed to it, and
//! no blocking job is queued or running. Each shard publishes the deadline of
//! its earliest timer before it parks, and tries the jump itself (as does
//! the blocking pool when its last job ends, and a thread leaving `block_on`):
//!
//! 1. it claims the clock's advance (one at a time; another waits, briefly);
//! 2. it looks at every shard: each must be parked with nothing pushed, and
//!    it notes each one's park count and deadline;
//! 3. it looks again: the same shards, still parked, with the same counts,
//!    and no shard joined or left. Any shard that woke in between (its park
//!    flag cleared by a waker, or a new park) fails this, so the jump only
//!    happens over a moment at which all of them were idle;
//! 4. it moves the clock to the earliest deadline and wakes the shards whose
//!    timers are due;
//! 5. it lets go of the advance.
//!
//! A shard that wakes while an advance is claimed waits for it to be let go
//! before it reads the clock, so no timer is armed against the time before
//! the jump and found late after it. All of this runs only while a clock is
//! paused: a running clock costs the park path nothing. `proofs/tla/clock/`
//! checks the protocol.

use std::sync::Arc;

use super::runtime::{self, Inner};
use super::shard::Shared;

/// Pauses the clock of the runtime this thread is inside (or the default
/// one) and has every shard of it look at its timers again.
pub(crate) fn pause() {
    let inner = runtime::current();
    inner.clock.pause();
    // The calling thread's own shard, inside this runtime's `block_on` from
    // before the pause, counts from now on.
    Guest::join_current(&inner);
    wake_all(&inner);
}

/// Starts a paused clock again from where it stands.
pub(crate) fn resume() {
    let inner = runtime::current();
    inner.clock.resume();
    wake_all(&inner);
}

/// Moves the clock forward by `by` and has every shard fire what came due:
/// the calling thread's shard at once, so the tasks it wakes run before the
/// caller's next poll, and the others as they wake.
pub(crate) fn advance(by: core::time::Duration) {
    let inner = runtime::current();
    inner.clock.advance(by);
    super::context::current(|current| {
        if let Some(shard) = current {
            shard.fire_timers_now();
        }
    });
    wake_all(&inner);
}

/// Wakes every shard of `inner`, so each looks at its timers again.
fn wake_all(inner: &Inner) {
    for shard in inner.workers.shards() {
        shard.unpark();
    }
    for shard in inner.guests.values() {
        shard.unpark();
    }
}

/// Moves `inner`'s paused clock to the earliest armed timer if every shard of
/// it is idle and no blocking job runs (see the module docs). `me` is the
/// calling shard, parked; answers whether its own timer is due now.
pub(crate) fn try_advance(inner: &Inner, me: Option<&Shared>) -> bool {
    let clock = &inner.clock;
    if !clock.is_paused() {
        return false;
    }
    // Another attempt holds the advance for a few loads and stores.
    while !clock.claim_advance() {
        core::hint::spin_loop();
    }
    let due = attempt(inner, me);
    clock.release_advance();
    due
}

fn attempt(inner: &Inner, me: Option<&Shared>) -> bool {
    let clock = &inner.clock;
    let members = clock.members();
    if inner.pool.jobs() != 0 {
        return false;
    }
    let mut seen: Vec<(Arc<Shared>, u64)> = Vec::new();
    let mut earliest = u64::MAX;
    for shard in inner.workers.shards().cloned().chain(inner.guests.values()) {
        if !shard.idle() {
            return false;
        }
        earliest = earliest.min(shard.timer_at());
        let parks = shard.parks();
        seen.push((shard, parks));
    }
    if earliest == u64::MAX {
        // Idle with no timer anywhere: nothing to move to.
        return false;
    }
    let unchanged = clock.members() == members
        && inner.pool.jobs() == 0
        && seen
            .iter()
            .all(|(shard, parks)| shard.idle() && shard.parks() == *parks);
    if !unchanged {
        return false;
    }
    clock.advance_to_nanos(earliest);
    let mut due = false;
    for (shard, _) in &seen {
        if shard.timer_at() <= earliest {
            if me.is_some_and(|me| core::ptr::eq(me, &**shard)) {
                due = true;
            } else {
                shard.unpark();
            }
        }
    }
    due
}

/// A thread's shard counted among a runtime's shards while the thread is
/// inside its `block_on` and its clock is paused.
pub(crate) struct Guest<'a> {
    inner: &'a Arc<Inner>,
    key: usize,
}

impl<'a> Guest<'a> {
    /// Counts `shard` in `inner` if its clock is paused (or was ever
    /// altered, so a pause during the call is seen when it ends).
    #[inline]
    pub(crate) fn enter(inner: &'a Arc<Inner>, shard: &Arc<Shared>) -> Guest<'a> {
        let key = Arc::as_ptr(shard) as usize;
        if inner.clock.is_paused() {
            Self::join(inner, key, shard);
        }
        Guest { inner, key }
    }

    #[inline(never)]
    fn join(inner: &Inner, key: usize, shard: &Arc<Shared>) {
        inner.guests.insert(key, Arc::clone(shard));
        inner.clock.member_changed();
    }

    /// Counts the calling thread's own shard in `inner`, if the thread is
    /// inside `inner`'s `block_on` on it.
    fn join_current(inner: &Arc<Inner>) {
        let Some(own) = super::context::own_shard_if_made() else {
            return;
        };
        let on_own = super::context::current(|current| {
            current.is_some_and(|shard| core::ptr::eq(shard, &*own))
        });
        if on_own {
            let key = Arc::as_ptr(&own.shared) as usize;
            if inner.guests.insert(key, Arc::clone(&own.shared)).is_none() {
                inner.clock.member_changed();
            }
        }
    }
}

impl Drop for Guest<'_> {
    #[inline]
    fn drop(&mut self) {
        if self.inner.clock.is_altered() {
            self.leave();
        }
    }
}

impl Guest<'_> {
    #[inline(never)]
    fn leave(&self) {
        if self.inner.guests.remove(&self.key).is_some() {
            self.inner.clock.member_changed();
            // The shards left may all be idle now.
            try_advance(self.inner, None);
        }
    }
}
