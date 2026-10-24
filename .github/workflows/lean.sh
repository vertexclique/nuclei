#!/usr/bin/env bash
# Install a pinned, checksum-verified elan and build every Lean proof under proofs/lean.
#
# elan is the Lean toolchain manager: it reads proofs/lean/lean-toolchain and fetches exactly
# the Lean release named there. This script only fetches elan's release archive, checks its
# SHA-256, installs it and hands over to `proofs/lean/check.sh build`, the same build a local
# run makes, so CI and a local check cannot drift apart. The scan for unfinished proofs and the
# axiom check are their own steps of lean.yml. A tree without proofs/lean/check.sh fails: a
# proof job that checked nothing is not a pass.
set -euo pipefail

ELAN_RELEASE="v4.2.4" # elan 4.2.4 (227caca13), 25 Aug 2026
ELAN_SHA256="42b94d4244e8353142c456ec0e4ca6528fd898a6c604d4059f494e706e431f63"
ELAN_URL="https://github.com/leanprover/elan/releases/download/${ELAN_RELEASE}/elan-x86_64-unknown-linux-gnu.tar.gz"

[ -f proofs/lean/check.sh ] || { echo "proofs/lean/check.sh not found, so no Lean proof was checked." >&2; exit 1; }

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT
ARCHIVE="$WORKDIR/elan.tar.gz"

echo "Downloading elan (${ELAN_RELEASE})..."
curl -fsSL -o "$ARCHIVE" "$ELAN_URL"

ACTUAL_SHA256="$(sha256sum "$ARCHIVE" | cut -d' ' -f1)"
if [ "$ACTUAL_SHA256" != "$ELAN_SHA256" ]; then
  echo "ERROR: elan archive checksum mismatch, refusing to run an unverified installer." >&2
  echo "  expected: $ELAN_SHA256" >&2
  echo "  actual:   $ACTUAL_SHA256" >&2
  exit 1
fi
echo "Checksum verified."

tar -xzf "$ARCHIVE" -C "$WORKDIR"
"$WORKDIR/elan-init" -y --default-toolchain none --no-modify-path
export PATH="$HOME/.elan/bin:$PATH"
elan --version

bash proofs/lean/check.sh build
