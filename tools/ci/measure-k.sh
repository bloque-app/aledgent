#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
# The ceiling of a sharded service library (rd-shard-scope-2): k core Agents, each the whole
# library over a disjoint slice of accounts (its own book, its own two funded accounts), with no
# message between them, run side by side on one machine, one per core (cores 1..k; core 0 left to
# the hosts' own threads). For each mix and each k: every Agent's inputs per second over its own
# run (`batch:` line of `aledgent-core-host measure`, needs --batch > 1), their sum, and the
# aggregate (all inputs over the wall time of the slowest, set-up included).
#
# Env: WORK, OUT, CORE_KIND (agent-recompiler | agent-interpreter), MEASURE_ARGS (common
# arguments, e.g. `--n 1500 --warmup 50 --batch 32`), KS (e.g. "1 2 4 8 15"), MIXES (e.g. "card
# transfers"). Writes $OUT/measure-k.csv and a markdown table on stdout.
set -euo pipefail
RT=$WORK/out/core/agent_runtime.pvm
LOOKUP=0
if [ -s "$WORK/out/core/profiles.txt" ]; then
  read -r plabel plookup _ < "$WORK/out/core/profiles.txt"
  RT=$WORK/out/core/rt-$plabel.pvm
  LOOKUP=$plookup
fi
H=$WORK/out/core/aledgent-core-host
ncpu=$(nproc)
pin=""
command -v taskset > /dev/null && pin=taskset
echo "mix,k,agent,inputs,milli_ops_per_s_mean,round_p50_us,actor_gas_p50" > "$OUT/measure-k.csv"
echo "| Mix | k Agents | Inputs/s, sum of each Agent's rate | Inputs/s, aggregate over wall time | Per Agent (mean) | Round p50 ms | Failed |"
echo "|---|---:|---:|---:|---:|---:|---:|"
for mix in $MIXES; do
  for k in ${KS//,/ }; do
    d=$OUT/k/$mix-$k
    mkdir -p "$d"
    t0=$(date +%s%N)
    pids=()
    for i in $(seq 1 "$k"); do
      core=$(( i % ncpu ))
      # shellcheck disable=SC2086
      ( cd "$d" && ${pin:+taskset -c $core} env F025_BACKEND=${CORE_KIND#agent-} F025_PROGRAM_LOOKUP=$LOOKUP \
          "$H" measure "$RT" "$WORK/out/core/core.vos" "$d/m$i.csv" --mix "$mix" $MEASURE_ARGS \
          > "$d/out$i.txt" 2> "$d/log$i.txt" ) &
      pids+=($!)
    done
    failed=0
    for p in "${pids[@]}"; do wait "$p" || failed=$((failed + 1)); done
    t1=$(date +%s%N)
    sum=0; inputs=0; p50s=0; n=0
    for i in $(seq 1 "$k"); do
      line=$(grep '^batch: [0-9]' "$d/out$i.txt" | tail -n 1 || true)
      [ -n "$line" ] || continue
      IFS=, read -r _ _ inp _ rp50 _ _ agas _ mean <<<"${line#batch: }"
      echo "$mix,$k,$i,$inp,$mean,$rp50,$agas" >> "$OUT/measure-k.csv"
      sum=$((sum + mean)); inputs=$((inputs + inp)); p50s=$((p50s + rp50)); n=$((n + 1))
    done
    wall_ms=$(( (t1 - t0) / 1000000 ))
    agg=$(( wall_ms > 0 ? inputs * 1000 / wall_ms : 0 ))
    per=$(( n > 0 ? sum / n : 0 ))
    rp=$(( n > 0 ? p50s / n / 1000 : 0 ))
    printf '| %s | %s | %s.%02d | %s | %s.%02d | %s | %s |\n' "$mix" "$k" $((sum / 1000)) $(( (sum % 1000) / 10 )) "$agg" $((per / 1000)) $(( (per % 1000) / 10 )) "$rp" "$failed"
  done
done
