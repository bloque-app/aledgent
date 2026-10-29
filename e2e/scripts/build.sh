#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
# Build what the zombienet suite runs, inside a `rust:1-bookworm` container on the runner:
#
#   - `spawn_network` (e2e/network, zombienet-sdk), with the stable toolchain;
#   - the settlement contract's bundle, `cargo contract build --release` with Rust 1.91 and
#     cargo-contract 5.0.3, exactly as `.github/workflows/contracts.yml` builds it.
#
# Layout, all under the job-local directory mounted at /w (see e2e-zombienet.yml):
#   /w/src            a copy of the commit under test (the container never writes the checkout)
#   /w/target/*       cargo targets, one per build
#   /w/tools          cargo-contract, installed once and cached between runs
#   /w/dist           the outputs: spawn_network and the contract bundle
#
# The runner has no Rust toolchain of its own; this is how x86-bench.yml builds on the same
# kind of runner.

set -euo pipefail

CARGO_CONTRACT_VERSION="${CARGO_CONTRACT_VERSION:-5.0.3}"
export CARGO_TERM_COLOR=always CARGO_INCREMENTAL=0
mkdir -p /w/dist /w/target /w/tools

echo "::group::system packages"
# litep2p's build script (zombienet-sdk -> sc-network) needs protoc.
apt-get update -qq
apt-get install -y -qq --no-install-recommends protobuf-compiler >/dev/null
protoc --version
echo "::endgroup::"

echo "::group::spawn_network"
start=$(date +%s)
CARGO_TARGET_DIR=/w/target/network cargo build --locked --profile ci \
  --manifest-path /w/src/e2e/network/Cargo.toml --bin spawn_network
cp /w/target/network/ci/spawn_network /w/dist/spawn_network
echo "METER phase=build_network s=$(( $(date +%s) - start ))"
echo "::endgroup::"

echo "::group::cargo-contract ${CARGO_CONTRACT_VERSION}"
start=$(date +%s)
# The contract's toolchain, from contracts/settlement/rust-toolchain.toml. Installed explicitly:
# recent rustup no longer installs a toolchain file's toolchain on first use.
rustup toolchain install 1.91 --profile minimal --component rust-src,rustfmt,clippy --target wasm32-unknown-unknown
if ! /w/tools/bin/cargo-contract contract --version 2>/dev/null | grep -q "$CARGO_CONTRACT_VERSION"; then
  cargo +1.91 install cargo-contract --version "$CARGO_CONTRACT_VERSION" --locked --force --root /w/tools
fi
/w/tools/bin/cargo-contract contract --version
echo "METER phase=build_cargo_contract s=$(( $(date +%s) - start ))"
echo "::endgroup::"

echo "::group::settlement contract"
start=$(date +%s)
cd /w/src/contracts/settlement
PATH="/w/tools/bin:$PATH" CARGO_TARGET_DIR=/w/target/contract cargo contract build --release
bundle=/w/target/contract/ink/aledgent_settlement_contract.contract
[ -s "$bundle" ] || { echo "::error::cargo contract build wrote no bundle at $bundle"; exit 1; }
cp "$bundle" /w/dist/
cp "${bundle%.contract}.wasm" /w/dist/
size=$(stat -c %s "${bundle%.contract}.wasm")
echo "contract code: $size bytes"
echo "METER phase=build_contract s=$(( $(date +%s) - start ))"
echo "::endgroup::"

# The code an upgrade starts from (the `upgrade` phase, T-043-03), when the workflow extracted it
# to /w/v2src (its `upgrade_from` input): the same toolchain, its own target.
if [ -d /w/v2src/contracts/settlement ]; then
  echo "::group::settlement contract to upgrade from"
  start=$(date +%s)
  cd /w/v2src/contracts/settlement
  PATH="/w/tools/bin:$PATH" CARGO_TARGET_DIR=/w/target/contract-v2 cargo contract build --release
  mkdir -p /w/dist/v2
  cp /w/target/contract-v2/ink/aledgent_settlement_contract.contract /w/dist/v2/
  echo "METER phase=build_contract_v2 s=$(( $(date +%s) - start ))"
  echo "::endgroup::"
fi

# The aledgent driver (e2e/aledgent-driver) and the adapter's binary (`aledgent-settle`, with
# `fault-injection`, which only this build enables), only when step 3 runs through aledgent: they
# link the root workspace (ingress, the core, services/settle with its light client). Their target
# is not cached: it is several GB, and the cache is for what every run builds. The adapter runs in
# this same image, so the binary's libraries are the ones it was built against.
if [ "${BUILD_ALEDGENT:-0}" = 1 ]; then
  echo "::group::aledgent driver and adapter"
  start=$(date +%s)
  cd /w/src
  rustup toolchain install stable --profile minimal
  CARGO_TARGET_DIR=/w/target-aledgent CARGO_PROFILE_DEV_DEBUG=line-tables-only \
    cargo build --locked -p aledgent-e2e-driver -p settle --bin aledgent-e2e-driver \
    --bin aledgent-settle --features settle/fault-injection
  cp /w/target-aledgent/debug/aledgent-e2e-driver /w/target-aledgent/debug/aledgent-settle /w/dist/
  echo "METER phase=build_aledgent_driver s=$(( $(date +%s) - start ))"
  echo "::endgroup::"
fi

ls -la /w/dist
