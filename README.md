<h1 align="center">
    <img src="https://raw.githubusercontent.com/vertexclique/nuclei/master/art/nuclei-banner-2000x500.png" width="100%" alt="Nuclei"/>
</h1>
<div align="center">
 <strong>
   Nuclei: Proactive IO & Runtime system
 </strong>
<hr>
</div>

<div align="center">
    <!-- CI builds -->
    <a href="https://github.com/vertexclique/nuclei/actions">
    <img alt="Build Status" src="https://github.com/vertexclique/nuclei/workflows/CI/badge.svg" />
    </a>
    <!-- Crates version -->
    <a href="https://crates.io/crates/nuclei">
    <img alt="Crates.io" src="https://img.shields.io/crates/v/nuclei.svg?style=popout-square">
    </a>
    <!-- License -->
    <a href="https://github.com/vertexclique/nuclei/blob/master/LICENSE">
    <img alt="Crates.io" src="https://img.shields.io/crates/l/nuclei.svg?style=popout-square">
    </a>
    <!-- Downloads -->
    <a href="https://crates.io/crates/nuclei">
    <img src="https://img.shields.io/crates/d/nuclei.svg?style=flat-square"
      alt="Download" />
    </a>
    <!-- docs.rs docs -->
    <a href="https://docs.rs/nuclei">
    <img src="https://img.shields.io/badge/docs-latest-blue.svg?style=flat-square"
      alt="docs.rs docs" />
    </a>
</div>

Nuclei: proactive IO runtime. Thread-per-core shards each run their own tasks and drive their
own IO, timers and buffers. Every operation is completed for the task that asked for it; no
task waits for readiness and then makes the call itself.

It needs no configuration: the first `spawn` or `block_on` starts a runtime with one shard per
core. A `Builder` sets every setting for a runtime of one's own.

```rust
use nuclei::net::TcpListener;

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    let listener = TcpListener::bind("127.0.0.1:7000").await?;
    loop {
        let (stream, _) = listener.accept().await?;
        nuclei::spawn(async move {
            let mut buf = [0u8; 1024];
            while let Ok(n @ 1..) = stream.recv(&mut buf).await {
                if stream.send(&buf[..n]).await.is_err() {
                    break;
                }
            }
        })
        .detach();
    }
}
```

**[Learn Nuclei](https://nuclei.rs/learn/)** goes from this program to a server in production, with
an example that compiles and runs for every idea; the [API reference](https://docs.rs/nuclei) has
every item. For a language model, [nuclei.rs/llms.txt](https://nuclei.rs/llms.txt) points to all of
it as Markdown, and [llms-full.txt](https://nuclei.rs/llms-full.txt) holds it in one file.

## Backends

| Platform | Backend |
|---|---|
| Linux | io_uring where the kernel and the sandbox allow it, else epoll |
| macOS, FreeBSD, NetBSD, OpenBSD, DragonFly | kqueue |
| Windows | I/O completion ports |

On Linux the runtime first tries io_uring and falls back to epoll when it cannot be used: on a
kernel before 6.1 (its rings are single-issuer with deferred task work), or where a sandbox
refuses it. It probes safely, even under a seccomp filter that kills a process for calling
`io_uring_setup`. CI runs the suite on io_uring and on epoll on Linux 6.1, 6.6 and 6.12, on kqueue
on macOS, and on completion ports on Windows. `NUCLEI_BACKEND` picks one: `auto`, `io_uring` or
`epoll` on Linux, `kqueue` on macOS and the BSDs, `iocp` on Windows.

## What it has

* **Shards.** A shard per worker thread runs its tasks and owns its driver; a task stays on the
  shard it was placed on. `block_on` works anywhere, nested in a task too, and keeps its shard's
  other tasks and IO going. A task that blocks inside its poll holds up only itself: past
  `Builder::handoff_after` (10 ms) its shard, with every other task, timer and IO of it, goes on
  on another thread.
* **Tasks.** `spawn`, `spawn_local` for tasks that are not `Send`, `spawn_blocking` for blocking
  work; `JoinSet`, abort handles, task ids, task-local values and a cooperative budget.
  `select!`, `join!` and `try_join!` wait on several futures in one task.
* **Networking.** `nuclei::net`: TCP, UDP and Unix sockets, names looked up off the shards,
  socket builders, stream halves, pipes and FIFOs, readiness waits and `try_` calls.
* **IO.** `nuclei::io`: reads and writes as futures, copies, buffered readers and writers,
  in-memory pipes, the standard streams, and `AsyncFd` for descriptors of one's own. A stream
  lends the driver the caller's own buffer with `read_owned` and `write_owned`.
* **Files.** `nuclei::fs`: files, whole-file calls, metadata and directories read a page at a
  time.
* **Time.** `sleep`, `timeout` and intervals on a timer wheel per shard, and a clock tests can
  pause and move by hand.
* **Sync.** `nuclei::sync` has mutexes, reader-writer locks (with upgradable reads), reentrant
  locks, semaphores, `Notify`, barriers, events, latches, once-cells, cancellation tokens, and
  mpsc, oneshot, broadcast and watch channels. They need only futures and wakers, so they work
  under any executor.
* **Processes and signals.** `nuclei::process` spawns children and waits for their exits;
  `nuclei::signal` delivers Unix signals and Windows console events as streams.
* **Stats.** `nuclei::stats()` takes a snapshot of every shard: its tasks, polls, IO in flight,
  buffers, timers and errors by kind.
* **No allocation in steady state.** Once warm, a TCP round trip, UDP datagrams, file reads and
  writes, a spawn, a `JoinSet` spawn and join and a oneshot channel make no call into the
  allocator on the thread that runs them (`tests/zero_alloc.rs` counts it).
* **Dependencies.** `libc` (on Unix), the `kovan` crates, `futures-io` and `futures-core`.

## Checked

`proofs/` holds TLA+ models of the concurrent protocols (the task state machine, the park and wake
handshake, the IO source's sides, the ring's operations, the sync primitives' queues) and Lean
proofs of the arithmetic they rest on; `proofs/README.md` says what each one checks and what it
leaves out. The sync primitives and the runtime's handshakes have loom models, and CI runs the
suite under AddressSanitizer on both Linux backends.

## Examples

```shell script
$ cd examples
$ cargo run --example tcp-server
```

## Tests

```shell script
$ cargo test                       # io_uring where available, else epoll
$ NUCLEI_BACKEND=epoll cargo test  # epoll on Linux
```

## The site

The site, [nuclei.rs](https://nuclei.rs), is built from the commit: `python3 site/build.py` writes
its pages (from `site/src/` and the Learn examples in `site/examples/`), the API reference, the
search index and the files for language models (`llms.txt`, `llms-full.txt`, the API reference and
each Learn page as Markdown) into `site/`, and `python3 site/build.py --examples` also builds, lints and runs every
example on io_uring and on epoll. The Pages workflow does both for each deploy and pull request.

#### License

<sup>
Licensed under either of <a href="LICENSE-APACHE">Apache License, Version
2.0</a> or <a href="LICENSE-MIT">MIT license</a> at your option.
</sup>

<br>

<sub>
Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
</sub>
