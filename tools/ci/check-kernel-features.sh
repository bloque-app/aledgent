#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Fails if any crate in the graph enables an experimental CipherClerk layer.
#
# F-001 §1, T-001-01. The kernel's graph-, metadata- and payee-hiding layers
# are unaudited (CipherClerk's source). DEC-11 keeps V0 at L0 and DEF-4 defers
# them, so `fmd-experimental`, `stealth-experimental` and `notes` must never
# reach the build, through any crate, target or dependency kind (dev included).
#
# Usage: tools/ci/check-kernel-features.sh [--manifest-path <Cargo.toml>]
# Exit codes: 0 clean, 1 a forbidden feature is enabled, 2 the check could not run.
set -uo pipefail

FORBIDDEN='fmd-experimental|stealth-experimental|notes'

# Plain output: this script parses what cargo prints on stdout.
export CARGO_TERM_COLOR=never
errors=$(mktemp)
trap 'rm -f "$errors"' EXIT

manifest=()
if [[ $# -gt 0 ]]; then
  if [[ $1 != "--manifest-path" || $# -ne 2 ]]; then
    echo "usage: $0 [--manifest-path <Cargo.toml>]" >&2
    exit 2
  fi
  manifest=(--manifest-path "$2")
fi

# `-e features` with no other edge kind includes normal, build and dev edges.
if ! tree=$(cargo tree ${manifest[@]+"${manifest[@]}"} --workspace --all-features --target all \
  -e features -i cipher-clerk --prefix none 2>"$errors"); then
  echo "check-kernel-features: cargo tree failed:" >&2
  cat "$errors" >&2
  exit 2
fi

hits=$(grep -E "^cipher-clerk feature \"(${FORBIDDEN})\"" <<<"$tree" | sort -u)
if [[ -n $hits ]]; then
  echo "check-kernel-features: forbidden CipherClerk features are enabled (DEC-11, DEF-4):" >&2
  echo "$hits" >&2
  echo >&2
  echo "Who enables them:" >&2
  echo "$tree" >&2
  exit 1
fi

echo "check-kernel-features: no experimental CipherClerk feature is enabled."
