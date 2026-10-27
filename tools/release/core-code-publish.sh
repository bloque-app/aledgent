#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
#
# Publishes a core code set as one image (#285): `crane append` of the layer
# tools/release/pack-code-layer.py wrote, tagged `code-<H>` for people and used by digest by everything
# else. A code version is a function of its bytes, so a tag that exists already must hold exactly the
# files this run built: then nothing is pushed and the existing digest is the answer; any difference is
# an error. The caller is logged in to the registry (crane auth login) with a credential that may
# write that repository, and to no other.
#
#   tools/release/core-code-publish.sh <repository> <code-version> <set-dir> <layer.tar>
#
# Prints the image's digest (sha256:<64 hex>) on the last line.
set -euo pipefail

if [ "$#" -ne 4 ]; then
  sed -n '/^#   tools/,/^#   tools/p' "$0" | sed 's/^# \{0,3\}//' >&2
  exit 2
fi
repo=$1 code=$2 set_dir=$3 layer=$4
[[ $code =~ ^[0-9a-f]{64}$ ]] || { echo "core-code-publish: '$code' is not a code version" >&2; exit 1; }
ref="$repo:code-$code"

if existing=$(crane digest "$ref" 2>/dev/null); then
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
  digest=$(crane digest "$ref")
fi
[[ $digest =~ ^sha256:[0-9a-f]{64}$ ]] || { echo "core-code-publish: the registry answered '$digest'" >&2; exit 1; }
echo "$digest"
