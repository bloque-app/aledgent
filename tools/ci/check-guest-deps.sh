#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Fails if the core Agent's guest can reach a clock, randomness or a network.
#
# F-005, T-005-06, REQ-RT-7, OQ-36. The core's transition must be a function of
# its inputs: time comes in the command (the library takes `now` from its
# caller), randomness only as a parameter (CipherClerk patch 0001), and nothing
# is fetched. Two checks, over the guest's own lock file (a superset of what the
# guest target builds, so a crate is refused even if another target pulls it):
#
#   1. no denied crate in agents/core/Cargo.lock: OS randomness (getrandom,
#      rand, ring), clocks (chrono, time, instant, web-time, coarsetime,
#      quanta), async runtimes and networking (tokio, mio, socket2, async-std,
#      hyper, h2, reqwest, ureq, rustls, native-tls);
#   2. no use of std's clock, OS randomness or sockets in the guest's sources:
#      agents/core/src, and the parts of core-binding, ledger-core and
#      ledger-kernel the guest compiles (core-binding's host and core modules
#      are std-only and never in the guest).
#
# Deterministic generators seeded by the caller (rand_core's traits,
# rand_chacha) are allowed: they are how randomness arrives as a parameter.
#
# Usage: tools/ci/check-guest-deps.sh [<lock file> [<source dir>...]]
# Exit codes: 0 clean, 1 a denied crate or use, 2 the check could not run.
set -uo pipefail

root=$(cd "$(dirname "$0")/../.." && pwd)
lock=${1:-$root/agents/core/Cargo.lock}
shift || true
if [[ $# -gt 0 ]]; then
  sources=("$@")
else
  sources=(
    "$root/agents/core/src"
    "$root/crates/core-binding/src"
    "$root/crates/ledger-core/src"
    "$root/crates/ledger-kernel/src"
  )
fi
[[ -f $lock ]] || { echo "check-guest-deps: no lock file at $lock" >&2; exit 2; }

denied='getrandom|rand|ring|chrono|time|instant|web-time|coarsetime|quanta|tokio|mio|socket2|async-std|hyper|h2|reqwest|ureq|rustls|native-tls'
status=0

hits=$(grep -nE "^name = \"(${denied})\"$" "$lock" || true)
if [[ -n $hits ]]; then
  echo "check-guest-deps: the guest's graph ($lock) holds a clock, randomness or network crate:" >&2
  echo "$hits" | sed 's/^/  /' >&2
  status=1
fi

# Sources: std-only modules and tests are not the guest's. ledger-kernel's file store and file
# vault are behind its `std` feature (redb), which the guest never enables: they time their own
# commits (T-031-19). The shards crate's host, its R-03 side (`attest`) and native shard are behind
# its `std` and `host` features, its replay worker behind `native-decide`, and its cross-host relay (`net/`, sockets
# and clocks) behind `net`, none of which the shard guest enables (F-031, DEC-22, F-034, T-031-23).
uses='std::time|SystemTime|Instant::now|getrandom|thread_rng|OsRng|std::net|rand::random'
for d in "${sources[@]}"; do
  [[ -d $d ]] || { echo "check-guest-deps: no source directory $d" >&2; exit 2; }
  hits=$(grep -rnE "$uses" "$d" --include='*.rs' \
    | grep -vE '^[^:]+:[0-9]+:[[:space:]]*//' \
    | grep -vE '(^|/)core-binding/src/(host|core)\.rs:|(^|/)ledger-kernel/src/(store|vault)/file\.rs:|(^|/)shards/src/(host|proto|native|attest)\.rs:|(^|/)shards/src/net/[a-z_]+\.rs:|_tests?\.rs:|/tests/' || true)
  if [[ -n $hits ]]; then
    echo "check-guest-deps: the guest's sources use a clock, randomness or a socket:" >&2
    echo "$hits" | sed 's/^/  /' >&2
    status=1
  fi
done

if [[ $status -eq 0 ]]; then
  echo "check-guest-deps: the guest reaches no clock, randomness or network"
fi
exit $status
