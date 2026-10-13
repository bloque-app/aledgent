#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Fails if a production crate enables ledger-kernel's or ledger-core's
# `conformance-ports`.
#
# F-001 §11, T-001-09, REQ-CS-5. The raw KernelPort and StorePort offer
# arbitrary posting and arbitrary state mutation, which the command surface
# must never offer. Only the conformance suite may enable them. Each other
# workspace member is resolved on its own (as `cargo build -p <member>` would
# build it), over normal and build dependencies; dev-dependencies are tests.
#
# Usage: tools/ci/check-conformance-ports.sh [--manifest-path <Cargo.toml>]
# Exit codes: 0 clean, 1 a production crate enables the ports, 2 the check
# could not run.
set -uo pipefail

# Crates allowed to enable the ports: the conformance suite (F-015).
ALLOWED=" conformance "
# Crates that offer ports: ledger-kernel's raw kernel and store ports (F-001) and
# ledger-core's forged store records (F-003).
PORTED="ledger-kernel ledger-core"

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

if ! members=$(cargo tree ${manifest[@]+"${manifest[@]}"} --workspace --depth 0 \
  -e normal --prefix none 2>"$errors"); then
  echo "check-conformance-ports: cargo tree failed:" >&2
  cat "$errors" >&2
  exit 2
fi
members=$(awk 'NF { print $1 }' <<<"$members" | sort -u)

failed=0
for member in $members; do
  if [[ $ALLOWED == *" $member "* ]]; then
    continue
  fi
  for ported in $PORTED; do
    if ! tree=$(cargo tree ${manifest[@]+"${manifest[@]}"} -p "$member" --target all \
      -e normal,build,features -i "$ported" --prefix none 2>"$errors"); then
      if grep -q "did not match any packages" "$errors"; then
        continue # the member does not depend on this crate
      fi
      echo "check-conformance-ports: cargo tree failed for $member:" >&2
      cat "$errors" >&2
      exit 2
    fi
    if grep -q "^$ported feature \"conformance-ports\"" <<<"$tree"; then
      echo "check-conformance-ports: $member enables $ported/conformance-ports (REQ-CS-5):" >&2
      echo "$tree" >&2
      failed=1
    fi
  done
done

if [[ $failed -ne 0 ]]; then
  exit 1
fi
echo "check-conformance-ports: no production crate enables conformance-ports."
