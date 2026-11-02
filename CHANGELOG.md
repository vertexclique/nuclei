# Changelog

## 0.5.4

### Changed

- **A ring's stream reads by readiness while its reads keep up** (Linux, io_uring). A TCP or Unix
  stream read into the task's own buffer, under 16 KiB, on a socket whose kernel counts what a
  read leaves, reads straight from the socket as on epoll, with the count and its floor; a read
  that finds the socket empty arms the ring's multishot poll, which stays armed while the task
  reads, and a poll that ends is armed again by the next wait. A count that leaves more than
  another read takes means the bytes run ahead of the reads: the side then reads through the
  multishot receive, its poll stopped and the kernel's count turned off, so the receive's
  batching serves a stream that runs ahead; after four arrivals in a row that each come to a
  reader waiting with nothing read ahead and fit its read, it reads by readiness again, once
  every chunk the receive brought was read and its end came (`proofs/tla/ring/RingPoll.tla`,
  and `FdStarve.tla` with the ring's poll). Owned and lent reads, larger reads and datagrams keep
  their ring operations. A round trip on io_uring runs about 11% fewer user instructions (tcp
  3349 to 2970, 3146 to 2800 under fat LTO; tcp_asyncrw, tcp_many and unix_stream alike) for
  about 3% more in the kernel, where each read is now a system call of its own (unix_stream
  about 8%); tcp_split, whose bytes run ahead, keeps the receive (0.8% more in user space, 2 to
  3% in the kernel). The read on epoll is built apart from the ring's, and runs 8 to 16 fewer.
- **A stream read the kernel's last count covers asks for no new count** (Linux, epoll). A read
  into the task's own buffer asks the kernel how many bytes it leaves (`TCP_INQ`, `SO_INQ`), with
  `recvmsg` and its control message, to know without another call whether the socket is empty.
  What that count said was left, less what the reads since took, is a floor on what the socket
  holds while nothing else takes its bytes: a read the floor covers with more to spare is a plain
  `read`, the read that would take the floor's last bytes asks again, since more may have come
  after the count, and a read that finds the socket empty forgets the floor
  (`proofs/tla/starve`, the `floor_trusted` mutation). A request read in pieces costs less:
  `tcp_split` on epoll runs 452 user instructions per operation instead of 468 (394 instead of
  415 under fat LTO) and 6% fewer in the kernel, and the other stream rows on epoll (`tcp`,
  `tcp_asyncrw`, `unix_stream`, `tcp_many`, `bulk`) 10 to 20 fewer in user space.
- **A shard is made where it lives.** Its fields, and those of its shared half, are written
  straight into the `Arc` or `Rc` that holds it: a start no longer copies the 7.8 KB shard three
  or four times over, each of its two timer wheels level by level, and its driver several times.
  What is left is one copy of each wheel from a constant and one move of a ring from its
  constructor into its place. A short run that starts a runtime counts it: `file_read` runs 34 to
  45 fewer instructions per operation (2470 instead of 2514 on epoll) and `bulk` 8 to 12 fewer.
  A thread's own shard, once made, is taken with no result to look at, which takes 2 off each
  `block_on`.
- **A read that takes part of a ring's chunk bounds it once.** What a read takes off a chunk of
  the receive group is bounded by what the chunk holds, with no saturating steps: `tcp_split` on
  io_uring runs 3 fewer instructions per operation (401 of 404 under fat LTO). CI's budgets in
  `ci/op_budget.txt` were lowered to match these and the change above.

- **An owner's read or write on a watched worker marks its own call, and ends it as it settles.**
  A stream's poll and send as its side's owner, from a task's code on a worker the handoff watch
  follows, went out of line into a bracket around the path a worker with no watch takes. The step
  now marks the call itself (`context::mark_call`, as `After::enter` makes it) and keeps the mark
  in what it leaves, and its settle ends the call before it wakes or drops anything the step left
  (`After::settle_light_owned`, `Source::settle_rest`); a shard taken before the mark is left to
  the poll of any thread. The mark is the bracket's, as `proofs/tla/handoff` models it. Both
  sides of a stream also take the poll's waker once and hand it, not the context, to the owner's
  path and the general one, and the budget reads the waker only once it is spent. With the default
  watch on io_uring, default release build, user instructions per operation: a 64-byte TCP round
  trip 3788 before, 3682 now (3362 with the watch off, 3375 before); a 64 KiB chunk of a bulk
  stream 1220, 1179 (1059 off, 1067 before); a request read in pieces (`tcp_split`) 601, 553 (460
  off, 466 before); on epoll the TCP round trip 3045, 2959, and under fat LTO on io_uring 3535,
  3405. With the watch off each row costs what it did or less, but a few by a handful of
  instructions: the stream rows on epoll in the default build (2 more, of 2637: the read keeps its
  context where it kept its waker), the file read (2 more in its own code, which calls
  `After::run` where it inlined it; its count moves by ten from run to run), and `tcp_many` under
  fat LTO (1 more, in the general read's frame). Counted as the ops gate counts them, every row is
  at or below what it was, and CI's budgets in `ci/op_budget.txt` were lowered to match.

## 0.5.3

### Fixed

- **A shard taken from a blocked runner is never dropped with its tasks.** The monitor holds a
  worker's shard only weakly, and got a reference of its own only after its take had succeeded; a
  runner whose poll ended in between found the shard taken and let go of the last reference, so the
  shard was dropped with the tasks still queued on it, and each one's handle then found no output
  (on macOS, about one run in two thousand of the cadence test whose twelve tasks block their
  runners at once). The monitor now holds the shard before it claims the runner's slot, and lets go
  of it when the take does not happen (`proofs/tla/handoff/TakeRef.tla`).
- **A worker that backs out of its park wakes the monitor.** With every worker parked the monitor
  sleeps until a worker's park ends; a worker that said it parks, found work at its last look and
  ran on rang nobody, so a monitor that took that moment for every worker parked could sleep while
  the worker ran, and a poll of it that blocked then waited for its end instead of a handoff (found
  in the protocol and its model, not seen in a run). The back-out now rings a monitor that sleeps,
  as a park's end does (`proofs/tla/handoff/Asleep.tla`).
- **The monitor's ticks on macOS are as short as they say.** The monitor reads each worker's word
  every quarter of the handoff threshold with a timed park, which macOS coalesces with other
  timers: on a CI runner a 2.5 ms park took 18 ms at the median and up to 119 ms, so a blocked
  poll waited 70 ms or more for its handoff instead of 10 ms, and in the cadence test the first
  handoff came up to 800 ms after the runtime started. The monitor now waits on a kqueue
  there, its ring a user event and its tick a timer marked critical, which is not coalesced (as a
  shard's precise sleep's is): 2.5 ms take 3.8 ms at the median and 8.6 ms at most.
- **A shard on io_uring no longer sleeps through a wake behind a full submission queue.** Entries
  past the queue's room wait in the shard, the read of its wake's eventfd first in line, and a
  wait handed the kernel only what the queue held. A shard that queued more operations than its
  ring has entries between two waits (130 receives on a ring of 64), then waited with no timer
  due, slept with the wake's read outside the kernel: until one of its operations completed, and
  with idle peers never, no wake ended the wait, neither another thread's nor that of a timer the
  shard fired on its way to the wait, whose `block_on` future then never ran again. CI's Ubuntu
  6.8 job hung 5.5 hours so in `rt::shard::ring::tests_cap`, and a loaded host hangs that test
  about once in 300 runs, with the ring's table of fixed files and without. A wait that may sleep
  now hands the kernel every entry first, and only looks where the kernel takes not all of them
  (`proofs/tla/park`, the `no_submit_first` mutation). The cap tests bound their sends with a
  deadline, and a watchdog ends the test process with a message after 30 seconds, since a lost
  wake stops the shard's timers too.

### Changed

- **`File::open` opens on the calling thread when it can answer at once** (Linux). A regular file
  on a local file system whose path the kernel's caches resolve at once (`openat2` with
  `RESOLVE_CACHED`, Linux 5.12) is opened on the task's own thread, non-blocking, with no job of
  the blocking pool. What the path names is looked at first through an `O_PATH` descriptor, so no
  FIFO, device or file of a network or user-space file system is opened there; those, every open
  the kernel's caches cannot answer, and every kernel without the call go to the blocking pool as
  before, and its answer, an error included, is the one the caller gets.
- **A ring registers its table of fixed files at its first source's attach**, not at its start, so
  a shard that never attaches a socket or a file (a `block_on` of timers and channels) makes no
  registration call.
- **The bounded channel's `send` and `recv` are single-state futures.** `Sender::send` waits for a
  slot and pushes the value in one future, and the receiver looks at its queue inline; a send that
  finds a slot free and a receive that finds a message take no state of nested waits. The
  signatures are the same futures as before (`fn send(&self, value) -> impl Future + '_`), not
  `Unpin`, and a send dropped while it waits lets go of its place and of a slot it was handed, as
  before.
- **The stream and completion paths run fewer instructions per operation.** A stream read takes the
  bytes read ahead first, with one look at the first piece, keeps that piece out of the queue and
  consumes a piece it takes part of inline; a stream's direct read is decided in one place; the
  kernel counts what a read leaves out of line only the first time; `read` and `recvmsg` load only
  the registers they take; an arrival's hand-out and delivery and a chunk's take-in are inlined into
  the ring's completion loop, which enters its unwinding guard once per batch of completions and
  wakes the lone reader an arrival leaves inline; a read side's completion is applied at once when
  the side is free, as a write side's is; a step that left one waker alone settles inline; the run
  loop's look at the inbox and its spin are inlined in every codegen unit; a going shard asks its
  sources as their owner, with no heavy fence, so a runtime that drops with many streams open makes
  no `membarrier` call per stream (on epoll, 256 connections took about 250 calls and now take 1).
- **A stream's owner polls and sends on its own shard under its biased hold**, inlined, and its send
  looks at one bit each write-side release publishes rather than at the whole write side
  (`proofs/tla/sendsnow`), while owned writes still go out whole. On a worker the handoff watch
  follows (the default), a task's poll and send take them in a bracket into the shard (below).
- **A worker the handoff watch follows takes its shard's own paths from a task's code.** A call
  into the runtime from a task's code on such a worker (a wake, a spawn, a stream's poll and send
  as its owner) saw that it needed a bracket into its shard and went the general way with a
  bracket around it (a stream's side through its lock, a wake through a second look for its
  shard); it now takes the bracket and, in it, the path a worker with no watch takes. The bracket is
  the protocol a poll's end already used: the slot's word marked, the claim looked at, and the
  word the call found put back as it ends, so a poll that blocks after a call is still taken
  (`proofs/tla/handoff`: a call with no mark, `M_no_call_mark`, breaks `Exclusive`, and one that
  leaves its mark, `M_call_stays_marked`, breaks `Rescued`). A watched worker's run of a task keeps
  no guard to come back by (each part of the run that runs the task's code catches a panic or
  aborts on one), and a thread looks at one word per call where it looked at two. With the default
  watch on io_uring, default release build, user instructions per operation: a 64-byte TCP round
  trip 4321 before, 3792 now (3375 with the watch off, 3391 before); a 64 KiB chunk of a bulk
  stream 1414, 1217 (1060 off); a spawn and its join 899, 791 (632 off); a oneshot round 1127, 1018
  (859 off); a bounded channel round 651, 611 (530 off); a sleep 1019, 942 (834 off). With the watch
  off each row costs what it did or less, but a few by a handful of instructions: a datagram round
  trip on io_uring (3 and 4 more, of 3677 and 4150), a sleep on epoll (3 more, of 831), `block_on`
  under fat LTO (2 more, of 244), and the file read, whose count moves by ten from run to run. The
  ops gate counts the TCP row with the watch on too (`tcp_watched` in `ci/op_count`).
