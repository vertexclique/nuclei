#!/usr/bin/env bash
# Database rows of nuclei against tokio, monoio and glommio, under the default release
# profile and under fat LTO (as benches/vs_tokio_matrix.sh builds them), RUNS runs a row with
# the runtimes interleaved inside each run, raw results and a min/median/spread summary
# under $OUT, which is outside the repository.
#
# net: the benches/echo servers driven by one client binary on cores of its own. Each
# request of `req` bytes is answered with `resp` bytes: shapes 64:64, 4096:4096, 64:16384,
# 16384:64, at 1/16/256/4096 connections on 1/4/8 server cores. `closed` is the closed loop
# (saturation throughput); `steady` and `burst` are open loop, latency counted from each
# request's intended send time: steady at STEADY_FRAC of the cell's saturation, burst
# alternating between BURST_LOW and BURST_HIGH of it every BURST_MS. The saturation is the
# lowest median closed-loop rate among the runtimes of the cell, so every server sees the
# same offered load.
#
# fs: benches/echo fsbench rows (see its header) on a real disk under FS_DIR, counted by
# perf stat over the timed loop only. FS_NOCOW=1 marks FS_DIR NOCOW on btrfs: there a file
# with data checksums turns O_DIRECT writes into buffered ones, so the direct rows would not
# be direct; every direct row reports the pages it left cached, which must be 0.
#
# Every process runs on whole cores (all SMT siblings), the quietest of a 2 s /proc/stat
# sample taken before each row, one worker per core on its first sibling. The whole run
# holds LOCK, so it never overlaps another benchmark.
#
# Usage: benches/db_matrix.sh. Knobs, from the environment, with their defaults:
#   FAMILIES="net fs" PROFILES="default fat" RUNS=5 SMOKE=0 (1: one short run per row)
#   NET_RUNTIMES="nuclei nuclei-epoll tokio tokio-ct monoio glommio"
#   NET_CORES="1 4 8" NET_CONNS="1 16 256 4096" NET_SHAPES="64:64 4096:4096 64:16384 16384:64"
#   NET_LOADS="closed steady burst" NET_WARM=1 NET_DUR=5 CLIENT_CORES=(server cores, at least 2)
#   SPIN=1 STEADY_FRAC=0.7 BURST_LOW=0.1 BURST_HIGH=0.9 BURST_MS=100
#   FS_RUNTIMES="nuclei nuclei-epoll tokio monoio glommio" FS_ROWS=(all, see fs_rows) FS_DUR=5
#   FS_CORES=3 FS_DIR=target/fsbench FS_NOCOW=1 FS_FILE_MIB=64 BUSY_JOBS=4
#   OUT LOCK
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
scratch=/tmp/claude-1000/-home-vcq-projects-defradb-rs/4fac2f63-a8e9-41cf-a60c-4cd1dddb63eb/scratchpad
lock=${LOCK:-$scratch/bench.lock}
if [ -z "${DB_MATRIX_LOCKED:-}" ]; then
    mkdir -p "$(dirname "$lock")"
    echo "waiting for $lock" >&2
    DB_MATRIX_LOCKED=1 exec flock "$lock" "$0" "$@"
fi

smoke=${SMOKE:-0}
if [ "$smoke" = 1 ]; then
    runs=${RUNS:-1} net_warm=${NET_WARM:-0.2} net_dur=${NET_DUR:-0.6} fs_dur=${FS_DUR:-0.5}
else
    runs=${RUNS:-5} net_warm=${NET_WARM:-1} net_dur=${NET_DUR:-5} fs_dur=${FS_DUR:-5}
fi
families=${FAMILIES:-net fs}
profiles=${PROFILES:-default fat}
net_runtimes=${NET_RUNTIMES:-nuclei nuclei-epoll tokio tokio-ct monoio glommio}
net_cores=${NET_CORES:-1 4 8}
net_conns=${NET_CONNS:-1 16 256 4096}
net_shapes=${NET_SHAPES:-64:64 4096:4096 64:16384 16384:64}
net_loads=${NET_LOADS:-closed steady burst}
spin=${SPIN:-1}
steady_frac=${STEADY_FRAC:-0.7}
burst_low=${BURST_LOW:-0.1}
burst_high=${BURST_HIGH:-0.9}
burst_ms=${BURST_MS:-100}
fs_runtimes=${FS_RUNTIMES:-nuclei nuclei-epoll tokio monoio glommio}
fs_rows=${FS_ROWS:-sync_fsync_4k sync_fsync_64k sync_fsync_1m sync_fdatasync_4k sync_fdatasync_64k
    sync_fdatasync_1m direct_write_4k direct_write_64k direct_write_1m direct_read_4k direct_read_64k
    direct_read_1m append_fsync_n1 append_fsync_n8 append_fsync_n64 append_fdatasync_n1
    append_fdatasync_n8 append_fdatasync_n64 writers_one writers_many busy_pool}
