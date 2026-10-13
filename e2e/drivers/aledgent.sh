#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
# The aledgent driver for the zombienet suite's steps 3 and 4: ingress and the in-process core in
# `e2e/aledgent-driver`, and the settlement adapter as its own process (`aledgent-settle`, with its
# PostgreSQL store, over its light client), against the network and the contract the suite
# deployed. Run from contracts/settlement/scripts.
#
#   aledgent.sh drive    step 3: set the chain up, start the driver, wait for step 3
#   aledgent.sh stress   stress scenario c (F-032): the book under load (STRESS_PHASES,
#                        name:commands_per_min:secs,...) while withdrawals settle, wait for it
#   aledgent.sh faults   step 4: hand the running driver the fault cases, wait for them
#   aledgent.sh stop     stop the driver and remove every container and network it started
#
# The driver is one process across the steps (the core it hosts is in memory): `drive` starts it
# in the background and the markers in "$E2E_OUT/aledgent" hand it the phases.
#
# In (environment, set by .github/workflows/e2e-zombienet.yml):
#   DRIVER_BIN, SETTLE_BIN      the built `aledgent-e2e-driver` and `aledgent-settle`
#   E2E_OUT                     the suite's output directory (state, report, expect.json)
#   LIGHT_KREIVO_CHAIN_SPEC, LIGHT_RELAY_CHAIN_SPEC, KREIVO_PRUNED_P2P
#                               from spawn_network: the chain specs with one local boot node per
#                               chain, and the pruned node's multiaddr
#   ALEDGENT_IMAGE, ALEDGENT_TAG, ALEDGENT_CONNECTION
#                               the image the adapter runs in, the name of this run's containers,
#                               and how the adapter reaches Kreivo (light-client or rpc)
#   KREIVO_WS, STATE, BUNDLE    as for e2e.mjs
#
# `drive`, in order:
# 1. `aledgent-e2e-driver keys`: the PASS accounts (lanes and anchor key), their device keys and
#    the core's clerk key, all test constants of `settle::testkit`.
# 2. `e2e.mjs aledgent-setup`: registers each PASS account with its secp256k1 device on Kreivo's
#    Pass pallet, funds it, and makes the lanes and the anchor key the contract's keys, with the
#    clerk key as its attestation key.
# 3. `aledgent-e2e-driver run`: brings the adapter up (e2e/aledgent-driver/src/harness.rs), then
#    through ingress a deposit with an account reference (credited at finality), a withdrawal
#    (settled), a withdrawal the contract refuses (reported, nothing released), and an anchor (the
#    core's checkpoint, final). It writes expect.json in the form `e2e.mjs drive` writes, so
#    `e2e.mjs check` runs unchanged.

set -euo pipefail
: "${E2E_OUT:?}"
STATE_DIR="$E2E_OUT/aledgent"
LOG="$STATE_DIR/driver.log"
TAG="${ALEDGENT_TAG:-${GITHUB_RUN_ID:-local}}"
mkdir -p "$STATE_DIR"

# Wait until "$STATE_DIR/$1" appears (success) or "$STATE_DIR/$2" (failure), for at most $3
# seconds, printing the driver's log as it grows.
wait_for() {
  local ok="$1" failed="$2" end=$((SECONDS + $3)) printed
  printed=$(wc -l < "$LOG" 2>/dev/null || echo 0)
  while :; do
    local n
    n=$(wc -l < "$LOG" 2>/dev/null || echo 0)
    if [ "$n" -gt "$printed" ]; then sed -n "$((printed + 1)),${n}p" "$LOG"; printed=$n; fi
    [ -e "$STATE_DIR/$ok" ] && return 0
    if [ -e "$STATE_DIR/$failed" ]; then
      echo "::error::$(head -c 2000 "$STATE_DIR/$failed")"
      return 1
    fi
    if ! kill -0 "$(cat "$STATE_DIR/driver.pid" 2>/dev/null || echo 0)" 2>/dev/null; then
      echo "::error::the aledgent driver exited"
      tail -60 "$LOG" || true
      return 1
    fi
    if [ $SECONDS -ge $end ]; then
      echo "::error::the aledgent driver did not finish in time"
      return 1
    fi
    sleep 5
  done
}

case "${1:-}" in
  drive)
    : "${DRIVER_BIN:?}" "${SETTLE_BIN:?}" "${LIGHT_KREIVO_CHAIN_SPEC:?}" "${LIGHT_RELAY_CHAIN_SPEC:?}"
    "$DRIVER_BIN" keys > "$E2E_OUT/aledgent-keys.json"
    node e2e.mjs aledgent-setup
    # The book-only scenario's chain side (F-029) runs in polkadot-api, from e2e/papi.
    E2E_SCRIPTS="$PWD" E2E_PAPI="$(cd "$(dirname "$0")/../papi" && pwd)" ALEDGENT_TAG="$TAG" \
      RUST_LOG="${RUST_LOG:-info}" nohup "$DRIVER_BIN" run > "$LOG" 2>&1 &
    echo $! > "$STATE_DIR/driver.pid"
    # Step 3, then the book-only asset's registration, binding and anchor (F-029).
    wait_for step3.done step3.failed 2900
    ;;
  stress)
    rm -f "$STATE_DIR/stress.done" "$STATE_DIR/stress.failed"
    printf '%s' "${STRESS_PHASES:-}" > "$STATE_DIR/stress.go"
    wait_for stress.done stress.failed "${STRESS_WAIT:-3300}"
    ;;
  faults)
    touch "$STATE_DIR/faults.go"
    wait_for faults.done faults.failed 4400
    ;;
  stop)
    if [ -f "$STATE_DIR/driver.pid" ]; then
      kill -TERM "$(cat "$STATE_DIR/driver.pid")" 2>/dev/null || true
    fi
    # An adapter still running (a step cut short) leaves its log with the artifacts.
    for name in $(docker ps -a --filter "label=aledgent-e2e=$TAG" --format '{{.Names}}' | grep settle || true); do
      docker logs "$name" > "$STATE_DIR/$name.log" 2>&1 || true
    done
    ids=$(docker ps -aq --filter "label=aledgent-e2e=$TAG" || true)
    [ -n "$ids" ] && docker rm -f $ids >/dev/null || true
    docker network rm "aledgent-$TAG" >/dev/null 2>&1 || true
    # The adapter's test keys and token (public test constants) are not kept with the artifacts.
    rm -f "$STATE_DIR/lane.key" "$STATE_DIR/anchor.key" "$STATE_DIR/token"
    ;;
  *)
    echo "usage: aledgent.sh drive|stress|faults|stop" >&2
    exit 2
    ;;
esac
