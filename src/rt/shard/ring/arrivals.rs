//! What a multishot operation hands its source as it comes: a receive's
//! buffers of the group, an accept's connections, a poll's readiness. Each
//! goes to the source with nothing of the ring borrowed, so the source may
//! submit at once.

use std::sync::Arc;

use super::{ACCEPT_CAP, ShardRing};
use crate::io::side::After;
use crate::io::source::ring::{Arrival, RingMem};
use crate::rt::counters::tally;
use crate::rt::shard::Shard;

impl ShardRing {
    /// Buffer `bid` of the receive group came with `res` bytes for the
    /// multishot receive of `user_data`, which stays armed if `stays`: its
    /// source gets them as a chunk. Past the cap of bytes it holds, the
    /// receive is stopped; its source asks for another once its reader took
    /// them. A stream's receive not looked at yet is looked at for its end
    /// (see [`look_for_end`](Self::look_for_end)).
    pub(super) fn arrived(
        &self,
        shard: Option<&Shard>,
        user_data: u64,
        bid: u16,
        res: i32,
        more: bool,
        stays: bool,
    ) {
        let mut stop = false;
        let mut look = false;
        let delivery = {
            let mut recv = self.recv.borrow_mut();
            let Some(group) = recv.as_mut() else {
                return;
            };
            let mut ops = self.ops.borrow_mut();
            let held = ops.get_mut(user_data).and_then(|op| {
                let source = std::sync::Arc::as_ptr(&op.source);
                match &mut op.mem {
                    RingMem::RecvMulti {
                        held,
                        stopping,
                        looked,
                    } => {
                        look = !*looked;
                        Some((op.epoch, held, stopping, false, source))
                    }
                    RingMem::RecvFromMulti { held, stopping, .. } => {
                        Some((op.epoch, held, stopping, true, source))
                    }
                    _ => None,
                }
            });
            match (u32::try_from(res), held) {
                (Ok(len @ 1..), Some((epoch, held, stopping, datagram, source))) => {
                    match group.hand_out(user_data, bid, len, more) {
                        Some(chunk) => {
                            if !datagram {
                                tally::read(len as usize);
                            }
                            *held += chunk.charged(group);
                            if *held > group.cap() && !*stopping {
                                *stopping = true;
                                stop = true;
                            }
                            Some((source, chunk, epoch, datagram))
                        }
                        // An id or a length outside the group: the buffer goes back.
                        None => {
                            group.give_back(bid, len, more);
                            None
                        }
                    }
                }
                // No bytes, or no receive to give them to: the buffer goes
                // back at once.
                _ => {
                    group.give_back(bid, u32::try_from(res).unwrap_or(0), more);
                    None
                }
            }
        };
        if stop {
            self.cancel(user_data);
        }
        if look && stays {
            self.look_for_end(user_data);
        }
        // Handed over with nothing borrowed: the source may submit at once.
        if let Some((source, chunk, epoch, datagram)) = delivery {
            // Before the view, so it drops after it.
            let _unwound = crate::io::side::Unwound::enter();
            let mut after = After::default();
            let starved = {
                // SAFETY: the op's slab entry owns a strong count of the
                // source until its final completion, which this thread
                // processes only after this call returns. Until the view is
                // dropped below nothing runs foreign code or removes the
                // entry: a cancel only queues a cancel entry (a multishot
                // receive never waits for room), and the shard's shutdown
                // runs on this thread outside any source code. The wakes
                // run after the view is gone. The view is never dropped as
                // an `Arc`.
                let source =
                    core::mem::ManuallyDrop::new(unsafe { std::sync::Arc::from_raw(source) });
                #[cfg(test)]
                {
                    let (armed, arrived, dry) = self.counts.get();
                    self.counts.set((armed, arrived + 1, dry));
                }
                let arrival = if datagram {
                    // A buffer not laid out as the kernel says is only given back.
                    match crate::io::source::ring::datagram_of(chunk, source.kind()) {
                        Some((payload, from)) => {
                            tally::read(payload.len());
                            Arrival::Datagram(payload, from)
                        }
                        None => return,
                    }
                } else {
                    Arrival::Chunk(chunk)
                };
                source.ring_arrival(arrival, epoch, shard, &mut after);
                after.starved.then(|| std::sync::Arc::clone(&source))
            };
            if let Some(source) = starved {
                source.wait_for_buffer();
            }
            after.run();
        }
    }

