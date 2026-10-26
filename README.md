<h1 align="center">
    <img src="https://github.com/vertexclique/nuclei/raw/master/art/nuclei-logo-square-512.png" width="256" alt="Nuclei"/>
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

Nuclei is a proactive IO runtime: thread-per-core shards that each run their own tasks and drive
their own IO, timers and buffers. Every operation is completed for the task that asked for it; no
task waits for readiness and then makes the call itself.

It needs no configuration: the first `spawn` or `block_on` starts a runtime with one shard per
core. A `Builder` sets every setting for a runtime of one's own.

```rust
use std::net::TcpListener;
use nuclei::Handle;

#[nuclei::main]
async fn main() -> std::io::Result<()> {
    let listener = Handle::<TcpListener>::bind("127.0.0.1:7000")?;
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

## Backends

| Platform | Backend |
|---|---|
| Linux | io_uring where the kernel and the sandbox allow it, else epoll |
| macOS, FreeBSD, NetBSD, OpenBSD, DragonFly | kqueue |
| Windows | I/O completion ports |

On Linux the runtime first tries io_uring and falls back to epoll when it cannot be used: on a
kernel before 6.1 (its rings are single-issuer with deferred task work), or where a sandbox
refuses it. It probes safely, even under a seccomp filter that kills a process for calling
`io_uring_setup`. CI runs the
suite on io_uring and on epoll on Linux 6.1, 6.6 and 6.12, on kqueue on macOS, and on completion
ports on Windows. `NUCLEI_BACKEND` picks one: `auto`, `io_uring` or `epoll` on Linux, `kqueue` on
macOS and the BSDs, `iocp` on Windows.

## What it has

* **Shards.** A shard per worker thread runs its tasks and owns its driver; a task stays on the
  shard it was placed on. `block_on` works anywhere, nested in a task too, and keeps its shard's
  other tasks and IO going. `spawn_local` runs tasks that are not `Send` on the current shard;
  `spawn_blocking` runs blocking work on a pool.
* **IO.** `Handle<T>` takes over one of std's IO objects: on Unix anything with a file
  descriptor (TCP, UDP and Unix sockets, files, pipes), on Windows TCP and UDP sockets, files and
  a child's pipes. It implements the `futures-io` traits, and on streams lends the driver the
  caller's own buffer (`read_owned`, `write_owned`).
* **Time.** `sleep`, `timeout` and intervals, on a timer wheel per shard.
* **Sync.** `nuclei::sync` has mutexes, reader-writer locks (with upgradable reads), reentrant
  locks, semaphores, `Notify`, barriers, events, latches, once-cells, cancellation tokens, and
  mpsc, oneshot, broadcast and watch channels. They need only futures and wakers, so they work
  under any executor.
* **Processes and signals.** `nuclei::process` spawns children and waits for their exits;
  `nuclei::signal` delivers Unix signals and Windows console events as streams.
* **Stats.** `nuclei::stats()` takes a snapshot of every shard: its tasks, polls, IO in flight,
  buffers, timers and errors by kind.
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

##### Credits

<sub><sup>Gif is from the documentary called "Particle Fever".<sup><sub>
