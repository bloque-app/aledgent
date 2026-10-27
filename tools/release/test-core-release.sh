#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
#
# Tests of the core release's scripts (.github/workflows/core-release.yml; #285): the layout of a code
# set (core-code-set.sh), the layer it is published as (pack-code-layer.py), and the workflow's
# promise that it never moves the core image. Fixtures only: no host, no registry, no Docker.
#
#   tools/release/test-core-release.sh
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

b2() { b2sum -l 256 "$1" | cut -d' ' -f1; }
fail() { echo "FAIL: $*" >&2; exit 1; }
# Runs a command that must be refused, with a message that names why.
refuses() { # <why (regex)> <command...>
  local why=$1 out; shift
  if out=$("$@" 2>&1); then fail "accepted, expected a refusal matching '$why': $*"; fi
  grep -Eq "$why" <<<"$out" || fail "refused for another reason than '$why': $out"
}

# --- fixtures: three files, and the report a host would print for them ------------------------
fx="$work/fx"; mkdir -p "$fx"
head -c 3000 /dev/urandom >"$fx/package"
head -c 1200 /dev/urandom >"$fx/runtime"
head -c 800 /dev/urandom >"$fx/host"
head -c 700 /dev/urandom >"$fx/manifest"
version=$(b2 "$fx/manifest")
report() { # <out> [package digest] [runtime len]
  jq -n --arg v "$version" --arg pkg "${2:-$(b2 "$fx/package")}" --arg rt "$(b2 "$fx/runtime")" \
        --arg host "$(b2 "$fx/host")" --argjson pkglen "$(wc -c <"$fx/package" | tr -d ' ')" \
        --argjson rtlen "${3:-$(wc -c <"$fx/runtime" | tr -d ' ')}" '
    {version: $v, writer: "core", generation: 1,
     platform: {name: "vos-saga", revision: "bd8d1bb70551391a4c80a96c2249313ab7e0a0fc", identities: []},
     artefacts: [
       {role: "package", digest: $pkg, len: $pkglen, profile: {source_revision: "abc"}},
       {role: "runtime", digest: $rt, len: $rtlen, profile: {source_revision: "def"}},
       {role: "native-decide", digest: $host, len: 800, profile: {source_revision: "ghi"}}]}' >"$1"
}
report "$fx/report.json"
set_sh="$here/core-code-set.sh"

