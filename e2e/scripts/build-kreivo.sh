#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Build Kreivo's runtime from a commit, as Kreivo's own release workflow builds it
# (`.github/workflows/release.yml` in virto-network/kreivo: Rust 1.91, `wasm32v1-none`,
# `cargo build --profile production --locked -p kreivo-runtime`), for a runtime that has no release
# yet. Runs inside the `rust:1-bookworm` container, like build.sh.
#
#   KREIVO_REF=<branch, tag or commit> build-kreivo.sh
#
# Writes /w/kreivo/kreivo_runtime.compact.compressed.wasm and /w/kreivo/ref (the commit built).
# A release is still the normal input (`kreivo_release`): this exists so that the gate can run
# against a commit before its release is published.

set -euo pipefail
: "${KREIVO_REF:?set KREIVO_REF}"
export CARGO_TERM_COLOR=always CARGO_INCREMENTAL=0

mkdir -p /w/kreivo
cd /w/kreivo
if [ ! -d src/.git ]; then
  for i in 1 2 3; do
    git clone --filter=blob:none https://github.com/virto-network/kreivo.git src && break
    rm -rf src; sleep $((i * 10))
  done
fi
cd src
git fetch --quiet origin "$KREIVO_REF" || true
git checkout --quiet --detach "$KREIVO_REF" 2>/dev/null || git checkout --quiet --detach FETCH_HEAD
sha="$(git rev-parse HEAD)"
echo "Kreivo at $sha ($(git log -1 --format=%s))"

echo "::group::system packages"
apt-get update -qq
apt-get install -y -qq --no-install-recommends protobuf-compiler clang >/dev/null
echo "::endgroup::"

rustup toolchain install 1.91 --profile minimal --component rust-src --target wasm32v1-none
start=$(date +%s)
CARGO_TARGET_DIR=/w/kreivo/target cargo +1.91 build --profile production --locked -p kreivo-runtime
echo "METER phase=build_kreivo_runtime s=$(( $(date +%s) - start ))"
cp /w/kreivo/target/production/wbuild/kreivo-runtime/kreivo_runtime.compact.compressed.wasm /w/kreivo/
echo "$sha" > /w/kreivo/ref
ls -la /w/kreivo/kreivo_runtime.compact.compressed.wasm
