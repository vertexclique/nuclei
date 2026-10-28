# Changelog

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
