#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
# CipherClerk for the vos-core spike's guest (W2): e9a7b08 with proposals/cipherclerk/0001 and
# 0003-0005, as tools/cipherclerk-patched.sh makes it, plus this spike's patch
# (patches/0007-vos-core-blake2b.patch), which hashes through vos-core's precompile 0x400.
#
# The patches are applied with `git apply`, so nothing is committed and no identity is needed.
#
#   spikes/vos-core/cipherclerk.sh     # idempotent
set -euo pipefail

REV=e9a7b0851f183c14dbd1635dbf0437c86682ad6f
SRC=${CIPHERCLERK_SRC:-https://codeberg.org/Virto/CipherClerk.git}
HERE=$(cd "$(dirname "$0")" && pwd)
ROOT=$(cd "$HERE/../.." && pwd)
DEST=$HERE/.cipherclerk
PATCHES=("$ROOT"/proposals/cipherclerk/0001*.patch "$ROOT"/proposals/cipherclerk/000[3-5]*.patch "$HERE"/patches/0007-vos-core-blake2b.patch)

stamp=$( { echo "$REV"; cat "${PATCHES[@]}"; } | shasum -a 256 | cut -d' ' -f1)
if [ -f "$DEST/.spike-stamp" ] && [ "$(cat "$DEST/.spike-stamp")" = "$stamp" ]; then
  echo "cipherclerk: $DEST is current"
  exit 0
fi
rm -rf "$DEST"
git init -q "$DEST"
git -C "$DEST" fetch -q --depth 1 "$SRC" "$REV"
git -C "$DEST" checkout -q FETCH_HEAD
for p in "${PATCHES[@]}"; do git -C "$DEST" apply "$p"; done
echo "$stamp" > "$DEST/.spike-stamp"
echo "cipherclerk: $DEST at $REV + ${#PATCHES[@]} patches"
