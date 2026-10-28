#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
#
# Publishes a core code set as one image (#285): `crane append` of the layer
# tools/release/pack-code-layer.py wrote, then `crane mutate` to label it with the repository it comes
# from (org.opencontainers.image.source, which also links the package to the repository), tagged
# `code-<H>` for people and used by digest by everything else. A code version is a function of its
# bytes, so a tag that exists already must hold exactly what this run would publish: one layer, whose
# uncompressed digest is this layer.tar's, the expected label, and the same files; then nothing is
# pushed and the existing digest is the answer. Any difference is an error. The caller is logged in to
# the registry (crane auth login) with a credential that may write that repository, and to no other.
#
#   tools/release/core-code-publish.sh <repository> <code-version> <set-dir> <layer.tar> [<source-url>]
#
# <source-url> defaults to https://github.com/bloque-app/aledgent. Prints the image's digest
# (sha256:<64 hex>) on the last line.
set -euo pipefail

if [ "$#" -ne 4 ] && [ "$#" -ne 5 ]; then
  sed -n '/^#   tools/,/^#   tools/p' "$0" | sed 's/^# \{0,3\}//' >&2
  exit 2
fi
repo=$1 code=$2 set_dir=$3 layer=$4 source=${5:-https://github.com/bloque-app/aledgent}
[[ $code =~ ^[0-9a-f]{64}$ ]] || { echo "core-code-publish: '$code' is not a code version" >&2; exit 1; }
ref="$repo:code-$code"
diff_id="sha256:$(sha256sum "$layer" | cut -d' ' -f1)"

# Exactly what this script publishes: one layer, this layer.tar (its uncompressed digest, the config's
# diff_id), and the source label. Nothing else is accepted under a code version's tag.
check_image() {
  local at=$1 why=$2
  [ "$(crane manifest "$at" | jq '.layers | length')" = 1 ] \
    || { echo "core-code-publish: $at is not one layer: $why" >&2; return 1; }
  crane config "$at" | jq -e --arg d "$diff_id" --arg s "$source" \
    '.rootfs.diff_ids == [$d] and .config.Labels["org.opencontainers.image.source"] == $s' >/dev/null \
    || { echo "core-code-publish: $at is not this layer.tar with the label $source: $why" >&2; return 1; }
}

if existing=$(crane digest "$ref" 2>/dev/null); then
  check_image "$repo@$existing" "a code version's image never changes" || exit 1
  tmp=$(mktemp -d)
  # The layer's directory is read-only: make it removable before it is removed.
  trap 'chmod -R u+w "$tmp"; rm -rf "$tmp"' EXIT
  crane export "$repo@$existing" - | tar -x -C "$tmp"
  diff -r "$set_dir" "$tmp/code" >&2 \
    || { echo "core-code-publish: $ref holds a different set than this run built: a code version's files never change" >&2; exit 1; }
  echo "core-code-publish: $ref exists and holds exactly these files; nothing pushed" >&2
  digest=$existing
else
  crane append -f "$layer" -t "$ref" >&2
  crane mutate "$ref" --label "org.opencontainers.image.source=$source" -t "$ref" >&2
  digest=$(crane digest "$ref")
  check_image "$repo@$digest" "the registry did not keep what was pushed" || exit 1
fi
[[ $digest =~ ^sha256:[0-9a-f]{64}$ ]] || { echo "core-code-publish: the registry answered '$digest'" >&2; exit 1; }
echo "$digest"
