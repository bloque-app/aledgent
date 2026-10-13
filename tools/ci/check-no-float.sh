#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Fails if a floating-point type appears in the Rust sources under crates/.
#
# INV-7, REQ-KER-7: every monetary amount is an exact integer count of minor
# units, and floating point must not touch a monetary value at any layer.
# Clippy denies float arithmetic (workspace lints); this gate also rejects the
# types themselves, in code, tests and doc examples alike.
#
# Usage: tools/ci/check-no-float.sh [root]   (default root: crates)
# Exit codes: 0 clean, 1 a float type was found.
set -uo pipefail

root=${1:-crates}
hits=$(grep -RnwE --include='*.rs' 'f32|f64' "$root" || true)
if [[ -n $hits ]]; then
  echo "check-no-float: floating-point types found under $root (INV-7, REQ-KER-7):" >&2
  echo "$hits" >&2
  exit 1
fi
echo "check-no-float: no floating-point type under $root."
