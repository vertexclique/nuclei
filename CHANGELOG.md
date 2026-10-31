# Changelog

## 0.5.2

### Fixed

- **A runtime's drop whose bound runs out no longer drops a linger it was running.** The drop
  runs what dropped handles left for at most the linger time, and a linger it had started then was
  dropped with its run, closing the stream with the bytes behind never sent. It now goes on on a
  thread of its own, to its own deadline, as the ones the run never took do
  (`proofs/tla/going/Linger.tla`).
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

### Changed

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
