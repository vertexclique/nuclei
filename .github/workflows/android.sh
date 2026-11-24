#!/usr/bin/env bash
# Builds and runs the suite for Android, with the Android NDK's toolchain.
#
#   android.sh build <target>   builds the test binaries of `<target>` without running them
#   android.sh test             runs the suite for x86_64-linux-android, each test binary on the
#                               emulator `adb` sees (cargo calls `android.sh run` for each)
#   android.sh run <binary> ... cargo's runner: pushes the binary to /data/local/tmp, runs it
#                               there with the arguments and the `NUCLEI_*` variables, and
#                               prints the crash lines of logcat when it fails
#
# Two limits, stated plainly. A process started by `adb shell` is not started by the app launcher,
# so it does not carry Android's seccomp filter for apps: the suite's test
# `the_runtime_makes_no_call_an_app_filter_traps` installs the same refusals itself. And the suite
# runs on epoll only, with SELinux permissive (see `test` below).
set -euo pipefail

# The lowest Android API the toolchain targets (the NDK's clang wrapper is named by it).
api=24
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")
remote=/data/local/tmp/nuclei-test

toolchain() {
    local target=$1 ndk=${ANDROID_NDK_LATEST_HOME:-${ANDROID_NDK_HOME:-}}
    [[ -n $ndk ]] || { echo "set ANDROID_NDK_LATEST_HOME or ANDROID_NDK_HOME to the Android NDK" >&2; exit 2; }
    local bin=$ndk/toolchains/llvm/prebuilt/linux-x86_64/bin
    local triple=${target//-/_}
    local upper=${triple^^}
    # clang names the ARM target `armv7a-linux-androideabi`; the two targets here need no mapping.
    export "CARGO_TARGET_${upper}_LINKER=$bin/${target}${api}-clang"
    export "CC_${triple}=$bin/${target}${api}-clang"
    export "AR_${triple}=$bin/llvm-ar"
}

case ${1:?usage: android.sh build <target> | test | run <binary> [args...]} in
build)
    target=${2:?usage: android.sh build <target>}
    toolchain "$target"
    exec cargo test -p nuclei --target "$target" --lib --tests --no-run
    ;;
test)
    toolchain x86_64-linux-android
    export CARGO_TARGET_X86_64_LINUX_ANDROID_RUNNER="$here run"
    # An app cannot reach io_uring (its seccomp filter traps the calls), so the suite runs on what
    # an app gets, epoll. An `adb shell` process can reach io_uring, which is not run here.
    export NUCLEI_BACKEND=epoll NUCLEI_REQUIRE_IO_URING=0
    # SELinux refuses the `adb shell` domain a socket, a FIFO or a hard link under /data/local/tmp,
    # which an app may make in its own data directory (a hard link excepted: an app is refused it
    # too). Permissive for the run, and the tests still run as the unprivileged shell user.
    adb root >/dev/null
    adb wait-for-device
    adb shell setenforce 0
    adb unroot >/dev/null
    adb wait-for-device
    exec cargo test -p nuclei --target x86_64-linux-android --lib --tests --no-fail-fast
    ;;
run)
    shift
    binary=${1:?usage: android.sh run <binary> [args...]}
    shift
    name=$(basename "$binary")
    adb shell "mkdir -p $remote/tmp"
    adb push "$binary" "$remote/$name" >/dev/null
    adb shell "chmod 755 $remote/$name"
    command="cd $remote && TMPDIR=$remote/tmp RUST_BACKTRACE=1"
    while IFS='=' read -r key value; do
        command+=" $(printf '%q=%q' "$key" "$value")"
    done < <(env | grep '^NUCLEI_' || true)
    command+=" ./$name"
    for arg in "$@"; do
        command+=" $(printf '%q' "$arg")"
    done
    status=0
    adb shell "$command" || status=$?
    if ((status != 0)); then
        # Android names a refused system call here ("seccomp prevented call to disallowed ...").
        adb logcat -d -s DEBUG libc | grep -iE 'seccomp|signal|fatal|backtrace' | tail -n 40 || true
    fi
    adb shell "rm -f $remote/$name"
    exit "$status"
    ;;
*)
    echo "usage: android.sh build <target> | test | run <binary> [args...]" >&2
    exit 2
    ;;
esac
