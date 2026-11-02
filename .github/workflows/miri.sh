#!/usr/bin/env bash
# Runs, under Miri, the unit tests Miri can run: the code that makes none of the kernel calls the
# runtime's IO rests on (io_uring, epoll, kqueue, IOCP, timerfd, eventfd, sockets, files; threads
# and clock reads Miri carries out itself) and calls no foreign code but the C library's `memchr`,
# which Miri also carries out itself. The lock-free structures of the kovan crates underneath
# (maps, queues, channels, memory reclamation) run under Miri with them. Each test filter below is
# a module path or a test's full path, with the reason its tests can run; a test inside one of
# these modules that cannot is marked `#[cfg_attr(miri, ignore = "...")]` with its reason, and
# `skips` lists the tests left out of the shards, with theirs. MIRIFLAGS and RUSTFLAGS come from
# miri.yml.
#
# Left out whole: the shards, the runtime, the file, socket, process and signal code and the time
# tests that run on a shard (all on the kernel); the side tests biased to an owner and the source
# tests that need a heavy fence, which Miri does not have, so they return at once.
# nuclei-macros is not run: `cargo miri test` runs a proc-macro crate's tests natively, not under
# Miri, and the crate forbids unsafe code.
set -euo pipefail

filters=(
  # The task: its header's state machine, the raw task and its vtable, join handles, wakers and
  # panics, run by a schedule of the test's own on the test's threads.
  rt::task::
  # The inbox, linked through the tasks' headers, pushed from many threads.
  rt::inbox::
  # The handoff's packed words, a runner's life, and the bell that parks a thread.
  rt::handoff::slot::
  rt::handoff::life::
  rt::handoff::bell::
  # A shard's spin, its counted queue, the source tokens, and the backend choice made from a given
  # probe's answer.
  rt::shard::spin::
  rt::shard::counted::
  rt::sources::
  rt::backend::tests::
  # The blocking pool's threads and jobs, on std's threads.
  rt::blocking::
  # The block allocator: per-thread caches, blocks freed on another thread, its Arc and Rc.
  mem::
  # The caller's buffers: owned, aligned and vectored, the spare-capacity reuse, the pool's
  # classes, reservations and waiters, and a direct file call's bounce buffer (no file).
  buf::spare::
  buf::tests::aligned::
  buf::tests::owned::
  buf::tests::vectored::
  buf::tests::a_waiter_is_woken_at_once_when_nothing_is_out_to_come_back
  buf::tests::a_reservation_is_charged_and_the_pool_has_the_rest_of_the_cap
  buf::tests::a_request_takes_the_smallest_class_that_holds_it
  buf::tests::a_request_takes_the_largest_class_the_cap_allows_but_a_whole_one_does_not_shrink
  buf::tests::a_reservation_must_leave_room_for_one_smallest_buffer
  buf::tests::a_fill_larger_than_the_buffer_takes_what_fits
  buf::tests::a_waiter_on_a_pool_holding_only_its_reservation_is_woken_at_once
  buf::tests::appended_bytes_follow_the_unread_ones_moving_them_to_the_front
  buf::tests::buffers_outliving_their_pool_are_freed_by_its_home
  fs::tests::direct::a_direct_call_goes_through_memory_placed_as_the_callers
  # A read buffer over memory that may be uninitialized, and the read-ahead's pieces.
  io::util::read_buf::
  io::source::read::ahead::
  # The source's sides shared by any thread, their waker sets and the steps after a call.
  io::side::tests::
  io::side::after::
  # Encodings and plain logic: readiness, a Unix socket address, a cleared tick, the errors of a
  # lost accept, error kinds, the configuration.
  io::ready::
  io::sys::unix::unix_addr::
  io::source::tests::caller_clear::clears_left_together_keep_the_newest
  io::source::tests::lost_accept::
  rt::counters::tests::a_kind_outside_the_table_counts_as_other
  config::tests::
  process::tests::into_std_hands_the_command_over
  signal::tests::unix::signals_no_handler_can_take_are_refused
  # io_uring's user-space side, with no ring: the setup's flags and fallbacks against a stand-in
  # for the kernel, the operation slab, the file table's slots, completion and recvmsg decoding,
  # the probe's reasons.
  syscore::linux::iouring::ring::tests_setup::
  syscore::linux::iouring::ops::
  syscore::linux::iouring::files::
  syscore::linux::iouring::ring::tests::enter_results_map_to_the_caller_facing_outcome
  syscore::linux::iouring::ring::tests_net::buffer_id_decodes_the_completion_flags
  syscore::linux::iouring::ring::tests_net::recvmsg_out_splits_name_control_and_payload
  syscore::linux::iouring::ring::tests_net::recvmsg_out_rejects_short_buffers_and_clamps_claims
  syscore::linux::probe::tests::child_reports_map_to_reasons
  syscore::linux::probe::tests::errors_map_to_reasons
  syscore::linux::probe::tests::reasons_read_in_plain_words
  rt::shard::ring::tests_zc::released_is_send_and_unpin
  # Time: the timer wheel, the clock and its pauses, units and deadlines.
  time::wheel::
  time::clock::
  time::units::
  time::nanos_tests::
  # Every sync primitive, polled by hand or on the test's threads: the locks, semaphores, Notify,
  # the once cells, barrier, event and latch with their waiter nodes and lists, the channels and
  # their queues, cancellation, the waker cell and the internals.
  sync::
)

