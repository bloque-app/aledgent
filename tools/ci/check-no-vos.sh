#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Fails if anything outside agents/ and spikes/ depends on VOS.
#
# F-005, licensing. VOS (every crate of it, vos-agent-sdk included) is
# AGPL-3.0-or-later; aledgent is Apache-2.0 OR MIT. Everything that links VOS
# lives in the agents/ workspace (licence AGPL-3.0-or-later, excluded from the
# root workspace), and F-002's measurement code in spikes/. The rest of
# aledgent reaches the core across a process boundary (ingress::remote). This
# check keeps it so:
#
#   1. no Cargo.toml outside agents/ and spikes/ names a VOS crate or VOS's
#      repository;
#   2. the root workspace's lock file resolves no VOS crate;
#   3. every manifest under agents/ declares AGPL-3.0-or-later.
#
# Usage: tools/ci/check-no-vos.sh [<repository root>]
# Exit codes: 0 clean, 1 a violation, 2 the check could not run.
set -uo pipefail

root=${1:-$(cd "$(dirname "$0")/../.." && pwd)}
[[ -f $root/Cargo.toml ]] || { echo "check-no-vos: no Cargo.toml at $root" >&2; exit 2; }

# VOS's crates, by the names its workspace publishes, and its repositories.
crates='vos|vos-agent-sdk|vos-protocol|vos-macros|vos-pvm|vos-pvm-program|vos-pvm-compiler|vos-pvm-precompiles|vosx|clerk-witness'
repos='codeberg\.org/[A-Za-z0-9_-]+/VOS'

status=0

# 1. Manifests outside agents/ and spikes/ (and outside build output and local clones).
while IFS= read -r m; do
  hits=$(grep -nE "^[[:space:]]*(${crates})[[:space:]]*=|^[[:space:]]*\[(dev-|build-)?dependencies\.(${crates})\]|${repos}|package[[:space:]]*=[[:space:]]*\"(${crates})\"" "$m" || true)
  if [[ -n $hits ]]; then
    echo "check-no-vos: ${m#"$root"/} depends on VOS (AGPL); only agents/ and spikes/ may:" >&2
    echo "$hits" | sed 's/^/  /' >&2
    status=1
  fi
done < <(find "$root" -name Cargo.toml \
  -not -path "$root/agents/*" -not -path "$root/spikes/*" \
  -not -path "*/target/*" -not -path "$root/.cipherclerk-patched/*" \
  -not -path "$root/.scratch/*" -not -path "$root/.claude/*" \
  -not -path "$root/tools/ci/fixtures/*")

# 2. The root lock file.
if [[ -f $root/Cargo.lock ]]; then
  hits=$(grep -nE "^name = \"(${crates})\"$|${repos}" "$root/Cargo.lock" || true)
  if [[ -n $hits ]]; then
    echo "check-no-vos: the root workspace's Cargo.lock resolves VOS:" >&2
    echo "$hits" | sed 's/^/  /' >&2
    status=1
  fi
fi

# 3. The AGPL side declares it.
while IFS= read -r m; do
  if ! grep -qE '^(license[[:space:]]*=[[:space:]]*"AGPL-3\.0-or-later"|license\.workspace[[:space:]]*=[[:space:]]*true)' "$m"; then
    echo "check-no-vos: ${m#"$root"/} links VOS and must declare license = \"AGPL-3.0-or-later\"" >&2
    status=1
  fi
done < <(find "$root/agents" -name Cargo.toml -not -path "*/target/*" 2>/dev/null)

if [[ $status -eq 0 ]]; then
  echo "check-no-vos: nothing outside agents/ and spikes/ depends on VOS"
fi
exit $status
