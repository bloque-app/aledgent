#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
# One stress run (F-032): a throwaway deployment (ingress, and the core as a VOS Agent when asked),
# the harness against it, faults injected when asked, and the report.
#
#   e2e/stress/run.sh <out-dir>
#
# Environment:
#   BIN            directory with aledgent-ingress, aledgent-e2e-driver and aledgent-signer (release builds)
#   DATABASE_URL   an empty PostgreSQL database for ingress (dropped and recreated tables are not
#                  needed: ingress migrates on start)
#   CORE           inprocess (default) | agent: the core as a VOS Agent, aledgent-core-host serve
#   CORE_HOST      for CORE=agent: the aledgent-core-host binary
#   CORE_RUNTIME   for CORE=agent: agent_runtime.pvm      CORE_ACTOR: core.vos
#   CORE_ENV       for CORE=agent: extra environment for the host (e.g. F025_BACKEND=recompiler)
#   CORE_ARGS      for CORE=agent: extra arguments to `serve` (e.g. --replica-in-memory)
#   CORE_DIAG_SECS for CORE=agent: the host's report period (default 15 s; 0: none), T-032-11
#   PLAN           smoke | ladder | sustained | burst | soak (default smoke)
#   PHASES         name:cmds_per_min:secs,... (replaces the plan's)
#   HOLDERS        funded holders (default 200)      PASSKEY_PCT (default 30)
#   POLLERS        completion pollers at once (default 2000; each polls its operation until decided)
#   SERVICE_PCT    share of transfers and reservations the operator makes for holders (default 50)
#   ANCHORING_MS   anchoring interval (scenario b); unset: no anchoring
#   FAULTS         1: kill and restart the core host, restart PostgreSQL, kill and restart ingress,
#                  under load (scenario d). Needs PG_RESTART, a command that restarts PostgreSQL.
#   DRAIN_SECS     how long to wait for the backlog after the load (default 900)
#   DRILL          1 (CORE=agent only): the restore drill of NFR-10 on the core Agent (T-005-17).
#                  The core host streams every decision to an off-node archive (a directory
#                  here, S3 in a deployment); after the load the node is killed and its state
#                  moved away, a fresh one is restored from the archive (`opsctl restore core
#                  --from`, which runs `aledgent-core-host restore`), the received-log's tail is
#                  replayed (`opsctl restore core-tail`), and the restored node must be the lost
#                  one: the same logs, byte for byte. Needs OPSCTL (the opsctl binary).
#   LABEL          the run's name in the report
#   GROWN          (CORE=agent only, F-036) found the book on grown state: a synthetic chainspec of
#                  GROWN times the synthetic reference book (1: 100,000 identities and as many
#                  accounts, synthetic; 10: 2 M), built by `migrate chainspec build` for this run's
#                  genesis, the deployment's genesis committing it and the core host building its
#                  genesis state from it (`serve --chainspec`). Needs MIGRATE (the operator's migration
#                  tool, not in this repository).
#                  The workload's own holders are opened as usual, on top of it.
#
# The secrets it draws are test values, fresh per run, in <out-dir>/deploy/secrets.env (0600).
set -euo pipefail

OUT=${1:?out dir}
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)
D=$OUT/deploy
LOGS=$OUT/logs
EVENTS=$OUT/events.txt
mkdir -p "$D" "$LOGS"
: > "$EVENTS"
BIN=${BIN:?BIN}
CORE=${CORE:-inprocess}
PORT=${PORT:-18080}
RECEIVER=${RECEIVER:-127.0.0.1:18099}
SOCK=$OUT/core.sock

args=(stress-config --out "$D" --database-url "${DATABASE_URL:?DATABASE_URL}" --listen "127.0.0.1:$PORT")
if [ "$CORE" = agent ]; then args+=(--core-endpoint "unix:$SOCK"); fi
if [ -n "${ANCHORING_MS:-}" ]; then args+=(--anchoring-ms "$ANCHORING_MS"); fi
"$BIN/aledgent-e2e-driver" "${args[@]}"

event() { echo "$(($(date +%s) * 1000)) $*" >> "$EVENTS"; echo "[run.sh] $*"; }

