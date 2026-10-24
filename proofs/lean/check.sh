#!/usr/bin/env bash
# Check the Lean proofs of nuclei (the library in this directory).
#   bash proofs/lean/check.sh          the scan for unfinished proofs, the build, then the axioms
#   bash proofs/lean/check.sh build    `lake build`, failing on any warning
#   bash proofs/lean/check.sh sorry    fail if `sorry` or `admit` appears in a Lean source
#   bash proofs/lean/check.sh axioms   fail unless every theorem rests only on Lean's standard axioms
# Lean accepts a proof with a hole in it (`sorry`, `admit`) and only warns, so the build fails on
# any warning and the scan fails on either word anywhere in a Lean source, comments included:
# a proof with a hole is not a proof. The axiom check compiles Axioms.lean, whose `#print axioms`
# lines list what each theorem depends on, and accepts only propext, Quot.sound and
# Classical.choice. The build needs `lake` on PATH (elan installs it, and fetches the Lean release
# lean-toolchain names on first use; see ../README.md).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"

scan() {
  local status=0
  grep -rnwE --include='*.lean' --exclude-dir=.lake 'sorry|admit' "$here" || status=$?
  case "$status" in
    0) echo "the Lean proofs above are unfinished: remove every sorry and admit" >&2; return 1 ;;
    1) echo "no sorry and no admit in the Lean sources" ;;
    *) echo "could not scan the Lean sources" >&2; return 1 ;;
  esac
}

need_lake() {
  command -v lake >/dev/null || {
    echo "lake is not on PATH: install elan, see proofs/README.md" >&2; return 1; }
}

build() {
  need_lake
  (cd "$here" && lake build --wfail)
}

axioms() {
  need_lake
  local out
  out="$(cd "$here" && lake env lean Axioms.lean 2>&1)" || { echo "$out" >&2; return 1; }
  echo "$out"
  local listed bad
  listed="$(printf '%s\n' "$out" | grep -cE "depends on axioms|does not depend on any axioms" || true)"
  if [ "$listed" != "$(grep -c '^#print axioms' "$here/Axioms.lean")" ]; then
    echo "Axioms.lean printed $listed axiom lists, not one per #print axioms line" >&2; return 1
  fi
  bad="$(printf '%s\n' "$out" | grep -oE "depends on axioms: \[[^]]*\]" | sed 's/.*\[//; s/\]//' |
         tr ',' '\n' | tr -d ' ' | grep -vxE 'propext|Quot\.sound|Classical\.choice' | sort -u || true)"
  if [ -n "$bad" ]; then
    echo "a theorem rests on a non-standard axiom: $bad" >&2; return 1
  fi
  echo "every theorem rests only on propext, Quot.sound and Classical.choice"
}

case "${1:-all}" in
  sorry) scan ;;
  build) build ;;
  axioms) axioms ;;
  all) scan && build && axioms ;;
  *) echo "usage: check.sh [all|build|sorry|axioms]" >&2; exit 2 ;;
esac
