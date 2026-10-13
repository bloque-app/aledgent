#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
# One run of a shards-fleet group (stress.yml, core=shards-fleet): the K-th item of COUNTS
# (space-separated `N` or `N/phases`, or comma-separated `N`s), on this node if its index < N.
# Writes $OUT/run-K/ (summary.md: the phase table on node 0; the CSV; this node's log) and prints
# the table on the console too, so that it survives a cancelled job. Each run is a fresh book: a
# Local Agent's image fails near 4.4 MB, about 2 000 transfers a shard (F-034's notes).
#
# Env: K, INDEX, NODES, COUNTS, FLEET_ARGS, BATCH, ACCOUNTS, RDV_WAIT_SECS, WORK, OUT,
#      GITHUB_RUN_ID, GITHUB_RUN_ATTEMPT.
set -euo pipefail
C=$WORK/core
read -r label lookup features < "$C/profiles.txt"
[ -n "$COUNTS" ] || COUNTS=$NODES
items=$COUNTS
case $COUNTS in */*) ;; *) items=${COUNTS//,/ } ;; esac
# shellcheck disable=SC2206
arr=($items)
if [ "$K" -gt "${#arr[@]}" ]; then echo "run $K: none"; exit 0; fi
item=${arr[$((K - 1))]}
n=${item%%/*}
phases=""
[ "$item" != "$n" ] && phases="--phases ${item#*/}"
if [ "$INDEX" -ge "$n" ]; then echo "run $K ($n nodes): not on this one"; exit 0; fi
R=$OUT/run-$K
mkdir -p "$R"
bind=$(cat "$WORK/fleet/bind.txt" 2>/dev/null || echo 0.0.0.0)
{
  echo "### Run $K: $n node(s), ${item#*/}"
  echo
  echo "Node $INDEX ($(nproc) vCPU, $(grep -m1 'model name' /proc/cpuinfo | sed 's/^model name[[:space:]]*: //')); relay on $bind; \`aledgent-core-host fleet\` --batch $BATCH --accounts $ACCOUNTS $phases $FLEET_ARGS; profile \`$label\` (runtime features: $features)."
  echo
} > "$R/summary.md"
rc=0
# shellcheck disable=SC2086
env F025_MAX_LIMITS=1 F025_PROGRAM_LOOKUP="$lookup" F025_BACKEND=recompiler timeout $((RDV_WAIT_SECS + 2400)) \
  "$C/aledgent-core-host" fleet "$C/rt-$label.pvm" "$C/shard.vos" "$R/fleet-$K-$n.csv" \
  --index "$INDEX" --nodes "$n" --net-id "$GITHUB_RUN_ID-$GITHUB_RUN_ATTEMPT-$K" --peers "$WORK/fleet/peers.txt" \
  --port $((47100 + K)) --bind "$bind" --key "$WORK/fleet/relay.key" --batch "$BATCH" --accounts "$ACCOUNTS" \
  --wait-secs "$RDV_WAIT_SECS" $phases $FLEET_ARGS \
  > >(tee -a "$R/summary.md") 2> >(tee -a "$R/node$INDEX.log" >&2) || rc=$?
echo >> "$R/summary.md"
if [ "$INDEX" = 0 ]; then
  cat "$R/summary.md" >> "${GITHUB_STEP_SUMMARY:-/dev/null}"
  cat "$R"/fleet-*.csv 2>/dev/null || true
fi
exit "$rc"
