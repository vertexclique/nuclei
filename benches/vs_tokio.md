# nuclei against tokio

Row-by-row comparison of nuclei 0.5.2 (branch `fix/io-drive-cadence`) with tokio 1.54.1, from `benches/vs_tokio.rs`. Every row of `features.rs` and `run_loop.rs` is a criterion group with a `nuclei` and a `tokio` function running the identical workload. Ratio is nuclei time / tokio time: below 1 means nuclei is faster, above 1 means tokio is faster.

## Method

- Host: Intel Xeon W-2295 @ 3.00 GHz (`lscpu`), Linux 7.3.0-4-cachyos (`uname -r`). The host is shared; load average sampled before each of the 8 criterion invocations ranged 7.95 to 22.33 (1 minute), typically 11 to 15.
- Every run under `taskset -c 4-7`, both sides.
- Runtime shapes, matched, no pinning on either side: one thread rows use nuclei `worker_threads(1).pin_workers(false).handoff_after(None)` against tokio `new_current_thread().enable_all()`; `oneshot_pairs` and `mpsc_across_workers` use nuclei `worker_threads(2)` against tokio `new_multi_thread().worker_threads(2).enable_all()`.
- Same message types, counts, sizes and connection counts on both sides. Socket options are defaults on both.
- `file_read_16MiB` is the real-world difference, not an engine-for-engine one: `tokio::fs` goes through tokio's blocking thread pool, nuclei reads through its own file IO.
- Timing: each profile was run as one ABBA sequence of whole-bench invocations (nuclei, tokio, tokio, nuclei), each filtered to one side. Tables give the median of the two runs per side (criterion's central estimate each) and both runs. Two runs per side is a small sample: a gap between a side's own two runs larger than the ratio's distance from 1 means the row is not resolved (see flagged rows).
- Instructions and cycles: `benches/vs_tokio_count.rs` (an example) runs a fixed count on one runtime; `perf stat -x, -e instructions:u,cycles:u`, 3 runs per side, median reported. User-space only, so kernel work (syscalls, io_uring submission) is not in these numbers. The counts include process start and runtime build, a fixed cost that differs per runtime, so the cheap rows carry some overhead that is not per iteration.

Commands:

```sh
# default release profile
cargo bench --bench vs_tokio --no-run
taskset -c 4-7 <bench binary> --bench --noplot '/nuclei$'   # then tokio, tokio, nuclei
cargo build --release --example vs_tokio_count
taskset -c 4-7 perf stat -x, -e instructions:u,cycles:u target/release/examples/vs_tokio_count <nuclei|tokio> <block_on|mpsc|timeout|tcp>

# defradb profile
export CARGO_TARGET_DIR=$PWD/target-fat CARGO_PROFILE_BENCH_LTO=fat CARGO_PROFILE_BENCH_CODEGEN_UNITS=1 CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1
# same four commands
```

## Wall time

### Default release profile
| row | nuclei | tokio | ratio | nuclei, 2 runs | tokio, 2 runs |
|---|---|---|---|---|---|
| block_on_ready | 153 ns | 175 ns | 0.87 | 147 ns, 160 ns | 175 ns, 175 ns |
| spawn_join | 532 µs | 654 µs | 0.81 | 530 µs, 534 µs | 653 µs, 656 µs |
| yield | 13.8 ms | 16.5 ms | 0.84 | 13.8 ms, 13.8 ms | 16.8 ms, 16.2 ms |
| mpsc_ping_pong | 701 µs | 674 µs | 1.04 | 628 µs, 773 µs | 687 µs, 661 µs |
| oneshot_pairs | 1.91 ms | 2.65 ms | 0.72 | 1.77 ms, 2.05 ms | 2.65 ms, 2.66 ms |
| mpsc_across_workers | 873 µs | 3.55 ms | 0.25 | 876 µs, 870 µs | 3.54 ms, 3.56 ms |
| arm_and_fire_1000_sleeps | 10.1 ms | 10.8 ms | 0.94 | 10 ms, 10.2 ms | 10.8 ms, 10.8 ms |
| timeout_hit | 32.5 µs | 43.6 µs | 0.74 | 30.4 µs, 34.6 µs | 44.3 µs, 43 µs |
| timeout_miss_1ms | 40 ms | 41.3 ms | 0.97 | 40 ms, 40 ms | 41.3 ms, 41.4 ms |
| tcp_ping_pong_one_connection | 14.8 ms | 13.1 ms | 1.13 | 14.6 ms, 15 ms | 12.8 ms, 13.5 ms |
| tcp_request_reply_256_connections | 41 ms | 36.2 ms | 1.13 | 38.1 ms, 44 ms | 37.5 ms, 34.9 ms |
| tcp_bulk_transfer_64MiB | 18.4 ms | 14.8 ms | 1.24 | 18.2 ms, 18.6 ms | 15.6 ms, 14.1 ms |
| udp_round_trips_128B | 16.1 ms | 14.2 ms | 1.14 | 16.1 ms, 16.2 ms | 12.9 ms, 15.5 ms |
| file_read_16MiB | 2.35 ms | 6.82 ms | 0.34 | 2.52 ms, 2.18 ms | 7.04 ms, 6.59 ms |
### Defradb profile (fat LTO, 1 codegen unit)
| row | nuclei | tokio | ratio | nuclei, 2 runs | tokio, 2 runs |
|---|---|---|---|---|---|
| block_on_ready | 117 ns | 133 ns | 0.88 | 115 ns, 119 ns | 133 ns, 133 ns |
| spawn_join | 528 µs | 589 µs | 0.90 | 525 µs, 532 µs | 588 µs, 589 µs |
| yield | 11.7 ms | 15.6 ms | 0.75 | 11.7 ms, 11.7 ms | 15.7 ms, 15.5 ms |
| mpsc_ping_pong | 554 µs | 595 µs | 0.93 | 562 µs, 546 µs | 594 µs, 596 µs |
| oneshot_pairs | 1.57 ms | 2.58 ms | 0.61 | 1.54 ms, 1.6 ms | 2.52 ms, 2.65 ms |
| mpsc_across_workers | 851 µs | 3.52 ms | 0.24 | 867 µs, 836 µs | 3.49 ms, 3.55 ms |
| arm_and_fire_1000_sleeps | 10 ms | 10.7 ms | 0.93 | 10.1 ms, 10 ms | 10.7 ms, 10.7 ms |
| timeout_hit | 35.5 µs | 36.5 µs | 0.97 | 29.5 µs, 41.5 µs | 33 µs, 40.1 µs |
| timeout_miss_1ms | 41.7 ms | 42 ms | 0.99 | 40.1 ms, 43.2 ms | 41.3 ms, 42.8 ms |
| tcp_ping_pong_one_connection | 14.7 ms | 12.9 ms | 1.14 | 14.8 ms, 14.7 ms | 12.3 ms, 13.4 ms |
| tcp_request_reply_256_connections | 38.9 ms | 49.2 ms | 0.79 | 38.1 ms, 39.6 ms | 34 ms, 64.4 ms |
| tcp_bulk_transfer_64MiB | 17.8 ms | 13.9 ms | 1.28 | 17.8 ms, 17.8 ms | 13.5 ms, 14.4 ms |
| udp_round_trips_128B | 15.7 ms | 12.4 ms | 1.27 | 15.9 ms, 15.6 ms | 12.4 ms, 12.3 ms |
| file_read_16MiB | 2.28 ms | 6.58 ms | 0.35 | 2.42 ms, 2.13 ms | 6.4 ms, 6.75 ms |

## Instructions and cycles per iteration

Iterations: block_on 200000 `block_on` calls; mpsc 400000 messages (200000 round trips); timeout 200000 ready timeouts; tcp 50000 64-byte round trips on one connection. Cycle spread is min-max across the 3 runs, nuclei / tokio.

### Default release profile

| row | nuclei instr | tokio instr | ratio | nuclei cycles | tokio cycles | ratio | cycle spread |
|---|---|---|---|---|---|---|---|
| block_on | 1135 | 899 | 1.26 | 588 | 661 | 0.89 | 586-600 / 659-662 |
| mpsc | 843 | 1039 | 0.81 | 559 | 627 | 0.89 | 556-560 / 624-633 |
| timeout | 223 | 245 | 0.91 | 116 | 166 | 0.70 | 116-117 / 166-166 |
| tcp | 12771 | 4857 | 2.63 | 7883 | 4218 | 1.87 | 7880-7898 / 4185-4269 |

### Defradb profile

| row | nuclei instr | tokio instr | ratio | nuclei cycles | tokio cycles | ratio | cycle spread |
|---|---|---|---|---|---|---|---|
| block_on | 762 | 571 | 1.34 | 489 | 505 | 0.97 | 471-493 / 505-522 |
| mpsc | 685 | 883 | 0.78 | 499 | 563 | 0.89 | 492-503 / 553-567 |
| timeout | 200 | 184 | 1.09 | 140 | 135 | 1.03 | 139-140 / 133-147 |
| tcp | 11160 | 3853 | 2.90 | 6967 | 3287 | 2.12 | 6842-7129 / 3275-3288 |

## Reading it

- Tokio wins every network row on wall time under the default profile and on `tcp_ping_pong`, `tcp_bulk_transfer_64MiB` and `udp_round_trips_128B` under both: nuclei takes 13 to 14 percent longer on ping-pong, 24 to 28 percent longer on bulk, 14 to 27 percent longer on UDP. User-space instructions per TCP round trip are 2.6x (default) and 2.9x (defradb) tokio's, so the gap is nuclei's own per-operation work, not noise.
- `tcp_request_reply_256_connections`: nuclei takes 13 percent longer under the default profile; under the defradb profile nuclei looks 21 percent faster, but one tokio run was 64.4 ms against 34 ms in the other, so that row is not resolved there.
- `mpsc_ping_pong`: default profile 1.04 with nuclei's own two runs spanning 628 to 773 µs, not resolved; defradb profile nuclei 7 percent faster.
- `block_on_ready`: nuclei is faster on wall time (0.87, 0.88) though it executes more user instructions per call (1.26x, 1.34x). `timeout_hit` under the defradb profile (0.97) and `timeout_miss_1ms` (0.97, 0.99, dominated by the 1 ms timer wait) are ties within the spread.
- Nuclei wins by a clear margin on `file_read_16MiB` (0.34 and 0.35, with tokio on its blocking pool), `mpsc_across_workers` (0.25 and 0.24), `oneshot_pairs` (0.72 and 0.61), `yield` (0.84 and 0.75), `spawn_join` (0.81 and 0.90), and `timeout_hit` under the default profile (0.74).
- Not measured: memory use, latency percentiles, multi-worker network rows, more than 256 connections, other hosts.

Raw per-run criterion output is not committed; the tables above are the extraction.