- **A connection costs less to open and close.** A source's attach allocates nothing beside the
  source itself, nor does a dropped handle's close when the handle's object fits in a word (a
  socket's descriptor does), a new source is written field by field into its allocation, and a
  stream a listener accepts inherits the listener's `TCP_INQ` with no `setsockopt` of its own (256
  connections on epoll made 513 calls and now make 259). CI's budgets in `ci/op_budget.txt` were
  lowered to match these and the changes above.
- **The API reference is built with `io-stats`**, on the site and on docs.rs, so the IO counts and
  latencies of `stats` are listed.

## 0.5.2

### Fixed

- **A read on io_uring gets a stream's end that came with its last bytes.** Linux's multishot
  receive stops trying once the socket's count of what is left says none, a count that leaves out
  a Unix stream's every end and a TCP reset, and waits for a wake. Bytes and the end that came
  between its first try and the arming of its poll woke nobody, so the receive took the bytes and
  waited for good with the end in the socket, and so did the read (three times in CI's full Linux
  suites, in the Unix reset fault test). The ring's thread now looks at the socket
  once, at the receive's first arrival after the arming, and stops a receive whose end is queued;
  the next receive takes it (`proofs/tla/ring/RingEnd.tla`).
- **A Unix connect to a full backlog no longer holds a thread of the blocking pool.** Linux answers
  it `EAGAIN`, and the connect went to a blocking connect of a new socket on the pool, which a
  dropped future never stopped, and which lost the options set on the caller's socket. It now
  connects again on its own socket after a pause on a timer (1 ms, doubled up to 64 ms), until the
  listener takes it, as std's blocking connect waits; a drop frees everything
  (`proofs/tla/connect`).
