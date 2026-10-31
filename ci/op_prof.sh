#!/usr/bin/env bash
# Temporary: per-function instruction counts of rows on two revisions, on one runner, and their
# difference (cg_diff), to find where a row's cost moved.
set -euo pipefail
cd "$(dirname "$0")/.."
BASE=${PROF_BASE:?}
git fetch --no-tags --depth=1 origin "$BASE"
git worktree add --detach /tmp/prof-base "$BASE"
(cd /tmp/prof-base && CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_TARGET_DIR=/tmp/prof-base-target cargo build --release -q -p nuclei --example op_count)
CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_TARGET_DIR=/tmp/prof-head-target cargo build --release -q -p nuclei --example op_count
for spec in "file_read io_uring 1024" "file_read epoll 1024" "sleep epoll 15" "tcp_split epoll 6000"; do
  read -r row backend n <<<"$spec"
  for side in base head; do
    NUCLEI_BACKEND=$backend valgrind --tool=cachegrind --cache-sim=no \
      --cachegrind-out-file=/tmp/cg-$side-$row-$backend.out /tmp/prof-$side-target/release/examples/op_count "$row" "$n" 2>&1 | grep -E 'I *refs|ops=' || true
  done
  echo "=== $row $backend: head minus base, by function"
  cg_annotate --diff --mod-funcname='s/::h[0-9a-f]{16}$//' /tmp/cg-base-$row-$backend.out /tmp/cg-head-$row-$backend.out 2>&1 \
    | grep -v '^\s*$' | head -120 || true
done
