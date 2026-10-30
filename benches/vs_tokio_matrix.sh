#!/usr/bin/env bash
# Instructions and cycles per operation of every vs_tokio_count mode, nuclei (default
# backend and epoll) against tokio, under the default release profile and under fat LTO.
# Usage: benches/vs_tokio_matrix.sh [mode ...]
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

modes=("$@")
if [ ${#modes[@]} -eq 0 ]; then
    modes=(block_on mpsc timeout tcp unix_dgram udp unix_stream tcp_split tcp_asyncrw
        tcp_many tcp_2w bulk file_read spawn_join spawn_blocking sleep oneshot)
fi
runs=3
cpus=4-7
out=$(mktemp)
trap 'rm -f "$out"' EXIT

echo "building default profile" >&2
CARGO_TARGET_DIR="$root/target" cargo build --release --example vs_tokio_count >&2
echo "building fat LTO profile" >&2
CARGO_TARGET_DIR="$root/target-fat" CARGO_PROFILE_RELEASE_LTO=fat \
    CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 cargo build --release --example vs_tokio_count >&2

median() { sort -n | awk '{ v[NR] = $1 } END { print v[int((NR + 1) / 2)] }'; }

# measure <binary> <runtime> <mode> <backend>: prints "iterations instr ucycles kcycles" medians.
measure() {
    local bin=$1 rt=$2 mode=$3 backend=$4 i iters=0
    local -a ins=() ucy=() kcy=()
    for ((i = 0; i < runs; i++)); do
        if [ -n "$backend" ]; then
            NUCLEI_BACKEND=$backend taskset -c "$cpus" perf stat -x, -o "$out" \
                -e instructions:u,cycles:u,cycles:k "$bin" "$rt" "$mode" >"$out.txt"
        else
            taskset -c "$cpus" perf stat -x, -o "$out" \
                -e instructions:u,cycles:u,cycles:k "$bin" "$rt" "$mode" >"$out.txt"
        fi
        iters=$(sed -n 's/.*iterations=\([0-9]*\).*/\1/p' "$out.txt")
        [ -n "$iters" ] || { echo "no iteration count from $bin $rt $mode" >&2; exit 1; }
        ins+=("$(awk -F, '$3 == "instructions:u" { print $1 }' "$out")")
        ucy+=("$(awk -F, '$3 == "cycles:u" { print $1 }' "$out")")
        kcy+=("$(awk -F, '$3 == "cycles:k" { print $1 }' "$out")")
    done
    rm -f "$out.txt"
    local v
    for v in "${ins[@]}" "${ucy[@]}" "${kcy[@]}"; do
        case $v in '' | *[!0-9]*) echo "unreadable counter '$v' for $rt $mode (see perf stat)" >&2; exit 1 ;; esac
    done
    echo "$iters $(printf '%s\n' "${ins[@]}" | median) $(printf '%s\n' "${ucy[@]}" | median) $(printf '%s\n' "${kcy[@]}" | median)"
}

printf '%-8s %-15s %-13s %10s %10s %10s %9s %9s %9s\n' \
    profile mode runtime instr/op ucyc/op kcyc/op r_instr r_ucyc r_kcyc
for profile in default fat; do
    if [ $profile = default ]; then bin=$root/target/release/examples/vs_tokio_count; else bin=$root/target-fat/release/examples/vs_tokio_count; fi
    for mode in "${modes[@]}"; do
        [ "$mode" = file_read ] && "$bin" nuclei file_read >/dev/null
        read -r it ti tu tk <<<"$(measure "$bin" tokio "$mode" "")"
        read -r _ ni nu nk <<<"$(measure "$bin" nuclei "$mode" "")"
        read -r _ ei eu ek <<<"$(measure "$bin" nuclei "$mode" epoll)"
        row() { awk -v p="$profile" -v m="$mode" -v r="$1" -v it="$it" -v i="$2" -v u="$3" -v k="$4" \
            -v ti="$ti" -v tu="$tu" -v tk="$tk" -v ratio="$5" 'BEGIN {
                if (ratio) printf "%-8s %-15s %-13s %10.1f %10.1f %10.1f %9.2f %9.2f %9.2f\n", p, m, r, i/it, u/it, k/it, i/ti, u/tu, (tk ? k/tk : 0)
                else printf "%-8s %-15s %-13s %10.1f %10.1f %10.1f %9s %9s %9s\n", p, m, r, i/it, u/it, k/it, "-", "-", "-" }'; }
        row nuclei "$ni" "$nu" "$nk" 1
        row nuclei-epoll "$ei" "$eu" "$ek" 1
        row tokio "$ti" "$tu" "$tk" ""
    done
done
