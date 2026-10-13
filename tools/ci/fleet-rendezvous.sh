#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
# The rendezvous of one shards-fleet group (stress.yml, core=shards-fleet). Every node of the group
# has published "rdv-<group>-a<attempt>-<index>-<ipv4>-<public key hex>" as the name of an artifact
# of this run: the names carry everything, so nothing is downloaded.
#
# Node 0 (the router) waits until all NODES have checked in, late joiners included, for up to
# RDV_WAIT_SECS, and writes "<index> <ipv4> <public key hex>" per node to $1, ordered by index. It
# also writes $(dirname $1)/arrivals.md: when each node checked in, and what GitHub says of the
# group's jobs (how many machines the pool actually gave). Every other node waits for node 0 only
# and writes node 0's line and its own: it learns the rest from node 0 over the relay's
# authenticated channel. Polls are spaced (node 0 every 20 s, the others every 30 s) so that a
# group of 16 stays far below the token's API rate.
#
# Env: GH_TOKEN (actions: read), GITHUB_REPOSITORY, GITHUB_RUN_ID, GITHUB_RUN_ATTEMPT, GROUP,
#      INDEX, NODES, RDV_WAIT_SECS (default 2400).
set -euo pipefail
out=$1
: "${GROUP:?}" "${NODES:?}" "${INDEX:?}"
wait_secs=${RDV_WAIT_SECS:-2400}
attempt=${GITHUB_RUN_ATTEMPT:-1}
prefix="rdv-$GROUP-a$attempt-"
api="${GITHUB_API_URL:-https://api.github.com}/repos/$GITHUB_REPOSITORY/actions"
get() { curl -fsS --retry 3 -H "Authorization: Bearer $GH_TOKEN" -H "Accept: application/vnd.github+json" "$1"; }
start=$(date +%s)
started_iso=$(date -u +%Y-%m-%dT%H:%M:%SZ)
arrivals=$(dirname "$out")/arrivals.md
if [ "$INDEX" = 0 ]; then need=$NODES; every=20; else need=1; every=30; fi

# One "<index> <ip> <key> <created_at>" line per node checked in.
list() {
  get "$api/runs/$GITHUB_RUN_ID/artifacts?per_page=100" \
    | tr -d '\n' | grep -o '"name": *"'"$prefix"'[^"]*"[^}]*"created_at": *"[^"]*"' \
    | sed 's/"name": *"\([^"]*\)".*"created_at": *"\([^"]*\)"/\1 \2/' \
    | while read -r name at; do
        rest=${name#"$prefix"}
        idx=${rest%%-*}; rest=${rest#*-}
        ip=${rest%%-*}; key=${rest#*-}
        echo "$idx $ip $key $at"
      done | sort -n -u -k1,1
}

while :; do
  seen=$(list || true)
  if [ "$INDEX" = 0 ]; then
    have=$(grep -c . <<<"$seen" || true)
  else
    have=$(grep -c '^0 ' <<<"$seen" || true)
  fi
  waited=$(( $(date +%s) - start ))
  if [ "$have" -ge "$need" ] || [ "$waited" -ge "$wait_secs" ]; then break; fi
  echo "rendezvous: $have of $need after ${waited} s"
  sleep "$every"
done

if [ "$INDEX" = 0 ]; then
  {
    echo "**Rendezvous** (node 0 waited from $started_iso, $waited s): $have of $NODES nodes checked in."
    echo
    echo "| Node | Address | Checked in (UTC) |"
    echo "|---:|---|---|"
    while read -r idx ip key at; do [ -n "$idx" ] && echo "| $idx | $ip | $at |"; done <<<"$seen"
    echo
    # What GitHub says of the group's jobs: how many the pool gave a machine, how many still queue.
    jobs=$(get "$api/runs/$GITHUB_RUN_ID/attempts/$attempt/jobs?per_page=100" | tr -d '\n' || true)
    for st in queued in_progress completed; do
      n=$(grep -o '"status": *"'"$st"'"[^}]*"name": *"fleet '"$GROUP"'-n[0-9]*"' <<<"$jobs" | grep -c . || true)
      printf '%s: %s; ' "$st" "$n"
    done
    echo "(jobs of group $GROUP, by GitHub's status)"
  } > "$arrivals"
  cat "$arrivals"
  [ -n "${GITHUB_STEP_SUMMARY:-}" ] && cat "$arrivals" >> "$GITHUB_STEP_SUMMARY"
fi

cut -d' ' -f1-3 <<<"$seen" | grep . > "$out" || true
if [ "$INDEX" != 0 ]; then
  grep '^0 ' "$out" > "$out.tmp" || true
  cat "$(dirname "$out")/me.txt" >> "$out.tmp"
  mv "$out.tmp" "$out"
fi
cat "$out"
if [ "$have" -lt "$need" ]; then
  echo "::error::rendezvous: $have of $need node(s) after $waited s (group $GROUP, node $INDEX)"
  exit 1
fi
