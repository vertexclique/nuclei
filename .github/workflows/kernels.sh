#!/usr/bin/env bash
# Runs the suite on one Linux kernel series (`$1`, such as 6.1), on io_uring and on epoll, in a
# virtual machine virtme-ng boots on the runner with the runner's own filesystem as its root. The
# kernel is the newest point release of the series the Ubuntu mainline archive has an amd64 build
# of. The test binaries are built once, on the runner, and each runs in the machine twice.
set -euo pipefail
series=${1:?usage: kernels.sh <series, such as 6.1>}
base=https://kernel.ubuntu.com/mainline

# The series' releases, newest first; the first with an amd64 kernel package is the one.
pattern="v${series//./\\.}(\\.[0-9]+)?"
mapfile -t releases < <(curl -fsSL "$base/" | grep -oE "href=\"$pattern/\"" |
    sed -E 's/^href="(v[^/]+)\/"$/\1/' | sort -Vr)
version=""
for release in "${releases[@]}"; do
    if curl -fsSL "$base/$release/amd64/" 2>/dev/null | grep -qE 'linux-image-[^"]*generic[^"]*_amd64\.deb'; then
        version=$release
        break
    fi
done
[[ -n $version ]] || { echo "no amd64 kernel of the $series series in $base" >&2; exit 2; }
echo "kernel: $version"

mapfile -t bins < <(cargo test -p nuclei --no-run --message-format=json 2>/dev/null |
    jq -r 'select(.reason == "compiler-artifact" and .profile.test == true and .executable != null) | .executable')
((${#bins[@]})) || { echo "no test binary was built" >&2; exit 2; }

# What the machine runs: every binary on each backend; the first failure ends it, with its status.
runner=$PWD/target/kernels-run.sh
{
    echo '#!/bin/sh'
    echo 'set -e'
    echo 'uname -r'
    for bin in "${bins[@]}"; do
        for backend in io_uring epoll; do
            echo "echo '== $(basename "$bin") on $backend'"
            echo "NUCLEI_BACKEND=$backend NUCLEI_REQUIRE_IO_URING=$([[ $backend == io_uring ]] && echo 1 || echo 0) '$bin' --test-threads=4"
        done
    done
} >"$runner"
chmod +x "$runner"
vng --run "$version" --memory 4G --cpus 4 -- "$runner"
