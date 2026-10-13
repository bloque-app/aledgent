#!/bin/bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: AGPL-3.0-or-later
# Turn a `vosx space up` debug log (RUST_LOG=info,vos::agent=debug, ANSI
# stripped) into the spikes CSV format: one row per field of each
#   "Agent supervisor dispatch completed" (queue_wait_us, service_us, payload_bytes) and
#   "physical Agent runtime execution"   (elapsed_us, preparation_us, execution_us, input_bytes, gas_used)
# event, in log order.
#
#   parse-debug.sh <daemon.log> > s4-vosx-debug.csv
set -euo pipefail
echo "spike,case,variant,size,sample,metric,value,unit"
awk '
/Agent supervisor dispatch completed/ {
  n = d++
  for (i = 1; i <= NF; i++) {
    split($i, kv, "=")
    if (kv[1] == "queue_wait_us" || kv[1] == "service_us") printf "s4,vosx,dispatch,0,%d,%s,%s,us\n", n, kv[1], kv[2]
    if (kv[1] == "payload_bytes") printf "s4,vosx,dispatch,0,%d,%s,%s,bytes\n", n, kv[1], kv[2]
  }
}
/physical Agent runtime execution/ {
  n = r++
  for (i = 1; i <= NF; i++) {
    split($i, kv, "=")
    if (kv[1] ~ /_us$/) printf "s4,vosx,runtime-execution,0,%d,%s,%s,us\n", n, kv[1], kv[2]
    if (kv[1] == "input_bytes") printf "s4,vosx,runtime-execution,0,%d,%s,%s,bytes\n", n, kv[1], kv[2]
    if (kv[1] == "gas_used") printf "s4,vosx,runtime-execution,0,%d,%s,%s,gas\n", n, kv[1], kv[2]
  }
}' "$1"
