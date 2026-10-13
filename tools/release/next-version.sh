#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
#
# The version the next release would have, computed the way release-please computes it with this
# repository's configuration (release-please-config.json): from the last release, and the
# conventional-commit subjects of the commits after it.
#
#   tools/release/next-version.sh [--title <subject>] [--print version|release-type|baseline] [<rev>]
#
#   version        (the default) `X.Y.Z`, the next release
#   release-type   `major`, `minor` or `patch`: how the next release differs from the last, which
#                  cargo-semver-checks takes as `--release-type` (.github/workflows/release-checks.yml)
#   baseline       the last release's tag, or the configuration's `bootstrap-sha` before the first:
#                  what cargo-semver-checks compares with (`--baseline-rev`)
#
# `<rev>` defaults to HEAD. `--title` counts one more subject as if it were a commit after `<rev>`: a
# pull request's title, which is the subject its squash merge will have.
#
# The last release is the highest `vX.Y.Z` tag that `<rev>` contains (pre-release tags such as
# `v0.2.0-dev+abc1234` are not releases); with none, the version in .release-please-manifest.json,
# counted from the configuration's `bootstrap-sha`. The commits are those of `<rev>` that the release
# does not contain, merges left out: a pull request lands as one squashed commit whose subject is its
# title.
#
#   a breaking change (`type!:` or a `BREAKING CHANGE:` footer)   major; minor before 1.0.0
#   a `feat`                                                     minor
#   anything else, or nothing                                    patch
#
# (`bump-minor-pre-major: true`, `bump-patch-for-minor-pre-major: false`.) release-please cuts a
# release only for releasable commits (`feat`, `fix`, ...); this always answers, so a pre-release
# built from `chore` commits alone names the patch release after the last. The history must be
# complete: run it in a clone with every commit and tag (fetch-depth: 0).
set -euo pipefail

title=""
print=version
rev=HEAD
while [ $# -gt 0 ]; do
  case "$1" in
    --title) title="$2"; shift 2 ;;
    --print) print="$2"; shift 2 ;;
    -*) echo "next-version.sh: unknown option $1" >&2; exit 2 ;;
    *) rev="$1"; shift ;;
  esac
done
case "$print" in version|release-type|baseline) ;; *) echo "next-version.sh: --print $print?" >&2; exit 2 ;; esac

root="$(git rev-parse --show-toplevel)"

last_tag="$(git tag --merged "$rev" --list 'v*' | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | sort -V | tail -n 1 || true)"
if [ -n "$last_tag" ]; then
  last="${last_tag#v}"
  base="$last_tag"
else
  # As `<rev>` has them; from the checkout when it has none yet (the pull request adding them).
  at_rev() { git show "$rev:$1" 2>/dev/null || cat "$root/$1"; }
  last="$(at_rev .release-please-manifest.json | jq -er '."."')"
  base="$(at_rev release-please-config.json | jq -er '."bootstrap-sha"')"
fi
if [ "$print" = baseline ]; then
  echo "$base"
  exit 0
fi
IFS=. read -r major minor patch <<<"$last"

breaking=0
feat=0
classify() {
  local message="$1" subject="${1%%$'\n'*}"
  if grep -qE '^[a-z]+(\([^)]*\))?!:' <<<"$subject" || grep -qE '^BREAKING[ -]CHANGE:' <<<"$message"; then
    breaking=1
  elif grep -qE '^feat(\([^)]*\))?:' <<<"$subject"; then
    feat=1
  fi
}
while IFS= read -r -d '' message; do
  classify "$message"
done < <(git log -z --no-merges --format=%B "$base..$rev")
if [ -n "$title" ]; then
  classify "$title"
fi

if [ "$breaking" = 1 ] && [ "$major" -ge 1 ]; then
  next="$((major + 1)).0.0" type=major
elif [ "$breaking" = 1 ] || [ "$feat" = 1 ]; then
  next="$major.$((minor + 1)).0" type=minor
else
  next="$major.$minor.$((patch + 1))" type=patch
fi
# cargo-semver-checks reads a 0.y.z release as Cargo does: a minor bump there may break, so a
# breaking change before 1.0.0 is a `minor` release-type, as release-please makes it.
case "$print" in
  version) echo "$next" ;;
  release-type) echo "$type" ;;
esac
