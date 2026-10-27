#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
#
# Reads a core code set that .github/workflows/core-release.yml published (#285) back from its image, and
# checks it the way it was written: the record is a code set's, every file the record names is there,
# named by its own blake2b-256 digest, of the length the record says, and nothing else is.
#
#   tools/release/core-code-read.sh <image-ref> <out-dir>
#
# <out-dir> (created) receives manifest.bin, core.vos and agent_runtime.pvm (the set's manifest, package
# and runtime), and set.json. The code version is printed on the last line. The host checks the bytes
# against the manifest again (`check-code`) before anything is built on them.
set -euo pipefail

if [ "$#" -ne 2 ]; then
  sed -n '/^#   tools/,/^#   tools/p' "$0" | sed 's/^# \{0,3\}//' >&2
  exit 2
fi
ref=$1 out=$2
[ ! -e "$out" ] || { echo "core-code-read: $out exists already" >&2; exit 1; }
raw=$(mktemp -d)
# The layer's directory is read-only: make it removable before it is removed.
trap 'chmod -R u+w "$raw"; rm -rf "$raw"' EXIT

crane export "$ref" - | tar -x -C "$raw"
rec="$raw/set.json"
[ -f "$rec" ] && [ ! -L "$rec" ] || { echo "core-code-read: $ref has no set.json: it is not a core code set" >&2; exit 1; }
jq -e '.format == "aledgent/core-code-set/v1" and (.code | test("^[0-9a-f]{64}$")) and (.files | length == 3)
       and ([.files[].role] | sort == ["manifest", "package", "runtime"])' "$rec" >/dev/null \
  || { echo "core-code-read: $ref's set.json is not a core code set of this format" >&2; exit 1; }

b2() { b2sum -l 256 "$1" | cut -d' ' -f1; }
for role in manifest package runtime; do
  digest=$(jq -er --arg r "$role" '.files[] | select(.role == $r) | .digest' "$rec")
  len=$(jq -er --arg r "$role" '.files[] | select(.role == $r) | .len' "$rec")
  [[ $digest =~ ^[0-9a-f]{64}$ ]] || { echo "core-code-read: the $role's digest is '$digest'" >&2; exit 1; }
  f="$raw/code/$digest"
  if [ ! -f "$f" ] || [ -L "$f" ]; then echo "core-code-read: the $role ($digest) is not in the image" >&2; exit 1; fi
  [ "$(b2 "$f")" = "$digest" ] || { echo "core-code-read: the $role is not $digest" >&2; exit 1; }
  [ "$(wc -c <"$f" | tr -d ' ')" = "$len" ] || [ "$role" = manifest ] || { echo "core-code-read: the $role is not $len bytes" >&2; exit 1; }
done
# Nothing but the set's three digests, once each.
found=$(find "$raw/code" -mindepth 1 -maxdepth 1 -exec basename {} \; | sort)
named=$(jq -r '.files[].digest' "$rec" | sort -u)
[ "$found" = "$named" ] || { echo "core-code-read: the image holds files the record does not name" >&2; exit 1; }
[ "$(jq -r '.files[] | select(.role == "manifest") | .digest' "$rec")" = "$(jq -r .code "$rec")" ] \
  || { echo "core-code-read: the manifest is not the code version" >&2; exit 1; }

mkdir -p "$out"
file() { jq -er --arg r "$1" '.files[] | select(.role == $r) | .digest' "$rec"; }
cp "$raw/code/$(file manifest)" "$out/manifest.bin"
cp "$raw/code/$(file package)" "$out/core.vos"
cp "$raw/code/$(file runtime)" "$out/agent_runtime.pvm"
cp "$rec" "$out/set.json"
jq -r .code "$rec"
