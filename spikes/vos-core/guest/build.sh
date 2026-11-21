#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: AGPL-3.0-or-later
# Build the spike's guest: target/riscv64em-vos/release/aledgent-core-vos.elf.
#   OPT=s|z|3   opt-level (default: the manifest's, s)
#   FEATURES=heap-probe
#   SECTIONS=one   one text section a codegen unit (-Zfunction-sections=no): with --emit-relocs
#                  every input section keeps a relocation section, and vos-core's ELF parser takes
#                  at most 4096 sections (opt-level z makes 5309)
#   MERGED=1       link without `--unique` (merged/riscv64em-vos.json): the relocation sections
#                  of the text merge, so opt-level z stays under the 4096 sections
# The ELF is copied to target/out/aledgent-core-vos-<opt>[-probe].elf.
set -euo pipefail
cd "$(dirname "$0")"
../cipherclerk.sh
args=(--release --target riscv64em-vos.json)
[ -n "${MERGED:-}" ] && args=(--release --target merged/riscv64em-vos.json)
[ -n "${FEATURES:-}" ] && args+=(--features "$FEATURES")
[ -n "${SOFT:-}" ] && args+=(--no-default-features --features full)
[ -n "${CORE_ONLY:-}" ] && args+=(--no-default-features --features precompile)
if [ -n "${OPT:-}" ]; then export CARGO_PROFILE_RELEASE_OPT_LEVEL=$OPT; fi
if [ "${SECTIONS:-}" = one ]; then
  export RUSTFLAGS="-Zunstable-options -Clink-arg=-Tpvm.ld -Zfunction-sections=no"
fi
export CARGO_TARGET_DIR="$PWD/target"
CARGO_INCREMENTAL=0 cargo build "${args[@]}"
opt=${OPT:-s}; suffix=""; [ -n "${FEATURES:-}" ] && suffix="-${FEATURES//,/-}"
[ "${SECTIONS:-}" = one ] && suffix="$suffix-one"
[ -n "${MERGED:-}" ] && suffix="$suffix-merged"
[ -n "${SOFT:-}" ] && suffix="$suffix-soft"
[ -n "${CORE_ONLY:-}" ] && suffix="$suffix-core-only"
mkdir -p target/out
cp target/riscv64em-vos/release/aledgent-core-vos.elf "target/out/aledgent-core-vos-$opt$suffix.elf"
echo "target/out/aledgent-core-vos-$opt$suffix.elf"