fs_cores=${FS_CORES:-3}
fs_dir=${FS_DIR:-$root/target/fsbench}
fs_nocow=${FS_NOCOW:-1}
fs_file_mib=${FS_FILE_MIB:-64}
busy_jobs=${BUSY_JOBS:-4}
out=${OUT:-$scratch/db-rows}

stamp=$(date +%Y%m%d-%H%M%S)
mkdir -p "$out"
raw=$out/raw-$stamp.txt
failures=$out/failures-$stamp.txt
summary=$out/summary-$stamp.txt
tmp=$(mktemp -d)
srv_pid=
cleanup() {
    [ -n "$srv_pid" ] && kill -KILL "$srv_pid" 2>/dev/null
    rm -rf "$tmp" "$fs_dir/run"
}
trap cleanup EXIT

say() { echo "$*" | tee -a "$raw" >&2; }

# The echo crate is its own workspace; glommio's bundled liburing needs open_how declared.
build() {
    CFLAGS="${CFLAGS:-} -include linux/openat2.h" CARGO_TARGET_DIR=$1 \
        cargo build --release --manifest-path "$root/benches/echo/Cargo.toml" --bins >&2
}
for profile in $profiles; do
    case $profile in
    default) echo "building default profile" >&2 && build "$root/target/db-bench" ;;
    fat)
        echo "building fat LTO profile" >&2
        CARGO_PROFILE_RELEASE_LTO=fat CARGO_PROFILE_RELEASE_CODEGEN_UNITS=1 build "$root/target-fat/db-bench"
        ;;
    *) echo "unknown profile $profile (default or fat)" >&2 && exit 2 ;;
    esac
done
bindir() { if [ "$1" = default ]; then echo "$root/target/db-bench/release"; else echo "$root/target-fat/db-bench/release"; fi; }

# Whole cores as comma lists of their CPUs, from sysfs.
mapfile -t all_cores < <(
    for c in /sys/devices/system/cpu/cpu[0-9]*/topology/thread_siblings_list; do
        awk -F, '{ s = ""; for (i = 1; i <= NF; i++) { n = split($i, r, "-"); lo = r[1]; hi = (n > 1 ? r[2] : r[1]);
            for (c = lo; c <= hi; c++) s = s (s == "" ? "" : ",") c } print s }' "$c"
    done | sort -t, -k1,1n -u
)

# pick <n>: the n quietest whole cores over a 2 s sample, quietest first, one per line.
pick() {
    local n=$1 a b
    [ "$n" -le "${#all_cores[@]}" ] || { echo "need $n whole cores, the host has ${#all_cores[@]}" >&2; exit 2; }
    a=$(grep '^cpu[0-9]' /proc/stat)
    sleep 2
    b=$(grep '^cpu[0-9]' /proc/stat)
    paste -d' ' <(echo "$a") <(echo "$b") | awk -v cores="$(printf '%s\n' "${all_cores[@]}")" '
        { ta = 0; tb = 0; for (i = 2; i <= 11; i++) { ta += $i; tb += $(i + 11) }
          busy[substr($1, 4)] = (tb - $16 - $17) - (ta - $5 - $6) }
        END { m = split(cores, list, "\n")
              for (k = 1; k <= m; k++) { s = 0; c = split(list[k], cpu, ","); for (j = 1; j <= c; j++) s += busy[cpu[j]]
                                         printf "%d %s\n", s, list[k] } }' |
        sort -n | head -n "$n" | cut -d' ' -f2
}
firsts() { local c; for c in "$@"; do echo "${c%%,*}"; done | paste -sd,; }
alls() { printf '%s\n' "$@" | paste -sd,; }

median() { sort -g | awk '{ v[NR] = $1 } END { if (NR) print v[int((NR + 1) / 2)] }'; }
num() { case $1 in '' | na | *[!0-9.e+-]*) return 1 ;; esac; }

