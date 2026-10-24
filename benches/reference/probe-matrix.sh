#!/usr/bin/env bash
# Runs the io_uring availability probe (src/syscore/linux/probe.rs) in each sandbox and checks
# that it reports what that sandbox should produce. Prints a table of environment, expected,
# actual and verdict; exits non-zero on any mismatch or any case that could not run.
#
# The probe is the ignored test `probe_report`, built once as a static musl binary so the
# same file runs on the host, under systemd and in a container. See benches/reference/README.md.
set -euo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"

target=x86_64-unknown-linux-musl
test_name=syscore::linux::probe::tests::probe_report
image=${PROBE_IMAGE:-alpine:3}
# Named syscalls: systemd 262 has no `@io-uring` group (these sit in `@aio`), and it ignores an
# unknown group in a deny list without applying any filter, which the cases below would flag.
deny='~io_uring_setup io_uring_enter io_uring_register'

bin=$(cargo test -p nuclei --lib --release --target "$target" --no-run --message-format=json |
    jq -r 'select(.reason == "compiler-artifact" and .profile.test == true
                  and .target.name == "nuclei" and .executable != null) | .executable' |
    tail -n 1)
[[ -x $bin ]] || { echo "could not find the built test binary" >&2; exit 2; }

# A directory of our own: the container mounts it read-only and the unit runs from it, which a
# noexec /tmp would not allow.
stage=$root/target/probe-matrix
mkdir -p "$stage"
chmod 755 "$stage"
cp "$bin" "$stage/probe-test"
args=(--ignored --exact "$test_name" --nocapture)

printf 'kernel:  %s\n' "$(uname -r)"
printf 'systemd: %s\n' "$(systemctl --version 2>/dev/null | head -n 1 || echo 'not available')"
printf 'docker:  %s\n\n' "$(docker version --format '{{.Server.Version}}' 2>/dev/null || echo 'not available')"

rows=()
notes=()
failures=0

# run_case <environment> <expected line> <command...>
run_case() {
    local env=$1 want=$2 out rc=0 line verdict
    shift 2
    out=$("$@" 2>&1) || rc=$?
    line=$(grep -m 1 '^probe seccomp=' <<<"$out" || true)
    if [[ -z $line ]]; then
        line="no report (exit $rc)"
        notes+=("$env: ${out:0:300}")
    fi
    if [[ $line == "$want" && $rc -eq 0 ]]; then
        verdict=PASS
    else
        verdict=FAIL
        failures=$((failures + 1))
        if [[ $want == *seccomp=2* && $line == *seccomp=0* ]]; then
            notes+=("$env: the filter did not apply, the process reports no seccomp filter")
        fi
        [[ $rc -eq 0 ]] || notes+=("$env: the process exited with status $rc")
    fi
    rows+=("$env|${want#probe }|${line#probe }|$verdict")
}

run_case "plain host" \
    "probe seccomp=0 outcome=Available path=in-process" \
    "$stage/probe-test" "${args[@]}"

run_case "systemd, SystemCallFilter=~io_uring (kill)" \
    "probe seccomp=2 outcome=Unavailable(KilledBySeccomp) path=child" \
    systemd-run --user --wait --pipe -q -p "SystemCallFilter=$deny" "$stage/probe-test" "${args[@]}"

run_case "systemd, same + SystemCallErrorNumber=EPERM" \
    "probe seccomp=2 outcome=Unavailable(Denied(EPERM)) path=child" \
    systemd-run --user --wait --pipe -q -p "SystemCallFilter=$deny" \
    -p SystemCallErrorNumber=EPERM "$stage/probe-test" "${args[@]}"

run_case "docker default seccomp profile" \
    "probe seccomp=2 outcome=Unavailable(Denied(EPERM)) path=child" \
    docker run --rm -v "$stage":/t:ro "$image" /t/probe-test "${args[@]}"

run_case "docker seccomp=unconfined" \
    "probe seccomp=0 outcome=Available path=in-process" \
    docker run --rm --security-opt seccomp=unconfined -v "$stage":/t:ro "$image" /t/probe-test "${args[@]}"

{
    echo "environment|expected|actual|verdict"
    printf '%s\n' "${rows[@]}"
} | column -t -s '|'
if ((${#notes[@]})); then
    printf '\nnotes:\n'
    printf '  %s\n' "${notes[@]}"
fi
if ((failures)); then
    printf '\n%d case(s) did not match\n' "$failures"
    exit 1
fi
printf '\nall cases matched\n'
