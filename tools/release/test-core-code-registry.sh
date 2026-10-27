#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
#
# The publish and read paths of a core code set (core-code-publish.sh, core-code-read.sh; #285) against a
# real registry on this machine: publish, publish the same set again (nothing pushed, same digest), refuse
# a different set under the same code version, read it back, and refuse an image that is not a code set
# or whose files do not match their names. Needs docker, crane, jq and b2sum; skips without docker or crane.
#
#   tools/release/test-core-code-registry.sh
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
for tool in docker crane; do
  command -v "$tool" >/dev/null || { echo "skip: $tool is not installed"; exit 0; }
done
docker info >/dev/null 2>&1 || { echo "skip: no docker daemon"; exit 0; }
work=$(mktemp -d)
name="core-code-registry-test-$$"
cleanup() { docker rm -f "$name" >/dev/null 2>&1 || true; rm -rf "$work"; }
trap cleanup EXIT
fail() { echo "FAIL: $*" >&2; exit 1; }
b2() { b2sum -l 256 "$1" | cut -d' ' -f1; }

docker run -d --name "$name" -p 127.0.0.1::5000 registry:2 >/dev/null
port=$(docker port "$name" 5000/tcp | head -1 | sed 's/.*://')
repo="localhost:$port/aledgent-core-code"
for _ in $(seq 1 30); do curl -fsS "http://localhost:$port/v2/" >/dev/null 2>&1 && break; sleep 1; done

# A set, as the workflow writes one.
fx="$work/fx"; mkdir -p "$fx"
for f in package runtime manifest; do head -c 1500 /dev/urandom >"$fx/$f"; done
version=$(b2 "$fx/manifest")
jq -n --arg v "$version" --arg pkg "$(b2 "$fx/package")" --arg rt "$(b2 "$fx/runtime")" '
  {version: $v, generation: 1, platform: {name: "vos-saga", revision: "r", identities: []},
   artefacts: [{role: "package", digest: $pkg, len: 1500, profile: {source_revision: "s"}},
               {role: "runtime", digest: $rt, len: 1500, profile: {source_revision: "s"}}]}' >"$fx/report.json"
"$here/core-code-set.sh" "$fx/report.json" "$fx/manifest" "$fx/package" "$fx/runtime" "$work/set" "$work/set.json" >/dev/null
python3 -I "$here/pack-code-layer.py" "$work/set" "$work/layer.tar" "$work/set.json"

d1=$("$here/core-code-publish.sh" "$repo" "$version" "$work/set" "$work/layer.tar" 2>"$work/p1.log")
[[ $d1 =~ ^sha256:[0-9a-f]{64}$ ]] || fail "publish answered '$d1'"
d2=$("$here/core-code-publish.sh" "$repo" "$version" "$work/set" "$work/layer.tar" 2>"$work/p2.log")
[ "$d1" = "$d2" ] || fail "publishing the same set again changed the digest"
grep -q "nothing pushed" "$work/p2.log" || fail "the second publish pushed"

# A different set under the same code version is refused (a code version's files never change).
mkdir "$work/other"; head -c 1500 /dev/urandom >"$work/other/$(head -c 32 /dev/urandom | xxd -p -c 64)"
python3 -I "$here/pack-code-layer.py" "$work/other" "$work/other.tar"
if "$here/core-code-publish.sh" "$repo" "$version" "$work/other" "$work/other.tar" >/dev/null 2>"$work/p3.log"; then fail "a different set was accepted"; fi
grep -q "never change" "$work/p3.log" || fail "refused for another reason: $(cat "$work/p3.log")"

# Read it back, as the workflow reads a predecessor.
got=$("$here/core-code-read.sh" "$repo@$d1" "$work/read")
[ "$got" = "$version" ] || fail "read answered '$got'"
cmp "$work/read/manifest.bin" "$fx/manifest" || fail "the manifest read back differs"
cmp "$work/read/core.vos" "$fx/package" || fail "the package read back differs"
cmp "$work/read/agent_runtime.pvm" "$fx/runtime" || fail "the runtime read back differs"
cmp "$work/read/set.json" "$work/set.json" || fail "the record read back differs"

# An image that is not a code set, and one whose file is not what its name says.
printf 'x' >"$work/junk"; tar -C "$work" -cf "$work/junk.tar" junk
crane append -f "$work/junk.tar" -t "$repo:junk" >/dev/null
if "$here/core-code-read.sh" "$repo:junk" "$work/r2" 2>"$work/r2.log"; then fail "a junk image was read"; fi
grep -q "not a core code set" "$work/r2.log" || fail "junk refused for another reason: $(cat "$work/r2.log")"
mkdir -p "$work/bad/code"; cp -r "$work/set/." "$work/bad/code/"; chmod -R u+w "$work/bad"
echo tamper >>"$work/bad/code/$(jq -r '.files[] | select(.role == "package") | .digest' "$work/set.json")"
cp "$work/set.json" "$work/bad/set.json"
tar -C "$work/bad" -cf "$work/bad.tar" code set.json
crane append -f "$work/bad.tar" -t "$repo:bad" >/dev/null
if "$here/core-code-read.sh" "$repo:bad" "$work/r3" 2>"$work/r3.log"; then fail "a tampered set was read"; fi
grep -q "is not" "$work/r3.log" || fail "tampered set refused for another reason: $(cat "$work/r3.log")"
echo "ok: core code set publish and read, against a registry"