# Records one run: its key=value fields plus the result line, or the failure and its repro.
record() { echo "$*" >>"$raw"; }
failed() {
    local what=$1 reason=${2//\"/\'} repro=$3 log=${4:-}
    record "$what status=fail reason=\"$reason\""
    {
        echo "== $what"
        echo "reason: $reason"
        echo "repro: $repro"
        [ -n "$log" ] && [ -s "$log" ] && sed 's/^/  | /' "$log" | tail -n 40
    } >>"$failures"
    echo "FAIL $what: $reason" >&2
}

port=7100
next_port() { port=$((7100 + (port - 7100 + 1) % 2000)); }

# server_cmd <rt> <bin> <port> <cpus> <req> <resp>: the server's command line.
server_cmd() {
    local rt=$1 bin=$2 p=$3 cpus=$4 req=$5 resp=$6
    case $rt in
    nuclei) echo "env NUCLEI_BACKEND= $bin/nuclei_srv $p $cpus $req $resp" ;;
    nuclei-epoll) echo "env NUCLEI_BACKEND=epoll $bin/nuclei_srv $p $cpus $req $resp" ;;
    tokio) echo "$bin/tokio_srv $p $cpus $req $resp mt" ;;
    tokio-ct) echo "$bin/tokio_srv $p $cpus $req $resp ct" ;;
    monoio) echo "$bin/monoio_srv $p $cpus $req $resp" ;;
    glommio) echo "$bin/glommio_srv $p $cpus $req $resp" ;;
    *) echo "unknown runtime $rt" >&2 && exit 2 ;;
    esac
}

stop_server() {
    [ -n "$srv_pid" ] || return 0
    kill -TERM "$srv_pid" 2>/dev/null || true
    local deadline=$((SECONDS + 5)) delay=0.01
    while kill -0 "$srv_pid" 2>/dev/null && ((SECONDS < deadline)); do
        sleep "$delay"
        delay=$(awk -v d="$delay" 'BEGIN { print (d * 2 > 0.5 ? 0.5 : d * 2) }')
    done
    kill -KILL "$srv_pid" 2>/dev/null || true
    wait "$srv_pid" 2>/dev/null || true
    srv_pid=
}

# net_run <profile> <rt> <run> <req> <resp> <conns> <scpus> <smask> <ccpus> <cmask> <load> <load args>
# Leaves the client's achieved rate in net_rate (empty on failure).
net_rate=
net_run() {
    local profile=$1 rt=$2 run=$3 req=$4 resp=$5 conns=$6 scpus=$7 smask=$8 ccpus=$9 cmask=${10} load=${11} largs=${12}
    local bin cores row what scmd ccmd log=$tmp/srv.log res rc=0 deadline delay=0.01
    bin=$(bindir "$profile")
    cores=$(awk -F, '{ print NF }' <<<"$scpus")
    row=r${req}_s${resp}_c${conns}_k${cores}_$load
    what="profile=$profile family=net row=$row rt=$rt run=$run req=$req resp=$resp conns=$conns cores=$cores load=$load $largs"
    net_rate=
    next_port
    scmd="taskset -c $smask $(server_cmd "$rt" "$bin" "$port" "$scpus" "$req" "$resp")"
    ccmd="taskset -c $cmask $bin/client port=$port conns=$conns req=$req resp=$resp load=$load $largs warm=$net_warm dur=$net_dur pid=PID cpus=$ccpus spin=$spin"
    : >"$log"
    $scmd >>"$log" 2>&1 &
    srv_pid=$!
    deadline=$((SECONDS + 30))
    until grep -q '^ready' "$log"; do
        if ! kill -0 "$srv_pid" 2>/dev/null; then
            failed "$what" "the server exited before it was ready" "$scmd" "$log"
            srv_pid=
            return 0
        fi
        if ((SECONDS >= deadline)); then
            failed "$what" "the server was not ready within 30 s" "$scmd" "$log"
            stop_server
            return 0
        fi
        sleep "$delay"
        delay=$(awk -v d="$delay" 'BEGIN { print (d * 2 > 0.2 ? 0.2 : d * 2) }')
    done
    local limit
    limit=$(awk -v w="$net_warm" -v d="$net_dur" 'BEGIN { printf "%d", w + d + 60 }')
    res=$(timeout --kill-after=5 "$limit" ${ccmd/pid=PID/pid=$srv_pid} 2>>"$log") || rc=$?
    if ! kill -0 "$srv_pid" 2>/dev/null; then
        failed "$what" "the server died during the run (client exit $rc: $res)" "$scmd; ${ccmd}" "$log"
        wait "$srv_pid" 2>/dev/null || true
        srv_pid=
        return 0
    fi
    local nofile
    nofile=$(sed -n 's/^ready \(.*\)/\1/p' "$log" | sed 's/nofile/server_nofile/g')
    stop_server
    if [ "$rc" = 124 ] || [ "$rc" = 137 ]; then
        failed "$what" "the client hung past ${limit} s (killed)" "$scmd; ${ccmd}" "$log"
    elif [ "$rc" != 0 ]; then
        failed "$what" "client exit $rc: $res" "$scmd; ${ccmd}" "$log"
    else
        record "$what status=ok $res $nofile"
        net_rate=$(sed -n 's/^rate=\([0-9.]*\) .*/\1/p' <<<"$res")
        echo "ok $profile $row $rt run=$run: ${res%% cpu_us*}" >&2
    fi
}

