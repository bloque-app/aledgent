#!/bin/bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: AGPL-3.0-or-later
# Build and run the F-002 VOS harness. Runs INSIDE the Linux container (see
# spikes/README.md): VOS saga 7bae926 at /vos, this directory at
# /spikes-vos-host, packages in /out, results written to /results.
#
#   run.sh <vos-host arguments...>
set -euo pipefail
rm -rf /work/vos-host && mkdir -p /work && cp -r /spikes-vos-host /work/vos-host
# Resolve dependencies exactly as the pinned VOS workspace does.
cp /vos/Cargo.lock /work/vos-host/Cargo.lock
cd /work/vos-host
# VOS saga after 7bae926 (2bb7876, MEASUREMENTS-F002 §11) adds a signed
# `external_state_limits` field to the runtime manifest; `None` is the
# byte-identical r19 tag the bundled runtime carries. Older checkouts lack it.
if grep -q 'pub external_state_limits' /vos/vos-agent-sdk/src/package.rs; then
  sed -i 's/^\( *\)outer_program: BlobRef::of_bytes(pvm),/\1external_state_limits: None,\n&/' src/main.rs
fi
# VOS proposal 0003 (declared actor resources): enables `pad-package`.
if grep -q 'pub resources: Option<ActorResources>' /vos/vos-agent-sdk/src/package.rs; then
  export RUSTFLAGS="${RUSTFLAGS:-} --cfg vos_declared"
fi
# VOS proposal 0005 (recompiled Agent execution): enables `F025_BACKEND`,
# `F025_LOCKSTEP` and `backends`.
if grep -q 'pub enum RefineBackend' /vos/pvm/runtime/src/refine.rs; then
  export RUSTFLAGS="${RUSTFLAGS:-} --cfg vos_backends"
fi
T=${VOS_HOST_TARGET:-/target/vos-host}
CARGO_TARGET_DIR=$T cargo +nightly-2025-05-09 build --release -j 8 2>&1 | grep -E "^(error|warning: unused)|Finished" || true
exec $T/release/vos-host "$@"
