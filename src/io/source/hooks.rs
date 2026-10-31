//! What a holder of one of a source's sides does for the work others left it
//! ([`SourceHooks`]): the driver's part, side-local work, the source's
//! requests, and whose side it is.

use core::sync::atomic::Ordering;
use std::sync::Arc;

#[cfg(completion)]
use super::Io;
use super::read::ReadState;
use super::write::WriteState;
use super::{Source, read_work, write_work};
use crate::io::side::{After, Hooks};
use crate::rt::shard::{IoOp, Shared};

/// What a holder of one of the source's sides does for the work others
/// left it: the driver's part with the descriptor in place, side-local
/// work, the source's requests, and a side's catching up with a revival.
#[derive(Clone, Copy)]
pub(super) struct SourceHooks<'a> {
    pub(super) src: &'a Arc<Source>,
    /// The shard this thread runs, when the caller already looked it up.
    pub(super) shard: Option<&'a crate::rt::shard::Shard>,
    /// The shard a registration under way is made for by its own thread.
    pub(super) claim: Option<&'a Arc<Shared>>,
    /// For the driver's own edge: the registration the event came for.
    #[cfg(readiness)]
    pub(super) token: Option<u64>,
}

#[cfg(completion)]
impl SourceHooks<'_> {
    /// [`Source::with_route`], with the shard the step knew, if it did.
    fn with_route(&self, f: impl FnOnce(Io<'_>)) {
        self.src.with_route(self.shard, f);
    }
}

impl SourceHooks<'_> {
    /// The owner of a biased side: the address of the shard this thread
    /// runs, inside it, when the source is homed there or not homed yet, or
    /// of the shard it is being registered with by that shard's thread; else
    /// 0. A side keeps the first shard that claims it (see `Side::role`).
    fn owner(&self) -> usize {
        if let Some(claim) = self.claim {
            return Arc::as_ptr(claim) as usize;
        }
        match self.shard {
            Some(shard) => match self.src.reg.home.load(Ordering::Relaxed) {
                0 => shard.shared_addr(),
                home if home == shard.shared_addr() => home,
                _ => 0,
            },
            None => 0,
        }
    }

    /// Keeps the shard that claimed a side, for a nudge to reach.
    fn claimed(&self, cell: &crate::sync::OnceCell<Arc<Shared>>) {
        let shard = self
            .claim
            .cloned()
            .or_else(|| self.shard.map(|s| Arc::clone(&s.shared)));
        if let Some(shard) = shard {
            let _ = cell.set(shard);
        }
    }

    /// Asks the shard that owns a side to take and let go of the source's
    /// sides (a ring source's kick, else a touch), serving what a thread
    /// whose heavy fence failed left there.
    fn nudge(&self, cell: &crate::sync::OnceCell<Arc<Shared>>) {
        if let Some(owner) = cell.get() {
            #[cfg(ring)]
            owner.post_io(IoOp::RingKick(Arc::clone(self.src)));
            #[cfg(not(ring))]
            owner.post_io(IoOp::Touch(Arc::clone(self.src)));
        }
    }
}

impl Hooks<ReadState> for SourceHooks<'_> {
    fn taken(&self, st: &mut ReadState, after: &mut After) {
        if self.src.catch_up(st.epoch_mut()) {
            st.discard(after);
        }
    }

    #[inline]
    fn serve(&self, st: &mut ReadState, after: &mut After) {
        // Only a readiness driver serves edges.
        #[cfg(readiness)]
        if let Some(fd) = self.src.serving_fd(self.token) {
            st.serve(fd, self.src.kind, true, after);
        }
        #[cfg(not(readiness))]
        let _ = (st, after);
    }

    fn deferred(&self, st: &mut ReadState, bits: u8, after: &mut After) {
        if bits & read_work::UNSURE != 0 {
            st.short_reads_unsure();
        }
        if bits & read_work::UNSTARVE != 0 {
            st.unstarve(after);
        }
        if bits & read_work::ABANDON != 0 {
            st.abandon_lent(after);
        }
        #[cfg(completion)]
        {
            // An operation's arrivals go first: its end comes after them.
            #[cfg(ring)]
            if bits & (read_work::ARRIVALS | read_work::DONE) != 0 {
                while let Ok((arrival, epoch)) = self.src.read_arrivals.try_pop() {
                    st.take_arrival(arrival, epoch, after);
                }
            }
            if bits & read_work::DONE != 0
                && let Some(done) = self.src.read_done.take()
            {
                st.op_done(done, self.src.serving_fd(None), after);
            }
            // After a completion, a kick or the registration, what the
            // side still wants goes to the ring: the tasks that wait are
            // woken by what it brings.
            if bits & (read_work::DONE | read_work::SUBMIT) != 0 {
                self.with_route(|io| st.submit_wanted(io, self.src.kind, after));
            }
        }
        #[cfg(port)]
        if bits & read_work::LOOK != 0 {
            st.wake_all(after);
        }
    }

    fn requests(&self, st: &mut ReadState, after: &mut After) {
        self.src.requests_holding_read(self, st, after);
    }

    fn owner(&self) -> usize {
        SourceHooks::owner(self)
    }

    fn claimed(&self) {
        SourceHooks::claimed(self, &self.src.read_owner);
    }

    fn nudge(&self) {
        SourceHooks::nudge(self, &self.src.read_owner);
    }
}

impl Hooks<WriteState> for SourceHooks<'_> {
    fn taken(&self, st: &mut WriteState, after: &mut After) {
        if self.src.catch_up(st.epoch_mut()) {
            st.discard(after);
        }
    }

    #[inline]
    fn serve(&self, st: &mut WriteState, after: &mut After) {
        // Only a readiness driver serves edges.
        #[cfg(readiness)]
        if let Some(fd) = self.src.serving_fd(self.token) {
            st.serve(fd, self.src.kind, true, after);
        }
        #[cfg(not(readiness))]
        let _ = (st, after);
    }

    fn deferred(&self, st: &mut WriteState, bits: u8, after: &mut After) {
        if bits & write_work::UNSTARVE != 0 {
            st.unstarve(after);
        }
        if bits & write_work::ABANDON != 0 {
            st.abandon_lent(after);
            #[cfg(completion)]
            if let Some(user_data) = st.withdrawal() {
                self.src.cancel_op(user_data);
            }
        }
        #[cfg(completion)]
        {
            if bits & write_work::DONE != 0
                && let Some(done) = self.src.write_done.take()
            {
                st.op_done(done, self.src.serving_fd(None), after);
            }
            if bits & (write_work::DONE | write_work::SUBMIT) != 0 {
                self.with_route(|io| st.submit_wanted(io, self.src.kind, after));
            }
        }
        #[cfg(port)]
        if bits & write_work::LOOK != 0 {
            st.wake_all(after);
        }
    }

    #[inline]
    fn released(&self, saw_requests: bool, after: &mut After) {
        if saw_requests {
            self.src.requests_after_write(self, after);
        }
    }

    fn releasing(&self, st: &mut WriteState) {
        // Release: a drop on another thread reads it.
        self.src.unsent.store(st.has_unsent(), Ordering::Release);
    }

    fn owner(&self) -> usize {
        SourceHooks::owner(self)
    }

    fn claimed(&self) {
        SourceHooks::claimed(self, &self.src.write_owner);
    }

    fn nudge(&self) {
        SourceHooks::nudge(self, &self.src.write_owner);
    }
}
