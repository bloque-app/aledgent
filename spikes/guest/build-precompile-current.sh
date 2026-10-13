#!/bin/bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: AGPL-3.0-or-later
# F-025 (T-025-02): build kernel-probe with CipherClerk's `pvm-precompile`
# against the precompile shim of the mounted VOS (pvm/proof/precompiles,
# crate vos-pvm-precompiles) instead of the shim CipherClerk e9a7b08 pins
# (zkpvm-precompiles at VOS 9f5337f). The pinned shim emits an unmarked
# `ecall`, which today's VOS PVM compiler translates into a trap; the current
# shim emits the `csrw 0x801` marker that makes it `ecalli 110..114`.
# Runs INSIDE the Linux container, like build.sh, with a copy of CipherClerk
# e9a7b08 at /cc whose only change is that dependency line:
#   zkpvm-precompiles = { package = "vos-pvm-precompiles",
#                         path = "/vos/pvm/proof/precompiles", optional = true }
set -euo pipefail
work=/work/kernel-probe-precompile-current
rm -rf "$work" /tmp/scaffold
mkdir -p /work /out
vosx actor new /tmp/scaffold >/dev/null
cp -r /spikes-guest/kernel-probe "$work"
cp -r /tmp/scaffold/.cargo /tmp/scaffold/pvm.ld /tmp/scaffold/riscv64em-vos.json \
      /tmp/scaffold/rust-toolchain.toml "$work"/
sed -i 's/^default = \["bin"\]/default = ["bin", "precompile"]/' "$work/Cargo.toml"
cat >> "$work/Cargo.toml" <<'PATCH'

[patch."https://codeberg.org/Virto/CipherClerk.git"]
cipher-clerk = { path = "/cc" }
PATCH
vosx actor build "$work" --name kernel-probe-precompile-current --out-dir /out
ls -l /out/kernel-probe-precompile-current*