# F-036: grown state. The chainspec is built for the genesis the deployment file chose, with its
# registry plus COPM (the synthetic openings' asset), and the genesis is patched to commit it.
if [ -n "${GROWN:-}" ]; then
  [ "$CORE" = agent ] || { echo "GROWN needs CORE=agent" >&2; exit 2; }
  read -r GOP GAT ASSETS < <(python3 - "$D/ingress.json" <<'PY'
import json, sys
c = json.load(open(sys.argv[1]))
g = c["genesis"]
if not any(a["id"] == "COPM" for a in g["assets"]):
    g["assets"].append({"id": "COPM", "precision": 2, "kind": "stable"})
json.dump(c, open(sys.argv[1], "w"), indent=1)
print(g["genesis_op"], g["genesis_at"], ",".join(f'{a["id"]}:{a["precision"]}:{a["kind"]}' for a in g["assets"]))
PY
)
  "${MIGRATE:?MIGRATE}" chainspec build --genesis-op "$GOP" --at "$GAT" --assets "$ASSETS" \
    --identities $((100000 * GROWN)) --accounts $((100000 * GROWN)) --system-accounts $((200 * GROWN)) \
    --openings $((1000 * GROWN)) --out "$D/chainspec" > "$LOGS/chainspec-build.log" 2>&1
  python3 - "$D/ingress.json" "$D/chainspec/chainspec.json" <<'PY'
import json, sys
c = json.load(open(sys.argv[1])); s = json.load(open(sys.argv[2]))
c["genesis"]["chainspec"] = s["genesis"]["chainspec"]
c["genesis"]["opening_sources"] = s["genesis"]["opening_sources"]
json.dump(c, open(sys.argv[1], "w"), indent=1)
PY
  # A stress book names no genesis code (amendment 0023): the core host is told not to check its
  # binaries against one. A deployment's chainspec always names it (`migrate chainspec build --code`).
  # Nor does it wait for a verify-genesis report (T-036-25): `--unverified-genesis` is for test and
  # stress books only; a deployment's book takes commands once `install-verified` has run.
  CORE_ARGS="${CORE_ARGS:-} --chainspec $D/chainspec/chainspec.bin --unnamed-code --unverified-genesis"
  event "grown state: x$GROWN ($(grep -o '[0-9]* chunks, [0-9]* records' "$LOGS/chainspec-build.log"))"
fi


ARCHIVE=$OUT/archive
DRILL=${DRILL:-0}
CORE_STATE=$OUT/core-state
if [ "$DRILL" = 1 ] && [ "$CORE" = agent ]; then
  CORE_ARGS="${CORE_ARGS:-} --archive fs:$ARCHIVE --archive-interval-ms 500 --checkpoint-every 2000"
fi

# F-035: with the core host, its keys are the signer's (development keys, test values, fresh per
# run, in <out-dir>/deploy/signer/), and the core host takes none from its environment.
start_signer() {
  [ "$CORE" = agent ] || return 0
  rm -f "$D/signer/signer.sock"
  "$BIN/aledgent-signer" serve "$D/signer/signer.json" >> "$LOGS/signer.log" 2>&1 &
  echo $! > "$OUT/signer.pid"
  for _ in $(seq 1 60); do [ -S "$D/signer/signer.sock" ] && return 0; sleep 1; done
  echo "the signer did not listen" >&2
  tail -n 40 "$LOGS/signer.log" >&2
  return 1
}

start_core() {
  [ "$CORE" = agent ] || return 0
  rm -f "$SOCK"
  (
    set -a
    # shellcheck disable=SC1091
    . "$D/secrets.env"
    set +a
    # shellcheck disable=SC2086
    exec env ALEDGENT_CORE_DIAG_SECS="${CORE_DIAG_SECS:-15}" ${CORE_ENV:-} "${CORE_HOST:?CORE_HOST}" serve --config "$D/ingress.json" \
      --runtime "${CORE_RUNTIME:?}" --actor "${CORE_ACTOR:?}" --state "$CORE_STATE" ${CORE_ARGS:-}
  ) >> "$LOGS/core-host.log" 2>&1 &
  echo $! > "$OUT/core.pid"
  for _ in $(seq 1 "${CORE_START_SECS:-$(( ${GROWN:-0} > 0 ? 3600 : 600 ))}"); do [ -S "$SOCK" ] && return 0; sleep 1; done
  echo "the core host did not listen" >&2
  tail -n 40 "$LOGS/core-host.log" >&2
  return 1
}