net_family() {
    local profile=$1 cores conns shape req resp ccores picks scores ccpus cmask scpus smask run rt load
    for cores in $net_cores; do
        ccores=${CLIENT_CORES:-$((cores < 2 ? 2 : cores))}
        for shape in $net_shapes; do
            req=${shape%:*} resp=${shape#*:}
            for conns in $net_conns; do
                mapfile -t picks < <(pick $((cores + ccores)))
                scores=("${picks[@]:0:cores}")
                scpus=$(firsts "${scores[@]}") smask=$(alls "${scores[@]}")
                ccpus=$(firsts "${picks[@]:cores}") cmask=$(alls "${picks[@]:cores}")
                say "# cell profile=$profile req=$req resp=$resp conns=$conns server=$smask client=$cmask $(cut -d' ' -f1-3 /proc/loadavg | sed 's/^/loadavg=/; s/ /,/g')"
                declare -A rates=()
                for run in $(seq "$runs"); do
                    for rt in $net_runtimes; do
                        net_run "$profile" "$rt" "$run" "$req" "$resp" "$conns" "$scpus" "$smask" "$ccpus" "$cmask" closed ""
                        rates[$rt]+="$net_rate "
                    done
                done
                local sat="" m
                for rt in $net_runtimes; do
                    m=$(printf '%s\n' ${rates[$rt]} | median)
                    num "$m" && awk -v m="$m" 'BEGIN { exit !(m > 0) }' || continue
                    if [ -z "$sat" ] || awk -v a="$m" -v b="$sat" 'BEGIN { exit !(a < b) }'; then sat=$m; fi
                done
                unset rates
                if [ -z "$sat" ]; then
                    say "# no runtime finished the closed loop of this cell: its open-loop rows are not run"
                    continue
                fi
                say "# saturation (lowest median closed-loop rate) $sat/s"
                for load in $net_loads; do
                    local largs
                    case $load in
                    closed) continue ;;
                    steady) largs=$(awk -v s="$sat" -v f="$steady_frac" 'BEGIN { printf "rate=%.0f", s * f }') ;;
                    burst) largs=$(awk -v s="$sat" -v l="$burst_low" -v h="$burst_high" -v p="$burst_ms" \
                        'BEGIN { printf "low=%.0f high=%.0f period_ms=%s", s * l, s * h, p }') ;;
                    *) echo "unknown load $load" >&2 && exit 2 ;;
                    esac
                    for run in $(seq "$runs"); do
                        for rt in $net_runtimes; do
                            net_run "$profile" "$rt" "$run" "$req" "$resp" "$conns" "$scpus" "$smask" "$ccpus" "$cmask" "$load" "$largs"
                        done
                    done
                done
            done
        done
    done
}

bytes() { case $1 in *k) echo $((${1%k} * 1024)) ;; *m) echo $((${1%m} * 1048576)) ;; *) echo "$1" ;; esac; }
fs_args() {
    local k
    case $1 in
    sync_fsync_* | sync_fdatasync_*) k=${1#sync_} && echo "sync sync=${k%_*} size=$(bytes "${k##*_}")" ;;
    direct_write_* | direct_read_*) echo "${1%_*} size=$(bytes "${1##*_}")" ;;
    append_*_n*) k=${1#append_} && echo "append sync=${k%_n*} group=${k##*_n}" ;;
    writers_one | writers_many) echo "$1" ;;
    busy_pool) echo "busy_pool jobs=$busy_jobs" ;;
    *) echo "unknown fs row $1" >&2 && exit 2 ;;
    esac
}

counter() { awk -F, -v e="$2" '$3 == e { print $1 }' "$1"; }

