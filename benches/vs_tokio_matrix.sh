#!/usr/bin/env bash
# User and kernel instructions, cycles and locked loads per operation of every vs_tokio_count
# mode, nuclei (default backend and epoll) against tokio and monoio (io_uring and legacy), under
# the default release profile and under fat LTO. Ends with each nuclei side over its best rival.
# Usage: benches/vs_tokio_matrix.sh [mode ...]
# Knobs: SIDES RUNS CPUS EVENTS TARGET TARGET_FAT OUT LOCK
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
scratch=/tmp/claude-1000/-home-vcq-projects-defradb-rs/4fac2f63-a8e9-41cf-a60c-4cd1dddb63eb/scratchpad
lock=${LOCK:-$scratch/bench.lock}
if [ -z "${VS_MATRIX_LOCKED:-}" ]; then
    mkdir -p "$(dirname "$lock")"
    echo "waiting for $lock" >&2
    VS_MATRIX_LOCKED=1 exec flock "$lock" "$0" "$@"
fi

modes=("$@")
if [ ${#modes[@]} -eq 0 ]; then
    modes=(block_on mpsc timeout tcp unix_dgram udp unix_stream tcp_split tcp_asyncrw
        tcp_many tcp_2w bulk file_read spawn_join spawn_blocking sleep oneshot)
fi
read -r -a sides <<<"${SIDES:-tokio nuclei nuclei-epoll monoio monoio-legacy}"
runs=${RUNS:-3}
cpus=${CPUS:-4-7}
events=${EVENTS:-instructions:u,instructions:k,cycles:u,cycles:k,mem_inst_retired.lock_loads:u}
target=${TARGET:-$root/target}
target_fat=${TARGET_FAT:-$root/target-fat}
results=${OUT:-$target/vs_tokio_matrix.tsv}
IFS=, read -r -a names <<<"$events"
out=$(mktemp)
trap 'rm -f "$out" "$out.txt"' EXIT

echo "building default profile" >&2
CARGO_TARGET_DIR="$target" nice -n 10 cargo build --release --example vs_tokio_count >&2
echo "building fat LTO profile" >&2
CARGO_TARGET_DIR="$target_fat" CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 \
    nice -n 10 cargo build --release --example vs_tokio_count >&2

median() { sort -n | awk '{ v[NR] = $1 } END { print v[int((NR + 1) / 2)] }'; }

# measure <binary> <side> <mode>: prints the iterations, then the median of each event in order.
measure() {
    local bin=$1 side=$2 mode=$3 i e iters='' v
    local -a env=() samples=()
    case $side in
        nuclei-epoll) env=(NUCLEI_BACKEND=epoll) side=nuclei ;;
        nuclei) env=(-u NUCLEI_BACKEND) ;;
    esac
    for ((i = 0; i < runs; i++)); do
        env "${env[@]}" nice -n 10 taskset -c "$cpus" perf stat -x, -o "$out" -e "$events" \
            "$bin" "$side" "$mode" >"$out.txt"
        iters=$(sed -n 's/.*iterations=\([0-9]*\).*/\1/p' "$out.txt")
        [ -n "$iters" ] || { echo "no iteration count from $bin $side $mode" >&2; exit 1; }
        for e in "${names[@]}"; do
            v=$(awk -F, -v e="$e" '$3 == e { print $1 }' "$out")
            case $v in '' | *[!0-9]*) echo "unreadable counter $e='$v' for $side $mode (see perf stat)" >&2; exit 1 ;; esac
            awk -F, -v e="$e" '$3 == e && $5 + 0 < 99.5 { exit 1 }' "$out" ||
                echo "warning: $e was multiplexed for $side $mode" >&2
            samples+=("$e $v")
        done
    done
    printf '%s' "$iters"
    for e in "${names[@]}"; do
        printf ' %s' "$(printf '%s\n' "${samples[@]}" | awk -v e="$e" '$1 == e { print $2 }' | median)"
    done
    echo
}

printf 'profile\tmode\truntime' >"$results"
printf '\t%s/op' "${names[@]}" >>"$results"
echo >>"$results"
for profile in default fat; do
    if [ $profile = default ]; then dir=$target; else dir=$target_fat; fi
    bin=$dir/release/examples/vs_tokio_count
    for mode in "${modes[@]}"; do
        [ "$mode" = file_read ] && "$bin" nuclei file_read >/dev/null
        for side in "${sides[@]}"; do
            read -r -a m <<<"$(measure "$bin" "$side" "$mode")"
            awk -v p="$profile" -v mode="$mode" -v s="$side" -v it="${m[0]}" -v vals="${m[*]:1}" 'BEGIN {
                n = split(vals, v, " "); printf "%s\t%s\t%s", p, mode, s
                for (i = 1; i <= n; i++) printf "\t%.1f", v[i] / it
                printf "\n" }' >>"$results"
        done
    done
done

column -t -s $'\t' "$results"
echo
echo "each nuclei side over its best rival (below 1.00: nuclei is ahead; the rival named per row)"
awk -F'\t' '
    NR == 1 { for (i = 4; i <= NF; i++) head[i] = $i; cols = NF; next }
    { key = $1 SUBSEP $2; val[key, $3] = 1; for (i = 4; i <= NF; i++) x[key, $3, i] = $i; order[++n] = key }
    END {
        printf "%-8s %-15s %-13s", "profile", "mode", "nuclei side"
        for (i = 4; i <= cols; i++) printf " %26s", head[i]
        printf "\n"
        for (k = 1; k <= n; k++) {
            key = order[k]; if (key in seen) continue; seen[key] = 1
            split(key, pm, SUBSEP)
            for (s = 1; s <= 2; s++) {
                side = s == 1 ? "nuclei" : "nuclei-epoll"; mon = s == 1 ? "monoio" : "monoio-legacy"
                if (!((key, side) in val)) continue
                printf "%-8s %-15s %-13s", pm[1], pm[2], side
                for (i = 4; i <= cols; i++) {
                    best = ""; who = "-"
                    if ((key, "tokio") in val) { best = x[key, "tokio", i]; who = "tokio" }
                    if ((key, mon) in val && (best == "" || x[key, mon, i] + 0 < best + 0)) { best = x[key, mon, i]; who = mon }
                    if (best == "") printf " %26s", "-"
                    else if (best + 0 == 0) printf " %26s", (x[key, side, i] + 0 == 0 ? "0 = 0 " who : "inf vs " who)
                    else printf " %26s", sprintf("%.2f vs %s", x[key, side, i] / best, who)
                }
                printf "\n"
            }
        }
    }' "$results"
