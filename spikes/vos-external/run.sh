#!/bin/bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: AGPL-3.0-or-later
# F-002 re-check (notes/MEASUREMENTS-F002.md §11): S2, S3 and S4 through VOS
# saga's opt-in external-state standard runtime (`experimental-state-blocks`),
# which no released path selects. Runs INSIDE the Linux container of
# spikes/README.md, with VOS saga/agents at 2bb7876 mounted at /vos and
# f002-external-state.patch applied to that checkout (a measurement test in
# vos/src/agent/state_block_pvm_lifecycle_tests.rs plus test-only timing in
# state_block_pvm.rs). Never shipped; links VOS (AGPL-3.0-or-later).
#
#   run.sh build
#   F002_MODE=s4 F002_N=1000 F002_SIZES=65536,1048576,2097152,4063232 [F002_RETAIN=31] \
#     F002_OUT=/results/s4x.csv run.sh run /out/state-blob.vos
#   F002_MODE=s2|s3 F002_N=1000 F002_N_NOOP=200 F002_INPUTS=a.bin,b.bin \
#     F002_OUT=/results/s2x.csv run.sh run /out/kernel-probe.vos
set -euo pipefail
export CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-/target}
case "$1" in
  build)
    mkdir -p "$CARGO_TARGET_DIR/task-tmp"; export TMPDIR="$CARGO_TARGET_DIR/task-tmp"
    # The candidate standard runtime guest (as `just build-agent-standard-state-guest`).
    (cd /vos/services/agent-runtime && CARGO_TARGET_DIR="$CARGO_TARGET_DIR/agent-state-standard" \
      cargo +nightly-2026-03-20 actor --locked --features experimental-state-blocks)
    cd /vos && cargo +nightly-2025-05-09 test --locked --release -p vos --lib \
      --features "agent-runtime storage network http-ingress experimental-state-blocks" \
      --config 'profile.release.lto="off"' --config 'profile.release.codegen-units=16' --no-run
    ;;
  run)
    export F002_PACKAGE=$2
    exe=$(ls -t "$CARGO_TARGET_DIR"/release/deps/vos-* | grep -v '\.d$' | head -1)
    "$exe" --ignored --exact agent::state_block_pvm::lifecycle_tests::f002_external_state_invocation_cost \
      --nocapture --test-threads=1
    ;;
  *) echo "usage: run.sh build | run <package.vos>" >&2; exit 2 ;;
esac
