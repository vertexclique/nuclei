#!/usr/bin/env bash
# Instructions per operation for each row of ci/op_count on each backend, counted under
# cachegrind, against the budgets in ci/op_budget.txt. A row costs
# (I(N2) - I(N1)) / (ops(N2) - ops(N1)) at two iteration counts, so the runtime's start and
# every other fixed cost cancel. Each measurement is run RUNS times; the median is judged and
# the min and max are printed. OP_REPORT=1 reports everything and fails nothing.
set -euo pipefail

cd "$(dirname "$0")/.."
BUDGET=ci/op_budget.txt
RUNS=${RUNS:-3}
BACKENDS=${BACKENDS:-"io_uring epoll"}
ONLY=${ONLY:-}
BIN="${CARGO_TARGET_DIR:-target}/release/examples/op_count"

# row N1 N2: the iteration counts, sized so the whole job takes a few minutes under valgrind.
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

cargo build --release -p nuclei --example op_count

# measure BACKEND ROW ITERATIONS: prints "instructions operations".
measure() {
  local out refs ops
  out=$(NUCLEI_BACKEND="$1" valgrind --tool=cachegrind --cache-sim=no \
    --cachegrind-out-file=/dev/null "$BIN" "$2" "$3" 2>&1) || {
    echo "$2 on $1: valgrind or the program failed:" >&2
    echo "$out" | tail -n 15 >&2
    return 1
  }
  refs=$(sed -n 's/^==[0-9]*== I *refs: *//p' <<<"$out" | tr -d ',')
  ops=$(sed -n 's/.*ops=//p' <<<"$out")
  [[ $out == *"backend=$(sed 's/io_uring/IoUring/;s/epoll/Epoll/' <<<"$1") "* ]] || {
    echo "$2 on $1: the program ran on another backend:" >&2
    echo "$out" | tail -n 15 >&2
    return 1
  }
  [[ -n $refs && -n $ops ]] || {
    echo "$2 on $1: no instruction count in the output:" >&2
    echo "$out" | tail -n 15 >&2
    return 1
  }
  echo "$refs $ops"
}

budget_of() { awk -v r="$1" -v b="$2" '$1 == r && $2 == b { print $3, $4 }' "$BUDGET" 2>/dev/null; }

failed=0
report=${OP_REPORT:-0}
printf '%-15s %-9s %10s %10s %10s %10s\n' row backend median min max spread
while read -r row n1 n2; do
  [[ -n $row && ( -z $ONLY || " $ONLY " == *" $row "* ) ]] || continue
  for backend in $BACKENDS; do
    vals=()
    ok=1
    for ((i = 0; i < RUNS; i++)); do
      read -r i1 o1 < <(measure "$backend" "$row" "$n1") || { ok=0; break; }
      read -r i2 o2 < <(measure "$backend" "$row" "$n2") || { ok=0; break; }
      vals+=("$(awk -v a="$i1" -v b="$i2" -v c="$o1" -v d="$o2" 'BEGIN { printf "%.0f", (b - a) / (d - c) }')")
    done
    if ((!ok)); then
      echo "$row on $backend: could not be measured"
      failed=1
      continue
    fi
    read -r min med max < <(printf '%s\n' "${vals[@]}" | sort -n | awk '{ v[NR] = $1 } END { print v[1], v[int((NR + 1) / 2)], v[NR] }')
    spread=$(awk -v lo="$min" -v hi="$max" -v m="$med" 'BEGIN { printf "%.2f%%", (m > 0) ? (hi - lo) * 100 / m : 0 }')
    printf '%-15s %-9s %10s %10s %10s %10s\n' "$row" "$backend" "$med" "$min" "$max" "$spread"
    read -r budget tol < <(budget_of "$row" "$backend") || true
    if [[ -z ${budget:-} ]]; then
      echo "$row on $backend: $med instructions per operation, no budget"
      continue
    fi
    verdict=$(awk -v m="$med" -v b="$budget" -v t="$tol" 'BEGIN {
      if (m > b * (1 + t / 100)) print "over"
      else if (m < b * (1 - 2 * t / 100)) print "under"
      else print "ok" }')
    case $verdict in
      over)
        echo "$row on $backend: $med instructions per operation, budget $budget (+$tol%): this change made it costlier"
        failed=1
        ;;
      under)
        echo "$row on $backend: $med instructions per operation, budget $budget (+$tol%): the budget can be lowered"
        ;;
    esac
    unset budget tol
  done
done <<<"$ROWS"

if ((failed)) && [[ $report != 1 ]]; then
  exit 1
fi
