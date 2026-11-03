//! The cursor writes of a file that lands each before it answers: one opened
//! for direct IO, or to sync each write (`O_DSYNC`, `O_SYNC`,
//! `FILE_FLAG_WRITE_THROUGH`).
//!
//! Such a file has no write-behind. A write is one kernel write, made once
//! nothing else is in flight, and is Ready only once it landed, with its own
//! result: its error is its call's, not the next call's, and it moves the
//! cursor nowhere. The write's buffer comes back with its end, and the end
//! waits in `answers` for a poll of the same write (the same bytes at the
//! same address, as a write polled again passes the same slice), since the
//! future that made the write may have gone away while the write still
//! lands. At most [`FILE_SLOTS`] ends wait; the oldest goes for a newer, and
//! an error nobody came back for becomes the file's failure, as a dropped
//! lent write's does. A failed end still waiting when the file's state goes
//! is said and counted as a write that failed with no caller, as
//! [`Stats::write_behind_lost`](crate::stats::Stats::write_behind_lost)
//! counts. `proofs/tla/lent/LentFile.tla` checks these rules
//! (scenarios "direct" and "dsync").

use core::task::{Poll, Waker};
use std::io;

use super::work::bounce;
use super::{FILE_SLOTS, FileIo, FileState};
use crate::buf::PoolBuf;
use crate::io::side::After;
use crate::io::source::Failure;

/// Which write a poll is of: the address and length of the bytes it was
/// given.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Tag {
    addr: usize,
    len: usize,
}

/// The write in flight: whose it is, where it began, and how many bytes it
/// writes.
pub(super) struct Landing {
    tag: Tag,
    at: u64,
    n: usize,
}

/// A landed write's end, kept for a poll of the write: its result, and the
/// buffer it wrote, whose bytes tell the same write from another.
pub(super) struct Answer {
    tag: Tag,
    result: io::Result<usize>,
    buf: PoolBuf,
}

/// The ends of landed writes that wait for a poll, oldest first. When it
/// goes, each failed end still in it is a write that failed with no caller:
/// said once at warn and counted in
/// [`Stats::write_behind_lost`](crate::stats::Stats::write_behind_lost), as a
/// dropped handle's is. A successful end nobody took goes silently.
#[derive(Default)]
pub(super) struct Answers(Vec<Answer>);

impl Answers {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn push(&mut self, answer: Answer) {
        self.0.push(answer);
    }

    /// The oldest end, out of the list and so no longer reported by it.
    fn remove_oldest(&mut self) -> Answer {
        self.0.remove(0)
    }

    /// The first end `pred` accepts, out of the list.
    fn take_where(&mut self, pred: impl FnMut(&Answer) -> bool) -> Option<Answer> {
        let at = self.0.iter().position(pred)?;
        Some(self.0.remove(at))
    }
}

impl Drop for Answers {
    fn drop(&mut self) {
        for answer in &self.0 {
            if let Err(err) = &answer.result {
                crate::rt::counters::write_behind_lost(1);
                log::warn!("bytes a file handle had accepted could not be written: {err}");
            }
        }
    }
}

impl FileState {
    /// A write at the cursor of a file that lands each write: the bytes of
    /// the first buffer of `bufs` that has any, answered once they landed.
    /// A write's own answer comes before the state's failure.
    pub(super) fn write_landed(
        &mut self,
        io: &FileIo,
        bufs: &[io::IoSlice<'_>],
        waker: &Waker,
        after: &mut After,
    ) -> Poll<io::Result<usize>> {
        let Some(bytes) = bufs.iter().find(|b| !b.is_empty()) else {
            return Poll::Ready(Ok(0));
        };
        let tag = Tag {
            addr: bytes.as_ptr() as usize,
            len: bytes.len(),
        };
        if let Some(done) = self.take_answer(tag, bytes, after) {
            return Poll::Ready(done);
        }
        // This write's call is still out: its end is its answer, and the
        // state's failure is another write's.
        if self.landing.as_ref().is_some_and(|l| l.tag == tag) {
            return Poll::Pending;
        }
        if let Some(failure) = self.failed {
            return Poll::Ready(Err(failure.error()));
        }
        if self.writing.is_some() || !self.orphans.is_empty() || self.behind.is_some() {
            return Poll::Pending;
        }
        let Some((mut buf, from, n)) = bounce(bytes.len(), tag.addr, io.direct, waker) else {
            return Poll::Pending;
        };
        buf.fill_at(from, &bytes[..n]);
        self.invalidate(after);
        self.writing = Some(io.submit_write_kept(buf, self.cursor, waker));
        self.landing = Some(Landing {
            tag,
            at: self.cursor,
            n,
        });
        self.cursor += n as u64;
        // Polled once more: the task's waker is with the job, and a write
        // that already landed is answered now.
        let _ = self.poll_writes(io, waker, after);
        self.take_answer(tag, bytes, after)
            .map_or(Poll::Pending, Poll::Ready)
    }

    /// The write in flight ended with `result` (where an append left the
    /// file's offset) and its `buf`. Its result is the write's own and not
    /// the file's failure; the cursor goes back where the write began.
    /// Returns the file's failure, if the end cannot be told as an answer,
    /// and where an append left the offset.
    pub(super) fn landed(
        &mut self,
        io: &FileIo,
        result: io::Result<Option<u64>>,
        buf: Option<PoolBuf>,
        after: &mut After,
    ) -> (Option<Failure>, Option<u64>) {
        let (Some(landing), Some(buf)) = (self.landing.take(), buf) else {
            return (result.as_ref().err().map(Failure::of), None);
        };
        let (result, end) = match result {
            Ok(end) => (Ok(landing.n), end),
            Err(err) => {
                self.cursor = landing.at;
                (Err(err), None)
            }
        };
        if self.answers.len() == FILE_SLOTS {
            let oldest = self.answers.remove_oldest();
            self.forget(io, oldest, after);
        }
        self.answers.push(Answer {
            tag: landing.tag,
            result,
            buf,
        });
        (None, end)
    }

    /// The end of the write of `tag` whose bytes are `bytes`, if it waits.
    fn take_answer(
        &mut self,
        tag: Tag,
        bytes: &[u8],
        after: &mut After,
    ) -> Option<io::Result<usize>> {
        let answer = self.answers.take_where(|a| {
            let kept = a.buf.filled();
            a.tag == tag && bytes.get(..kept.len()) == Some(kept)
        })?;
        after.drop_buf(answer.buf);
        Some(answer.result)
    }

    /// An end nobody came back for: an error is the file's failure.
    fn forget(&mut self, io: &FileIo, answer: Answer, after: &mut After) {
        if let Err(err) = &answer.result {
            self.fail(io, Failure::of(err));
        }
        after.drop_buf(answer.buf);
    }

    /// Every end waiting that is an error becomes the file's failure, for a
    /// flush to report; the others stay for their writes.
    pub(super) fn fail_answers(&mut self, io: &FileIo, after: &mut After) {
        while let Some(answer) = self.answers.take_where(|a| a.result.is_err()) {
            self.forget(io, answer, after);
        }
    }
}
