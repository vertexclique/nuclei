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
taskset -c 2 cargo test -p nuclei --release --lib -- --ignored --nocapture phase0_nop_round_trip
```

Both print `nop batch=<n> ops=200000 runs=7 median_ns_per_op=<x>`. Phase 0a's kill criterion is the
Rust median more than 10% above the C median for either batch size. Needs Linux 6.1 or newer; both
programs retry without `NO_SQARRAY` on kernels before 6.6.
