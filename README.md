<h1 align="center">
    <img src="https://github.com/vertexclique/nuclei/raw/master/img/nuclei.gif"/>
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
    <!-- Discord -->
    <a href="https://discord.gg/DqRqtRT">
    <img src="https://img.shields.io/discord/628383521450360842.svg?logo=discord" />
    </a>
</div>

Nuclei is a [proactor-based](http://citeseerx.ist.psu.edu/viewdoc/summary?doi=10.1.1.49.9183) IO system which is runtime agnostic and can work with any runtime.
The proactor system's design principles are matching [Boost Asio](https://www.boost.org/doc/libs/1_47_0/doc/html/boost_asio/overview/core/async.html).
Nuclei is not using a conventional reactor approach. It is completely asynchronous, and it's wrapping poll based IO in a proactive fashion.

Nuclei uses [epoll](https://en.wikipedia.org/wiki/Epoll) on Linux as the primary evented IO backend, secondarily (given system support) you can use
[io_uring](https://kernel.dk/io_uring.pdf). On MacOS, Nuclei is using [kqueue](https://en.wikipedia.org/wiki/Kqueue).
On Windows, the [IOCP](https://en.wikipedia.org/wiki/Input/output_completion_port) backend [will be used](https://github.com/vertexclique/nuclei/pull/3).   

The current io_uring implementation needs a modern Linux kernel (5.6+).

## Features

* Async TCP, UDP, Unix domain sockets and files...
* The proactor system doesn't block
* Scatter/Gather operations
* Minimal allocation
* More expressive than any other runtime
* Completely asynchronous I/O system with lock free programming

## Examples
Please head to `examples` directory to run the examples:
```shell script
$ cd examples
$ cargo run --example fread-vect
```

## Tests

```shell script
$ cargo test --no-default-features --features=iouring # For iouring
$ cargo test # For others
```

## Configurations

### Evented IO backend

When the `iouring` feature gate is not enabled, the platforms evented backend is used. For example, on Linux, `epoll` would be used.

### Executor

Executor is by default set to Bastion's executor. If you want to use
different executor, you can use one of the available runtimes with one of these features: 
`bastion`, `asyncstd`, `tokio`, `smol`.

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
