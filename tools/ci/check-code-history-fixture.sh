#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Fails unless the code-history fixture only grew since <base>: its founding is unchanged and its
# recorded entries are a byte prefix of the ones now (T-037-12, amendment 0023 (f)).
#
# The fixture is a book's history recorded by earlier builds. The gate replays it under this build
# (crates/core-binding/tests/code_history.rs); a change that alters how a recorded input decides
# fails there. Re-recording the history would make any such change pass, so the recorded part
# may never change: an extension appends a range, nothing else. code-map.json is rewritten by every
# extension and is held to the replay instead.
#
# Usage: tools/ci/check-code-history-fixture.sh <base>   (a commit, branch or tag)
# Exit codes: 0 only appended (or no fixture at <base>), 1 the recorded part changed, 2 usage.
set -euo pipefail

base=${1:?usage: check-code-history-fixture.sh <base>}
dir=crates/core-binding/tests/fixtures/code-history
git rev-parse --verify --quiet "$base^{commit}" >/dev/null \
  || { echo "check-code-history-fixture: $base is not a commit here" >&2; exit 2; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

if ! git cat-file -e "$base:$dir/founding.bin" 2>/dev/null; then
  echo "check-code-history-fixture: no fixture at $base; nothing recorded to keep."
  exit 0
fi
for f in founding.bin entries.bin; do
  [[ -f $dir/$f ]] || { echo "check-code-history-fixture: $dir/$f was removed" >&2; exit 1; }
done

git show "$base:$dir/founding.bin" >"$tmp/founding.bin"
if ! cmp -s "$tmp/founding.bin" "$dir/founding.bin"; then
  echo "check-code-history-fixture: founding.bin changed since $base: the recorded book is another" >&2
  exit 1
fi

git show "$base:$dir/entries.bin" >"$tmp/entries.bin"
old=$(wc -c <"$tmp/entries.bin" | tr -d ' ')
new=$(wc -c <"$dir/entries.bin" | tr -d ' ')
if (( new < old )); then
  echo "check-code-history-fixture: entries.bin shrank since $base ($old -> $new bytes)" >&2
  exit 1
fi
head -c "$old" "$dir/entries.bin" >"$tmp/prefix.bin"
if ! cmp -s "$tmp/entries.bin" "$tmp/prefix.bin"; then
  echo "check-code-history-fixture: the entries recorded at $base were rewritten, not appended to" >&2
  exit 1
fi
echo "check-code-history-fixture: $old recorded bytes kept, $((new - old)) appended since $base."
