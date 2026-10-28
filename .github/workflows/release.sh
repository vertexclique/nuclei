#!/usr/bin/env bash
# Makes the GitHub release of a version tag, as the Release workflow does when `cargo release
# --execute` pushes one.
#   bash .github/workflows/release.sh v0.5.0             makes the release (needs GH_TOKEN)
#   bash .github/workflows/release.sh v0.5.0 --dry-run   prints its name and notes only
# The release is named `v0.5.0 "Codename"`: the codename is the first word of
# .github/release-codenames.txt that no release's name has used yet, so none is used twice; when
# every one was used the script fails and says so. Its notes are the version's section of
# CHANGELOG.md (the `## ` heading that names the version, to the next one) and every commit since
# the previous version tag, with a link to the whole diff. A tag with a pre-release part
# (v0.6.0-rc.1) makes a pre-release. A release that exists already is left as it is.
set -euo pipefail
here="$(cd "$(dirname "$0")/../.." && pwd)"
tag="${1:?usage: release.sh <tag> [--dry-run]}"
dry="${2:-}"
version="${tag#v}"
repo="${GITHUB_REPOSITORY:-vertexclique/nuclei}"

if [ "$dry" != "--dry-run" ] && gh release view "$tag" >/dev/null 2>&1; then
  echo "the release of $tag exists; left as it is"
  exit 0
fi

# The codenames every release used: the last word of each release's name, unquoted.
used="$(gh release list --limit 1000 --json name --jq '.[].name' 2>/dev/null \
  | awk 'NF {print tolower($NF)}' | tr -d '"')"
codename=""
while IFS= read -r word; do
  case "$word" in ''|'#'*) continue ;; esac
  if ! grep -qxF "$(printf '%s' "$word" | tr '[:upper:]' '[:lower:]')" <<<"$used"; then
    codename="$word"
    break
  fi
done < "$here/.github/release-codenames.txt"
if [ -z "$codename" ]; then
  echo "every codename in .github/release-codenames.txt was used by a release: add new ones at its end" >&2
  exit 1
fi

notes="$(mktemp)"
trap 'rm -f "$notes"' EXIT
pattern="$(printf '%s' "$version" | sed 's/[.]/[.]/g')"
section="$(awk -v re="(^|[^0-9.])${pattern}([^0-9.]|\$)" '
  /^## / { if (on) exit; if ($0 ~ re) { on = 1; next } }
  on' "$here/CHANGELOG.md")"
prev="$(git -C "$here" describe --tags --abbrev=0 --match 'v[0-9]*' "$tag^" 2>/dev/null || true)"
range="$tag"
[ -n "$prev" ] && range="$prev..$tag"
# A release's notes hold at most 125000 characters: past this many commits the list stops, and
# says so, with the full diff linked.
most=1500
commits="$(git -C "$here" rev-list --no-merges --count "$range")"
{
  if [ -n "$section" ]; then
    printf '%s\n' "$section"
  else
    printf 'CHANGELOG.md has no section for %s.\n' "$version"
  fi
  printf '\n## Changes\n\n'
  git -C "$here" log --no-merges --format='- %s (%h)' -n "$most" "$range"
  if [ "$commits" -gt "$most" ]; then
    printf -- '- ... and %d more commits, in the full diff below\n' "$((commits - most))"
  fi
  if [ -n "$prev" ]; then
    printf '\nFull diff: https://github.com/%s/compare/%s...%s\n' "$repo" "$prev" "$tag"
  fi
} > "$notes"

title="$tag \"$codename\""
pre=()
case "$version" in *-*) pre=(--prerelease) ;; esac
if [ "$dry" = "--dry-run" ]; then
  echo "title: $title ${pre[*]}"
  cat "$notes"
  exit 0
fi
gh release create "$tag" --verify-tag --title "$title" --notes-file "$notes" "${pre[@]}"
echo "released $title"
