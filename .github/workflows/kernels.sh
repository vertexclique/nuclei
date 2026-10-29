#!/usr/bin/env bash
# Runs the suite on one Linux kernel series (`$1`, such as 6.1), on io_uring and on epoll, in a
# virtual machine virtme-ng boots on the runner with the runner's own filesystem as its root. The
# kernel is the newest point release of the series the Ubuntu mainline archive has an amd64 build
# of. The test binaries are built once, on the runner, and each runs in the machine twice.
#
# `ubuntu-24.04` instead boots Ubuntu 24.04's own distribution kernel (the newest 6.8 generic of
# its archive), which lacks provided buffer rings: there io_uring is correctly refused, so each
# binary runs with the backend left to `auto` (which must pick epoll and say why) and with epoll
# named, and never with io_uring required.
set -euo pipefail
series=${1:?usage: kernels.sh <series, such as 6.1 | ubuntu-24.04>}
base=https://kernel.ubuntu.com/mainline

if [[ $series == ubuntu-24.04 ]]; then
    # The newest 6.8 generic kernel of the archive, installed without an initramfs (the machine
    # boots the image directly and takes its modules from the runner's /lib/modules).
    release=$(apt-cache search --names-only '^linux-image-6\.8\.0-[0-9]+-generic$' |
        awk '{print $1}' | sed -E 's/^linux-image-//' | sort -V | tail -n 1)
    [[ -n $release ]] || { echo "no 6.8 generic kernel in the Ubuntu archive" >&2; exit 2; }
    packages=("linux-image-$release" "linux-modules-$release")
    if apt-cache show "linux-modules-extra-$release" >/dev/null 2>&1; then
        packages+=("linux-modules-extra-$release")
    fi
    sudo env INITRD=No DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends "${packages[@]}"
    sudo chmod a+r "/boot/vmlinuz-$release"
    version=/boot/vmlinuz-$release
    passes=("auto|NUCLEI_EXPECT_MISSING='provided buffer rings'" "epoll|NUCLEI_BACKEND=epoll NUCLEI_REQUIRE_IO_URING=0")
    echo "kernel: $release"
else
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
    passes=("io_uring|NUCLEI_BACKEND=io_uring NUCLEI_REQUIRE_IO_URING=1" "epoll|NUCLEI_BACKEND=epoll NUCLEI_REQUIRE_IO_URING=0")
fi

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
        for pass in "${passes[@]}"; do
            echo "echo '== $(basename "$bin") on ${pass%%|*}'"
            echo "${pass#*|} '$bin' --test-threads=4"
        done
    done
} >"$runner"
chmod +x "$runner"
vng --run "$version" --memory 4G --cpus 4 -- "$runner"
