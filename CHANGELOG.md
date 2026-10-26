# Changelog

## 0.5.0 (unreleased)

Nuclei is now a runtime of its own: thread-per-core shards that each run their tasks and drive
their own IO, timers and buffers.

### Added

- `Runtime`, `Builder`, `RuntimeHandle`, `Task` and `FallibleTask`; `spawn`, `spawn_local`,
  `spawn_blocking` and `block_on` on the process default runtime, which starts on first use with
  no configuration. `block_on` nests, in a task too, and keeps its shard's other tasks and IO
  going. A runtime may start more workers up to a maximum and drain them again.
- An I/O completion port backend on Windows.
- Multishot receives on io_uring, from a provided buffer pool per shard.
- `read_owned` and `write_owned` on TCP and Unix streams, which lend the driver the caller's
  buffer.
- `nuclei::time`: `sleep`, `sleep_until`, `timeout`, `timeout_at`, `interval`, `interval_at`.
- `nuclei::sync`: mutexes, reader-writer locks with upgradable reads, reentrant locks keyed by
  `Owner`, semaphores, `Notify`, `Barrier`, `Event`, `Latch`, `OnceCell`, `SetOnce`, `Lazy`,
  `CancellationToken`, and mpsc, oneshot, broadcast and watch channels. They need only futures
  and wakers, so they work under any executor.
- `nuclei::process`: child processes, their exits and their pipes.
- `nuclei::signal`: Unix signals and Windows console events as streams.
- `nuclei::stats()`: a snapshot of every shard's tasks, polls, IO in flight, buffers, timers and
  errors by kind, and of the blocking pool's threads.
- `#[nuclei::main]`, `#[nuclei::test]` and `#[nuclei::bench]`, from the new `nuclei-macros`.
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
- `Handle<T>` takes an `IoObject`: on Unix anything that implements `AsFd` (it was `AsRawFd`),
  on Windows TCP and UDP sockets, files and a child's pipes.
- The io_uring ring is nuclei's own, on `libc` only, and needs Linux 6.1 or later.
- Edition 2024; the minimum Rust version is 1.90.

### Removed

- The `async-exec` feature and the feature that ran the executor on another crate's runtime, and
  with them every dependency but `libc`, the `kovan` crates, `futures-io` and `futures-core`.
- `nuclei-attributes`, replaced by `nuclei-macros`.
