//! A borrowed buffer big enough goes between the caller's memory and the
//! kernel in one non-blocking system call made by the polling task, with no
//! pool buffer and no user-space copy.
//!
//! The side must hold nothing for the descriptor, so bytes keep their
//! order: a write needs an empty write-behind and nothing lent, queued or
//! out, a read an empty read-ahead and nothing lent. A call the socket
//! cannot take or give is the normal path's: the side waits for readiness
//! as it does for a pool buffer (an edge on a readiness poller, a poll
//! operation on a ring). The call ends inside the poll, so nothing keeps the
//! caller's slice.

use super::{Io, Via};

// Smallest borrowed buffer a stream reads or writes directly, by driver and
// direction. Measured with `tcp_size_<KiB>` of benches/vs_tokio_count.rs,
// against the pool path, 5 runs a size over several passes: user
// instructions fall at every size; cycles fall for epoll writes from 1 KiB
// and reads from 16 KiB (below, the read that finds the socket empty costs
// more than the copy). Ring writes go direct at every size: a 64-byte TCP
// ping-pong on Linux 7.3, 5 alternating runs each, falls from 11,077 to 8,061
// user instructions a round trip (kernel 31,100 to 30,700, user cycles median
// 11,700 to 9,990), a unix stream from 11,080 to 8,064 (kernel 11,920 to
// 11,590). Ring reads stay at 16 KiB.
/// Epoll and kqueue reads.
pub(super) const READ_READINESS: usize = 16 << 10;
/// Epoll and kqueue writes.
const WRITE_READINESS: usize = 0;
/// Ring reads.
#[cfg(ring)]
const READ_RING: usize = 16 << 10;
/// Ring writes.
#[cfg(ring)]
const WRITE_RING: usize = 0;

/// Whether a borrowed buffer of `len` bytes is read or written directly.
#[inline]
pub(super) fn worth(io: Io<'_>, len: usize, write: bool) -> bool {
    len >= match (io.via, write) {
        (Via::Fd(_), false) => READ_READINESS,
        (Via::Fd(_), true) => WRITE_READINESS,
        #[cfg(ring)]
        (Via::Ops(_), false) => READ_RING,
        #[cfg(ring)]
        (Via::Ops(_), true) => WRITE_RING,
        #[cfg(not(completion))]
        (Via::None(never, _), _) => match never {},
    }
}
