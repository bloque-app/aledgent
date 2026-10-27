#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
#
# Lays out one core code version as the `fs` code source reads it (agents/core-host/src/code_source.rs,
# T-037-06): one regular file per artefact, named by its blake2b-256 digest in 64 lowercase hex digits
# and by nothing else. The set is the manifest (named by the code version H, which is what
# `Upgrade { code, from }` names), the package and the runtime. Bytes are never trusted on the way
# in or out: every file is hashed here, with a tool that shares no code with the host, and must be the
# digest and the length the host's own report (`code-manifest`'s output) gives for it. The core host
# checks every byte again when it fetches them, so what this protects is the release, not the node.
#
#   tools/release/core-code-set.sh <report.json> <manifest> <package> <runtime> <out-dir> <set.json>
#
#   report.json   what `aledgent-core-host code-manifest` printed (the manifest as JSON)
#   out-dir       created; holds exactly the three files
#   set.json      written: the record of the release (code version, generation, platform, files)
set -euo pipefail

if [ "$#" -ne 6 ]; then
  sed -n '/^#   tools/,/^#   set.json/p' "$0" | sed 's/^# \{0,3\}//' >&2
  exit 2
fi
report=$1 manifest=$2 package=$3 runtime=$4 out=$5 record=$6

command -v b2sum >/dev/null || { echo "core-code-set: b2sum (coreutils) is required" >&2; exit 1; }
command -v jq >/dev/null || { echo "core-code-set: jq is required" >&2; exit 1; }
for f in "$report" "$manifest" "$package" "$runtime"; do
  [ -f "$f" ] && [ ! -L "$f" ] || { echo "core-code-set: $f is not a regular file" >&2; exit 1; }
done
[ ! -e "$out" ] || { echo "core-code-set: $out exists already" >&2; exit 1; }

hex64='^[0-9a-f]{64}$'
b2() { b2sum -l 256 "$1" | cut -d' ' -f1; }
size() { wc -c <"$1" | tr -d ' '; }

# The report must be a manifest of amendment 0033's format: a version, a generation, a platform, and
# exactly one package and one runtime (a native-decide host is recorded in the manifest and is not
# part of what a node fetches: staging reads the package and the runtime only).
jq -e '
  (.version | type == "string" and test("^[0-9a-f]{64}$"))
  and (.generation | type == "number")
  and (.platform.name | type == "string") and (.platform.revision | type == "string")
  and ([.artefacts[] | select(.role == "package")] | length == 1)
  and ([.artefacts[] | select(.role == "runtime")] | length == 1)
' "$report" >/dev/null || { echo "core-code-set: $report is not a code manifest report with one package and one runtime" >&2; exit 1; }

want() { jq -r --arg role "$1" --arg field "$2" '.artefacts[] | select(.role == $role) | .[$field]' "$report"; }

version=$(jq -r .version "$report")
pkg_digest=$(want package digest) pkg_len=$(want package len)
rt_digest=$(want runtime digest) rt_len=$(want runtime len)
for d in "$version" "$pkg_digest" "$rt_digest"; do
  [[ $d =~ $hex64 ]] || { echo "core-code-set: '$d' is not a lowercase blake2b-256 digest" >&2; exit 1; }
done

check() { # <what> <file> <digest> <len>
  local got len
  got=$(b2 "$2") len=$(size "$2")
  if [ "$got" != "$3" ]; then
    echo "core-code-set: the $1 ($2) hashes to $got, the host reports $3" >&2; exit 1
  fi
  if [ -n "$4" ] && [ "$len" != "$4" ]; then
    echo "core-code-set: the $1 ($2) is $len bytes, the host reports $4" >&2; exit 1
  fi
}
# The manifest's digest is the code version; its length is not part of the report.
check "manifest" "$manifest" "$version" ""
check "package" "$package" "$pkg_digest" "$pkg_len"
check "runtime" "$runtime" "$rt_digest" "$rt_len"
if [ "$(size "$manifest")" -gt 65536 ]; then
  echo "core-code-set: the manifest is over the 64 KiB the host reads for one" >&2; exit 1
fi

umask 022
mkdir -p "$out"
cp "$manifest" "$out/$version"
cp "$package" "$out/$pkg_digest"
# The runtime of an upgrade is the code in force's, so it can be the same file as another version's:
# one file per digest, whatever many roles name it.
cp "$runtime" "$out/$rt_digest"
chmod 0444 "$out"/*

# Exactly the files that were meant, each named by its own digest.
names=$(find "$out" -mindepth 1 -maxdepth 1 -exec basename {} \; | sort)
expected=$(printf '%s\n' "$version" "$pkg_digest" "$rt_digest" | sort -u)
[ "$names" = "$expected" ] || { echo "core-code-set: $out holds unexpected files: $names" >&2; exit 1; }
for f in "$out"/*; do
  [ "$(b2 "$f")" = "$(basename "$f")" ] || { echo "core-code-set: $f is not named by its digest" >&2; exit 1; }
done

jq -n --slurpfile report "$report" --arg package "$pkg_digest" --arg runtime "$rt_digest" \
      --argjson pkg_len "$pkg_len" --argjson rt_len "$rt_len" --argjson man_len "$(size "$manifest")" '
  $report[0] as $r | {
    format: "aledgent/core-code-set/v1",
    code: $r.version,
    generation: $r.generation,
    platform: {name: $r.platform.name, revision: $r.platform.revision},
    files: [
      {role: "manifest", digest: $r.version, len: $man_len},
      {role: "package", digest: $package, len: $pkg_len},
      {role: "runtime", digest: $runtime, len: $rt_len}
    ],
    native_decide: ([$r.artefacts[] | select(.role == "native-decide") | .digest] | first // null),
    source_revision: ([$r.artefacts[] | select(.role == "package") | .profile.source_revision] | first)
  }' >"$record"
echo "core-code-set: code $version, generation $(jq -r .generation "$record"), 3 files in $out"
