#!/usr/bin/env bash
# Model-check every TLA+ model under proofs/tla/ with a pinned, checksum-verified
# tla2tools.jar.
#
# The models' own runner does the work: `proofs/tla/run_tlc.sh all` finds every family (a
# directory of proofs/tla/ with an EXPECTED.txt), translates its PlusCal, checks each of its
# configurations and exits non-zero unless every result matches what EXPECTED.txt says it must
# be. This script only fetches the tools, verifies them and hands them to the runner through
# TLA2TOOLS, so CI checks the same models the same way a local run does. A tree without
# proofs/tla/run_tlc.sh fails: a model-checking job that checked nothing is not a pass.
set -euo pipefail

TLA_RELEASE="v1.7.4" # TLC2 Version 2.19 (rev 5a47802), 08 Aug 2024; PlusCal translator 1.11
TLA_SHA256="936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88"
TLA_URL="https://github.com/tlaplus/tlaplus/releases/download/${TLA_RELEASE}/tla2tools.jar"

command -v java >/dev/null || { echo "java is required on PATH" >&2; exit 1; }
[ -f proofs/tla/run_tlc.sh ] || { echo "proofs/tla/run_tlc.sh not found, so no TLA+ model was checked." >&2; exit 1; }

WORKDIR="$(mktemp -d)"
trap 'rm -rf "$WORKDIR"' EXIT
JAR="$WORKDIR/tla2tools.jar"

echo "Downloading tla2tools.jar (${TLA_RELEASE})..."
curl -fsSL -o "$JAR" "$TLA_URL"

ACTUAL_SHA256="$(sha256sum "$JAR" | cut -d' ' -f1)"
if [ "$ACTUAL_SHA256" != "$TLA_SHA256" ]; then
  echo "ERROR: tla2tools.jar checksum mismatch, refusing to run an unverified jar." >&2
  echo "  expected: $TLA_SHA256" >&2
  echo "  actual:   $ACTUAL_SHA256" >&2
  exit 1
fi
echo "Checksum verified."
java -cp "$JAR" tlc2.TLC 2>&1 | head -1 || true

TLA2TOOLS="$JAR" bash proofs/tla/run_tlc.sh all