start_ingress() {
  local tok attempt
  tok=$(cat "$D/operator.token")
  # Ingress exits if PostgreSQL refuses its first connection (a database still starting): start it
  # again, a few times, as a supervisor would.
  for attempt in 1 2 3 4 5; do
    (
      set -a
      # shellcheck disable=SC1091
      . "$D/secrets.env"
      set +a
      exec "$BIN/aledgent-ingress" serve --config "$D/ingress.json"
    ) >> "$LOGS/ingress.log" 2>&1 &
    echo $! > "$OUT/ingress.pid"
    for _ in $(seq 1 120); do
      if curl -sf -o /dev/null -H "Authorization: Bearer $tok" "http://127.0.0.1:$PORT/metrics"; then return 0; fi
      kill -0 "$(cat "$OUT/ingress.pid")" 2>/dev/null || break
      sleep 1
    done
    echo "[run.sh] ingress did not serve (attempt $attempt)"
    kill "$(cat "$OUT/ingress.pid")" 2>/dev/null || true
    sleep 5
  done
  echo "ingress did not serve" >&2
  tail -n 40 "$LOGS/ingress.log" >&2
  return 1
}

stop_all() {
  for f in ingress core signer; do
    if [ -f "$OUT/$f.pid" ]; then kill "$(cat "$OUT/$f.pid")" 2>/dev/null || true; fi
  done
}
trap stop_all EXIT

export RUST_LOG=${RUST_LOG:-warn}
# The operator's notifications go to the harness's receiver, as an issuing client's go to it (after ingress
# has migrated its store).
register_recipient() {
  local attempt
  for attempt in 1 2 3 4 5; do
    if (
      set -a
      # shellcheck disable=SC1091
      . "$D/secrets.env"
      set +a
      export STRESS_RECIPIENT_SECRET=stress-recipient-secret
      "$BIN/aledgent-ingress" recipient --config "$D/ingress.json" --principal "$(cat "$D/operator.identity")" \
        --endpoint "http://$RECEIVER/notifications" --secret-env STRESS_RECIPIENT_SECRET
    ) >> "$LOGS/ingress.log" 2>&1; then
      return 0
    fi
    sleep 3
  done
  echo "[run.sh] recipient not registered (see logs/ingress.log)"
}
start_signer
start_core
start_ingress
register_recipient
event "deployment up (core: $CORE${ANCHORING_MS:+, anchoring every ${ANCHORING_MS} ms})"

pids=(--pid "ingress=$(cat "$OUT/ingress.pid")")
if [ "$CORE" = agent ]; then pids+=(--pid "core-host=$(cat "$OUT/core.pid")"); fi

faults() {
  # Wait for the load to start (setup can take minutes through the Agent).
  for _ in $(seq 1 3600); do grep -q "phase " "$LOGS/harness.log" 2>/dev/null && break; sleep 1; done
  sleep "${FAULT_AT:-60}"
  if [ "$CORE" = agent ]; then
    event "fault: kill -9 the core host"
    kill -9 "$(cat "$OUT/core.pid")" || true
    sleep 15
    start_core && event "fault: core host restarted"
  else
    event "fault: kill -9 ingress (the in-process core goes with it)"
    kill -9 "$(cat "$OUT/ingress.pid")" || true
    sleep 15
    start_ingress && event "fault: ingress restarted"
  fi
  sleep 60
  if [ -n "${PG_RESTART:-}" ]; then
    event "fault: PostgreSQL restart"
    bash -c "$PG_RESTART" || event "fault: PostgreSQL restart failed"
    event "fault: PostgreSQL back"
  fi
  sleep 60
  event "fault: kill -9 ingress"
  kill -9 "$(cat "$OUT/ingress.pid")" || true
  sleep 10
  start_ingress && event "fault: ingress restarted"
}
if [ "${FAULTS:-0}" = 1 ]; then faults & fpid=$!; fi

hargs=(stress --url "http://127.0.0.1:$PORT" --config-dir "$D" --out "$OUT" --receiver "$RECEIVER"
  --plan "${PLAN:-smoke}" --holders "${HOLDERS:-200}" --passkey-pct "${PASSKEY_PCT:-30}"
  --service-pct "${SERVICE_PCT:-50}" --pollers "${POLLERS:-2000}"
  --drain-secs "${DRAIN_SECS:-900}" --events "$EVENTS" --label "${LABEL:-${CORE}-${PLAN:-smoke}}" "${pids[@]}")
if [ -n "${PHASES:-}" ]; then hargs+=(--phases "$PHASES"); fi
rc=0
"$BIN/aledgent-e2e-driver" "${hargs[@]}" 2>&1 | tee "$LOGS/harness.log" || rc=$?
if [ -n "${fpid:-}" ]; then wait "$fpid" || true; fi

