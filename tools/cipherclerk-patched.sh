#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
# CipherClerk for the core's guest (F-005): e9a7b08 with proposals/cipherclerk/0001 and 0003-0005 applied.
#
# At e9a7b08, `cipher-clerk/signing` pulls getrandom, which does not build for the VOS guest
# target (`riscv64em-vos`; notes/MEASUREMENTS-F002.md §5). Patch 0001 takes randomness as a
# parameter and moves OsRng behind a new `os-rng` feature. It is not upstream yet and there is
# no CipherClerk fork, so this script makes the patched tree reproducibly, without one:
#
#   1. clone CipherClerk at the pinned commit into `.cipherclerk-patched/` (gitignored);
#   2. `git am` every `proposals/cipherclerk/0001*.patch` onto it.
#
# The agents workspace (`agents/Cargo.toml`) points its `[patch]` at that folder. The root
# workspace is untouched: it still builds CipherClerk e9a7b08 from Codeberg.
#
# TODO(F-025): once patch 0001 is merged upstream (T-025-07), pin the upstream commit in every
# manifest and delete this script and the `[patch]` section.
#
#   tools/cipherclerk-patched.sh            # idempotent: does nothing if the tree is current
#   CIPHERCLERK_SRC=/path/to/clone tools/cipherclerk-patched.sh   # clone from a local mirror
set -euo pipefail

REV=e9a7b0851f183c14dbd1635dbf0437c86682ad6f
SRC=${CIPHERCLERK_SRC:-https://codeberg.org/Virto/CipherClerk.git}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
DEST=$ROOT/.cipherclerk-patched
# 0001 (randomness as a parameter), then the guest's performance patches 0003-0005 (VOS's BLAKE2b,
# the static empty-subtree chain, the current precompile shim). 0002 is upstream review only.
PATCHES=("$ROOT"/proposals/cipherclerk/0001*.patch "$ROOT"/proposals/cipherclerk/000[3-5]*.patch)

# The patched tree is identified by the base commit and the patches' contents.
stamp=$( { echo "$REV"; cat "${PATCHES[@]}"; } | shasum -a 256 | cut -d' ' -f1)
if [ -f "$DEST/.aledgent-stamp" ] && [ "$(cat "$DEST/.aledgent-stamp")" = "$stamp" ]; then
  echo "cipherclerk-patched: $DEST is current ($REV + ${#PATCHES[@]} patch)"
  exit 0
fi

rm -rf "$DEST"
git init -q "$DEST"
git -C "$DEST" fetch -q --depth 1 "$SRC" "$REV"
git -C "$DEST" checkout -q FETCH_HEAD
# `git am` needs an identity for the commits it writes; the patches keep their own authors.
git -C "$DEST" -c user.name=aledgent -c user.email=aledgent@invalid \
  am -q --committer-date-is-author-date "${PATCHES[@]}"
echo "$stamp" > "$DEST/.aledgent-stamp"
echo "cipherclerk-patched: $DEST at $(git -C "$DEST" rev-parse --short HEAD) ($REV + ${#PATCHES[@]} patch)"
