#!/usr/bin/env bash
# Temporary A/B of the op-count rows on one runner: each commit named in AB_REVS is built in a
# worktree of its own and every row is measured with each binary in turn, so the CPU model is the
# same for all. Prints one table: row backend, then each revision's instructions per operation.
set -euo pipefail
cd "$(dirname "$0")/.."
REVS=${AB_REVS:?}
ROWS="
block_on 20000 60000
mpsc 10000 30000
timeout 10000 30000
tcp 2000 6000
unix_dgram 2000 6000
udp 2000 6000
unix_stream 2000 6000
tcp_split 2000 6000
tcp_asyncrw 2000 6000
tcp_many 4 12
tcp_2w 2000 6000
bulk 128 512
file_read 256 1024
spawn_join 10000 30000
spawn_blocking 2000 8000
sleep 5 15
oneshot 10000 30000
local_mpsc 10000 30000
local_oneshot 10000 30000
"
bins=()
for rev in $REVS; do
  if [[ $rev == HEAD ]]; then
    dir=$PWD
  else
    git fetch --no-tags --depth=1 origin "$rev"
    dir=/tmp/ab-$rev
    git worktree add --detach "$dir" "$rev"
  fi
  (cd "$dir" && CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 CARGO_TARGET_DIR=/tmp/ab-target-$rev cargo build --release -q -p nuclei --example op_count)
  bins+=("/tmp/ab-target-$rev/release/examples/op_count")
done
measure() {
  local out
  out=$(NUCLEI_BACKEND="$2" valgrind --tool=cachegrind --cache-sim=no --cachegrind-out-file=/dev/null "$1" "$3" "$4" 2>&1)
  echo "$(sed -n 's/^==[0-9]*== I *refs: *//p' <<<"$out" | tr -d ',') $(sed -n 's/.*ops=//p' <<<"$out")"
}
printf '%-15s %-9s' row backend; for rev in $REVS; do printf ' %12s' "${rev:0:7}"; done; echo
while read -r row n1 n2; do
  [[ -n $row ]] || continue
  for backend in io_uring epoll; do
    printf '%-15s %-9s' "$row" "$backend"
    for bin in "${bins[@]}"; do
      read -r i1 o1 < <(measure "$bin" "$backend" "$row" "$n1")
      read -r i2 o2 < <(measure "$bin" "$backend" "$row" "$n2")
      printf ' %12s' "$(awk -v a="$i1" -v b="$i2" -v c="$o1" -v d="$o2" 'BEGIN { printf "%.0f", (b - a) / (d - c) }')"
    done
    echo
  done
done <<<"$ROWS"
