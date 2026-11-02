# Changelog

## 0.5.4

### Fixed

- **A shard's ring kept past its shard wakes for what is posted to it, and lets go of every post**
  (io_uring). A worker that goes with a zero-copy notification still out hands its ring to a
  keeper, which waited for the ring's next completion without saying it sleeps, so a post from
  another thread woke nothing: a holder's detach, whose cancel is what ends the operation out,
  could wait for good, keeping a pool thread, the socket and the ring's memory. The keeper now
  takes part in the park handshake, so a post finds it parked and its wake ends the wait. What it
  could not apply it dropped: a source's registration posted by a thread that runs no shard
  stayed claimed, so no later wait registered it and its task waited for good, and a child's exit
  watch was never made. It now goes as for a shard that went: the registration is made where the
  task polls next, the watch where its user polls next. The keeper waits with no bound of its own
  for a peer that never reads; that is the kernel's to end (`proofs/tla/keeper/Kept.tla`, and
  `WZ_keeper_forever` in `LentZc.tla`).
- **A ring's wake never writes memory its shard freed** (io_uring). The read of the eventfd that
  wakes a ring stayed armed as its shard went, and landed in a buffer the shard freed; the kernel
  keeps the ring past its shard while a receive group's copy of the ring's descriptor holds it, so
  a waker that saw the shard parked just before, or a thread that kept its notifier, could have
  the kernel write 8 bytes into freed memory. The read now lands in memory that lives as long as
  the process (`proofs/tla/keeper/Kept.tla`).
- **An operation a going shard gives up on ends, with an error** (io_uring). A thread's own shard,
  on a ring only that thread can reap, waits a bounded time for what it cancelled as it goes. An
  operation still out then was forgotten with no end: its task waited for good, a later `get_mut`
  or `into_inner` of its handle waited for good for it to count out, and a file call's task never
  heard of it. It now ends with an error ("the shard went before the kernel ended the operation")
  and counts out, while the memory the kernel may still write is kept for good, as before (a
  zero-copy send that waits only for its notification ended at its first completion, and only
  keeps its buffer); the receive group is kept only when a receive that draws from it is out
  (`proofs/tla/keeper/Forgot.tla`).
- **A thread's own shard that leaves waits for an attach another thread still has to perform**
  (io_uring; every backend for the stale token). A source registered in a thread's `block_on`
  while another thread held one of its sides has its attach left to that holder. Once the
  `block_on` returned, its shard left at once; the holder then posted the file's install, with a
  lease that kept the handle's descriptor open until the thread's next `block_on`, and the slot's
  release, which that `block_on` applied after its first registration took the slot under the
  same token: that source was then in no table, never evicted, and its IO stalled once the shard
  left again. The shard now waits for an attach still asked of it as for an eviction, and applies
  what it posts before it leaves; and a drain of the shard's table starts its generations past
  every token it handed out, so a release that comes late names nothing
  (`proofs/tla/going/Pending.tla`).
- **A connection reset before it was accepted no longer ends a listener's accepts** (Windows; and
  `accept` on every system). `AcceptEx` completes with `WSAECONNRESET`, or `ERROR_NETNAME_DELETED`,
  for a connection its peer reset in the backlog before or while the accept took it. That error is
  the connection's, but the side kept it for the listener's task: `accept` returned it, and
  `accept_multi`'s stream ended with it, so the soak's server, whose peers reset with zero linger,
  stopped serving within its first seconds on Windows. Every backend now tells such an error from
  the listener's own (`lost_before_accept`) and skips the connection: a readiness side accepts the
  next one at once, a completion side keeps nothing and its task's poll sends the next accept, and a
  plain accept on io_uring that finds a connection a multishot accept brought gone (`ENOTCONN` from
  its peer) takes the next one. `accept`, `incoming` and `UnixListener::accept` skip `ECONNABORTED`
  as well, which they returned before (on macOS and the BSDs, for a connection reset in the
  backlog), as `accept_multi` already skipped it on unix; Linux hands such a connection over as it
  is, unchanged. The listener's own errors (out of descriptors or memory, not listening) still reach
  `accept` and end the stream (`proofs/tla/accept/AcceptReset.tla`, and `AcceptStream.tla` with
  reset connections).
- **`accept_multi` on Windows pauses for a socket the system has no room for, as for one it has no
  descriptor for.** Windows answers a socket it cannot make, the one an accept takes its connection
  into among them, with `WSAENOBUFS` as well as `WSAEMFILE`, and an `AcceptEx` that ends for want of
  system resources with `ERROR_NO_SYSTEM_RESOURCES`. Only `WSAEMFILE` paused the stream; the other
  two ended it for good, though the backlog would be served once resources came back. All three
  now pause it, as `EMFILE` and `ENFILE` do on unix.
- **A datagram received ahead is never cut** (Windows; and io_uring once its receive group ran
  dry). A port receives a UDP datagram ahead into a buffer of the pool for the call that waits, and
  sized that buffer by the call's length: a look at the sender (`peek_sender`, which asks for no
  bytes on Windows) or a short peek took a 1400-byte datagram into 512 bytes, the cut completed as
  a success, and the next `recv_from` got the first 512 bytes as the whole datagram. The buffer of
  a receive ahead now holds the largest UDP datagram whatever the call asked for, as the ring's
  receive group does; a pool with no such buffer has the port look with a zero-byte receive and the
  task receive into its own buffer, as before (`proofs/tla/ring/RingRecv.tla`, `Backend =
  "port"`, the `task_sized` mutation). A side that waits for a datagram on Windows holds a 64 KiB
  buffer of the pool while it waits, where it held one of its own length.
