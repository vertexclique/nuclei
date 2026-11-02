//! A call that writes the stream outside the side: a `send_file` chunk the
//! blocking pool sends with `sendfile(2)` on the stream's descriptor, since a
//! page that is not cached blocks the call.
//!
//! While the call is out the side holds its place: a lent write that is out
//! ([`Lent::outside`]), which no driver writes, so every write after it
//! waits, as after any lent write, and a write the handle makes once the
//! call's future was dropped (a timeout) goes out after the chunk, never
//! before it. The place goes once the call ended and its task went: the job
//! records its end ([`Ended`]) and the task that it went ([`Left`]), each
//! with one `fetch_or` of the call's word, and whichever comes second has the
//! side clear the place, which looks that the call ended first
//! ([`WriteState::end_outside`]). In the common case that is the task, on its
//! own thread, once it took the call's answer
//! (`proofs/tla/sendfile/SendFile.tla`).

use core::sync::atomic::{AtomicU8, Ordering};

use super::*;
use crate::buf::owned::OwnedBuf;

/// The call's word: [`ENDED`] once the job ended (or was dropped unrun),
/// [`LEFT`] once its task went.
#[derive(Default)]
pub(in crate::io::source) struct Outside(AtomicU8);

/// The call ended, or its job was dropped unrun.
const ENDED: u8 = 1;
/// The task that made the call went: it took the answer, or its future was
/// dropped.
const LEFT: u8 = 2;

impl Outside {
    /// Records `bit`; returns whether the other was recorded first, so the
    /// caller is the one to clear the place.
    fn record(&self, bit: u8) -> bool {
        // AcqRel: the one that comes second sees the other's bit, and the
        // clear it leaves reads the end the job recorded.
        self.0.fetch_or(bit, Ordering::AcqRel) & (ENDED | LEFT) & !bit != 0
    }

    fn ended(&self) -> bool {
        self.0.load(Ordering::Acquire) & ENDED != 0
    }
}

impl Lent {
    /// The place of a call outside the side: out, with no bytes of its own.
    fn outside() -> Self {
        Self {
            buf: OwnedBuf::default(),
            sent: 0,
            done: None,
            waker: None,
            out: true,
            orphan: false,
            to: None,
            #[cfg(ring)]
            zero_copy: false,
            #[cfg(ring)]
            pinned: None,
        }
    }
}

impl WriteState {
    /// Holds the place of the call `outside` once no other lent write is
    /// held (`waker` waits for that), unless writing failed.
    pub(super) fn lend_outside(
        &mut self,
        outside: &Arc<Outside>,
        waker: &Waker,
    ) -> Poll<io::Result<()>> {
        if let Some(failure) = self.failed {
            return Poll::Ready(Err(failure.error()));
        }
        if let Some(err) = self.error.take() {
            return Poll::Ready(Err(err));
        }
        if self.lent.is_some() {
            self.waiters.register(waker);
            return Poll::Pending;
        }
        self.lent = Some(Lent::outside());
        self.outside = Some(Arc::clone(outside));
        Poll::Ready(Ok(()))
    }

    /// Clears the place of the call outside the side once that call ended,
    /// and wakes the writes that waited for it. A call still out keeps it.
    pub(super) fn end_outside(&mut self, after: &mut After) {
        if self.outside.as_ref().is_some_and(|o| o.ended()) {
            self.outside = None;
            // The place is the side's lent write while `outside` is set, unless
            // the side discarded both for a descriptor that went.
            self.lent = None;
            after.wake(&mut self.waiters);
        }
    }

    /// A call outside the side holds its place, whose lent write no failure
    /// ends: only the call's end does.
    pub(super) fn outside_out(&self) -> bool {
        self.outside.is_some()
    }
}

impl Source {
    /// Runs `call`, which writes the stream outside the side, on the blocking
    /// pool, and answers what it returned; the side holds the call's place
    /// until it ended, so a write the handle makes meanwhile, or after this
    /// future was dropped, goes out after the call's bytes. An error is the
    /// side's (writing failed before) or the pool's (the job was lost).
    pub(crate) async fn send_outside<T, F>(self: &Arc<Self>, call: F) -> io::Result<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let outside = Arc::new(Outside::default());
        poll_fn(|cx| {
            self.poll_writing(cx, |st, _io, waker, _after| {
                st.lend_outside(&outside, waker)
            })
        })
        .await?;
        let _left = Left {
            source: self,
            outside: &outside,
        };
        let ended = Ended {
            source: Arc::clone(self),
            outside: Arc::clone(&outside),
        };
        crate::runtime::spawn_blocking(move || {
            let _ended = ended;
            call()
        })
        .join()
        .await
        .map_err(Into::into)
    }

    /// Has the side clear the place of a call outside it, now if it is free,
    /// else by its holder as it lets go.
    fn clear_outside(self: &Arc<Self>) {
        let mut after = After::default();
        self.write.defer(
            super::super::write_work::UNSTARVE,
            &mut after,
            &self.hooks(),
        );
        self.settle(after);
    }
}

/// The task's side of a call outside the side: dropped once the task took
/// the call's answer or its future went, it records that the task went.
struct Left<'a> {
    source: &'a Arc<Source>,
    outside: &'a Outside,
}

impl Drop for Left<'_> {
    fn drop(&mut self) {
        if self.outside.record(LEFT) {
            self.source.clear_outside();
        }
    }
}

/// The job's side of a call outside the side: dropped once the call
/// returned, or with the job if the pool drops it unrun (or the call
/// panics), it records that the call ended.
struct Ended {
    source: Arc<Source>,
    outside: Arc<Outside>,
}

impl Drop for Ended {
    fn drop(&mut self) {
        if self.outside.record(ENDED) {
            self.source.clear_outside();
        }
    }
}