    /// At an arrival that leaves the stream's multishot receive of `user_data` armed, before
    /// `looked`: stops the receive if the stream's end is queued already, which it may never
    /// report. Linux's receive stops trying once the socket's count of what is left says none, a
    /// count that leaves a queued end out (a Unix stream's every end, a TCP reset), and waits for
    /// a wake. Bytes and the end that come between its first try and the arming of its poll wake
    /// nobody: the arming look's one pass takes the bytes and waits for a wake that came already
    /// (`io_poll_check_events` in io_uring/poll.c passes again on a hangup only with a second
    /// reference). A look made after the arming that finds no end settles it, as every later end
    /// wakes the armed poll; one that finds an end stops the receive, and the next one's first
    /// try takes what is left and the end (proofs/tla/ring/RingEnd.tla). Out of line: the
    /// arrivals' path only reads `looked`.
    #[cold]
    #[inline(never)]
    fn look_for_end(&self, user_data: u64) {
        let ended = {
            let mut ops = self.ops.borrow_mut();
            let Some(op) = ops.get_mut(user_data) else {
                return;
            };
            let RingMem::RecvMulti {
                stopping, looked, ..
            } = &mut op.mem
            else {
                return;
            };
            let Ok(fd) = op.source.raw_fd() else {
                return;
            };
            if *stopping {
                *looked = true;
                return;
            }
            let tcp = op.source.kind() == crate::io::source::Kind::Tcp;
            let ended = crate::io::sys::end_queued(fd, tcp);
            *looked = ended || self.submissions_issued();
            *stopping = ended;
            ended
        };
        if ended {
            self.cancel(user_data);
        }
    }

    /// Whether the kernel issued every entry submitted so far, the arming of each multishot
    /// receive included: on a ring with no submission thread, always by the time this thread
    /// reaps one of their completions, since the enter that hands an entry over issues it before
    /// it returns; on a submission thread's ring, once the queue's head, which that thread
    /// commits after issuing what it took, has reached its tail.
    fn submissions_issued(&self) -> bool {
        use crate::syscore::linux::iouring::ring::sys::IORING_SETUP_SQPOLL;
        self.poller.try_borrow().is_ok_and(|poller| {
            let ring = poller.ring();
            ring.flags() & IORING_SETUP_SQPOLL == 0 || ring.sq_ready() == 0
        })
    }

    /// A completion `res` of the operation of `user_data` that carries no
    /// buffer: for a multishot accept, a connection (its descriptor), handed
    /// to the source as it comes; for a multishot poll, readiness. Past the
    /// cap of connections one accept hands over, the accept is stopped; its
    /// source asks for another once its tasks took them.
    pub(super) fn accepted(&self, shard: Option<&Shard>, user_data: u64, res: i32) {
        let delivery = {
            let mut ops = self.ops.borrow_mut();
            let Some(op) = ops.get_mut(user_data) else {
                return;
            };
            let (handed, stopping) = match &mut op.mem {
                RingMem::AcceptMulti { handed, stopping } => (handed, stopping),
                RingMem::ReadyMulti => {
                    let (source, epoch) = (Arc::as_ptr(&op.source), op.epoch);
                    drop(ops);
                    self.readied(shard, source, epoch);
                    return;
                }
                _ => return,
            };
            *handed += 1;
            let stop = *handed > ACCEPT_CAP && !*stopping;
            if stop {
                *stopping = true;
            }
            // SAFETY: the kernel made this descriptor for us.
            let conn =
                unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(res) };
            (std::sync::Arc::clone(&op.source), conn, op.epoch, stop)
        };
        let (source, conn, epoch, stop) = delivery;
        tally::accepted(1);
        if stop {
            self.cancel(user_data);
        }
        let mut after = After::default();
        source.ring_arrival(Arrival::Conn(conn), epoch, shard, &mut after);
        if after.starved {
            std::sync::Arc::clone(&source).wait_for_buffer();
        }
        after.run();
    }

    /// The multishot poll of `source`, issued in `epoch`, says its socket may
    /// be readable: the side gets the edge as an arrival, with nothing
    /// borrowed, so it may submit at once.
    fn readied(&self, shard: Option<&Shard>, source: *const crate::io::source::Source, epoch: u32) {
        // Before the view, so it drops after it.
        let _unwound = crate::io::side::Unwound::enter();
        let mut after = After::default();
        {
            // SAFETY: as in `arrived`: the poll's slab entry owns a strong count of the source
            // until its final completion, which this thread processes only after this call
            // returns, and nothing removes the entry while the view lives (a cancel only queues
            // an entry). The wakes run after the view is gone; it is never dropped as an `Arc`.
            let source = core::mem::ManuallyDrop::new(unsafe { Arc::from_raw(source) });
            source.ring_arrival(Arrival::Ready, epoch, shard, &mut after);
        }
        after.run();
    }

    /// Takes in the buffers chunks gave back and gives them to the kernel
    /// again, counting each off the receive it went to.
    pub(super) fn take_back(&self) {
        let mut recv = self.recv.borrow_mut();
        let Some(group) = recv.as_mut().filter(|group| group.has_returns()) else {
            return;
        };
        let mut ops = self.ops.borrow_mut();
        group.take_back(|user_data, charged| super::release_charge(&mut ops, user_data, charged));
    }
}