skips=(
  # Each takes longer under Miri than the whole job may: one finished in 13 minutes, the other two
  # had not in 15.
  sync::cancellation::tests::dropping_the_last_handle_of_a_deep_chain_frees_every_node
  sync::cancellation::tests::the_registry_is_empty_after_concurrent_trees_are_cancelled_and_dropped
  rt::shard::counted::tests_lazy::pushers_that_race_to_make_the_queue_share_one_and_lose_nothing
  # Runs on its own, below.
  time::clock::tests::reads_never_go_backwards_across_pauses_on_many_threads
)

# The clock test that runs on its own, with Miri's weak memory emulation off. A clock that was
# never paused hands out the system's time and keeps no trace of it; that such a reading came
# before a pause that marks the clock and then reads its own time rests on the hardware keeping a
# clock reading ahead of the next load (`Clock::now_from` in src/time/clock.rs says how), which
# the language's memory model does not promise. Under the emulation a reader can still find the
# clock unmarked after the pause read its time, and then read later than the paused clock (22 of
# 48 seeds, once another clock test had set the origin). With the emulation off, Miri still checks
# every access of the test for data races and undefined behaviour.
alone=time::clock::tests::reads_never_go_backwards_across_pauses_on_many_threads

# The tests are picked here, from the harness's own list, and handed to it by exact name: the
# harness's substring filters, interpreted by Miri over every test name, take longer than the
# tests (these were still being matched after 13 minutes; the exact names take 12 seconds).
names="$(cargo miri test -p nuclei --lib -- --list --format terse | sed -n 's/: test$//p')"

# A filter or a skip that names no test (a module or a test renamed or moved) fails the job
# rather than leave tests out, or let them in, unseen.
missing=()
for filter in "${filters[@]}"; do
  grep -qF -- "$filter" <<<"$names" || missing+=("$filter")
done
for skip in "${skips[@]}"; do
  grep -qxF -- "$skip" <<<"$names" || missing+=("$skip")
done
if ((${#missing[@]})); then
  printf 'No test matches %s in .github/workflows/miri.sh.\n' "${missing[@]}" >&2
  exit 1
fi

mapfile -t tests < <(
  grep -F -f <(printf '%s\n' "${filters[@]}") <<<"$names" |
    grep -vxF -f <(printf '%s\n' "${skips[@]}")
)

# Miri runs on one thread, so the tests go in shards, one Miri process each, all at once: four,
# the runner's cores, and the clock test's own process beside them. Each process's output is
# shown whole once all are done, and one that fails fails the job. Proptests on the default
# configuration take 16 cases each; the timer wheel's model sets its own count. One test thread
# per process: a test reading the clock sees only its own steps between two reads, not those of
# the tests Miri would interleave with it.
shards=4
# A shard given no name would run every test.
if ((${#tests[@]} < shards)); then
  echo "Only ${#tests[@]} tests are selected; update .github/workflows/miri.sh." >&2
  exit 1
fi
echo "Running ${#tests[@]} tests under Miri in $shards shards, and $alone on its own."
logs="$(mktemp -d)"
trap 'rm -rf "$logs"' EXIT
export PROPTEST_CASES="${PROPTEST_CASES:-16}"
pids=()
labels=()
for ((s = 0; s < shards; s++)); do
  shard=()
  for ((i = s; i < ${#tests[@]}; i += shards)); do
    shard+=("${tests[i]}")
  done
  cargo miri test -p nuclei --lib -- --exact --test-threads=1 "${shard[@]}" \
    >"$logs/${#pids[@]}" 2>&1 &
  pids+=("$!")
  labels+=("shard $((s + 1)) of $shards")
done
MIRIFLAGS="${MIRIFLAGS:-} -Zmiri-disable-weak-memory-emulation" \
  cargo miri test -p nuclei --lib -- --exact --test-threads=1 "$alone" >"$logs/${#pids[@]}" 2>&1 &
pids+=("$!")
labels+=("$alone, weak memory emulation off")

failed=0
for i in "${!pids[@]}"; do
  wait "${pids[i]}" || failed=1
  echo "=== Miri: ${labels[i]}"
  cat "$logs/$i"
done
exit "$failed"
