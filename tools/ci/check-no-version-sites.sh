#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
#
# The core image carries no binary that reports the crate version (bloque-app/bloque-ledgent#312,
# step 3: a release takes the core image from its develop pre-release, and its binaries must not say
# a version that is not the release's).
#
#   tools/ci/check-no-version-sites.sh <binary>...     # exit 1 if any of them has a version site
#   tools/ci/check-no-version-sites.sh --self-test     # the check can fail, and can pass
#
# `CARGO_PKG_VERSION` is used in two places: the OpenAPI document of services/ingress
# (`/openapi.json`, `info.version`) and the light client's network name in services/settle
# (`aledgent-settle/<version>`). A binary that contains either is one that reports the version, so
# the check looks, in the binary, for what only that code brings: the route `/openapi.json`, the
# source paths of the two files (their panic locations), and the network name `aledgent-settle`.
# (A version string of five bytes is an immediate in the code, so it cannot be searched for itself.)
# The shipped `aledgent-ingress`, `aledgent-settle` and sidecar binaries have them; `aledgent-core-host`
# and `opsctl` have none. `ops/src/kit.rs` calls `ingress::surface::router`, but only the restore
# drill's test harness does, and `opsctl` does not link it: this check is what keeps that true.
set -euo pipefail

needles=(
  '/openapi.json'
  'ingress/src/openapi.rs'
  'settle/src/chain/light.rs'
  'aledgent-settle'
)

check() {
  local status=0 f n
  for f in "$@"; do
    test -s "$f" || { echo "::error::$f is missing or empty"; status=1; continue; }
    for n in "${needles[@]}"; do
      if grep -aqF -- "$n" "$f"; then
        echo "::error::$(basename "$f") contains '$n': it reports the crate version, and a core image taken from a pre-release would report the pre-release's"
        status=1
      fi
    done
  done
  return $status
}

if [ "${1:-}" = "--self-test" ]; then
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  head -c 4096 /dev/urandom > "$tmp/clean"
  check "$tmp/clean" > /dev/null || { echo "self-test: a clean file was refused"; exit 1; }
  for n in "${needles[@]}"; do
    { head -c 512 /dev/urandom; printf '%s' "$n"; head -c 512 /dev/urandom; } > "$tmp/dirty"
    if check "$tmp/dirty" > /dev/null; then echo "self-test: a file with '$n' was accepted"; exit 1; fi
  done
  : > "$tmp/empty"
  if check "$tmp/empty" > /dev/null; then echo "self-test: an empty file was accepted"; exit 1; fi
  echo "self-test: ok"
  exit 0
fi

test $# -gt 0 || { echo "usage: $0 <binary>... | --self-test" >&2; exit 2; }
check "$@"
