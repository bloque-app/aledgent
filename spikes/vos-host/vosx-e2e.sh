#!/bin/bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: AGPL-3.0-or-later
# S4 through the canonical `vosx` path (Linux container, daemon already up,
# Local Agent created and state-blob installed; intents written by
# `vos-host atq1`). Times each `vosx space invoke-local` at the caller.
#
#   vosx-e2e.sh <space> <intent-dir> <first> <count> <out.csv>
set -uo pipefail
space=$1; dir=$2; first=$3; count=$4; out=$5
[ -f "$out" ] || echo "spike,case,variant,size,sample,metric,value,unit" > "$out"
for ((i = first; i < first + count; i++)); do
  f=$(printf "%s/touch-%05d.atq1" "$dir" "$i")
  t0=$(date +%s%N)
  vosx space invoke-local "$space" --intent "$f" > "/tmp/invoke-$i.json" 2>&1
  rc=$?
  t1=$(date +%s%N)
  retired=$(grep -o '"delivery_retired":[a-z]*' "/tmp/invoke-$i.json" | head -1)
  metric=e2e_us; [ $rc -eq 0 ] || metric=failed_e2e_us
  echo "s4,touch,vosx-invoke-local,65536,$i,$metric,$(( (t1 - t0) / 1000 )),us" >> "$out"
  echo "$i rc=$rc $retired $(( (t1 - t0) / 1000000 )) ms"
  if [ $rc -ne 0 ]; then tail -3 "/tmp/invoke-$i.json"; fi
done