fs_run() {
    local profile=$1 rt=$2 run=$3 row=$4 mask=$5 bin args name envs what cmd res rc=0 limit
    bin=$(bindir "$profile")
    args=$(fs_args "$row")
    name=$rt envs="NUCLEI_BACKEND="
    [ "$rt" = nuclei-epoll ] && name=nuclei envs="NUCLEI_BACKEND=epoll"
    what="profile=$profile family=fs row=$row rt=$rt run=$run"
    rm -rf "$fs_dir/run" "$tmp/ctl" "$tmp/ack"
    mkdir -p "$fs_dir/run"
    mkfifo "$tmp/ctl" "$tmp/ack"
    exec {ctl_fd}<>"$tmp/ctl" {ack_fd}<>"$tmp/ack"
    cmd="env $envs FSBENCH_PERF_CTL=$tmp/ctl FSBENCH_PERF_ACK=$tmp/ack taskset -c $mask $bin/fsbench $name $args dir=$fs_dir/run dur=$fs_dur file_mib=$fs_file_mib"
    limit=$(awk -v d="$fs_dur" 'BEGIN { printf "%d", d + 120 }')
    res=$(timeout --kill-after=5 "$limit" perf stat -D -1 --control "fifo:$tmp/ctl,$tmp/ack" -x, -o "$tmp/perf" \
        -e instructions:u,cycles:u,cycles:k $cmd 2>"$tmp/fs.log") || rc=$?
    exec {ctl_fd}>&- {ack_fd}>&-
    rm -rf "$fs_dir/run"
    local load
    load=$(cut -d' ' -f1,2 /proc/loadavg | awk '{ printf "load1=%s load5=%s", $1, $2 }')
    if [ "$rc" = 124 ] || [ "$rc" = 137 ]; then
        failed "$what" "fsbench hung past ${limit} s (killed)" "$cmd" "$tmp/fs.log"
    elif [ "$rc" != 0 ]; then
        failed "$what" "fsbench exit $rc: $res" "$cmd" "$tmp/fs.log"
    elif [[ $res == skip=* ]]; then
        record "$what status=skip $res"
        echo "skip $profile $row $rt: $res" >&2
    else
        local ops ins ucy kcy
        ops=$(sed -n 's/^ops=\([0-9]*\) .*/\1/p' <<<"$res")
        ins=$(counter "$tmp/perf" instructions:u) ucy=$(counter "$tmp/perf" cycles:u) kcy=$(counter "$tmp/perf" cycles:k)
        local per
        per=$(awk -v o="$ops" -v i="$ins" -v u="$ucy" -v k="$kcy" 'BEGIN {
            f = "%.0f"; if (o + 0 == 0) { print "ins_op=na ucyc_op=na kcyc_op=na"; exit }
            printf "ins_op=%s ucyc_op=%s kcyc_op=%s\n", (i ~ /^[0-9]+$/ ? sprintf(f, i / o) : "na"),
                (u ~ /^[0-9]+$/ ? sprintf(f, u / o) : "na"), (k ~ /^[0-9]+$/ ? sprintf(f, k / o) : "na") }')
        record "$what status=ok $res $per $load"
        echo "ok $profile $row $rt run=$run: ${res%% io_rd*} $per" >&2
        grep -q 'did not bypass' "$tmp/fs.log" && failed "$what" "a direct row left pages cached: its IO was not all direct" "$cmd" "$tmp/fs.log"
    fi
    return 0
}

fs_family() {
    local profile=$1 row picks mask run rt
    for row in $fs_rows; do
        mapfile -t picks < <(pick "$fs_cores")
        mask=$(alls "${picks[@]}")
        say "# fs row profile=$profile row=$row cpus=$mask $(cut -d' ' -f1-3 /proc/loadavg | sed 's/^/loadavg=/; s/ /,/g')"
        for run in $(seq "$runs"); do
            for rt in $fs_runtimes; do
                fs_run "$profile" "$rt" "$run" "$row" "$mask"
            done
        done
    done
}

