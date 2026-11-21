#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
# W2 (gate G1): build the spike and measure it. Local or CI (.github/workflows/vos-core-fit.yml).
#
#   spikes/vos-core/run.sh [OUT]
#
# Environment:
#   VOS_REF     vos-core commit (default 1650c474...); a `.vos` already present (a symlink to a
#               checkout) is used as it is
#   STACKS      stack floors to run the workloads at, bytes (default "65536 131072")
#   MIXES       workloads (default "reserve card transfers open tick issue")
#   N           operations a workload (default 40)
#   GROW        people opened before a `reserve` run on a grown book (default 300)
#   CONFORMANCE 1: also the conformance suite through the spike (through-agent/vos)
#
# Measurement only: patches/vos-measure.patch raises vos-core's 2 MiB code bound to 4 MiB and
# lets ALEDGENT_W2_MIN_STACK raise an actor's stack, so that the other bounds can be measured on a
# program the real bounds refuse. Neither change is proposed; both are reported as failures.
set -uo pipefail
HERE=$(cd "$(dirname "$0")" && pwd)
OUT=${1:-$HERE/results/raw/run-$(date -u +%Y%m%dT%H%M%SZ)}
VOS_REF=${VOS_REF:-1650c474da06408026923b8397d338d767e1cd27}
STACKS=${STACKS:-65536 131072}
MIXES=${MIXES:-reserve card transfers open tick issue}
N=${N:-40}
GROW=${GROW:-300}
mkdir -p "$OUT"
LOG=$OUT/run.log
exec > >(tee -a "$LOG") 2>&1
step() { echo; echo "=== $* ($(date -u +%H:%M:%S))"; }

step host
uname -a
grep -m1 'model name' /proc/cpuinfo 2>/dev/null || sysctl -n machdep.cpu.brand_string 2>/dev/null || true
nproc 2>/dev/null || sysctl -n hw.ncpu
cat /proc/sys/vm/unprivileged_userfaultfd 2>/dev/null | sed 's/^/vm.unprivileged_userfaultfd=/' || true

step vos-core
if [ ! -e "$HERE/.vos" ]; then
  git init -q "$HERE/.vos"
  git -C "$HERE/.vos" fetch -q --depth 1 https://github.com/virto-network/vos "$VOS_REF"
  git -C "$HERE/.vos" checkout -q FETCH_HEAD
  git -C "$HERE/.vos" apply "$HERE/patches/vos-measure.patch"
fi
echo "vos-core $(git -C "$HERE/.vos" rev-parse HEAD)"
git -C "$HERE/.vos" diff --stat
(
  cd "$HERE/.vos/agent/programs"
  CARGO_TARGET_DIR=$PWD/target CARGO_INCREMENTAL=0 cargo build -q --release -p vos-agent \
    --target ../../pvm/guest/riscv64em-vos.json
  cargo fetch -q --locked
  touch target/riscv64em-vos/release/.built
) || { echo "FAILED: the vos-core agent program"; exit 1; }

step guests
# opt:features:flag (MERGED: linked without --unique; SOFT: BLAKE2b in software)
for spec in "s:be-memcmp:" "s:heap-probe,be-memcmp:" "s::" "s:be-memcmp:SOFT" "z:be-memcmp:MERGED"; do
  IFS=: read -r opt feats flag <<< "$spec"
  case $flag in
    MERGED) OPT=$opt FEATURES=$feats MERGED=1 "$HERE/guest/build.sh" | tail -1 ;;
    SOFT) OPT=$opt FEATURES=$feats SOFT=1 "$HERE/guest/build.sh" | tail -1 ;;
    *) OPT=$opt FEATURES=$feats "$HERE/guest/build.sh" | tail -1 ;;
  esac
done
G=$HERE/guest/target/out

step host and client
(cd "$HERE/host" && CARGO_TARGET_DIR=$PWD/target cargo build -q --release) || exit 1
(cd "$HERE/client" && CARGO_TARGET_DIR=$PWD/target cargo build -q --release) || exit 1
HOST=$HERE/host/target/release/aledgent-vos-core-host
CLIENT=$HERE/client/target/release/aledgent-g1

step fit
for elf in aledgent-core-vos-s-be-memcmp.elf aledgent-core-vos-z-be-memcmp-merged.elf; do
  echo "--- $elf"
  "$HOST" fit "$G/$elf" 2>/dev/null | tee "$OUT/fit-${elf%.elf}.txt"
