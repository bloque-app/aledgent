#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Fails if any crate depends on `guest-heap` other than as a dev-dependency.
#
# T-036-34, REQ-RT-1. `crates/guest-heap` is a global allocator that models the
# core guest's heap for native tests, and the one crate of the workspace that
# allows `unsafe` (on its `GlobalAlloc` impl). No guest and no host may link it:
# it is a dev-dependency only. Normal and build dependencies are checked, for
# every target; dev-dependencies are tests.
#
# Usage: tools/ci/check-guest-heap-dev-only.sh [--manifest-path <Cargo.toml>]
# Exit codes: 0 clean, 1 a crate links guest-heap, 2 the check could not run.
set -uo pipefail

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

if ! tree=$(cargo tree ${manifest[@]+"${manifest[@]}"} --workspace --target all \
  -e normal,build -i guest-heap --prefix none 2>"$errors"); then
  if grep -q "did not match any packages" "$errors"; then
    echo "check-guest-heap-dev-only: nothing depends on guest-heap."
    exit 0
  fi
  echo "check-guest-heap-dev-only: cargo tree failed:" >&2
  cat "$errors" >&2
  exit 2
fi

# The inverted tree's first line is guest-heap itself; any other is a crate that links it.
dependents=$(awk 'NF && $1 != "guest-heap" { print }' <<<"$tree")
if [[ -n $dependents ]]; then
  echo "check-guest-heap-dev-only: guest-heap is a normal or build dependency (T-036-34):" >&2
  echo "$tree" >&2
  exit 1
fi
echo "check-guest-heap-dev-only: guest-heap is a dev-dependency only."
