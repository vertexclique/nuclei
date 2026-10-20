# Reference measurements

Hand-written C over raw syscalls (no liburing), used as the yardstick for the Rust ring layer.

## `io_uring_nop.c`

Measures a NOP round trip through an io_uring ring: batches of 1 and of 32, 200000 ops each, 7 runs,
median nanoseconds per op. It sets the ring up the way `src/syscore/linux/iouring/ring` does
(`SINGLE_ISSUER | DEFER_TASKRUN | SUBMIT_ALL | CLAMP | CQSIZE`, plus `NO_SQARRAY` with a retry
without it on `EINVAL`; 256 submission entries, 512 completion entries) and prints one line per
batch size in the same format as the Rust test.

Build and run:

```sh
gcc -O2 -o /tmp/io_uring_nop benches/reference/io_uring_nop.c
taskset -c 2 /tmp/io_uring_nop
```

The Rust side, on the same machine and the same CPU, back to back:

```sh
cargo test -p nuclei --release --lib --no-run
taskset -c 2 cargo test -p nuclei --release --lib -- --ignored --nocapture bench_nop_round_trip
```

Both print `nop batch=<n> ops=200000 runs=7 median_ns_per_op=<x>`. The ring layer's acceptance bound is the
Rust median more than 10% above the C median for either batch size. Needs Linux 6.1 or newer; both
programs retry without `NO_SQARRAY` on kernels before 6.6.

## `probe-matrix.sh`

Runs the io_uring availability probe (`src/syscore/linux/probe.rs`) in each sandbox and checks what
it reports. It builds the lib test binary once for `x86_64-unknown-linux-musl` (static, so the same
file runs on the host, under systemd and in a container) and runs the ignored test
`probe_report` in five environments: the plain host (no seccomp filter, in-process probe),
a user systemd unit that kills io_uring syscalls, the same with `SystemCallErrorNumber=EPERM`,
Docker's default seccomp profile, and Docker with `seccomp=unconfined`. Needs `jq`, `column`,
`systemd-run --user` and a running Docker daemon with the `alpine:3` image (override with
`PROBE_IMAGE`).

```sh
bash benches/reference/probe-matrix.sh
```

It prints a table of environment, expected, actual and verdict and exits non-zero on any mismatch,
including a filter that did not apply (the process reports `seccomp=0` where `2` is expected) and a
case that could not run. The systemd cases name the three syscalls instead of a group because
systemd 262 has no `@io-uring` group and silently applies no filter for an unknown one. The kill
case ends the probe's child with `SIGSYS`; the child lowers its own core limit first, so no core
dump entry is left. To check that after a change, compare `coredumpctl list --no-pager | grep -c probe`
before and after a run: the count must not grow.