done
if [ "$(uname -s)-$(uname -m)" = Linux-x86_64 ]; then
  echo "--- recompiler required (VOS_PVM_BACKEND=recompiler)"
  VOS_PVM_BACKEND=recompiler "$HOST" fit "$G/aledgent-core-vos-s-be-memcmp.elf" 2>/dev/null \
    | tee "$OUT/fit-recompiler.txt"
  BACKENDS="recompiler interpreter"
else
  BACKENDS="interpreter"
fi

port=47100
serve() { # elf csv stack backend [--probe]
  port=$((port + 1))
  ALEDGENT_W2_MIN_STACK=$3 VOS_PVM_BACKEND=$4 "$HOST" serve "$1" "127.0.0.1:$port" "$2" ${5:-} \
    > "$2.serve.log" 2>&1 &
  SERVER=$!
  for _ in $(seq 1 100); do grep -q serving "$2.serve.log" 2>/dev/null && return 0; sleep 0.2; done
  echo "the host did not start"; cat "$2.serve.log"; return 1
}
stop() { kill "$SERVER" 2>/dev/null; wait "$SERVER" 2>/dev/null; }

step "genesis, natively and in the guest, per backend, stack and memcmp"
for backend in $BACKENDS; do
  for stack in $STACKS; do
   for elf in s-be-memcmp s; do
    csv=$OUT/genesis-$backend-$stack-$elf.csv
    serve "$G/aledgent-core-vos-$elf.elf" "$csv" "$stack" "$backend" || continue
    "$CLIENT" "tcp:127.0.0.1:$port" reserve --n 0 --tee > "$csv.client.log" 2>&1
    echo "$backend stack $stack: $(tail -1 "$csv" | cut -d, -f3,8,9) $(grep -c 'TEE MISMATCH' "$csv.client.log") mismatch(es)"
    grep -m1 -A2 'TEE MISMATCH' "$csv.client.log" | grep -oE 'service: \[[0-9, ]{1,24}' || true
    stop
   done
  done
done

BACK=${BACKENDS%% *}
step workloads on $BACK
for stack in $STACKS; do
  for mix in $MIXES; do
    csv=$OUT/mix-$mix-$stack.csv
    serve "$G/aledgent-core-vos-s-be-memcmp.elf" "$csv" "$stack" "$BACK" || continue
    "$CLIENT" "tcp:127.0.0.1:$port" "$mix" --n "$N" > "$csv.client.log" 2>&1
    echo "$mix stack $stack: exit $? $(tail -1 "$csv.client.log")"
    stop
  done
  csv=$OUT/grown-reserve-$stack.csv
  serve "$G/aledgent-core-vos-s-be-memcmp.elf" "$csv" "$stack" "$BACK" || continue
  "$CLIENT" "tcp:127.0.0.1:$port" reserve --n "$N" --grow "$GROW" > "$csv.client.log" 2>&1
  echo "grown reserve stack $stack: exit $? $(tail -1 "$csv.client.log")"
  stop
done

step "BLAKE2b in software, for the precompile's saving"
csv=$OUT/soft-reserve.csv
if serve "$G/aledgent-core-vos-s-be-memcmp-soft.elf" "$csv" "${STACKS##* }" "$BACK"; then
  "$CLIENT" "tcp:127.0.0.1:$port" reserve --n 10 > "$csv.client.log" 2>&1
  echo "soft reserve: exit $? $(tail -1 "$csv.client.log")"
  stop
fi

step heap probe
stack=${STACKS##* }
for mix in $MIXES; do
  csv=$OUT/heap-$mix.csv
  serve "$G/aledgent-core-vos-s-heap-probe-be-memcmp.elf" "$csv" "$stack" "$BACK" --probe || continue
  "$CLIENT" "tcp:127.0.0.1:$port" "$mix" --n "$N" > "$csv.client.log" 2>&1
  echo "heap $mix: exit $? $(tail -1 "$csv.client.log")"
  stop
done

python3 "$HERE/summarize.py" "$OUT" > "$OUT/summary.md"

if [ "${CONFORMANCE:-0}" = 1 ]; then
  step conformance through the spike
  csv=$OUT/conformance.csv
  if serve "$G/aledgent-core-vos-s-be-memcmp.elf" "$csv" "$stack" "$BACK"; then
    (cd "$HERE/../.." && CONFORMANCE_TARGETS=through-agent/vos \
      CONFORMANCE_AGENT_HOST="tcp:127.0.0.1:$port" timeout "${CONFORMANCE_MINUTES:-40}m" \
      cargo test -q --release -p conformance > "$csv.client.log" 2>&1)
    echo "conformance: exit $? $(grep -E '^test result' "$csv.client.log" | tail -1)"
    stop
  fi
fi

step summary
python3 "$HERE/summarize.py" "$OUT" | tee "$OUT/summary.md"
