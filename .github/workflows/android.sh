#!/usr/bin/env bash
# Builds the suite for Android with the Android NDK's toolchain, and runs tests of it on an emulator.
#
#   android.sh build <target> [args]  builds test binaries of `<target>` without running them:
#                                     all of them, or those the cargo test arguments name
#   android.sh test                   runs the tests of Android's app filter for
#                                     x86_64-linux-android on the emulator `adb` sees (cargo
#                                     calls `android.sh run` for the test binary)
#   android.sh run <binary> ...       cargo's runner: pushes the binary to /data/local/tmp, runs
#                                     it there with the arguments and the `NUCLEI_*` variables,
#                                     and prints the crash lines of logcat when it fails
#
# Two limits, stated plainly. A process started by `adb shell` is not started by the app launcher,
# so it does not carry Android's seccomp filter for apps: the test
# `the_runtime_makes_no_call_an_app_filter_traps` installs the same refusals itself. And the
# emulator runs those tests only, not the whole suite (see `test` below).
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

case ${1:?usage: android.sh build <target> [args...] | test | run <binary> [args...]} in
build)
    target=${2:?usage: android.sh build <target> [cargo test arguments]}
    shift 2
    (($#)) || set -- --lib --tests
    toolchain "$target"
    exec cargo test -p nuclei --target "$target" --no-run "$@"
    ;;
test)
    toolchain x86_64-linux-android
    export CARGO_TARGET_X86_64_LINUX_ANDROID_RUNNER="$here run"
    # vertexia: only the tests of the app filter run here. The whole suite ran on this emulator
    # once: 2140 tests passed, the zero-copy write tests timed out, and the tests that make a
    # socket, a FIFO or a hard link by path were refused by SELinux, which gives the `adb shell`
    # domain none of them under /data/local/tmp. Run the whole suite (`--lib --tests`, no filter)
    # once those are understood, from an app's own data directory.
    exec cargo test -p nuclei --target x86_64-linux-android --lib -- \
        syscore::linux::probe the_runtime_makes_no_call_an_app_filter_traps
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
    echo "usage: android.sh build <target> [args...] | test | run <binary> [args...]" >&2
    exit 2
    ;;
esac