- **A runtime's drop whose bound runs out no longer drops a linger it was running.** The drop
  runs what dropped handles left for at most the linger time, and a linger it had started then was
  dropped with its run, closing the stream with the bytes behind never sent. It now goes on on a
  thread of its own, to its own deadline, as the ones the run never took do
  (`proofs/tla/going/Linger.tla`).
- **A registration a stopping shard never made no longer fails the source's IO.** A worker whose
  shard had stopped still registered sources its thread polled, and one asked to register a source
  for a thread that runs no shard could stop first; either failed the registration, so the
  source's next operation returned "the shard stopped before the registration", and a dropped
  stream's drain (a runtime dropping with bytes behind) took that for its end and gave the bytes
  up (on macOS, about one run in seven of the drop test). A stopped shard now registers nothing:
  the source stays unregistered, and is registered where its tasks are polled next, as
  `proofs/tla/attach` has it.
- **A stream dropped after its runtime's drop delivered its bytes closes.** A thread's own shard
  that leaves (its outermost `block_on` returned, the runtime's drop among them) evicts its sources
  and waits for each eviction; one a thread holding a side performs, as a dropped stream's linger
  does, showed the source unregistered before it posted the ring's detach, so the shard could
  look at its mailbox in between and leave, and the detach waited there until the thread's next
  `block_on`, with the ring's file and the lease that keeps the stream open. Every byte reached
  the peer, which then waited for the end that never came (on io_uring, one to six runs in four
  hundred of the drop test). The end of a registration now shows only once what its shard must undo is in
  that shard's mailbox (`proofs/tla/going/Going.tla`).
- **A dropped TCP stream whose linger ran out resets its connection.** The bytes still behind were
  given up and the stream closed with a FIN, so the peer read a cut stream as a whole one; it now
  reads `ConnectionReset` (`proofs/tla/close`).

- **IO of a shard handed over repeatedly no longer waits for 61 task runs.** When a task blocks its
  thread past the handoff threshold the shard goes to another thread, and the count of runs before
  the next look at the driver went with it. If the first task of every new runner blocked as well,
  the shard was handed on before any runner reached the driver, and an operation a task had queued
  (a socket receive pushed into the ring's submission queue) reached the kernel only after 61 runs,
  about 0.9 s with 15 ms blockers. The runner that takes a shard over now drives its IO once, without
  waiting, before it runs any task.
- **IO is driven at least every 1 ms of task running, not only every 61 runs.** A single runner whose
  tasks each ran just under the handoff threshold looked at the driver once per 61 runs (61 x 9 ms is
  half a second). The run loop now reads a coarse monotonic clock of its own, `Quartz` (a vDSO read on
  Linux, a few nanoseconds, never a system call), once every 16 task runs and looks at the driver when
  the last look is older than 1 ms. It costs one masked branch per run and needs no extra thread.
  The clock steps by no more than the bound on every platform: the coarse clock where its tick is
  1 ms or shorter (a 1000 Hz kernel), else `CLOCK_MONOTONIC`; `CLOCK_UPTIME_RAW` on macOS; the
  performance counter on Windows, whose tick count would step by 15.6 ms.
- **The handoff acts at its threshold on Windows.** The watch ticks every quarter of the threshold,
  but a timed park on Windows ends at the system timer's tick (15.6 ms), so `handoff_after(10 ms)`
  handed a shard over only after about 62 ms. The watch now waits on a high-resolution waitable
  timer and an event there, and its ticks end on time; elsewhere it parks as before.

- **A ring shrunk under memory pressure no longer deadlocks a request behind its own reply's
  receive.** Its receive group now fits within half its completion queue (a ring whose queue cannot
  hold one buffer runs without a group), and long-lived receives and accepts no longer count against
  the admission cap, so no finite operation waits behind idle connections. Before, a ring made with
  128 entries or fewer and the default 256-buffer group had a cap of 1: the multishot receive armed
  for an HTTP client's reply held it, and the request's send waited for the receive to end.

- **A file cut by `set_len` no longer hands out bytes read ahead past its new end.** A cut, a range
  call (`allocate`) and a copy into the file move its generation on, so a cursor read made after them
  reads the file as it is.

### Changed

- **Each operation's instructions are counted, and CI holds them.** The IO, task and timer paths
  were reworked one operation at a time: a stream's direct read and write inline into their
  callers, a step that leaves nothing is made and settled with one store and one look, a task run
  starts with one addition, timers keep their deadlines in nanoseconds and fire without a removal,
  and a Unix datagram socket's ring poll stays armed across waits. `ci/op_counts.sh` counts the
  user instructions per operation of 19 operations, on io_uring and on epoll, under cachegrind, and
  CI fails when one rises above its budget in `ci/op_budget.txt`.
- **The owned calls are generic over their buffer.** A caller handing a `Vec<u8>` in, and binding the
  buffer it gets back, compiles as before. Two forms that leaned on the old `Vec<u8>` parameter no
  longer infer a type: an argument the compiler had to infer from it (`Default::default()`, `.into()`,
  `Vec::new().into()`), which now needs its type written (`Vec::<u8>::new()`); and naming the address
  type of `UdpSocket::send_to_owned` or `UnixDatagram::send_to_owned` explicitly, which now takes the
  buffer type too (`send_to_owned::<A, _>`).
- **Sizes are validated where they take effect**, and a value outside what the runtime can work
  with is refused as `InvalidInput` naming the setting, its value and the allowed range (the first
  runtime build for the io_uring settings, `Builder::build` for the builder's). Values that were
  accepted before and are refused now:
  - `queue_len` below 64: the floor a ring is shrunk to under memory pressure.
  - `recv_buffers` that is not 0 or a power of two, or not below twice `queue_len`: the group would
    take more than half the completion queue. The profiles (`interrupt_driven` and the others) now
    give a `queue_len` under 256 that many buffers, rounded up to a power of two, where they gave 256,
    which left a cap of 1 for a `queue_len` from 64 to 128.
  - `recv_buffer_size` below 512 or above 1 GiB while `recv_buffers` is not 0.
  - `Builder::thread_stack_size` below 128 KiB: a worker starting, a timer and a TCP connect take
    77 KiB in a debug build and 64 KiB in a release one.
  - `Builder::max_nesting(0)`: no `block_on` could run.
  - `Builder::max_blocking_threads(0)`, which was raised to 1 silently.
  - `Builder::handoff_after(Some(d))` with `d` under 1 ms: the watch measures in milliseconds.
- **The runtime's clock read is inlined** (`time::Instant::now` and the read timers and timeouts
  make), so whether it is inlined no longer depends on how a build splits the crate into codegen
  units: a `timeout` whose future is ready takes about 4% fewer instructions than in 0.5.1 with fat
  LTO and one codegen unit.

### Added

- **The attribute macros take a builder.** `#[nuclei::main]`, `#[nuclei::test]` and
  `#[nuclei::bench]` take `builder = "path"`: the runtime is built from the `nuclei::Builder` that
  function returns, with `threads` and `start_paused` applied on top when given (nuclei-macros
  0.5.1).

- **Owned calls take any buffer type.** `read_owned`, `write_owned`, `write_zero_copy`, the datagram
  owned calls and the file owned calls take any `IoBuf` (writes) or `IoBufMut` (reads) and give the
  same type back; a caller passing a `Vec<u8>` gets a `Vec<u8>` back as before (the
  runtime holds the buffer in a vector's size, and boxes any other type once). `Released` is
  `Released<B = Vec<u8>>`. A caller's own type implements the two unsafe traits.
- **`AlignedBuf`**, a buffer of a fixed capacity at an address of a chosen alignment, made by a safe
  constructor: the buffer for a file opened for direct IO. Its memory is the caller's, not charged to
  the IO buffer cap.
- **Owned vectored IO**: `write_vectored_owned` writes every buffer of an `IoVecBuf` (`Vec<B>`,
  `[B; N]`) in order as one write, with no other write of the handle between them, and
  `read_vectored_owned` fills the rooms of an `IoVecBufMut`'s buffers in order, on TCP and Unix
  streams, pipes and files. Each kernel call takes every buffer at once: `writev`/`readv` or a
  `sendmsg`/`recvmsg` of several buffers on epoll and kqueue, `IORING_OP_WRITEV`/`READV` or
  `SENDMSG`/`RECVMSG` on io_uring, a `WSASend`/`WSARecv` of several `WSABUF`s on I/O completion
  ports, `pwritev`/`preadv` for files; a call the kernel takes part of goes on from the byte it
  stopped at. Where the system has no vectored call of the kind, the buffers go one call each, in
  order: a buffered file on Windows reads and writes so, and a Windows handle that is not a socket
  writes so and reads into its first room (a short read).
- **Positional file IO on a shared handle**: `read_at`, `write_at`, `read_exact_at`, `write_all_at`,
  `read_at_owned`, `write_at_owned` and `read_many_at` (many reads in one call, at most 64 out at
  once, together on the shard's ring), ordered after the writes accepted at the cursor before them.
- **More file calls**: `allocate` (`Allocate::{Extend, KeepSize, PunchHole, ZeroRange}`, on the ring
  as `IORING_OP_FALLOCATE`; a mode the system has not answers `Unsupported`), `sync_range`
  (`IORING_OP_SYNC_FILE_RANGE` on Linux, a data sync elsewhere), `fs::sync_dir` with the
  rename-then-sync recipe, `copy_range_to` (`copy_file_range` in bounded chunks, read and write
  where the kernel cannot), and advisory `lock_file`, `try_lock_file` and `unlock_file` (the
  blocking lock on the pool, never left held by a dropped future). `sync_all` and `sync_data` are operations of the ring,
  and a failed sync makes every later one of the handle fail. A file opened for direct IO is read and
  written with exactly the caller's lengths and offsets.
- **`read_at` and `write_at` on `Handle<File>` are the async calls**, and shadow std's
  `FileExt::read_at` and `write_at` reached through `Deref`, which block the worker; std's stay
  reachable as `(*file).read_at(..)`. The lock calls take names of their own, so std's `File::lock`,
  `try_lock` and `unlock` through `Deref` are unchanged.
- **TLA+ models** of a file's positional calls, syncs, direct IO and descriptor leases
  (`proofs/tla/lent/LentFile.tla`), of a file lock's cancellation (`proofs/tla/flock`) and of many
  reads in one call (`proofs/tla/many`), each with mutations that break their rules, and a Lean
  proof that a vectored read fills its buffers' rooms in order (`proofs/lean/Nuclei/Fill.lean`).
- **`shut` and `abort` on streams.** `TcpStream::shut(how)` and `UnixStream::shut(how)` make std's
  shutdown once the bytes written behind reached the kernel (`Write`, `Both`), and the reading
  half's through the ring's `SHUTDOWN` where the calling thread runs a ring; reads after it answer
  as std's on each system (macOS and Windows drop what was read ahead). It is not named `shutdown`,
  so `stream.shutdown(how)` still reaches std's through `Deref`. `TcpStream::abort()` sets
  `SO_LINGER` to zero, drops the bytes behind and closes now: the peer reads a reset
  (`proofs/tla/close`).
- **IO settings on the builder**: `write_behind_linger` (how long a dropped handle's bytes may
  take, 60 s by default, more than zero), `zero_copy_threshold` (the least a `write_zero_copy`
  must have left for a zero-copy send, 1 MiB by default) and `send_file_chunk` (the most one call
  of a `send_file` sends, 1 MiB by default, from 1 to `0x7ffff000` bytes). Each is read once, as
  the runtime builds.
- **TCP options**: `TcpKeepalive { time, interval, retries }` with `set_keepalive_timing` and
  `keepalive_timing` on `TcpSocket` and `TcpStream`; `TcpSocket::set_incoming_cpu`,
  `set_user_timeout`, `set_only_v6`, `set_fastopen` and `set_defer_accept`, and
  `TcpStream::set_quickack` and `set_user_timeout`, each with its getter. An option the system
  does not have answers `Unsupported`.
- **`UdpSocketBuilder`**, a UDP socket set up before it binds (`SO_REUSEADDR`, `SO_REUSEPORT`,
  buffer sizes, `IP_TOS`, `IPV6_TCLASS`, `IPV6_V6ONLY`, `SO_BINDTODEVICE`), and
  `UdpSocket::poll_recv_from` and `poll_send_to`.
- **What std's calls through `Deref` do**, on `Handle`: `shutdown`, `set_nonblocking(false)`,
  `take_error`, `Read` and `Write` go around the bytes read ahead and written behind, or block the
  shard; `UdpSocket::connect` is a constructor where `socket.connect(addr)` is std's.
- **`sync::local`**: `mpsc`, `oneshot`, `Semaphore` and `Mutex` for the tasks of one shard, with the
  names, signatures and errors of their `nuclei::sync` twins. They are `!Send`, no path runs an atomic
  instruction or a fence, and full sends, acquires and locks are served in arrival order.
- **`sync::mesh`**: a full mesh of bounded channels between a fixed number of parties, one channel
  for every ordered pair, with backpressure per pair and a `Party::recv` that is fair across senders.
- **`time::sleep_precise` and `sleep_until_precise`**: waits to the microsecond, where the
  millisecond of `sleep` is too coarse. A precise deadline that is its shard's earliest arms a kernel
  timer of its own.
- **The cooperative budget covers channels and locks**: a receive, acquire or lock of `nuclei::sync`
  that completes without queueing spends one unit of the task's budget, as completed IO does, so a
  task on a channel that is always ready yields to the other tasks of its shard.
- **Each worker's IO in `stats`, behind the `io-stats` feature** (off by default): counts with
  `Builder::count_io` and `ShardStats::io` (`IoCounts`: reads, writes, the bytes of each,
  accepts), and the IO latencies `Builder::record_latencies` records in `ShardStats::io_latencies`
  (`IoLatencies`). The feature costs every IO call a few instructions, counted and timed or not;
  without it no IO path carries that code, and those items do not exist. The poll-time histograms
  need no feature.
- **A TLA+ model of the cadence** (`proofs/tla/cadence`): the wait of a queued operation is bounded
  whatever mix of quick, slow and long tasks runs; removing the new runner's first drive or the time
  bound each breaks it.
- **A TLA+ model of the admission cap** (`proofs/tla/admission`): a finite operation never waits in
  room while none is in the kernel, and arrivals plus finite completions fit the queue; counting
  long-lived operations breaks the first, an uncut group the second. An accept's connections and
  the last completion of a multishot operation have no room kept: past the queue the kernel holds
  them (`IORING_FEAT_NODROP`), as before.
- **Tests** (`tests/cadence.rs`, `rt::quartz`, `rt::shard::ring::tests_cap`, `config::tests`,
  `tests/config_sizes.rs`, `tests/config_refused.rs`) for these fixes and for the clock, and
  benchmarks of the cost of a task run and of IO on a shard.

## 0.5.1

### Fixed

- **A kernel that refuses provided buffer rings no longer stops a runtime from starting.** Ubuntu's
  6.8 kernels from 6.8.0-136 on (24.04's own, and 22.04's HWE kernel) refuse every valid buffer
  ring registration with `EINVAL`: a backport inverted the check of its reserved words
  ([Launchpad bug 2162843](https://bugs.launchpad.net/ubuntu/+source/linux/+bug/2162843)). A
  runtime there failed to start with `io_uring recv_buffers and recv_buffer_size: Invalid
  argument`. The backend choice now builds the ring a shard runs (the ring, its table of fixed
  files, the buffer ring for multishot receives while `recv_buffers` is not 0, and its enabling),
  so a kernel that refuses a feature a shard needs makes `auto` pick the poller. A backend named
  as io_uring on such a kernel fails naming the refused feature, and a shard of an `auto` runtime
  whose ring is refused anyway runs the poller. With `recv_buffers: 0` io_uring stays in use there.
- **A kernel's refusal and a configuration mistake are told apart.** The kernel's `EINVAL` from a
  buffer ring or a fixed file table registration is now an `Unsupported` error naming what it
  refused, and only nuclei's own checks of `recv_buffers` and `recv_buffer_size` are reported as
  the configuration's mistake. A shard whose receive group fails for any reason other than memory
  is no longer started without it, unseen.

### Added

- **A CI job on Ubuntu 24.04's own kernel** (6.8 generic) beside the mainline ones, which all
  register buffer rings: the suite runs with the backend left to `auto`, which must choose epoll
  and name the refusal, and with epoll named.

## Unreleased: 0.5.0

Nuclei is now a runtime of its own: thread-per-core shards that each run their tasks and drive
their own IO, timers and buffers. The guide at <https://nuclei.rs/learn/> covers every part below
with a program that compiles and runs.

### Added

- **Runtime.** `Runtime`, `Builder` and `RuntimeHandle`; `spawn`, `spawn_local`,
  `spawn_blocking` and `block_on` on the process default runtime, which starts on first use with
  no configuration. `block_on` nests, in a task too, and keeps its shard's other tasks and IO
  going. `RuntimeHandle::current` and `try_current`, `enter` with its `EnterGuard`, `RuntimeId`,
  `worker_index`, `shutdown_timeout` and `shutdown_background`. Builder settings for the workers,
  their threads and stacks, the blocking pool, the IO buffer cap, `event_interval`,
  `global_queue_interval`, `max_io_events_per_tick`, `disable_lifo_slot`, `rng_seed`,
  `unhandled_panic`, `start_paused` and the backend, and hooks on threads and tasks. A runtime
  may start more workers up to a maximum and drain them again.
- **Shard handoff.** A worker whose task stays in one poll past `Builder::handoff_after` (10 ms
  by default; `None` turns it off) hands its shard to another thread on the same core: the
  shard's other tasks, timers and IO in flight go on, the blocked task carries on once its poll
  returns, a nested `block_on` and the tasks `spawn_local` keeps on that thread finish on it, and
  a shutdown does not wait for it. A poll pays two plain stores and a load. `ShardStats` counts
  `handoffs` and the `longest_poll`. With the handoff on, an io_uring worker's ring takes
  submissions from any thread.
- **Tasks.** `Task` with `detach`, `cancel`, `abort`, `abort_handle`, `join` (a `JoinError`
  says why a task gave no output) and `fallible`; `JoinSet`; task ids; `task::Builder`;
  `task_local!`; `yield_now`; the cooperative budget in `task::coop`.
- **Macros.** `select!`, `join!` and `try_join!`; `#[nuclei::main]`, `#[nuclei::test]` and
  `#[nuclei::bench]`, from the new `nuclei-macros`, with the `threads`, `start_paused` and
  `crate` options.
- **`nuclei::net`.** `TcpListener`, `TcpStream`, `UdpSocket`, `UnixListener`, `UnixStream` and
  `UnixDatagram`. Addresses are anything `ToSocketAddrs` takes, and names are looked up on the
  blocking pool (`lookup_host` alone). `TcpSocket` and `UnixSocket` set a socket up before it
  listens or connects: buffer sizes, address and port reuse, keepalive, no-delay, linger, the type
  of service, and the network interface on Linux. Stream halves (`split`, `into_split`,
  `reunite`); Unix abstract names (`bind_addr`, `connect_addr`), `peer_cred` and
  `UnixDatagram::unbound`; `poll_accept`, `poll_peek`, `peek_sender`, `recv_buf` and
  `recv_buf_from`; readiness waits (`readable`, `writable`, `ready` and their `poll_` forms) and
  the `try_` calls, answered from the handle's read-ahead and write-behind; `try_io` and
  `async_io` for calls of one's own on a socket (Unix); `net::unix::pipe` for anonymous pipes and
  FIFOs.
- **`nuclei::io`.** `AsyncReadExt`, `AsyncWriteExt`, `AsyncBufReadExt` and `AsyncSeekExt`,
  `ReadBuf`, `copy`, `copy_buf` and `copy_bidirectional`, `BufReader`, `BufWriter` and
  `BufStream`, `duplex` and `simplex`, `split`, `empty`, `repeat` and `sink`, `stdin`, `stdout`
  and `stderr`, `Interest` and `Ready`; `io::unix::AsyncFd` drives a descriptor of one's own by
  readiness.
- **`nuclei::fs`.** `File`, `OpenOptions`, `DirBuilder`, `read_dir` streamed a page at a time,
  and the calls by path (`read`, `write`, `copy`, `rename`, `metadata`, links and the rest).
- **`nuclei::time`.** `sleep`, `sleep_until`, `timeout`, `timeout_at`, `interval`,
  `interval_at` and `Instant`; a clock that `pause`, `resume` and `advance` control, which jumps
  to the next timer when every shard is idle.
- **`nuclei::sync`.** Mutexes, reader-writer locks with upgradable reads, reentrant locks keyed
  by `Owner`, semaphores, `Notify`, `Barrier`, `Event`, `Latch`, `OnceCell`, `SetOnce`, `Lazy`,
  `CancellationToken`, and mpsc, oneshot, broadcast (with weak senders) and watch channels. They
  need only futures and wakers, so they work under any executor.
- **`nuclei::process`.** Child processes, their exits and their pipes; `kill_on_drop`, `kill`,
  `start_kill`, `try_wait`, `wait_with_output`, `spawn_with`, `as_std` and `into_std`.
- **`nuclei::signal`.** Unix signals and Windows console events as streams, and `ctrl_c`.
- **`nuclei::stats()`.** A snapshot of every shard's tasks, polls, IO in flight, buffers,
  timers and errors by kind, and of the blocking pool's threads.
- **Backends.** An I/O completion port backend on Windows; multishot receives on io_uring, from
  a provided buffer pool per shard; `read_owned` and `write_owned`, which lend the driver the
  caller's buffer, on TCP and Unix streams and on files.
- The `recycle` feature: node memory from a per-thread cache instead of the global allocator.
- `proofs/`: TLA+ models of the concurrent protocols and Lean proofs of the arithmetic they rest
  on.

### Changed

- The executor is nuclei's own; `init`, `init_with_config`, `spawn_more_threads`, `stop_thread`,
  `stop_current_thread` and `GlobalExecutorConfig` remain, on the shards.
- On Linux io_uring comes first, probed safely (a seccomp filter that kills the caller included),
  and epoll serves where io_uring cannot be used. `NUCLEI_BACKEND` and `Builder::backend` pick
  one.
- The epoll, kqueue and io_uring drivers are rewritten as proactive drivers of the shards: only
  the driver touches a descriptor, and every operation completes for the task that asked.
- TCP and UDP `bind` are async and take any `ToSocketAddrs`, a name included.
- `TcpListener::accept` gives `(Handle<TcpStream>, SocketAddr)` on every backend.
- `Handle<T>` takes an `IoObject`: on Unix anything that implements `AsFd` (it was `AsRawFd`),
  on Windows TCP and UDP sockets, files and a child's pipes.
- The io_uring ring is nuclei's own, on `libc` only, and needs Linux 6.1 or later.
  `IoUringConfiguration::default()` is interrupt-driven.
- Edition 2024; the minimum Rust version is 1.99.

### Removed

- The `async-exec` feature and the feature that ran the executor on another crate's runtime, and
  with them every dependency but `libc`, the `kovan` crates, `futures-io` and `futures-core`.
- `nuclei-attributes`, replaced by `nuclei-macros`.
- `HandleOpRegisterer`, `AsyncOp` and `Proactor::ring_params`.

### Migrating from 0.4

1. Ask for `nuclei = "0.5"` and Rust 1.99 or later, and drop the `async-exec` feature and any
   feature that named another runtime.
2. Name the socket types from `nuclei::net`: `TcpListener` is `Handle<std::net::TcpListener>`,
   so code that spells the handle out still compiles.
3. Await `bind` on TCP and UDP: `Handle::<TcpListener>::bind(addr)?` becomes
   `TcpListener::bind(addr).await?`. Unix `bind` stays synchronous.
4. Take the read and write helpers from `nuclei::io` (`AsyncReadExt`, `AsyncWriteExt`, ...); the
   handles implement the `futures-io` traits, so other crates' helpers keep working too.
5. Open files with `nuclei::fs` (`File::open(path).await`, `OpenOptions`), or keep
   `Handle::<File>::open`.
6. Use `#[nuclei::main]` and `#[nuclei::test]` from the crate itself; remove the
   `nuclei-attributes` dependency.
7. Code that used `HandleOpRegisterer`, `AsyncOp` or `Proactor::ring_params` calls the handle's
   own methods instead; the ring's settings are `IoUringConfiguration`.