# --- a good set ----------------------------------------------------------------------------------
"$set_sh" "$fx/report.json" "$fx/manifest" "$fx/package" "$fx/runtime" "$work/set" "$work/set.json" >/dev/null
[ "$(find "$work/set" -type f | wc -l | tr -d ' ')" = 3 ] || fail "a set holds three files"
for f in "$work"/set/*; do [ "$(b2 "$f")" = "$(basename "$f")" ] || fail "$f is not named by its digest"; done
[ -f "$work/set/$version" ] || fail "the manifest is not named by the code version"
jq -e --arg v "$version" '.code == $v and .generation == 1 and (.files | length == 3)
  and .platform.name == "vos-saga" and .source_revision == "abc"' "$work/set.json" >/dev/null || fail "set.json"
mode=$(python3 -I -c 'import os, stat, sys; print(oct(stat.S_IMODE(os.stat(sys.argv[1]).st_mode))[2:])' "$work/set/$version")
[ "$mode" = 444 ] || fail "files are read-only (mode $mode)"

# --- refusals ------------------------------------------------------------------------------------
refuses "exists already" "$set_sh" "$fx/report.json" "$fx/manifest" "$fx/package" "$fx/runtime" "$work/set" "$work/x.json"
report "$work/wrong-pkg.json" "$(printf '%064d' 7)"
refuses "package .* hashes to" "$set_sh" "$work/wrong-pkg.json" "$fx/manifest" "$fx/package" "$fx/runtime" "$work/o1" "$work/x.json"
report "$work/wrong-len.json" "" 1199
refuses "runtime .* is [0-9]+ bytes, the host reports 1199" "$set_sh" "$work/wrong-len.json" "$fx/manifest" "$fx/package" "$fx/runtime" "$work/o2" "$work/x.json"
refuses "manifest .* hashes to" "$set_sh" "$fx/report.json" "$fx/host" "$fx/package" "$fx/runtime" "$work/o3" "$work/x.json"
jq 'del(.artefacts[] | select(.role == "runtime"))' "$fx/report.json" >"$work/no-runtime.json"
refuses "one package and one runtime" "$set_sh" "$work/no-runtime.json" "$fx/manifest" "$fx/package" "$fx/runtime" "$work/o4" "$work/x.json"
jq '.version = "ABC"' "$fx/report.json" >"$work/bad-version.json"
refuses "one package and one runtime" "$set_sh" "$work/bad-version.json" "$fx/manifest" "$fx/package" "$fx/runtime" "$work/o5" "$work/x.json"
ln -s "$fx/package" "$fx/package-link"
refuses "not a regular file" "$set_sh" "$fx/report.json" "$fx/manifest" "$fx/package-link" "$fx/runtime" "$work/o6" "$work/x.json"
head -c 70000 /dev/urandom >"$work/big-manifest"
jq --arg v "$(b2 "$work/big-manifest")" '.version = $v' "$fx/report.json" >"$work/big.json"
refuses "64 KiB" "$set_sh" "$work/big.json" "$work/big-manifest" "$fx/package" "$fx/runtime" "$work/o7" "$work/x.json"

# --- the layer: the same bytes every time, and only digest-named files ----------------------------
pack="$here/pack-code-layer.py"
python3 -I "$pack" "$work/set" "$work/a.tar"
sleep 1
touch "$work/set"/*   # other mtimes must not matter
python3 -I "$pack" "$work/set" "$work/b.tar"
cmp "$work/a.tar" "$work/b.tar" || fail "the layer is not reproducible"
members=$( { echo code/; for f in "$work"/set/*; do echo "code/$(basename "$f")"; done; } | sort | tr '\n' ' ')
[ "$(tar -tf "$work/a.tar" | sort | tr '\n' ' ')" = "$members" ] || fail "layer members"
python3 -I "$pack" "$work/set" "$work/e.tar" "$work/set.json"
[ "$(tar -tf "$work/e.tar" | grep -c .)" = 5 ] || fail "the layer with a record"
tar -tf "$work/e.tar" | grep -qx set.json || fail "the layer with a record"
tar -xOf "$work/e.tar" set.json | cmp - "$work/set.json" || fail "the record in the layer"
mkdir "$work/bad-set"; cp "$fx/package" "$work/bad-set/notadigest"
refuses "named by a digest" python3 -I "$pack" "$work/bad-set" "$work/c.tar"
mkdir "$work/empty-set"
refuses "is empty" python3 -I "$pack" "$work/empty-set" "$work/d.tar"

# --- the build profile ------------------------------------------------------------------------------
cd "$repo"
head_rev=$(git rev-parse HEAD)
jq -n '{manifest: {artefacts: [
  {role: "package", profile: {target: "riscv64em-vos", toolchain: "t", opt_level: "s", features: [], source_revision: "x", cargo_lock: "0"}},
  {role: "runtime", profile: {target: "riscv64em-vos", toolchain: "nightly-2026-03-20", opt_level: "3", features: [], source_revision: "r", cargo_lock: "0"}},
  {role: "native-decide", profile: {target: "x86_64-unknown-linux-gnu", toolchain: "1.98.0", opt_level: "3", features: ["a"], source_revision: "h", cargo_lock: "0"}}]}}' >"$work/pred.json"
"$here/core-profile.sh" "$work/pred.json" "$head_rev" "$head_rev" v2 "$work/profile" >/dev/null
jq -e --arg rev "$head_rev" '
  .package.source_revision == $rev and .package.target == "riscv64em-vos" and .package.features == []
  and .package.cargo_lock == "core.Cargo.lock" and .runtime.opt_level == "3"
  and .runtime.cargo_lock == "predecessor.Cargo.lock" and .native_decide.features == ["a"]
  and (.kernel.revision | test("^[0-9a-f]{7}$")) and (.package.toolchain | test("^nightly-"))' "$work/profile/profile.json" >/dev/null || fail "the profile"
cmp "$work/profile/core.Cargo.lock" agents/core/Cargo.lock || fail "the package's lock"
"$here/core-profile.sh" "$work/pred.json" "$head_rev" "$head_rev" v1 "$work/profile1" >/dev/null
jq -e '.package.features == ["smt-v1"]' "$work/profile1/profile.json" >/dev/null || fail "v1 roots"
refuses "roots must be" "$here/core-profile.sh" "$work/pred.json" "$head_rev" "$head_rev" v3 "$work/p3"
refuses "not a full commit" "$here/core-profile.sh" "$work/pred.json" abc "$head_rev" v2 "$work/p4"
refuses "no agents/Cargo.lock at" "$here/core-profile.sh" "$work/pred.json" 0000000000000000000000000000000000000000 "$head_rev" v2 "$work/p5"
jq 'del(.manifest.artefacts[] | select(.role == "runtime"))' "$work/pred.json" >"$work/pred-no-rt.json"
refuses "no runtime" "$here/core-profile.sh" "$work/pred-no-rt.json" "$head_rev" "$head_rev" v2 "$work/p6"

# --- the workflow's promises ----------------------------------------------------------------------
wf="$repo/.github/workflows/core-release.yml"
# It never moves the core image: nothing in it pushes with a build step or `docker push`, the only
# registry writes are crane's, aimed at the code set's repository, and no tag of the image is named.
! grep -qE 'push: *true|docker (image )?push|docker/login-action' "$wf" || fail "the workflow pushes an image"
writes=$(grep -nE 'crane (append|copy|cp|tag|push|mutate|delete|rebase)' "$wf" | grep -vE '^[0-9]+: *#' || true)
# shellcheck disable=SC2016  # '"$ref"' is the workflow's text, not ours
if [ -n "$writes" ] && grep -vF -e '"$ref"' -e CODE_REPO <<<"$writes" >/dev/null; then
  echo "$writes" >&2; fail "the workflow writes to a registry somewhere but the code set's repository"
fi
# The publish script writes to the one repository it is given, never to a tag of anything else.
pub="$here/core-code-publish.sh"
[ "$(grep -cE '^ *crane (append|copy|cp|tag|push|mutate|delete)' "$pub")" = 1 ] || fail "core-code-publish.sh writes in more than one place"
# shellcheck disable=SC2016  # the script's text, not ours
grep -qE '^ *crane append -f "\$layer" -t "\$ref" >&2$' "$pub" || fail "core-code-publish.sh does not append to its own tag"
! grep -qE 'ghcr\.io/bloque-app/aledgent-core:' "$wf" || fail "the workflow names a tag of the core image"
! grep -qE '^[^#]*(aws-actions|configure-aws-credentials|role-to-assume|ssm send-command|aws )' "$wf" \
  || fail "the workflow reaches an environment"
! grep -qE 'repository_dispatch|dispatches|gh workflow run|--method POST' "$wf" || fail "the workflow starts something elsewhere"
grep -qE '^permissions: \{\}$' "$wf" || fail "the workflow's default permissions are not empty"
echo "ok: core release scripts"