summarize() {
    gawk -v FPAT='([^ ]+="[^"]*")|([^ ]+)' '
        function med(a, n,   s, i) { n = asort(a, s, "@val_num_asc"); return s[int((n + 1) / 2)] }
        function show(v) { return (v == int(v) || v >= 1000 || v <= -1000) ? sprintf("%.0f", v) : sprintf("%.4g", v) }
        /^#/ || !/status=/ { next }
        {
            delete f
            for (i = 1; i <= NF; i++) { k = $i; sub(/=.*/, "", k); v = substr($i, length(k) + 2); gsub(/"/, "", v); f[k] = v }
            key = f["profile"] SUBSEP f["family"] SUBSEP f["row"] SUBSEP f["rt"]
            if (!(key in seen)) { seen[key] = 1; order[++n] = key }
            if (f["status"] == "skip") { bad[key] = "SKIP: " f["skip"]; next }
            if (f["status"] != "ok") { bad[key] = bad[key] "FAILED: " f["reason"] "; "; next }
            runs[key]++
            cols[key] = f["family"] == "net" ? "rate p50_us p99_us p999_us max_us cpu_us_per_req unfinished lag_p99_us load1" \
                : "ops_s mib_s p50_us p99_us p999_us max_us ins_op ucyc_op kcyc_op io_rd_per_op io_wr_per_op cached_pages_after jobs_s load1"
            split(cols[key], ms, " ")
            for (j in ms) { m = ms[j]; if (f[m] ~ /^[0-9.e+-]+$/) { c = ++cnt[key, m]; val[key, m, c] = f[m] + 0 } }
            if (f["note"] != "") note[key] = f["note"]
        }
        END {
            print "median (min, spread = (max - min) / median) over the runs of each row"
            for (i = 1; i <= n; i++) {
                key = order[i]; split(key, p, SUBSEP)
                line = sprintf("%-7s %-3s %-30s %-12s runs=%d", p[1], p[2], p[3], p[4], runs[key])
                if (runs[key]) {
                    split(cols[key], ms, " ")
                    for (j = 1; j <= length(ms); j++) {
                        m = ms[j]; c = cnt[key, m]; if (!c) continue
                        delete a; lo = ""; hi = ""
                        for (r = 1; r <= c; r++) { a[r] = val[key, m, r]; if (lo == "" || a[r] < lo) lo = a[r]; if (hi == "" || a[r] > hi) hi = a[r] }
                        md = med(a)
                        line = line sprintf(" %s=%s(%s,%s)", m, show(md), show(lo), md ? sprintf("%.0f%%", (hi - lo) / md * 100) : "-")
                    }
                }
                if (key in bad) line = line " " bad[key]
                if (key in note) line = line " note: " note[key]
                print line
            }
        }' "$raw"
}

{
    echo "# db_matrix $stamp smoke=$smoke runs=$runs profiles=\"$profiles\" families=\"$families\""
    echo "# host $(uname -r) $(grep -m1 'model name' /proc/cpuinfo | cut -d: -f2 | sed 's/^ //') whole cores ${#all_cores[@]}"
    echo "# commit $(git -C "$root" rev-parse --short HEAD) $(git -C "$root" status --porcelain | grep -qv '^??' && echo dirty || echo clean)"
    echo "# nofile soft $(ulimit -Sn) hard $(ulimit -Hn): the client and every server raise the soft limit to the hard one and report it"
} | tee -a "$raw" >&2

for family in $families; do
    case $family in
    net)
        say "# net warm=${net_warm}s dur=${net_dur}s spin=$spin steady=$steady_frac burst=$burst_low/$burst_high every ${burst_ms}ms"
        for profile in $profiles; do net_family "$profile"; done
        ;;
    fs)
        mkdir -p "$fs_dir"
        if [ "$fs_nocow" = 1 ] && [ "$(findmnt -n -o FSTYPE -T "$fs_dir")" = btrfs ]; then
            chattr +C "$fs_dir" || say "# chattr +C $fs_dir failed: the direct rows may fall back to buffered IO"
        fi
        say "# fs dir=$fs_dir dur=${fs_dur}s cores=$fs_cores file=${fs_file_mib}MiB busy_jobs=$busy_jobs"
        say "# fs mount $(findmnt -n -o TARGET,SOURCE,FSTYPE,OPTIONS -T "$fs_dir")"
        say "# fs attrs $(lsattr -d "$fs_dir" 2>/dev/null | cut -d' ' -f1)"
        say "# fs the disk is shared with other work on this host: sync latencies carry its noise"
        for profile in $profiles; do fs_family "$profile"; done
        ;;
    *) echo "unknown family $family (net or fs)" >&2 && exit 2 ;;
    esac
done

summarize | tee "$summary"
echo "raw: $raw" >&2
echo "summary: $summary" >&2
[ -s "$failures" ] && echo "failures (with repros): $failures" >&2
exit 0