- **`allocate` on FreeBSD makes every change of space the system has a call for.** Only `Extend`
  worked there; every other mode answered `Unsupported`. `PunchHole` and `ZeroRange` are now
  `fspacectl(2)`'s `SPACECTL_DEALLOC` (FreeBSD 14 on, looked up when first used, so a program
  still starts on an older system, where these two answer `Unsupported`), which frees the range
  where the file system can (ZFS, tmpfs) and writes zeros over it where it cannot (UFS).
  `ZeroRange` then grows a file shorter than the range with a write of one zero at the range's
  last byte, through a descriptor that does not append, so a write of another's past the range
  is never cut, as a truncate to the range's end would cut it. `KeepSize` reserves a range inside
  the file with `posix_fallocate`; a range that ends past the end answers `Unsupported`, since no
  FreeBSD call reserves space there and keeps the length. ZFS reserves no space, so `Extend` and
  `KeepSize` answer its `EOPNOTSUPP` there as `Unsupported` (`proofs/tla/space/Space.tla`).
- **FreeBSD is tested, not only linted.** CI runs the suite in a FreeBSD 15 VM, and its file
  tests once more on UFS, since the VM's own ZFS reserves no space. The tests read FreeBSD's own
  limits where they differ from Linux's: a Unix datagram is at most `net.local.dgram.maxdgram`
  (8 KiB) whatever its buffers, and a send to a full peer is refused (`ENOBUFS`) as on macOS; a
  UDP datagram needs a send buffer its size; a TCP or Unix stream nobody reads holds 160 or
  64 KiB, so a case that writes before its peer reads sizes its buffers as on macOS, and a Unix
  stream's buffer of one byte moves one byte a call; `sendfile(2)` on a Unix stream whose peer
  went answers `ENOTCONN` where a write answers `EPIPE`; a TCP socket whose FIN was acknowledged
  is disconnected by a second shutdown of its writing half, or at once if its reading half is
  shut too, so its reads end and a shutdown after answers `ENOTCONN`, and the oracles of those
  cases now shut where the cases do; and FreeBSD has the keepalive timing options a test said it
  lacks. The tests that hold a process's descriptors flat counted three there, since FreeBSD's
  `/dev/fd` lists only the first three without `fdescfs`; they count them with the kernel's own
  count (`kern.proc.nfds`) now, the soak's too.
- **`allocate` on macOS says what a reservation covers.** `F_PREALLOCATE` reserves from the end
  of what the file has allocated, so a hole inside the file (as a `set_len` that grew it leaves)
  is not reserved, and HFS+ frees what was reserved past the end of the file at its close, with
  `F_ALLOCATEPERSIST` or without; APFS keeps it. The rustdoc and the Learn page say so, and a
  test pins APFS's reservation past the end across a close and a reopen.
- **`sync_dir` on Apple's systems says what it made.** A file system that refuses `F_FULLFSYNC`
  for a directory got a plain `fsync`, and an `Ok` that read as a full flush. It now gets
  `F_BARRIERFSYNC` first, which keeps the order of the writes around it, and `fsync` only if
  that is refused too; the rustdoc says that neither flushes the drive's cache, so a power loss
  may still lose the entries there (`proofs/tla/dirsync/DirSync.tla`).
- **The soak sizes its datagram sockets.** It sent UDP datagrams of up to 60 KB and Unix ones of
  up to 4000 bytes into sockets of the system's own size, which macOS and FreeBSD refuse past
  9216 bytes for UDP and macOS past 2048 for a Unix socket (`EMSGSIZE`), as std's sockets do:
  nuclei sizes no socket, as std does not. Its sockets now ask for buffers of 256 KiB, as an
  application that sends large datagrams does, so every system carries the same datagrams.
- **A Windows connect binds its port as before: `SO_REUSE_UNICASTPORT` is not set.** The option
  would have had a connect's explicit bind (which `ConnectEx` needs) share a port across
  destinations, as the bind std's connect makes itself does. On the CI runners' Windows it reads
  back unset after a set of either width, and a bind made with it still takes its port at once
  rather than leave it to the connect, so nothing shows it taking effect, and it was taken out. The
  soak's `WSAEADDRINUSE` it was meant for was TIME_WAIT toward the soak's one server (13455 of the
  16384 ephemeral ports), which no sharing across destinations helps.
- **A Windows datagram send answers what the kernel says of its own datagram.** A `send_to` on
  Windows copied its datagram, queued it for the completion port and answered taken at once, so a
  refusal the kernel made as it sent it (no route) reached the next call, whose own datagram was
  never sent. The task now hands its datagram to the kernel itself, as on unix, on a socket nuclei
  makes non-blocking (`FIONBIO`, which std's own calls on the socket then answer with `WouldBlock`,
  as on unix); a send that finds no room waits for an AFD poll of the socket that says there is,
  on the port, and tries again (`proofs/tla/ring/RingSend.tla`, `Backend = "port"`). Each send costs
  one call and no copy into a buffer of the pool, where it cost a copy and a completion.