# The restore drill (NFR-10, T-005-17): back up (the archive streamed all along), destroy,
# restore, verify. Its result is a section of the summary and the job's exit code.
drill() {
  local lost=$OUT/core-state-lost tail_rc=0 drc=0
  {
    echo
    echo "## Restore drill (NFR-10): the core Agent from its off-node archive"
    echo
  } >> "$OUT/summary.md"
  # Stop taking inputs, so the lost node's logs are final; then lose the node.
  kill "$(cat "$OUT/ingress.pid")" 2>/dev/null || true
  sleep 5
  "$OPSCTL" archive status --state "$CORE_STATE" > "$OUT/drill-status-before.json" 2>&1 || true
  event "drill: kill -9 the core host and move its state away (the node is lost)"
  kill -9 "$(cat "$OUT/core.pid")" || true
  sleep 2
  mv "$CORE_STATE" "$lost"
  local t0 t1
  t0=$(date +%s)
  (
    set -a
    # shellcheck disable=SC1091
    . "$D/secrets.env"
    set +a
    # shellcheck disable=SC2086
    exec env ${CORE_ENV:-} "$OPSCTL" restore core --from "fs:$ARCHIVE" --config "$D/ingress.json" \
      --state "$CORE_STATE" --no-anchors --report "$OUT/drill" --host-name stress \
      --core-host "$CORE_HOST" --runtime "$CORE_RUNTIME" --actor "$CORE_ACTOR"
  ) > "$LOGS/drill-restore.log" 2>&1 || drc=$?
  t1=$(date +%s)
  if [ "$drc" != 0 ]; then
    { echo "**FAILED**: \`opsctl restore core --from\` exited $drc"; echo; echo '```'; tail -n 40 "$LOGS/drill-restore.log"; echo '```'; } >> "$OUT/summary.md"
    # Evidence for the failure: the archive and the lost node's logs (test data, small).
    mkdir -p "$OUT/drill-debug"
    cp -R "$ARCHIVE" "$OUT/drill-debug/archive" || true
    cp "$lost/archive.bin" "$lost/replies.jsonl" "$OUT/drill-debug/" 2>/dev/null || true
    return 1
  fi
  event "drill: restored from the archive in $((t1 - t0)) s"
  # Up to the archive's point the restored logs are the lost node's, byte for byte: no
  # acknowledged operation is lost up to it.
  local a r
  a=$(stat -c %s "$CORE_STATE/archive.bin")
  r=$(stat -c %s "$CORE_STATE/replies.jsonl")
  if ! cmp -s -n "$a" "$CORE_STATE/archive.bin" "$lost/archive.bin" || ! cmp -s -n "$r" "$CORE_STATE/replies.jsonl" "$lost/replies.jsonl"; then
    echo "**FAILED**: the restored logs are not the lost node's up to the archive's point" >> "$OUT/summary.md"
    return 1
  fi
  # The window after the point, from the received-log, through the restored core; then the
  # restored node must stand where the lost one did: zero divergence.
  start_core
  (
    set -a
    # shellcheck disable=SC1091
    . "$D/secrets.env"
    set +a
    exec "$OPSCTL" restore core-tail --config "$D/ingress.json" --state "$CORE_STATE"
  ) > "$LOGS/drill-tail.log" 2>&1 || tail_rc=$?
  sleep 3
  kill "$(cat "$OUT/core.pid")" 2>/dev/null || true
  sleep 3
  local same=yes
  cmp -s "$CORE_STATE/archive.bin" "$lost/archive.bin" || same=no
  cmp -s "$CORE_STATE/replies.jsonl" "$lost/replies.jsonl" || same=no
  {
    echo "| | |"
    echo "|---|---|"
    echo "| Archive at the failure | $(tr -d '\n' < "$OUT/drill-status-before.json" | grep -o '"lag_inputs": *[0-9]*' || echo '?') |"
    echo "| Restore (\`opsctl restore core --from\`, VOS roll-forward included) | $((t1 - t0)) s |"
    echo "| Restored to | $(grep -o 'restored to input [0-9]*' "$LOGS/drill-restore.log" | tail -n 1) |"
    echo "| Received-log tail | $(tail -n 1 "$LOGS/drill-tail.log") (exit $tail_rc) |"
    echo "| Logs after the tail equal the lost node's (zero divergence) | $same |"
    echo
    echo '```'
    grep -E '^(INV-|NFR-|composite|archive-|roll-)' "$LOGS/drill-restore.log" || true
    echo '```'
  } >> "$OUT/summary.md"
  [ "$tail_rc" = 0 ] && [ "$same" = yes ]
}
if [ "$DRILL" = 1 ] && [ "$CORE" = agent ]; then
  drill || { echo "[run.sh] the restore drill failed"; rc=1; }
fi
exit "$rc"
