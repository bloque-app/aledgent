#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
#
# The build profile `aledgent-core-host code-manifest --profile` takes, for a core upgrade (#285): what
# the release says about how each artefact was built. It is a statement and is part of the code
# version (two builds of one source at different settings are two versions), so it is derived in one
# place, from facts, and never typed in.
#
#   tools/release/core-profile.sh <predecessor-report.json> <predecessor-revision> <source-revision> \
#                                 <roots> <out-dir>
#
#   predecessor-report.json  `check-code --out` for the code in force (its manifest as JSON)
#   predecessor-revision     the commit of the image whose host the node runs (its revision label)
#   source-revision          the commit this upgrade's package is built from
#   roots                    v1 or v2: the book's roots (smt-v1 is the package's one feature)
#   out-dir                  receives profile.json and the lock files it names
#
# - package: this build. Target and toolchain are the guest's (agents/core), the opt-level is the one
#   in agents/core/Cargo.toml, the lock is the committed agents/core/Cargo.lock that `--locked` builds
#   from, the revision is the commit.
# - runtime and native-decide: the code in force's. They are the predecessor's bytes (the runtime is
#   copied, the host is the one the node runs), so their profiles are the predecessor's, with the
#   lock file the host's workspace had at the image's revision (agents/Cargo.lock there; the runtime
#   is linked into the host's tree by `runtime-pvm`). A profile cannot be read back out of a binary:
#   this is the closest true statement, and it is the same on every run for the same inputs.
# - kernel: CipherClerk's pinned revision (tools/cipherclerk-patched.sh).
#
# Run from the repository root, at <source-revision>.
set -euo pipefail

if [ "$#" -ne 5 ]; then
  sed -n '/^#   tools/,/^#   out-dir/p' "$0" | sed 's/^# \{0,3\}//' >&2
  exit 2
fi
pred=$1 pred_rev=$2 src_rev=$3 roots=$4 out=$5
hex40='^[0-9a-f]{40}$'
[[ $pred_rev =~ $hex40 ]] || { echo "core-profile: the predecessor's revision is not a full commit: '$pred_rev'" >&2; exit 1; }
[[ $src_rev =~ $hex40 ]] || { echo "core-profile: the source revision is not a full commit: '$src_rev'" >&2; exit 1; }
case "$roots" in v1 | v2) ;; *) echo "core-profile: roots must be v1 or v2, not '$roots'" >&2; exit 1 ;; esac

toolchain=$(sed -n 's/^channel *= *"\(.*\)"/\1/p' agents/core/rust-toolchain.toml)
opt=$(awk '/^\[profile\.release\]/{f=1;next} /^\[/{f=0} f && /^opt-level/{gsub(/[" ]/,"",$3); print $3}' agents/core/Cargo.toml)
kernel=$(sed -n 's/^REV=\([0-9a-f]\{7\}\)[0-9a-f]*$/\1/p' tools/cipherclerk-patched.sh)
for v in "$toolchain" "$opt" "$kernel"; do
  [ -n "$v" ] || { echo "core-profile: could not read the toolchain, opt-level or kernel revision from the tree" >&2; exit 1; }
done

mkdir -p "$out"
cp agents/core/Cargo.lock "$out/core.Cargo.lock"
# The host's workspace at the image's revision. A revision that is not in this clone is an error, not
# a reason to write something else.
git show "$pred_rev:agents/Cargo.lock" >"$out/predecessor.Cargo.lock" \
  || { echo "core-profile: no agents/Cargo.lock at $pred_rev (fetch the history, or the image's revision label is wrong)" >&2; exit 1; }

jq -e '.manifest.artefacts | map(.role) | (index("runtime") != null and index("native-decide") != null)' "$pred" >/dev/null \
  || { echo "core-profile: $pred names no runtime or no native-decide artefact" >&2; exit 1; }

jq -n --slurpfile pred "$pred" --arg rev "$src_rev" --arg tc "$toolchain" --arg opt "$opt" \
      --arg roots "$roots" --arg kernel "$kernel" '
  ($pred[0].manifest.artefacts | map({key: .role, value: .profile}) | from_entries) as $p
  | def theirs($role): $p[$role]
      | {target, toolchain, opt_level, features, source_revision, cargo_lock: "predecessor.Cargo.lock"};
  {
    kernel: {revision: $kernel},
    package: {target: "riscv64em-vos", toolchain: $tc, opt_level: $opt,
              features: (if $roots == "v1" then ["smt-v1"] else [] end),
              source_revision: $rev, cargo_lock: "core.Cargo.lock"},
    runtime: theirs("runtime"),
    native_decide: theirs("native-decide")
  }' >"$out/profile.json"
echo "core-profile: $out/profile.json (package opt-level $opt on $toolchain, roots $roots)"