- **A Windows socket whose send waits for its peer moves to another shard without a hang.** A TCP
  send is never cancelled, and a handle cannot move to another port while one is out: a stream let
  go by its shard with a send out (a `block_on` that returned) could not register again until the
  send ended, and when its peer read only after it heard from it, both ends waited for good. A
  source that cannot move now stays on the port its handle is bound to, registered with its new
  shard: its operations go there, the port's keeper drains it while a source waits to move, and
  once none of its operations is out, or its send ended, it is registered again and the move is
  made (`proofs/tla/port/PortKeep.tla`, `Duplex`). One registering again with the shard whose port
  its handle is bound to (its thread's next `block_on`) needs no move.
- **A datagram an owned receive given up had cut is dropped, not handed on.** A `recv_from_owned`
  whose future was dropped after the driver received a datagram into its buffer hands the datagram
  to the next receive; one longer than that buffer was cut to it, and the next receive took its
  first bytes for the whole datagram. The kernel says whether it cut one (`MSG_TRUNC` on unix,
  `STATUS_BUFFER_OVERFLOW` on Windows): a cut one is now dropped and counted in
  `ErrorCounts::cut_datagrams_dropped`, and one that came whole, filling the buffer exactly or not,
  goes on as before (`proofs/tla/lent/LentOrphan.tla`). On epoll and kqueue the driver's receive
  into a lent buffer is a `recvmsg`, to read that flag.
- **A Windows file opened for overlapped IO is read and written as one opened for synchronous
  IO.** A disk file opened with `FILE_FLAG_OVERLAPPED` (a log's `FILE_FLAG_OVERLAPPED |
  FILE_FLAG_NO_BUFFERING | FILE_FLAG_WRITE_THROUGH`) was driven as a synchronous one: a `ReadFile`
  or `WriteFile` the system answered `ERROR_IO_PENDING` returned that as the call's error while the
  kernel still held it, and the kernel later wrote the call's end into an `OVERLAPPED` on a stack
  frame that had returned, and moved bytes into or out of a buffer the caller had dropped. Each
  read and write at an offset, and each `DeviceIoControl` (a sparse file, a zeroed range), now
  names the calling thread's own event, and one answered pending is waited out on it
  (`GetOverlappedResult`) before the call returns; the event's low bit keeps the end off the port
  the handle is bound to (`proofs/tla/port/PortFile.tla`). A test writes such a file at its cursor
  and at offsets at once, syncs it and reads every byte back.
- **`Allocate::KeepSize` says what it keeps on Windows.** NTFS holds the space reserved past a
  file's end only while a handle to the file is open, and frees it when the last one closes (a
  1 MiB reservation read back 1 MiB with a handle open and 0 once reopened after the last close);
  the docs promised it as a lasting reservation. A test checks both on Windows.
- **A failed sync is no longer forgotten by a dropped or a concurrent sync**
  (every backend). The kernel reports a lost write-back to one sync call only, and the handle
  kept the failure only once the failing call's task took its end. A sync whose future was
  dropped (a timeout) after its call failed, and before its task looked, left nothing, so the
  next sync made a call of its own, which the kernel answered with no error. Two syncs at once had
  the same gap: the second call, made after the first failed, succeeded, and its task looking
  first answered `Ok`. A handle's sync calls are now the file state's, one at a time: a sync
  called while another's call is out waits for that call's end, a call whose future went away
  stays in the state, and the next sync takes its end, failure and all. A sync fails for its own
  call's failure and for one of a call made before its own, never for a later one's
  (`proofs/tla/lent/LentFile.tla`, scenario "sync": `MF_sync_task_records`,
  `MF_sync_unserialized`, `MF_sync_any_failure`).
- **The first read after `get_mut` no longer answers `NotConnected` on a valid handle** (every
  backend). A registration asked while another thread held a side of the source is left to that
  holder, and so is the retire `get_mut` asks; the holder performs the two at once, the retire
  first, so the attach found no descriptor, and its refusal failed both sides as a failed
  registration does. A registration posted to a shard from a thread that runs none, which the
  shard made only after the retire, was refused the same way and kept as the source's failure.
  Either way the next read or write of the handle answered `NotConnected`. An attach refused
  because a retire or a close took the descriptor out now fails nothing, as one refused because
  its shard stopped: the source is unregistered, and its next wait registers it again
  (`proofs/tla/attach/Merged.tla`).
- **A write after a dropped `send_file` no longer lands before its last chunk** (Linux, Android,
  macOS, FreeBSD). Each chunk of a file sent in the kernel is a `sendfile(2)` call on the blocking
  pool, made on the stream's descriptor outside its write side, which held nothing for it. A call
  dropped (a timeout) while a chunk's call was out, on a page that was not cached say, left the
  side idle: the handle's next write, an error frame for instance, went out at once, and the chunk
  landed after it, so the peer read the file's prefix, the frame, then the chunk. While a chunk's
  call is out the write side now holds its place, a lent write that no driver writes, so every
  write after it waits for the chunk, as after any lent write; whichever of the call's job and its
  task ends second clears the place (`proofs/tla/sendfile/SendFile.tla`: `M_no_place`,
  `M_clear_at_drop`, `M_clear_unended`, and the four that leave the place held). Windows sent its
  chunks through the write side already.
- **A closed socket leaves a ring's table though the kernel once refused to take it out**
  (Linux, io_uring). A source that goes takes its file out of the ring's table of fixed files at
  once, so the kernel closes it as soon as its requests let go. When the kernel had no memory for
  that update, the failure was ignored and the slot released with the file still in the table:
  the socket stayed open (a dropped listener kept accepting and kept its port) until another
  source happened to take the slot. A refused take-out now keeps the slot held with the file in
  it, so no other source gets the slot meanwhile, and is tried again after the ring's next wait,
  which a 10 ms timer of the ring's own brings though nothing else is out
  (`proofs/tla/ring/RingClose.tla`: `MC_update_ignored`, `MC_update_frees`,
  `MC_update_no_wake`; `proofs/lean/Nuclei/Files.lean`: `refused_release_keeps_the_slot`).
- **A stream write that found the socket full no longer waits for good when its shard is handed
  off in the same poll** (epoll, kqueue). The owner's straight send finds the socket full, and the
  rest of its poll was told so and blocked the side on the next write edge without asking the
  kernel again. When the monitor took the shard between the two, the shard's driver on its new
  thread could take the edge that room brought and pass over it, since the send's hold had
  published nothing and the side still said it was only ever its owner's and idle; the rest of the
  poll, now on a thread with no shard, then blocked the side on an edge that had already passed,
  and a write or a flush waited until the peer spoke again. A poll whose shard was taken now asks
  the kernel itself under its own hold (`proofs/tla/sendsnow/TakenFull.tla`).
- **A shard handed off while its old thread sits in a wait no signal ends goes on with its polls
  and its wakes** (Linux, io_uring). A watched worker's ring is not single-issuer, so the kernel
  runs the completion of each request as work queued to the thread that submitted it, and a thread
  in a killable wait (a `vfork` parent, a read of a slow disk, a stalled network file system) runs
  none of it until it comes back. After a handoff, a stream's or a datagram's multishot receive, a
  datagram's poll, a write waiting for room, and the ring's own wake read, all armed by the old
  thread, then brought nothing: the stream and the write waited, and a task spawned or woken from
  another thread did not run, for as long as the old thread's wait lasted. The new runner now
  cancels each poll and multishot receive the old thread armed, before its first drive, and its
  side arms another of the new runner's; the old one stays until its own end, which goes to no
  side, and the ring's wake is armed again under an id of its own. What an old receive still takes
  before its thread sees the cancel goes to the side in the stream's order, since the kernel takes
  and posts each arrival under the ring's lock (`proofs/tla/ring/RingHandoff.tla`). A one-shot
  operation that takes or gives bytes (a receive into the task's buffer, a send, a connect), a
  file's call, a child's exit watch and an accept still complete only once the old thread comes
  back: an accept's poll is the first exclusive
  waiter of its listener, which takes each connection's wake even once cancelled, so it cannot be
  armed again from the new runner.
- **A call whose heavy fence fails no longer waits for good on a side whose claimant cannot serve
  it** (Linux, whose heavy fence, `membarrier`, can fail under memory pressure). Such a call leaves its work in a biased side and nudges the shard that claimed the side,
  which does it at its next take. The claim stayed with the first shard that claimed it, so once a
  source left that shard (a thread's own shard whose `block_on` returned, a worker that went), the
  nudge reached a shard that ran nothing, and once the source was homed on another shard the
  claimant's own calls, foreign now, nudged themselves without end; the call waited until something
  else took the side. A shard now gives its claim up as it lets go of its sources, doing any work a
  fenceless call left before, and a fenceless call that finds the claim given up, or that runs on
  the claimant's own thread with nothing held, takes the side itself. The shard claims the side
  again at its next take as its owner (`proofs/tla/side_biased/SideBiased.tla`, `ClaimStops` and
  `Rehomed`).
- **A caller's clear of readiness made while another holds the side is kept** (epoll, kqueue,
  io_uring). `AsyncFdReadyGuard::clear_ready`, and the clear of a socket's `try_io` and
  `async_io` after a call that would block, went through a poll of the side; one that found the
  side held for a moment (another task's look at it, the driver serving an edge) was dropped, so
  the task found the side ready again with no edge since and called again, and again, for as
  long as the other tasks kept the side busy. A clear that finds the side held now leaves its
  tick for the holder, who clears the side as it lets go, before any look can see it ready; of
  ticks left at once the newest is kept, the only one that can still clear. A free side costs
  what it did (`proofs/tla/asyncfd/AsyncFd.tla`, the `drop_held_clear` and `clear_last_wins`
  mutations).
- **Work left for a socket's holder whose poll panicked is done by the next holder.** A poll
  registers its task's waker while it holds the side, which clones the waker: a clone that
  panicked unwound the guard, whose drop swapped the side's word to zero, and the edge, the
  completion or the request another thread had left for that holder was lost (a read waited for
  an edge that had come, a ring completion went unapplied, a dropped handle's close never ran).
  The drop now lets the side go and wakes the polls waiting on it as before, and leaves that work
  in the word, where the next holder does it (`proofs/tla/side/Side.tla`, `Panics`, the
  `unwind_clears` mutation).
- **The IO buffers of a thread that runs no shard hold no more than its runtime's
  `io_buffer_cap`.** A thread of the blocking pool, one inside a runtime with `enter`, and a
  worker whose shard was handed over take their buffers from a pool of their own, which held up
  to the 64 MiB default whatever the runtime's `io_buffer_cap` said. That pool is now held to the
  cap of the runtime the thread serves as it starts serving (a pool thread and a worker before
  their first job, a thread at `enter`, whose pool frees its idle buffers past the cap at once and
  takes no new memory until it is back under it); a thread that serves no runtime keeps the
  default. The IO steps that
  reach the pool cost what they did (`proofs/tla/bufcap/BufCap.tla`).
- **A `with` in a poll that lost its shard to a handoff runs on that shard.** Once the handoff
  watch took a worker's shard from a poll that ran past `handoff_after`, every
  `ShardLocal::with` in the rest of that poll, in a nested `block_on` in it, and in the tasks
  `spawn_local` kept on its thread panicked with "this code runs on no worker", and `try_with`
  answered that error. The watch reads wall time, so a poll that never blocked met it too: a
  thread preempted, or throttled by a CPU quota, for 10 ms before its `with` (seen on macOS CI).
  Such a call now asks the thread that runs the shard for it: that thread lends it between two of
  its polls, hands its own queued tasks back to their schedules first (a task bound to a thread
  stays on it), and waits parked while the call runs on the shard's value; the shard comes back
  as the call returns or unwinds, and a call still waiting as the runtime shuts down answers that
  the runtime shut down (`proofs/tla/handoff/HandBack.tla`). Each such call is a round trip
  between two threads. A `with` that keeps its shard costs fewer instructions than before: its
  look at a value made already is inlined and the making is out of line, 16.9 to 13.9 user
  instructions per call with the watch off and 22.3 to 19.1 with it on (io_uring, default release
  build with one codegen unit, `ci/op_count`'s `with` row under `perf stat`).
- **A `block_on` future that finishes in the poll its worker's shard was taken in no longer
  writes into that shard** (handoff on). The loop looked at the future's output before the take,
  so a nested `block_on` kept its waker in the cell of the shard another thread now ran, a data
  race, and a worker's loop answered "stopped" for a shard it had lost, so its thread skipped its
  bound tasks and stayed marked as blocked. Both now look whether the thread was detached before
  they touch the shard (`proofs/tla/handoff`, `N_safe`; the `ready_first` and `stopped_on_take`
  mutations), at no cost to the outermost `block_on`, whose shard nobody takes. A poll on a shard
  nobody watches no longer looks whether it was taken, which it never is: about 7 fewer user
  instructions for each task run with the watch off, counted as CI's gate counts them (a spawn and
  its join 598 instead of 612 on io_uring, a TCP round trip 2858 instead of 2890, a bounded channel
  round 503 instead of 510; `block_on` 308 as before, and no row above what it was).
- **A runtime drop waits for a thread back from a blocked poll** (handoff on). The monitor stored
  "detached" after its take and the thread stored "back" once its bound tasks were done: a thread
  back before the monitor's store stayed marked as blocked for good, so every later drop skipped
  joining it, and a drop while its bound tasks ran returned before they were dropped. The two
  sides now add and take one off, in either order, and the thread says it is back as soon as its
  poll returns (`proofs/tla/handoff/TakeRef.tla`, the `life_stored` and `back_after_home`
  mutations).
- **An IO step that unwinds no longer keeps its worker from being rescued** (handoff on). A
  stream's step marks its call into the shard until it settles; a panic in the step that the task
  caught (a waker's clone that panics) skipped the settle, so the rest of the poll looked inside
  the shard and the monitor never took a poll that then blocked. The step's drop now ends the
  call (`proofs/tla/handoff`, the `marked_unwind` mutation).
- **A handoff given up because the kernel refused its fence is counted.** On Linux the monitor's
  take runs `membarrier`; when the call failed, the take was cancelled silently, and the blocked
  poll went on holding up its shard. `ShardStats::handoffs_failed` counts each one
  (`proofs/tla/handoff`, `F_fail`). The handoff model now also checks the full fence of macOS, the
  BSDs and a Linux without `membarrier` (`F_safe`, with a mutation for each side's fence), both
  compare-and-swaps of the claim (the `blind_take` and `blind_contest` mutations), and two calls
  into the runtime in one poll (`L_two`, `D_two`).
- **A runtime's drop never waits on a thread its monitor takes as it closes, and joins every runner
  it starts.** The monitor's loop looked at the close only at its head, so a pass under way as a
  drop closed it could still take a blocked worker's shard and start a runner for it, after the drop
  had taken its list of threads: the drop then waited on the thread blocked in the taken poll, which
  may never return, and never joined the runner, which dropped the shard's tasks after the drop
  returned. The drop now joins the monitor's thread first, then takes the list
  (`proofs/tla/teardown/Teardown.tla`).
- **A runtime dropped in one of its own tasks no longer joins its own thread.** The drop joined
  every runtime thread, the dropping one too: a failed join that panicked on Linux, macOS and
  FreeBSD, and a wait for good on Windows. It joins every other thread, and the dropping one goes on
  once the drop returned.
- **`shutdown_timeout` waits for each worker until it exits or the deadline passes.** It took one
  exit message for each worker not finished, from whichever thread sent it (the monitor, a worker
  stopped earlier, a detached thread), so it could return while a worker still ran its task, and
  the wait for what dropped handles left then started early. Each message now names its thread and
  only wakes the wait, which goes on until every thread it waits for read as finished or sent its
  own (`proofs/tla/teardown/Join.tla`).
- **A gift handed to a spare that left is dropped inside its runtime.** A spare that saw the close
  and left could still be popped by the monitor's last hand-off, whose reference was then the last:
  the shard went with it on the monitor's thread outside the runtime, and what its going handed on
  (a source's bytes behind, a shard-local value's drop) went to another runtime. The popped spare is
  now let go of inside the shard's runtime.
- **A thread hook or thread name function that panics loses no shard, task or blocking job.** A
  panic in `on_thread_start` left the thread: on a runner the monitor started for a blocked
  worker's shard it dropped that shard with its tasks, and on a blocking-pool thread it left the
  thread counted, so a pool at its cap never ran the job. A panic in `on_thread_park` or
  `on_thread_unpark` ended the worker the same way, its shard still marked parked. Every thread
  hook's panic is now caught where the runtime calls it, once the panic hook reported it, and the
  thread goes on. `thread_name_fn` lost the same: its panic as the monitor named a runner dropped
  the shard handed to it, on the monitor's thread, and ended the monitor; one as the pool named a
  thread left it counted and never started; one as a worker started left that worker's place
  claimed and empty for good. Where the runtime starts a thread of its own accord (a runner, a
  pool thread) the panic is caught and the thread takes the default name; as a worker starts
  (`build`, `spawn_more_threads`) the name now comes first, so the panic reaches the caller with
  nothing claimed. A name with a NUL byte, on which every thread's spawn panicked, is taken up to
  it (`proofs/tla/hooks/Hooks.tla`, `proofs/tla/pool/Start.tla`, which also checks a pool thread
  that fails to start).
- **A runtime's drop stops and joins a worker a task starts meanwhile, and none starts once it
  took its threads.** A worker started by `spawn_more_threads` as the runtime dropped was published
  after the drop stopped every shard: put on the list in time, the drop waited on its thread for
  good; put on it after the drop took the list, it ran on, never stopped or joined, once the drop
  returned (as did a monitor a first start began). A thread now starts, goes on the list and has
  its shard published under the list's lock unless the drop took the list, and the take stops every
  shard again, so a worker started before it is stopped and joined; `spawn_more_threads` starts
  none after it (`proofs/tla/teardown/Grow.tla`).
- **A worker whose thread cannot start drops no task placed on it.** `spawn_more_threads` published
  the new worker's shard to placement before it asked for the thread: a spawn in between queued its
  task there, and a failed start stopped the shard and dropped it with the task. The shard is
  published only once its thread started (`proofs/tla/placement/Publish.tla`).
- **What a handle dropped with no worker wrote behind reaches the peer without another `block_on`.**
  With no worker taking tasks, a linger (the bytes a socket's write-behind holds, a file's writes
  behind) went to the dropping thread's own shard, which runs only at that thread's next
  `block_on`, or to the runtime's list, which only its drop runs: with neither coming, the bytes
  never went and the socket stayed open. It runs on the linger thread, one for the process, inside
  its runtime and counted by it, so a drop waits for it; that thread also takes the lingers handed
  where no runtime runs, which each started a thread and a runtime of their own
  (`proofs/tla/going/Unworked.tla`).
- **A child whose exit a `try_wait` saw is still reaped** (unix). `try_wait` stores the status of
  a child that exited without reaping it, but that status counted as the watcher's own: once the
  shard watching the child let it go (the thread's own shard at the end of its `block_on`) before
  its driver reported the exit, neither the waiter nor the `Child`'s drop had it watched again,
  and the child stayed a zombie. Only the watcher's outcome now ends the need for a watch, and a
  wait or `try_wait` that takes a peeked status has the child watched, so it is reaped. A `Child`
  dropped while its shard let the child go could also leave it unwatched: the shard read whether
  the `Child` was gone off the count of the exit's holders before clearing its watch, the
  `Child` found the watch held and let go. The `Child` now says it is gone before it claims the
  watch, and the shard clears its watch before it looks, so one of the two always has the child
  watched (`proofs/tla/process/ExitWatch.tla`, the `count_orphan` and `outcome_settles`
  mutations).
- **`Proactor::wake` ends a `Proactor::wait` made inside a task or a `block_on`.** The wake reaches
  the shards waiting in `Proactor::wait`, but only a wait on a thread that runs no shard entered
  its shard among them: a wait on a thread that runs one (a task's, a `block_on`'s) ran its turn
  unseen, and waited until IO came (`proofs/tla/park/Park.tla`, `unregistered_wait`).
- **A turn driven by hand runs a bounded number of tasks.** `Proactor::wait` ran every task ready,
  and the ones they woke, before it looked at its timers or its poller: a task that kept yielding
  (waiting for a flag a timer's task sets) held the call for good at full CPU. Each of the turn's
  two runs of tasks now takes at most `event_interval` of them (`proofs/tla/loop/RunLoop.tla`,
  `Mode = "turn"`).
- **A dropped signal pump no longer leaves its listeners without one** (unix). A listener's poll
  starts a pump where its flag says none runs, and only a pump that ended on a read error lowered
  that flag. The flag was the thread's (or, for listeners polled where no shard runs, a static),
  while the pump is a task of a shard, which the handoff moves to another thread and which goes
  with its runtime: a pump dropped with its runtime left the flag raised, and a listener polled
  there was never woken. The flag is now the shard's, and the pump's drop lowers it, then wakes
  every listener, so each polls again and starts a pump where none serves it any more
  (`proofs/tla/signal/SignalPump.tla`).
- **A notification of a sleeping worker that fails is tried again** (macOS, the BSDs, Windows).
  `kevent` and `PostQueuedCompletionStatus` can fail for want of a resource, and the failure was
  dropped. The waker that takes a sleeping shard's flag back is the only one that notifies it, so
  the shard slept on with its new work. The notification is now tried again until the kernel
  takes it, and after 64 failures in a row reported as an error (`proofs/tla/park/Park.tla`,
  `notify_swallowed`).
- **A timer cancelled from another thread no longer stops a paused clock.** A sleep armed on a
  shard and dropped, or polled again, on another thread posts its removal to the shard, and only
  an arm request woke a parked shard; a shard holding a posted op is not idle, and a paused clock
  jumps only while every shard is, so a removal posted to a parked shard held every jump of the
  runtime, and every sleeper on it waited for good. While a clock is paused or advanced every
  timer op now wakes its shard (`proofs/tla/clock`, the `post_no_wake` mutation).
- **A paused clock counts a thread whose shard the handoff took.** The thread's bound tasks
  finish on its own shard, which no paused clock looked at, so a sleep one of them armed there
  never came due; and the rest of the poll the shard was taken from still ran on the thread, on
  no shard, while the clock could jump past what it was about to do. The own shard is now a guest
  of the paused clock, as a `block_on`'s is, and the rest of a taken poll is work in flight, as a
  blocking job is: counted from the take until the thread's bound tasks ended, except while the
  thread runs it on its own shard as a guest, which counts it then (`proofs/tla/clock`, the
  `M_uncounted_shard` and `detached_uncounted` runs).
- **A sleep moved to another task as its shard goes is armed again.** A sleep polled on a thread
  that runs no shard waits on a cell its shard fires; a shard that goes marks the cell orphaned
  and wakes the waker it held. A poll looked at that mark before it registered its waker, and
  after it only at whether the timer fired: a shard going in between woke the waker of the task
  that held the sleep before, and the task that held it now was never woken. The poll now looks
  at the mark after it registers (`proofs/tla/timer`, the `recheck_fired_only` mutation), and no
  more before it.
- **A clock read as it is paused never goes back.** A reading of a clock nobody paused yet
  leaves no trace, and read the system's time after it found the clock unpaused, while a pause
  read the time it stops at before it marked the clock: a reading taken between the two could be
  later than the paused time, and the next reading earlier than it. A reading now takes the
  system's time before it looks at the mark, and a pause marks the clock before it reads its time
  (`proofs/tla/clock/ClockRead.tla`); a clock that was paused or advanced reads the system's time
  once per reading, as before.
- **A precise sleep ends on time after the running clock is advanced** (epoll, kqueue). The
  poller's own timer counts the system's time but was armed again only when the deadline on the
  clock changed: after `time::advance` on a running clock it stayed at the deadline as the system
  saw it before, and the sleep ended on the wait's own millisecond timeout instead, up to a
  millisecond late. It is now armed for the deadline as the system sees it
  (`proofs/tla/precise`, the `key_on_clock` mutation). A clock nobody paused or advanced pays
  nothing for it.
- **A timer's key never reaches a later timer in its place.** Each entry of a timer wheel counts
  its reuses in a generation a key carries, which was 32 bits: a key held while its entry was
  let go of and taken again 2^32 times reached the timer in the entry then, as a sleep that fired
  and was not polled for minutes might on a shard cancelling millions of timeouts a second, and
  set that timer's waker or removed it. The generation is now 64 bits, which takes centuries to
  come round at one reuse a nanosecond (`proofs/lean/Nuclei/Wheel.lean`,
  `stale_key_stays_stale`). An entry of the wheel takes 56 bytes instead of 48.

- **A reentrant lock request that queued before its owner took the lock enters at its next
  poll.** A `ReentrantMutex` lock, or a `ReentrantRwLock` read, looked for its owner only on its
  first poll; once queued it only looked at its place in line. So an owner with two requests out
  at once (`join!(lock.write(&me), lock.read(&me))` behind another owner) took the write with the
  first and left the second waiting for a share its own write kept out, and the join, which held
  the write until the read ended, hung. A queued `ReentrantRwLock` write whose owner read
  meanwhile waited for that read instead of failing with `ReadHeldByOwner`, and two queued reads
  of one owner granted together claimed two records. Every poll now looks for the owner first,
  queued or not: a request whose owner holds the lock leaves the line, passing on a share it was
  granted, and enters on the owner's record; a write whose owner reads is refused. A request is
  not woken for that, so one last polled before its sibling entered (a `join!` that polls it
  first, or another task of the same owner) still waits as another owner's would
  (`proofs/tla/reentrant`, `L_join`, `RW_join_write_read`, `RW_join_reads`,
  `RW_write_queued_read`, and the witness `W_join_young_first`).
- **A `Notified` that takes a stored permit after a broadcast it was owed passes the permit on.**
  A `Notified` looks at the generation, then takes a stored permit, in two steps. A
  `notify_waiters` and a `notify_one` between them left a waiter the broadcast had served holding
  the permit too, so a waiter made after the `notify_one` found none and waited for good. A waiter
  that took the permit looks at the generation again and passes the permit on when it moved
  (`proofs/tla/notify/Notify.tla`, `L_owed_permit` and the `no_recheck` mutation; one load more on
  the path that takes a permit). The queue's pass reads the generation once as well, and a
  broadcast and a `notify_one` that land while it runs still lose the permit to a waiter the
  broadcast woke; that path is open (the witness `W_owed_to_woken`).
- **A barrier leader's wait polled again after it completed counts no second arrival.** The
  leader's `BarrierWait` kept no mark that it led, so a poll after its `Ready` arrived again: the
  phantom stood in for a real arrival, the next generation completed one task short, and a wait
  returned before its group met. The wait is now fused: polled again, it is ready with the same
  result (`proofs/tla/barrier/Barrier.tla`, the `leader_unfused` mutation).
- **An acquire, lock or bounded send granted in the poll that queued it spends the task's
  budget.** A wait whose first attempt failed queued, and the pass of its own queueing (or its
  retry after a nudge) could grant it at once; that grant completed without spending, so a hot
  task under contention ran past `coop::BUDGET` in one poll. It now spends one as a take at once
  does, and with the budget spent it gives what it got back and yields; a grant to a wait that
  queued in an earlier poll still spends nothing (`proofs/tla/coop/Coop.tla`, the
  `enqueue_grant_free` mutation).
- **A bounded send refused for the budget wakes a receiver waiting for its closed channel to go
  idle.** The refusal gave its slot back with a bare release; if the receiver had closed and was
  waiting for the last slot, nothing told it until the refused send was polled again or dropped,
  which nothing makes happen. The give-back now wakes it as every other give-back does
  (`proofs/tla/mpsc/BoundedChannel.tla`, `BC_live_refuse` and `ML_refuse_no_idle_wake`).
- **An interval never panics on a tick past the clock's range, and a skip lands after now
  however far behind it is.** The next tick was the last one plus the period with `+`, which
  panics past the clock's range: `interval(Duration::MAX)` panicked in its first tick, and
  `reset` did the same. Such a tick now waits in the far future, as a timeout's deadline does,
  so a period too large ticks once. `Skip` counted the periods it missed in a `u32`: late by more
  than 2^32 periods (49 days at a millisecond), its next tick was still in the past and fired at
  once. It now takes what is left of the period now falls in, so the next tick is the next one
  of the schedule after now (`proofs/tla/interval/Interval.tla`).

### Changed

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

- **A spawn or wake from a task's code on a watched worker pushes as a leaf.** Such a call put
  the task on its shard through a bracket out of line: the call's mark, the push and the mark's
  end, in a function that saved six registers to do it. A push into the shard's empty LIFO slot,
  the usual one, now first tries a leaf (`context::push_lifo_marked`): the call marked and its
  claim read as a bracket does, the task put in the slot and the poll's word put back. A claim
  up, a slot that holds a task or a task of another shard puts the word back as it found it and
  takes the full bracket, which settles the claim (`Slot::mark_unless_claimed`;
  `proofs/tla/handoff`, the leaf push, whose mutations go in with no look at the claim and back
  out with the mark still up). With the default watch on io_uring, default release build, user
  instructions per operation: a spawn and its join 790 before, 756 now (631 with the watch off);
  a oneshot round 1017, 983 (858 off); a bounded channel round 611, 594 (530 off); under fat
  LTO 757, 715; 864, 822; 530, 509. With the watch off the rows cost what they did, but for the
  file read, whose read's frame spills one more register: about 3 more of 2606 on a read the page
  cache serves, and about 4 more of 3692 on io_uring on one it misses, the same on epoll (counted
  on a 16 MiB file read in 64 KiB pieces, its cached pages dropped with
  `posix_fadvise(POSIX_FADV_DONTNEED)` before each read).
- **Still behind: the watch's cost of a task's run, and the file read's spilled register.** A
  watched worker still runs each task out of line (`run_task_watched`: 33 instructions, about 14
  of them its frame) where an unwatched one runs it inline, two runs to a spawn and its join: with
  the watch on, a spawn and its join costs 756 to the 631 of one with it off. Three ways were
  measured for 0.5.4 and left out, each for what it cost with the watch off. The worker's loop
  compiled once for a watched slot and once for none, its watched runs inline: a spawn and its
  join 737 with the watch on, but each stream and datagram row 29 to 38 more with it off under fat
  LTO and with one codegen unit. The run coming back on its own last word with its mark bit
  cleared, keeping no epoch and one register less: 746 with the watch on, but with it off the file
  read about 4 more on a page cache hit and about 10 more on an io_uring miss. The file read's
  rarer steps (its job in flight, its wait for a write before it, its job's submit) out of line,
  so a read the page cache serves spills nothing: 2617 to 2596 per read on io_uring, but each read
  that misses the cache 35 more on io_uring and 75 more on epoll, and with only the submit and the
  wait out of line still about 20 more on io_uring.

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
