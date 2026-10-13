#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
# The x86-64 bench modes of .github/workflows/x86-bench.yml (F-025, issue #66).
#
# Runs inside `rust:1-bookworm` on a `zombienet` runner, with the job-local work directory
# mounted at /w and nothing else of the host:
#
#   /w/aledgent   this commit of aledgent (a copy, not the runner's checkout)
#   /w/vos        the VOS checkout under test
#   /w/target     every cargo build          /w/cargo-{registry,git}  cargo's caches
#   /w/out        what the job uploads: summary.md, logs/, CSVs, the built programs
#
# The workflow deletes /w when the job ends, whatever the outcome.
#
#   run.sh vos-suites          VOS's own suites, on both engines where the branch has them
#   run.sh batch-curve         one invocation's cost against the commands it carries (F-032)
#   run.sh build-core          the core Agent's host, runtime and package only, for the stress
#                              workflow (F-032); with BUILD_SHARDS=1 also the shard Agents'
#                              package (F-031 T-031-20), every per-invocation bound raised
#   run.sh reservation-timing  the core Agent's reservation, interpreter against recompiler, and
#                              then batched on the recompiler
#   run.sh chainspec-genesis   F-036 (owner ruling genesis-chainspec): a synthetic chainspec of GROWN
#                              times the synthetic reference book (100,000 identities and as many
#                              accounts; synthetic, not any deployment's), built by the operator's
#                              migration tool `migrate` (not in this repository), its native `build-spec`, the core host's
#                              boot to a sealed genesis through `serve` and its restart on the
#                              recompiler, with a development signer; and `verify-genesis` on a
#                              small chainspec of VERIFY_IDENTITIES identities, or with
#                              VERIFY_FULL=1 on the GROWN chainspec itself (ruling a0022-q2: a
#                              chainspec book takes no command until it passes)
#
# Inputs (environment): VOS_URL, VOS_REF, and for reservation-timing VOS_PICKS (commits of
# VOS_URL cherry-picked onto VOS_REF, space-separated) and BATCHES (inputs per invocation of the
# batched runs, space-separated; default "1 8 32", F-031 T-031-05) and BATCH_GAS (VOS's actor gas
# per invocation for a second set of batched runs; empty: none).
# shellcheck disable=SC2016 # backticks in single quotes are Markdown for the job summary
set -euo pipefail

MODE=${1:?mode}
BATCHES=${BATCHES:-1 8 32}
# Reservations per batched run: whole rounds of 8, 32 and 128.
BATCH_N=384
W=/w
A=$W/aledgent
V=$W/vos
OUT=$W/out
LOGS=$OUT/logs
SUMMARY=$OUT/summary.md
mkdir -p "$LOGS"

export CARGO_TARGET_DIR=$W/target
export CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUG=line-tables-only
export CARGO_PROFILE_TEST_DEBUG=line-tables-only
export CARGO_NET_RETRY=10
export CARGO_NET_GIT_FETCH_WITH_CLI=true
export CARGO_TERM_COLOR=never
export CARGO_TERM_PROGRESS_WHEN=never

say() { echo "::group::$*"; }
done_() { echo "::endgroup::"; }
md() { printf '%s\n' "$@" >> "$SUMMARY"; }

# Codeberg is sometimes slow or refuses a connection: five attempts, 10 s doubling.
retry() {
  local n=1 delay=10
  until "$@"; do
    if [ "$n" -ge 5 ]; then
      echo "failed after $n attempts: $*" >&2
      return 1
    fi
    echo "attempt $n failed, retrying in ${delay}s: $*" >&2
    sleep "$delay"
    n=$((n + 1))
    delay=$((delay * 2))
  done
}

toolchain_of() { sed -n 's/^channel *= *"\(.*\)"/\1/p' "$1"; }

setup() {
  say "system packages"
  retry apt-get update -qq
  retry apt-get install -y -qq --no-install-recommends \
    clang lld pkg-config libssl-dev cmake protobuf-compiler > /dev/null
  done_

  say "VOS: $VOS_URL $VOS_REF"
  git init -q "$V"
  git -C "$V" remote add origin "$VOS_URL"
  # Depth 2: a cherry-pick needs the parent of what it picks, and so does `git log -2`.
  retry git -C "$V" fetch -q --depth 2 origin "$VOS_REF"
  git -C "$V" checkout -q FETCH_HEAD
  git -C "$V" log -1 --format='%H %s'
  done_

  say "toolchains VOS pins"
  local files=("$V/rust-toolchain.toml") f
  for f in services/agent-runtime examples/agent-runtimes/custom-linear; do
    if [ -f "$V/$f/rust-toolchain.toml" ]; then files+=("$V/$f/rust-toolchain.toml"); fi
  done
  if [ "$MODE" != vos-suites ]; then
    files+=("$A/agents/rust-toolchain.toml" "$A/agents/core/rust-toolchain.toml")
  fi
  # The conformance suite is the root workspace's (F-015); so are migrate and the signer (F-036).
  if [ "$MODE" = conformance ] || [ "$MODE" = chainspec-genesis ]; then
    files+=("$A/rust-toolchain.toml" "$A/agents/aledgent-runtime/rust-toolchain.toml")
  fi
  local tc
  for tc in $(for f in "${files[@]}"; do toolchain_of "$f"; done | sort -u); do
    retry rustup toolchain install "$tc" --profile minimal --component rust-src --component rustfmt
    rustup run "$tc" rustc --version
  done
  done_
}

host_facts() {
  md "### Host" "" '```'
  {
    uname -srm
    grep -m1 'model name' /proc/cpuinfo | sed 's/^model name[[:space:]]*: /cpu: /'
    echo "cpus: $(nproc)"
    grep -o -w -E 'avx2|avx512f' /proc/cpuinfo | sort -u | tr '\n' ' '
    echo
    grep MemTotal /proc/meminfo
    echo "load: $(cut -d' ' -f1-3 /proc/loadavg)"
  } >> "$SUMMARY"
  md '```' ""
}

vos_facts() {
  md "### VOS" "" "- \`$VOS_URL\` \`$VOS_REF\`" '' '```'
  git -C "$V" log --format='%h %s' -n "${1:-1}" >> "$SUMMARY"
  md '```' ""
}

# One line per test binary of a `cargo test` log: target, passed, failed, ignored. A binary that
# crashed has no result line; `failed_targets` still names it. A result with tests filtered out is
# a test re-running its own binary for one case in a child process, and is labelled so.
results() {
  awk '
    /^ *Running / { t = $0; sub(/^ *Running /, "", t); sub(/ \(.*$/, "", t) }
    /^ *Doc-tests / { t = "doc-tests " $2 }
    /^test result:/ {
      p = f = i = x = 0
      for (k = 1; k < NF; k++) {
        if ($(k + 1) ~ /^passed/) p = $k
        if ($(k + 1) ~ /^failed/) f = $k
        if ($(k + 1) ~ /^ignored/) i = $k
        if ($(k + 1) ~ /^filtered/) x = $k
      }
      printf "%s\t%s\t%s\t%s\n", (x > 0 ? t " (child run of one test)" : t), p, f, i
    }
  ' "$1"
}

# The targets cargo reports as failed (`--no-fail-fast` lists them at the end).
failed_targets() {
  awk '
    /^error: test failed, to rerun pass `/ { c = $0; sub(/^.*pass `/, "", c); sub(/`.*$/, "", c); print c }
    /^error: [0-9]+ targets failed:/ { l = 1; next }
    l && /^    `/ { c = $0; gsub(/^ *`|`$/, "", c); print c; next }
    { l = 0 }
  ' "$1"
}

# Test names listed under a `failures:` block of a libtest log.
failures_of() {
  awk '/^failures:$/ { f = 1; next } /^test result:/ { f = 0 } f && /^    [A-Za-z_][A-Za-z0-9_:]*$/ { print $1 }' "$1" | sort -u
}

FAILED=0

# suite <label> <log-name> <env ...> -- <cargo test args ...>
suite() {
  local label=$1 name=$2
  shift 2
  local envs=()
  while [ "$1" != -- ]; do envs+=("$1"); shift; done
  shift
  local log=$LOGS/$name.log
  say "$label"
  local started=$SECONDS rc=0
  (cd "$V" && env "${envs[@]}" cargo test --locked --no-fail-fast "$@") > "$log" 2>&1 || rc=$?
  local took=$((SECONDS - started))
  tail -n 30 "$log"
  done_
  [ "$rc" -eq 0 ] || FAILED=1
  md "#### $label" "" "\`${envs[*]} cargo test --locked --no-fail-fast $*\` — exit $rc, ${took} s (build included)" ""
  md "| Test binary | Passed | Failed | Ignored |" "|---|---:|---:|---:|"
  results "$log" | awk -F'\t' '{ printf "| `%s` | %s | %s | %s |\n", $1, $2, $3, $4 }' >> "$SUMMARY"
  local f
  f=$(failures_of "$log")
  if [ -n "$f" ]; then
    md "" "Failed tests: $(echo "$f" | sed 's/.*/`&`/' | paste -sd' ' -)"
  fi
  f=$(failed_targets "$log")
  if [ -n "$f" ]; then
    md "" "Failed targets (a target with no row above crashed): $(echo "$f" | sed 's/.*/`&`/' | paste -sd' ' -)"
  fi
  f=$(grep -c '^skipping ' "$log" || true)
  if [ "$f" -gt 0 ]; then
    md "" "Tests that passed by skipping themselves (\`skipping ...\` in the log): $f."
  fi
  md ""
}

# Re-run each failure of a log alone, one thread: separates contention from a real failure.
alone() {
  local name=$1
  shift
  local f
  f=$(failures_of "$LOGS/$name.log" | head -n 20)
  [ -n "$f" ] || return 0
  md "Each failure re-run alone (\`--exact --test-threads=1\`):" "" "| Test | Alone |" "|---|---|"
  local t rc
  for t in $f; do
    rc=0
    (cd "$V" && cargo test --locked "$@" -- --exact "$t" --test-threads=1) \
      >> "$LOGS/$name-alone.log" 2>&1 || rc=$?
    if [ "$rc" -eq 0 ]; then md "| \`$t\` | passed |"; else md "| \`$t\` | failed |"; fi
  done
  md ""
}

vector_lines() {
  grep -E '^test .*(v080|every_gas_budget|nested_refine|satisfies_every_vector).* \.\.\. ' "$1" |
    sed 's/^test //' | sort -u || true
}

vos_suites() {
  md "## x86-64 bench: vos-suites" ""
  host_facts
  vos_facts 1
  md "Toolchain: \`$(cd "$V" && rustc --version)\`." ""
  md "\`GREY_PVM\` picks the kernel path's engine (the recompiler by default on x86-64). The Refine" \
    "path (Agents) is the interpreter unless a test selects the recompiler: \`refine_backends.rs\`" \
    "and \`agent_runtime_pvm.rs\` run both engines in lockstep themselves." ""

  suite "vos-pvm, kernel path on the recompiler" vos-pvm-recompiler -- -p vos-pvm
  suite "vos-pvm, kernel path on the interpreter" vos-pvm-interpreter GREY_PVM=interpreter -- -p vos-pvm

  md "#### Gray Paper v0.8.0 vectors (\`pvm/runtime/tests/VECTORS.md\`)" ""
  md "Kernel path on the recompiler:" "" '```'
  vector_lines "$LOGS/vos-pvm-recompiler.log" >> "$SUMMARY"
  md '```' "" "Kernel path on the interpreter:" "" '```'
  vector_lines "$LOGS/vos-pvm-interpreter.log" >> "$SUMMARY"
  md '```' ""

  # Two `vos --lib` tests read a guest fixture that VOS's `just test` builds first
  # (`just build-agent-recovery-fixture`). Build it the same way.
  local fixture=$V/examples/agent-runtimes/custom-linear
  local scripted=$W/target/agent-recovery-artifacts/scripted
  if grep -qs scripted-fixture "$fixture/Cargo.toml"; then
    say "fixture: scripted recovery guest"
    if (cd "$fixture" && CARGO_TARGET_DIR=$scripted cargo actor --locked --features scripted-fixture) \
      > "$LOGS/build-recovery-fixture.log" 2>&1; then
      export AGENT_SCRIPTED_RUNTIME_ELF=$scripted/riscv64em-vos/release/custom_linear_agent_runtime.elf
      md "Fixture built first, as \`just build-agent-recovery-fixture\` does: the scripted recovery guest." ""
    else
      tail -n 40 "$LOGS/build-recovery-fixture.log"
      md "The scripted recovery guest fixture **did not build** (\`logs/build-recovery-fixture.log\`); the tests that read it fail." ""
    fi
    done_
  fi
  # One more reads the echo extension that `just build-extensions` builds; without it the test
  # passes by skipping itself. It looks in `<workspace>/target/debug`, whatever CARGO_TARGET_DIR says.
  if grep -qs 'name = "echo-extension"' "$V/tests/fixtures/extensions/echo/Cargo.toml"; then
    say "fixture: echo extension"
    if (cd "$V" && CARGO_TARGET_DIR=$V/target cargo build --locked -p echo-extension) > "$LOGS/build-echo-extension.log" 2>&1; then
      md "Fixture built first, as \`just build-extensions\` does: \`echo-extension\`." ""
    else
      tail -n 40 "$LOGS/build-echo-extension.log"
      md "\`echo-extension\` **did not build** (\`logs/build-echo-extension.log\`); the test that loads it skips itself." ""
    fi
    done_
  fi

  # VOS_LIB_FEATURES: extra features for `vos --lib` (its tests use the `pvm` helpers; at
  # e6f2bb45 they do not compile with the default features alone).
  local lib=(-p vos --lib)
  if [ -n "${VOS_LIB_FEATURES:-}" ]; then lib+=(--features "$VOS_LIB_FEATURES"); fi
  suite "vos --lib, kernel path on the recompiler" vos-lib-recompiler -- "${lib[@]}"
  alone vos-lib-recompiler "${lib[@]}"
  suite "vos --lib, kernel path on the interpreter" vos-lib-interpreter GREY_PVM=interpreter -- "${lib[@]}"
  alone vos-lib-interpreter "${lib[@]}"
  if [ -f "$V/vos/tests/agent_runtime_pvm.rs" ]; then
    suite "vos agent_runtime_pvm (every execution on both engines)" vos-agent-runtime-pvm -- -p vos --test agent_runtime_pvm
    alone vos-agent-runtime-pvm -p vos --test agent_runtime_pvm
  fi
  return "$FAILED"
}

# One constant raised in the VOS tree; the build fails if it is not where it is expected.
raise_one() {
  local file=$1 name=$2 value=$3 current
  # Never lower a bound a later VOS already set higher (perf/agent-stack: 4 608 KiB programs).
  current=$(sed -n -E "s/^(pub )?const ${name}: [A-Za-z0-9_]+ = ([0-9_ *]+);$/\2/p" "$V/$file" | tr -d '_ ')
  if [ -n "$current" ] && [ "$((current))" -ge "$(( $(echo "$value" | tr -d '_ ') ))" ]; then
    echo "kept ${name} in ${file} at $current (not below ${value})"
    return 0
  fi
  sed -i -E "s/^((pub )?const ${name}: [A-Za-z0-9_]+ = ).*;/\1${value};/" "$V/$file"
  grep -q -E "^(pub )?const ${name}: [A-Za-z0-9_]+ = ${value//\*/\\*};" "$V/$file" || {
    echo "raise_limits: ${name} not found in ${file}" >&2
    return 1
  }
  echo "raised ${name} in ${file} to ${value}"
}

# The per-invocation bounds, raised far for an experiment (the stress suite's, F-032 T-032-07):
# a layout-3 Agent (F-031 lever 2) keeps its book in its rows, which a Local Agent's image carries
# whole, so the stock execution-state bound refuses it within a few dozen operations.
raise_limits() {
  # The program: the core with layout 3 links more than 2 560 KiB (the summary gives its size).
  raise_one vos-agent-sdk/src/runtime.rs MAX_RUNTIME_PROGRAM_BYTES "4_096 * 1024"
  raise_one vos/src/agent/execution.rs MAX_EXECUTION_PROGRAM_BYTES "4096 * 1024"
  raise_one vos-agent-sdk/src/runtime.rs MAX_INVOCATION_MESSAGE_BYTES "16 * 1024 * 1024"
  raise_one vos-agent-sdk/src/runtime.rs MAX_INVOCATION_REPLY_BYTES "16 * 1024 * 1024"
  raise_one vos-agent-sdk/src/runtime.rs MAX_RUNTIME_CALLER_AVAILABILITY_BYTES "128 * 1024 * 1024"
  raise_one vos-agent-sdk/src/lib.rs MAX_RUNTIME_STATE_BYTES "512 * 1024 * 1024"
  raise_one vos/src/agent/execution.rs MAX_EXECUTION_REPLY_BYTES "16 * 1024 * 1024"
  raise_one vos/src/agent/execution.rs MAX_EXECUTION_STATE_BYTES "64 * 1024 * 1024"
  raise_one vos/src/agent/execution.rs MAX_EXECUTION_STATE_TOTAL_BYTES "64 * 1024 * 1024"
  raise_one vos/src/agent/execution.rs MAX_EXECUTION_GAS "1_000_000_000_000"
  raise_one vos/src/agent/execution.rs MAX_RUNTIME_STATE_BYTES "512 * 1024 * 1024"
  raise_one vos/src/agent/execution.rs MAX_EXECUTION_HOST_CALLS "1024 * 1024"
  # A storage read counts as a fetch: stock VOS allows 16 plus one per blob in an invocation,
  # which a committed map (a few reads per leaf touched) passes at once.
  raise_one vos/src/agent/execution.rs MAX_EXECUTION_FETCH_CALLS "1024 * 1024"
  perl -0pi -e 's/const MAX_EXECUTION_BLAKE2B_COMPRESS_CALLS: usize =\s*[^;]*;/const MAX_EXECUTION_BLAKE2B_COMPRESS_CALLS: usize = 16 * 1024 * 1024;/' "$V/vos/src/agent/execution.rs"
  grep -q '^const MAX_EXECUTION_BLAKE2B_COMPRESS_CALLS: usize = 16 \* 1024 \* 1024;' "$V/vos/src/agent/execution.rs"
  raise_one vos/src/agent/driver.rs DEFAULT_MANAGEMENT_GAS "1_000_000_000_000"
  # The heaps: the runtime's (first HEAP_SIZE; its code sits at 48 MiB, so 44 MiB is as far as it
  # goes) and an application actor's (the last); the core's code moves above the larger heap.
  sed -i -E '0,/^const HEAP_SIZE: usize = 32 \* 1024 \* 1024;/s//const HEAP_SIZE: usize = 44 * 1024 * 1024;/' "$V/vos/src/abi/pvm/alloc.rs"
  grep -q '^const HEAP_SIZE: usize = 44 \* 1024 \* 1024;' "$V/vos/src/abi/pvm/alloc.rs"
  local ld text lds
  if grep -q '^const HEAP_SIZE: usize = 256 \* 1024;' "$V/vos/src/abi/pvm/alloc.rs"; then
    sed -i -E 's/^const HEAP_SIZE: usize = 256 \* 1024;/const HEAP_SIZE: usize = 64 * 1024 * 1024;/' "$V/vos/src/abi/pvm/alloc.rs"
    grep -q '^const HEAP_SIZE: usize = 64 \* 1024 \* 1024;' "$V/vos/src/abi/pvm/alloc.rs"
    # Every guest's code moves above the 64 MiB heap.
    text=0x10000000
    lds="$A/agents/core/pvm.ld $A/agents/shard/pvm.ld"
  else
    # Later VOS (perf/agent-stack) sizes an actor's heap by a feature of `vos`: the shard guest
    # asks for the largest it offers (16 MiB), and only its code moves, to 32 MiB: an actor's
    # declared resources must stay within VOS's heap bound, so not to 256 MiB.
    grep -q 'actor-heap-16m' "$V/vos/Cargo.toml"
    ACTOR_HEAP_FEATURE=vos/actor-heap-16m
    text=0x2000000
    lds="$A/agents/shard/pvm.ld"
  fi
  for ld in $lds; do
    sed -i -E "s/^    \.text 0x900000 :/    .text $text :/" "$ld"
    grep -q "^    \.text $text :" "$ld"
  done
}

# The batched runs of reservation-timing (F-031 T-031-05): one table row per batch size.
# batched <label> <tag>; uses H, RT and PKG from reservation_timing.
batched() {
  local label=$1 tag=$2 bt rc
  md "### Batched invocations (recompiler, F-031 T-031-05): $label" ""
  md "\`aledgent-core-host measure ... --n $BATCH_N --warmup 3 --batch B\`: B reservations staged, then sent in as few invocations as VOS's bounds allow (\`BatchLimits\`, with the actor gas of this VOS). Gas is per invocation; inputs per second are of round time, staging included." ""
  md "| Batch | Exit | Rounds | Inputs | Invocations | Round p50 µs | Round p99 µs | Invoke gas p50 | Actor gas p50 | Inputs/s at round p50 | Inputs/s over the run |" \
    "|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|"
  for bt in $BATCHES; do
    say "recompiler, batch $bt ($tag)"
    rc=0
    F025_BACKEND=recompiler "$H" measure "$RT" "$PKG" "$OUT/core-batch-$tag-$bt.csv" --n "$BATCH_N" --warmup 3 \
      --batch "$bt" > "$OUT/core-batch-$tag-$bt.txt" 2> "$LOGS/core-batch-$tag-$bt.log" || rc=$?
    cat "$OUT/core-batch-$tag-$bt.txt"
    tail -n 5 "$LOGS/core-batch-$tag-$bt.log"
    [ "$rc" -eq 0 ] || FAILED=1
    done_
    awk -F, -v rc="$rc" -v b="$bt" '
      /^batch: [0-9]/ {
        sub(/^batch: /, "")
        printf "| %s | %s | %s | %s | %s | %s | %s | %s | %s | %.2f | %.2f |\n", $1, rc, $2, $3, $4, $5, $6, $7, $8, $9 / 1000, $10 / 1000
        found = 1
      }
      END { if (!found) printf "| %s | %s | | | | | | | | | |\n", b, rc }' "$OUT/core-batch-$tag-$bt.txt" >> "$SUMMARY"
  done
  md ""
}

# The batched runs of reservation-timing: VOS's actor gas as it is, then raised to BATCH_GAS.
# Uses H, RT, PKG, V, A and W of reservation_timing (bash scoping) and may replace RT.
batched_runs() {
  # F-031 Lever 1 (T-031-05): the same reservations on the recompiler, BATCHES inputs per
  # invocation. A round is the staging of its inputs and their invocations; throughput is inputs
  # per second of round. Observations, not requirements. First with VOS's actor gas per invocation
  # as it is (1 000 M at de0f12f: three reservations), then, if BATCH_GAS is set, with it raised
  # in the checkout to BATCH_GAS (a VOS change; the packs then bind, at about twenty).
  batched "VOS's actor gas per invocation as it is (\`MAX_EXECUTION_GAS\` $(sed -n 's/^pub const MAX_EXECUTION_GAS: u64 = \(.*\);/\1/p' "$V/vos/src/agent/execution.rs"))" stock
  if [ -n "${BATCH_GAS:-}" ]; then
    say "VOS with MAX_EXECUTION_GAS = $BATCH_GAS: core host and runtime rebuilt"
    sed -i -E "s/^(pub const MAX_EXECUTION_GAS: u64 = ).*;/\1$BATCH_GAS;/" "$V/vos/src/agent/execution.rs"
    grep -q "^pub const MAX_EXECUTION_GAS: u64 = $BATCH_GAS;" "$V/vos/src/agent/execution.rs"
    (cd "$A/agents" && cargo build --release -p core-host --config "$W/vos-patch.toml" $(composite_feature)) \
      > "$LOGS/build-core-host-gas.log" 2>&1 || { tail -n 60 "$LOGS/build-core-host-gas.log"; return 1; }
    (cd "$V/services/agent-runtime" &&
      CARGO_TARGET_DIR=$W/target/agent-runtime cargo actor --locked ${RUNTIME_FEATURES:+--features "$RUNTIME_FEATURES"}) \
      > "$LOGS/build-runtime-gas.log" 2>&1 || { tail -n 60 "$LOGS/build-runtime-gas.log"; return 1; }
    RT=$OUT/agent_runtime-gas.pvm
    "$H" runtime-pvm "$W/target/agent-runtime/riscv64em-vos/release/agent_runtime.elf" "$RT"
    done_
    batched "\`MAX_EXECUTION_GAS\` raised to $BATCH_GAS in the checkout (a VOS change)" gas
  fi
  for f in "$OUT"/core-*.txt; do
    md "<details><summary>$(basename "$f")</summary>" "" '```'
    cat "$f" >> "$SUMMARY"
    md '```' "</details>" ""
  done
}

# The core Agent's reservation (F-005, notes/MEASUREMENTS-F005.md §1), the way
# notes/MEASUREMENTS-F002.md §14 ran it under emulation, on real x86-64:
# - VOS: VOS_REF with VOS_PICKS cherry-picked (the host needs T-025-01's sequenced identities)
#   and both program limits at 4 096 KiB: the full core was 1.97 MB (F-005's own tree), and with
#   F-031's layout 3 linked it is 2.64 MB, over the 2 560 KiB this mode first raised them to;
# - the standard runtime built from that tree with its guest toolchain, linked by the host;
# - CipherClerk by tools/cipherclerk-patched.sh; the core's guest by `cargo actor`;
# - the host patched to take the engine from F025_BACKEND and, with F025_LOCKSTEP=1, to run
#   every execution on the other engine too and assert the same exit, counter, registers, gas
#   and output (core-host-backends.patch; the committed host has no engine choice).
reservation_timing() {
  local pick
  say "VOS tree for the core"
  # `none`: no picks (a dispatch cannot pass an empty input over its default).
  [ "${VOS_PICKS:-}" = none ] && VOS_PICKS=
  for pick in ${VOS_PICKS:-}; do
    retry git -C "$V" fetch -q --depth 2 origin "$pick"
    git -C "$V" -c user.name=x86-bench -c user.email=x86-bench@invalid cherry-pick "$pick"
  done
  # Older VOS admitted at most 1 792 or 2 560 KiB: raise both limits to 4 096 KiB there. A VOS that
  # already admits more (4 608 KiB on perf/agent-stack, for an opt-level 3 core) is left as it is.
  if grep -q -E '^pub const MAX_RUNTIME_PROGRAM_BYTES: usize = (1_280|1_792|2_560) \* 1024;' "$V/vos-agent-sdk/src/runtime.rs"; then
    sed -i -E 's/^(pub const MAX_RUNTIME_PROGRAM_BYTES: usize = ).*;/\14_096 * 1024;/' \
      "$V/vos-agent-sdk/src/runtime.rs"
    grep -q '^pub const MAX_RUNTIME_PROGRAM_BYTES: usize = 4_096 \* 1024;' "$V/vos-agent-sdk/src/runtime.rs"
    if grep -q -E '^pub const MAX_EXECUTION_PROGRAM_BYTES: usize = [0-9_]+ \* 1024;' "$V/vos/src/agent/execution.rs"; then
      sed -i -E 's/^(pub const MAX_EXECUTION_PROGRAM_BYTES: usize = ).*;/\14096 * 1024;/' \
        "$V/vos/src/agent/execution.rs"
    fi
  fi
  # Layout 3 is a feature of the guest (core-binding/mapped): only this mode links it.
  if [ "$MODE" = mapped-timing ]; then raise_limits; GUEST_FEATURES=mapped; fi
  # The batch curve (F-032 T-032-07) carries large batches: every bound raised, as above.
  if [ "$MODE" = batch-curve ]; then raise_limits; fi
  # The shard Agents (F-031 T-031-20) keep their books in their rows (layout 3 on a Local Agent):
  # every bound raised, as above.
  if [ "$MODE" = build-core ] && [ "${BUILD_SHARDS:-0}" = 1 ]; then raise_limits; fi
  # VOS before 2026-09-29 named the engine `RefineBackend` and the committed host had no engine
  # choice: patch it in. Later VOS selects it with `PvmBackend`, which the host reads from
  # F025_BACKEND itself.
  local OLD_ENGINE_API=no
  if grep -q 'pub enum RefineBackend' "$V/pvm/runtime/src/refine.rs"; then OLD_ENGINE_API=yes; fi
  git -C "$V" --no-pager diff --stat
  done_

  md "## x86-64 bench: reservation-timing${LABEL:+ ($LABEL)}" ""
  host_facts
  vos_facts $((1 + $(echo "${VOS_PICKS:-}" | wc -w)))
  md "Both program limits raised to 4 096 KiB in the checkout (2 560 KiB until F-031; the summary gives the core's size)." ""
  md "Runtime features: \`${RUNTIME_FEATURES:-none}\`; programs through historical_lookup: ${F025_PROGRAM_LOOKUP:-0}." ""

  say "CipherClerk (tools/cipherclerk-patched.sh)"
  retry "$A/tools/cipherclerk-patched.sh"
  done_

  cat > "$W/vos-patch.toml" <<EOF
[patch."https://codeberg.org/pandres95/VOS.git"]
vos = { path = "$V/vos" }
vos-pvm = { path = "$V/pvm/runtime" }
vos-pvm-compiler = { path = "$V/pvm/compiler" }
EOF
  # CipherClerk's guest patches depend on VOS's vos-protocol and precompile shim at the pinned
  # revision; the same paths keep one copy of each.
  if [ -d "$V/vos-protocol" ]; then
    echo "vos-protocol = { path = \"$V/vos-protocol\" }" >> "$W/vos-patch.toml"
    echo "vos-pvm-precompiles = { path = \"$V/pvm/proof/precompiles\" }" >> "$W/vos-patch.toml"
  fi

  say "core host"
  if [ "$OLD_ENGINE_API" = yes ]; then
    (cd "$A" && git apply --verbose .github/x86-bench/core-host-backends.patch)
  fi
  (cd "$A/agents" && cargo build --release -p core-host --config "$W/vos-patch.toml" $(composite_feature)) \
    > "$LOGS/build-core-host.log" 2>&1 || { tail -n 60 "$LOGS/build-core-host.log"; return 1; }
  local H=$CARGO_TARGET_DIR/release/aledgent-core-host
  (cd "$A/agents" && rustc --version) | tee -a "$LOGS/toolchains.txt"
  done_

  say "standard runtime from the tree"
  (cd "$V/services/agent-runtime" &&
    CARGO_TARGET_DIR=$W/target/agent-runtime cargo actor --locked ${RUNTIME_FEATURES:+--features "$RUNTIME_FEATURES"}) \
    > "$LOGS/build-runtime.log" 2>&1 || { tail -n 60 "$LOGS/build-runtime.log"; return 1; }
  local RT=$OUT/agent_runtime-limits.pvm
  "$H" runtime-pvm "$W/target/agent-runtime/riscv64em-vos/release/agent_runtime.elf" "$RT"
  done_

  # DEC-25: a host built with the opt-in `smt-v1` (CORE_HOST_FEATURES) builds its witnesses and
  # checks its replies in v1 roots, so the core's and the shard's guests are built with it too.
  if [[ ",${CORE_HOST_FEATURES:-}," == *,smt-v1,* ]]; then
    GUEST_FEATURES=${GUEST_FEATURES:+$GUEST_FEATURES,}smt-v1
    ACTOR_HEAP_FEATURE=${ACTOR_HEAP_FEATURE:+$ACTOR_HEAP_FEATURE,}smt-v1
  fi

  say "the core's guest (cargo actor)"
  # CORE_OPT_LEVEL: the guest's opt-level for this build only (F-032's gate: the 4.33 MB program
  # of opt-level 3 is above VOS's 4 MiB inner-preparation cache, MAX_CACHED_INNER_PROGRAM_BYTES).
  if [ -n "${CORE_OPT_LEVEL:-}" ]; then
    # TOML: a number bare (0-3), a letter level quoted ("s", "z").
    local lvl=$CORE_OPT_LEVEL
    case "$lvl" in [0-9]) ;; *) lvl="\"${lvl//\"/}\"" ;; esac
    sed -i -E "s/^opt-level = .*/opt-level = ${lvl}/" "$A/agents/core/Cargo.toml"
    grep -n '^opt-level' "$A/agents/core/Cargo.toml"
  fi
  (cd "$A/agents/core" &&
    CARGO_TARGET_DIR=$W/target/core cargo actor --config "$W/vos-patch.toml" ${GUEST_FEATURES:+--features "$GUEST_FEATURES"}) \
    > "$LOGS/build-core.log" 2>&1 || { tail -n 60 "$LOGS/build-core.log"; return 1; }
  local elf
  elf=$(find "$W/target/core/riscv64em-vos/release" -maxdepth 1 -name 'aledgent_core*.elf' | head -n 1)
  [ -n "$elf" ] || elf=$W/target/core/riscv64em-vos/release/aledgent_core
  local PKG=$OUT/core-full.vos
  local packaged
  packaged=$("$H" package "$elf" "$PKG")
  echo "$packaged"
  done_

  md "| Built | Bytes |" "|---|---:|"
  md "| core guest ELF | $(stat -c %s "$elf") |" "| core package (\`core-full.vos\`) | $(stat -c %s "$PKG") |"
  md "| runtime (\`agent_runtime-limits.pvm\`) | $(stat -c %s "$RT") |" "" "\`aledgent-core-host package\`: $packaged." ""
  md "Toolchains: host and guests \`$(cd "$A/agents" && rustc --version)\`." ""

  local rc
  if [ "$MODE" = mapped-timing ]; then
    mapped_compare
    return "$FAILED"
  fi
  if [ "$MODE" = batch-curve ]; then
    batch_curve "$H" "$RT" "$PKG"
    return "$FAILED"
  fi
  # The core Agent for another workflow (the stress suite, F-032): the host, the runtime and the
  # package, copied into $OUT/core, with no timing.
  if [ "$MODE" = build-core ]; then
    mkdir -p "$OUT/core"
    cp "$H" "$OUT/core/aledgent-core-host"
    cp "$RT" "$OUT/core/agent_runtime.pvm"
    cp "$PKG" "$OUT/core/core.vos"
    if [ "${BUILD_SHARDS:-0}" = 1 ]; then
      say "the shard Agents' guest (cargo actor)"
      (cd "$A/agents/shard" &&
        CARGO_TARGET_DIR=$W/target/shard cargo actor --config "$W/vos-patch.toml" ${ACTOR_HEAP_FEATURE:+--features "$ACTOR_HEAP_FEATURE"}) \
        > "$LOGS/build-shard.log" 2>&1 || { tail -n 60 "$LOGS/build-shard.log"; return 1; }
      local selfh
      selfh=$(find "$W/target/shard/riscv64em-vos/release" -maxdepth 1 -name 'aledgent_shard*.elf' | head -n 1)
      [ -n "$selfh" ] || selfh=$W/target/shard/riscv64em-vos/release/aledgent_shard
      "$H" package "$selfh" "$OUT/core/shard.vos"
      done_
    fi
    # The runtime profiles the shard Agents, or the core Agent of a stress run (F-032's gate on
    # the perf stack), are measured under, all in this one job (hosted runners differ in CPU from
    # job to job): SHARD_PROFILES is space-separated `label=features[@lookup]` (features
    # comma-separated, possibly none; `@lookup` for VOS's program-lookup profile, served by the
    # host with F025_PROGRAM_LOOKUP=1; `!aledgent` for aledgent's own runtime,
    # agents/aledgent-runtime, built against the same VOS: F-033). Each lands as rt-<label>.pvm,
    # listed in profiles.txt.
    if [ "${BUILD_SHARDS:-0}" = 1 ] || [ -n "${SHARD_PROFILES:-}" ]; then
      local spec plabel prest pfeat plookup pkind
      : > "$OUT/core/profiles.txt"
      for spec in ${SHARD_PROFILES:-standard=}; do
        plabel=${spec%%=*}
        prest=${spec#*=}
        pkind=standard
        case "$prest" in *!aledgent) pkind=aledgent; prest=${prest%!aledgent} ;; esac
        plookup=0
        case "$prest" in *@lookup) plookup=1; prest=${prest%@lookup} ;; esac
        pfeat=$prest
        say "runtime for profile $plabel ($pkind; features: ${pfeat:-none}, lookup $plookup)"
        build_runtime "$pkind" "$V" "$plabel" "$pfeat" "$H" "$OUT/core/rt-$plabel.pvm" || return 1
        echo "$plabel $plookup ${pfeat:-none}" >> "$OUT/core/profiles.txt"
        done_
      done
    fi
    ls -l "$OUT/core"
    return 0
  fi
  if [ "${COMPARE_ENGINES:-1}" != 1 ]; then
    batched_runs
    return "$FAILED"
  fi
  say "lockstep: every execution on both engines"
  rc=0
  F025_LOCKSTEP=1 F025_BACKEND=recompiler "$H" measure "$RT" "$PKG" "$OUT/core-lockstep.csv" \
    --n 100 --warmup 3 > "$OUT/core-lockstep.txt" 2> "$LOGS/core-lockstep.log" || rc=$?
  local same
  same=$(grep -c 'f025-lockstep: identical' "$LOGS/core-lockstep.log" || true)
  local diverged=no
  if grep -q 'the two engines diverge' "$LOGS/core-lockstep.log"; then diverged=yes; fi
  tail -n 5 "$LOGS/core-lockstep.log"
  done_
  md "### Lockstep" "" \
    "\`F025_LOCKSTEP=1 F025_BACKEND=recompiler aledgent-core-host measure ... --n 100 --warmup 3\`: exit $rc," \
    "**$same executions identical** on both engines (exit, counter, registers, gas, output bytes), divergence: **$diverged**." ""
  [ "$rc" -eq 0 ] && [ "$diverged" = no ] && [ "$same" -gt 0 ] || FAILED=1

  local round be
  for round in 1 2; do
    for be in interpreter recompiler; do
      say "$be, round $round"
      rc=0
      F025_BACKEND=$be "$H" measure "$RT" "$PKG" "$OUT/core-$be-$round.csv" --n 100 --warmup 3 \
        > "$OUT/core-$be-$round.txt" 2> "$LOGS/core-$be-$round.log" || rc=$?
      cat "$OUT/core-$be-$round.txt"
      [ "$rc" -eq 0 ] || FAILED=1
      done_
    done
  done

  md "### Timing (µs unless gas; nearest rank)" ""
  md "| Run | Samples | sync p50 | sync p95 | sync p99 | invoke p50 | invoke p99 | invoke gas p50 | actor gas p50 | ack p50 |" \
    "|---|---|---:|---:|---:|---:|---:|---:|---:|---:|"
  local f
  for f in "$OUT"/core-interpreter-1.txt "$OUT"/core-recompiler-1.txt \
    "$OUT"/core-interpreter-2.txt "$OUT"/core-recompiler-2.txt "$OUT"/core-lockstep.txt; do
    [ -s "$f" ] || continue
    awk -F, -v run="$(basename "$f" .txt)" '
      /^metric,p50/ { n = $0; sub(/^.*\(n = /, "", n); sub(/,.*$/, "", n) }
      { v[$1 ",50"] = $2; v[$1 ",95"] = $3; v[$1 ",99"] = $4 }
      END {
        printf "| %s | %s | %s | %s | %s | %s | %s | %s | %s | %s |\n", run, n,
          v["sync_us,50"], v["sync_us,95"], v["sync_us,99"], v["invoke_us,50"], v["invoke_us,99"],
          v["invoke_gas,50"], v["actor_gas,50"], v["ack_us,50"]
      }' "$f" >> "$SUMMARY"
  done
  md "" "The lockstep row includes the second engine's cold run in \`sync\`; it is not a timing." ""

  batched_runs
  return "$FAILED"
}

# One build of the core's guest for compare's GUESTS: CipherClerk e9a7b08 with the listed
# proposals/cipherclerk patches (comma-separated numbers), cipher-clerk features (comma-separated,
# or empty), an opt-level and the core's own features (comma-separated, or empty).
# build_guest <name> <patches> <features> <opt> [<core features>]; uses H, A, W, OUT.
# A guest built with the opt-in `smt-v1` (DEC-25) needs a host built with it too: the host
# builds its witnesses and checks its replies in the same root version. One such host is built
# for the job, and the guest's runs use it ($OUT/core-<name>.host names it).
build_guest() {
  local name=$1 patches=$2 features=$3 opt=$4 core_features=${5:-} n f dir=$W/cc-$1 manifest list=
  if [ ! -d "$W/cc-base" ]; then
    git init -q "$W/cc-base"
    git -C "$W/cc-base" remote add origin https://codeberg.org/Virto/CipherClerk.git
    retry git -C "$W/cc-base" fetch -q --depth 1 origin e9a7b0851f183c14dbd1635dbf0437c86682ad6f
    git -C "$W/cc-base" checkout -q FETCH_HEAD
  fi
  rm -rf "$dir"
  cp -R "$W/cc-base" "$dir"
  for n in ${patches//,/ }; do
    f=$(find "$A/proposals/cipherclerk" -maxdepth 1 -name "$(printf '%04d' "$((10#$n))")-*.patch" | head -n 1)
    git -C "$dir" apply "$f"
  done
  rm -rf "$A/.cipherclerk-patched"
  ln -s "$dir" "$A/.cipherclerk-patched"
  manifest=$A/agents/core/Cargo.toml
  cp "$manifest" "$W/core-Cargo.toml.orig"
  for f in ${features//,/ }; do list="$list${list:+, }\"$f\""; done
  if [ -n "$list" ]; then
    sed -i -E "s/^(cipher-clerk = .*default-features = false), features = \[[^]]*\] \}/\1, features = [$list] }/" "$manifest"
  else
    sed -i -E "s/^(cipher-clerk = .*default-features = false), features = \[[^]]*\] \}/\1 }/" "$manifest"
  fi
  grep '^cipher-clerk' "$manifest"
  if [[ ",$core_features," == *,smt-v1,* ]] && [ ! -x "$W/host-smt-v1/release/aledgent-core-host" ]; then
    local hf
    hf=$(composite_feature)
    hf=${hf:+${hf},smt-v1}
    (cd "$A/agents" && CARGO_TARGET_DIR=$W/host-smt-v1 cargo build --release -p core-host \
      --config "$W/vos-patch.toml" ${hf:---features smt-v1}) > "$LOGS/build-core-host-smt-v1.log" 2>&1 || {
      tail -n 40 "$LOGS/build-core-host-smt-v1.log"
      return 1
    }
  fi
  if [[ ",$core_features," == *,smt-v1,* ]]; then
    echo "$W/host-smt-v1/release/aledgent-core-host" > "$OUT/core-$name.host"
  fi
  (cd "$A/agents/core" && CARGO_PROFILE_RELEASE_OPT_LEVEL=$opt CARGO_TARGET_DIR=$W/target/core-$name \
    cargo actor --config "$W/vos-patch.toml" ${core_features:+--features "$core_features"}) \
    > "$LOGS/build-core-$name.log" 2>&1 || {
    tail -n 40 "$LOGS/build-core-$name.log"
    cp "$W/core-Cargo.toml.orig" "$manifest"
    return 1
  }
  cp "$W/core-Cargo.toml.orig" "$manifest"
  "$H" package "$W/target/core-$name/riscv64em-vos/release/aledgent_core.elf" "$OUT/core-$name.vos" |
    tee "$LOGS/package-$name.txt"
}

# Every VOS variant of one comparison on this one machine (GitHub-hosted runners differ in CPU
# from job to job, so only one job's runs compare in time). VARIANTS is space-separated
# `label=ref[+features][@lookup]`: the VOS commit of that variant's runtime, its runtime's cargo
# features, and `@lookup` for VOS's program-lookup profile. The core host and the core's guest are
# built once, from VOS_REF (which must have ExecutionLimits); every variant's runtime from its own
# ref. Each variant runs BATCHES at VOS's standard bounds (1 000 M of actor gas, F025_SKIP_LIMITS),
# then with COMPARE_GAS of actor gas: through the Agent's execution limits where the variant has
# them, otherwise with MAX_EXECUTION_GAS raised in its tree.
# The core host's `composite` feature (F-033), when the VOS under test has the composite engine.
composite_feature() {
  local f=${CORE_HOST_FEATURES:-}
  if grep -q '^runtime-composite' "$V/vos/Cargo.toml" 2>/dev/null; then f="$f composite"; fi
  # shellcheck disable=SC2086 # word splitting is the point: features as one comma list
  f=$(echo $f | tr ' ' ',')
  if [ -n "$f" ]; then echo "--features $f"; fi
}

# build_runtime <standard|aledgent> <vos checkout> <label> <features> <host> <out.pvm>: one runtime
# variant's program. `standard` is VOS's services/agent-runtime; `aledgent` is aledgent's own runtime
# (agents/aledgent-runtime, F-033), built against the same checkout. Both take the profile features
# (vos-native, blake2b-precompile, native-curve); aledgent's also direct-public-io.
build_runtime() {
  local kind=$1 vdir=$2 label=$3 features=$4 host=$5 out=$6 dir elf
  if [ "$kind" = aledgent ]; then
    {
      echo '[patch."https://codeberg.org/pandres95/VOS.git"]'
      echo "vos = { path = \"$vdir/vos\" }"
    } > "$W/vos-patch-rt-$label.toml"
    dir=$A/agents/aledgent-runtime
    if ! (cd "$dir" && CARGO_TARGET_DIR=$W/target/rt-$label \
      cargo actor --config "$W/vos-patch-rt-$label.toml" ${features:+--features "$features"}) \
      > "$LOGS/build-rt-$label.log" 2>&1; then
      tail -n 40 "$LOGS/build-rt-$label.log"
      return 1
    fi
    elf=$W/target/rt-$label/riscv64em-vos/release/aledgent_runtime.elf
  else
    if ! (cd "$vdir/services/agent-runtime" &&
      CARGO_TARGET_DIR=$W/target/rt-$label cargo actor --locked ${features:+--features "$features"}) \
      > "$LOGS/build-rt-$label.log" 2>&1; then
      tail -n 40 "$LOGS/build-rt-$label.log"
      return 1
    fi
    elf=$W/target/rt-$label/riscv64em-vos/release/agent_runtime.elf
  fi
  "$host" runtime-pvm "$elf" "$out"
}

# The conformance suite through the core Agent in VOS (F-005, F-033): every case of the current
# gate profile on `through-agent/vos`, against `aledgent-core-host agent-server` serving the core
# (built with conformance-ports) under each runtime of VARIANTS (compare's syntax, guests aside),
# on the recompiler. CONF_BATCH changes go to an invocation (CONFORMANCE_AGENT_BATCH); with a
# `~isolated` variant those batches are composites.
conformance() {
  local H PKG elf spec label rest ref features lookup runtime composite vdir RT port pid rc i=0
  md "## x86-64 bench: conformance through the Agent${LABEL:+ ($LABEL)}" ""
  host_facts
  vos_facts 1
  say "CipherClerk (tools/cipherclerk-patched.sh)"
  retry "$A/tools/cipherclerk-patched.sh"
  done_
  {
    echo '[patch."https://codeberg.org/pandres95/VOS.git"]'
    echo "vos = { path = \"$V/vos\" }"
    echo "vos-pvm = { path = \"$V/pvm/runtime\" }"
    echo "vos-pvm-compiler = { path = \"$V/pvm/compiler\" }"
    echo "vos-protocol = { path = \"$V/vos-protocol\" }"
    echo "vos-pvm-precompiles = { path = \"$V/pvm/proof/precompiles\" }"
  } > "$W/vos-patch.toml"
  say "core host, the core's conformance guest and the suite, from $VOS_REF"
  (cd "$A/agents" && cargo build --release -p core-host --config "$W/vos-patch.toml" $(composite_feature)) \
    > "$LOGS/build-core-host.log" 2>&1 || { tail -n 60 "$LOGS/build-core-host.log"; return 1; }
  H=$CARGO_TARGET_DIR/release/aledgent-core-host
  (cd "$A/agents/core" &&
    CARGO_TARGET_DIR=$W/target/core cargo actor --config "$W/vos-patch.toml" --features conformance-ports) \
    > "$LOGS/build-core.log" 2>&1 || { tail -n 60 "$LOGS/build-core.log"; return 1; }
  elf=$(find "$W/target/core/riscv64em-vos/release" -maxdepth 1 -name 'aledgent_core*.elf' | head -n 1)
  PKG=$OUT/core-conformance.vos
  "$H" package "$elf" "$PKG" | tee "$LOGS/package.txt"
  (cd "$A" && cargo test --release -p conformance --test suite --no-run) \
    > "$LOGS/build-suite.log" 2>&1 || { tail -n 60 "$LOGS/build-suite.log"; return 1; }
  done_
  md "Core package (conformance-ports): $(cat "$LOGS/package.txt"). Changes per invocation: ${CONF_BATCH:-1}." ""
  md "| Runtime | Composite | Batch | Result | Minutes |" "|---|---|---:|---|---:|"
  for spec in $VARIANTS; do
    label=${spec%%=*}
    rest=${spec#*=}
    composite=
    case "$rest" in *~*) composite=${rest##*~}; rest=${rest%~*} ;; esac
    runtime=standard
    case "$rest" in *!aledgent) runtime=aledgent; rest=${rest%!aledgent} ;; esac
    lookup=0
    case "$rest" in *@lookup) lookup=1; rest=${rest%@lookup} ;; esac
    ref=${rest%%+*}
    features=
    case "$rest" in *+*) features=${rest#*+} ;; esac
    vdir=$W/vos-$label
    say "variant $label: $ref ${features:+features $features} runtime $runtime ${composite:+composite $composite}"
    git init -q "$vdir"
    git -C "$vdir" remote add origin "$VOS_URL"
    retry git -C "$vdir" fetch -q --depth 1 origin "$ref"
    git -C "$vdir" checkout -q FETCH_HEAD
    RT=$OUT/rt-$label.pvm
    if ! build_runtime "$runtime" "$vdir" "$label" "$features" "$H" "$RT"; then
      FAILED=1
      done_
      continue
    fi
    i=$((i + 1))
    port=$((47800 + i))
    F025_BACKEND=recompiler F025_PROGRAM_LOOKUP=$lookup F033_COMPOSITE=$composite \
      "$H" agent-server "$RT" "$PKG" "127.0.0.1:$port" > "$LOGS/server-$label.log" 2>&1 &
    pid=$!
    sleep 3
    local started=$SECONDS
    rc=0
    (cd "$A" && CONFORMANCE_TARGETS=through-agent/vos CONFORMANCE_AGENT_HOST="tcp:127.0.0.1:$port" \
      CONFORMANCE_AGENT_BATCH=${CONF_BATCH:-1} CONFORMANCE_REPORT="$OUT/conformance-$label.json" \
      cargo test --release -p conformance --test suite) > "$LOGS/conformance-$label.log" 2>&1 || rc=$?
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    [ "$rc" -eq 0 ] || FAILED=1
    grep -E '^conformance: (profile|FAILED)' "$LOGS/conformance-$label.log" | tail -n 20
    done_
    md "| $label ($runtime) | ${composite:-no} | ${CONF_BATCH:-1} | exit $rc: $(grep -E '^conformance: profile' "$LOGS/conformance-$label.log" | sed 's/^conformance: //' | tr '|' '/') | $(((SECONDS - started) / 60)) |"
    grep -E '^conformance: FAILED' "$LOGS/conformance-$label.log" | head -n 20 | sed 's/^/    /' >> "$SUMMARY" || true
  done
  return "$FAILED"
}

compare() {
  local H RT PKG label rest ref features lookup vdir limits gas_rt bt rc set tag bounds elf spec guest vpkg runtime composite
  md "## x86-64 bench: compare${LABEL:+ ($LABEL)}" ""
  host_facts
  vos_facts 1
  say "CipherClerk (tools/cipherclerk-patched.sh)"
  retry "$A/tools/cipherclerk-patched.sh"
  done_
  {
    echo '[patch."https://codeberg.org/pandres95/VOS.git"]'
    echo "vos = { path = \"$V/vos\" }"
    echo "vos-pvm = { path = \"$V/pvm/runtime\" }"
    echo "vos-pvm-compiler = { path = \"$V/pvm/compiler\" }"
    echo "vos-protocol = { path = \"$V/vos-protocol\" }"
    echo "vos-pvm-precompiles = { path = \"$V/pvm/proof/precompiles\" }"
  } > "$W/vos-patch.toml"
  say "core host and the core's guest, from $VOS_REF"
  (cd "$A/agents" && cargo build --release -p core-host --config "$W/vos-patch.toml" $(composite_feature)) \
    > "$LOGS/build-core-host.log" 2>&1 || { tail -n 60 "$LOGS/build-core-host.log"; return 1; }
  H=$CARGO_TARGET_DIR/release/aledgent-core-host
  (cd "$A/agents/core" && CARGO_TARGET_DIR=$W/target/core cargo actor --config "$W/vos-patch.toml") \
    > "$LOGS/build-core.log" 2>&1 || { tail -n 60 "$LOGS/build-core.log"; return 1; }
  elf=$(find "$W/target/core/riscv64em-vos/release" -maxdepth 1 -name 'aledgent_core*.elf' | head -n 1)
  PKG=$OUT/core.vos
  "$H" package "$elf" "$PKG" | tee "$LOGS/package.txt"
  done_
  md "Core package: $(cat "$LOGS/package.txt")." ""
  # GUESTS: space-separated name=patches:features:opt, guest builds a variant may name (`/name`).
  local g gname gspec
  for g in ${GUESTS:-}; do
    gname=${g%%=*}
    gspec=${g#*=}
    say "guest $gname: $gspec"
    IFS=: read -r gp gf go gc <<<"$gspec"
    build_guest "$gname" "$gp" "$gf" "${go:-z}" "${gc:-}" || FAILED=1
    done_
    md "Guest \`$gname\` ($gspec): $(cat "$LOGS/package-$gname.txt" 2>/dev/null)." ""
  done
  md "| Variant | Bounds | Batch | Rounds | Invocations | Round p50 µs | Invoke gas p50 | Actor gas p50 | Runtime gas p50 | Inputs/s at p50 | Inputs/s over the run |" \
    "|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|"
  for spec in $VARIANTS; do
    label=${spec%%=*}
    rest=${spec#*=}
    guest=
    case "$rest" in */*) guest=${rest##*/}; rest=${rest%/*} ;; esac
    # `~isolated` or `~atomic` (F-033): batches reach the Agent as composites of aledgent's runtime.
    composite=
    case "$rest" in *~*) composite=${rest##*~}; rest=${rest%~*} ;; esac
    # `!aledgent` (F-033): the runtime is aledgent's own (agents/aledgent-runtime), against this VOS.
    runtime=standard
    case "$rest" in *!aledgent) runtime=aledgent; rest=${rest%!aledgent} ;; esac
    lookup=0
    case "$rest" in *@lookup) lookup=1; rest=${rest%@lookup} ;; esac
    ref=${rest%%+*}
    features=
    case "$rest" in *+*) features=${rest#*+} ;; esac
    vdir=$W/vos-$label
    say "variant $label: $ref ${features:+features $features} lookup $lookup runtime $runtime ${composite:+composite $composite} ${guest:+guest $guest}"
    if [ -d "$vdir" ] && [ -f "$OUT/rt-$label.pvm" ]; then
      # The same runtime variant with another guest: reuse its runtimes.
      RT=$OUT/rt-$label.pvm
      gas_rt=$RT
      [ -f "$OUT/rt-$label-gas.pvm" ] && gas_rt=$OUT/rt-$label-gas.pvm
      limits=no
      if grep -q 'SetExecutionLimits' "$vdir/vos-agent-sdk/src/runtime.rs"; then limits=yes; fi
      done_
    else
    git init -q "$vdir"
    git -C "$vdir" remote add origin "$VOS_URL"
    retry git -C "$vdir" fetch -q --depth 1 origin "$ref"
    git -C "$vdir" checkout -q FETCH_HEAD
    limits=no
    if grep -q 'SetExecutionLimits' "$vdir/vos-agent-sdk/src/runtime.rs"; then limits=yes; fi
    RT=$OUT/rt-$label.pvm
    if ! build_runtime "$runtime" "$vdir" "$label" "$features" "$H" "$RT"; then
      FAILED=1
      done_
      continue
    fi
    gas_rt=$RT
    if [ "$limits" = no ] && [ -n "${COMPARE_GAS:-}" ]; then
      sed -i -E "s/^(pub const MAX_EXECUTION_GAS: u64 = ).*;/\1$COMPARE_GAS;/" "$vdir/vos/src/agent/execution.rs"
      grep -q "^pub const MAX_EXECUTION_GAS: u64 = $COMPARE_GAS;" "$vdir/vos/src/agent/execution.rs"
      if ! (cd "$vdir/services/agent-runtime" &&
        CARGO_TARGET_DIR=$W/target/rt-$label cargo actor --locked ${features:+--features "$features"}) \
        > "$LOGS/build-rt-$label-gas.log" 2>&1; then
        tail -n 40 "$LOGS/build-rt-$label-gas.log"
        FAILED=1
        done_
        continue
      fi
      gas_rt=$OUT/rt-$label-gas.pvm
      "$H" runtime-pvm "$W/target/rt-$label/riscv64em-vos/release/agent_runtime.elf" "$gas_rt"
    fi
    done_
    fi
    vpkg=$PKG
    local VH=$H
    if [ -n "$guest" ]; then
      vpkg=$OUT/core-$guest.vos
      [ -f "$OUT/core-$guest.host" ] && VH=$(cat "$OUT/core-$guest.host")
      label=$label/$guest
    fi
    for set in ${COMPARE_SETS:-standard ${COMPARE_GAS:+raised}}; do
      for bt in $BATCHES; do
        tag=${label//\//-}-$set-$bt
        say "$label, $set bounds, batch $bt"
        rc=0
        if [ "$set" = standard ]; then
          bounds="standard (1 000 M)"
          F025_SKIP_LIMITS=1 F025_ACTOR_GAS=1000000000 F025_PROGRAM_LOOKUP=$lookup F025_BACKEND=recompiler \
            F033_COMPOSITE=$composite "$VH" measure "$RT" "$vpkg" "$OUT/$tag.csv" --n "$BATCH_N" --warmup 3 --batch "$bt" \
            > "$OUT/$tag.txt" 2> "$LOGS/$tag.log" || rc=$?
        elif [ "$limits" = yes ]; then
          bounds="Agent limits, $COMPARE_GAS gas"
          F025_ACTOR_GAS=$COMPARE_GAS F025_PROGRAM_LOOKUP=$lookup F025_BACKEND=recompiler \
            F033_COMPOSITE=$composite "$VH" measure "$RT" "$vpkg" "$OUT/$tag.csv" --n "$BATCH_N" --warmup 3 --batch "$bt" \
            > "$OUT/$tag.txt" 2> "$LOGS/$tag.log" || rc=$?
        else
          bounds="MAX_EXECUTION_GAS = $COMPARE_GAS"
          F025_SKIP_LIMITS=1 F025_ACTOR_GAS=$COMPARE_GAS F025_PROGRAM_LOOKUP=$lookup F025_BACKEND=recompiler \
            F033_COMPOSITE=$composite "$VH" measure "$gas_rt" "$vpkg" "$OUT/$tag.csv" --n "$BATCH_N" --warmup 3 --batch "$bt" \
            > "$OUT/$tag.txt" 2> "$LOGS/$tag.log" || rc=$?
        fi
        tail -n 3 "$LOGS/$tag.log"
        done_
        awk -F, -v v="$label" -v b="$bounds" -v k="$bt" -v rc="$rc" '
          /^batch: [0-9]/ {
            sub(/^batch: /, "")
            found = 1
            printf "| %s | %s | %s | %s | %s | %s | %s | %s | %s | %.2f | %.2f |\n", v, b, $1, $2, $4, $5, $7, $8, $7 - $8, $9 / 1000, $10 / 1000
          }
          END { if (!found) printf "| %s | %s | %s | exit %s | | | | | | | |\n", v, b, k, rc }' "$OUT/$tag.txt" >> "$SUMMARY"
      done
    done
  done
  md "" "Runtime gas is the invocation's gas less the actor's: the runtime's own work per invocation." ""
  return "$FAILED"
}

# One invocation of the core Agent against the commands it carries (F-032, T-032-07): sizes
# BATCH_SIZES, each a fresh book in its own process, on the recompiler, the bounds raised. A size
# that fails is recorded with its refusal; the others run. batch_curve <host> <runtime> <package>.
batch_curve() {
  local H=$1 RT=$2 PKG=$3 k rc
  : > "$OUT/batch.txt"
  for k in ${BATCH_SIZES:-1 8 32 128 512 2048}; do
    say "batch of $k"
    rc=0
    F025_BACKEND=recompiler timeout 2400 "$H" batch-curve "$RT" "$PKG" "$OUT/batch-$k.csv" \
      --sizes "$k" --rounds "${BATCH_ROUNDS:-5}" --accounts "${BATCH_ACCOUNTS:-64}" \
      > "$OUT/batch-$k.txt" 2> "$LOGS/batch-$k.log" || rc=$?
    cat "$OUT/batch-$k.txt"
    tail -n 5 "$LOGS/batch-$k.log"
    grep '^batch,[0-9]' "$OUT/batch-$k.txt" >> "$OUT/batch.txt" ||
      echo "batch,$k,0,,,,,,,,,,,,,,exit $rc: $(tail -n 1 "$LOGS/batch-$k.log" | tr ',' ';')" >> "$OUT/batch.txt"
    done_
  done
  md "### Commands per invocation (recompiler; medians of the recorded rounds)" ""
  md "| Size | Rounds | Invocation ms p50 | max | µs per command | Commands/s | Invoke gas M | Actor gas M | Ack gas M | Invoke gas per command M | Actor gas per command M | Native µs per command | Message B | Blobs B | Input B | Wall |" \
    "|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|"
  awk -F, '/^batch,[0-9]/ { printf "| %s |", $2; for (i = 3; i <= 17; i++) printf " %s |", $i; printf "\n" }' "$OUT/batch.txt" >> "$SUMMARY"
  md ""
}

# F-031 lever 2: the same reservations on a layout-2 Agent (witnesses) and a layout-3 Agent (the
# book in its committed maps), on the recompiler, the bounds raised for both. Uses H, RT, PKG.
mapped_compare() {
  local layout args rc
  for layout in 2 3; do
    args=()
    [ "$layout" = 3 ] && args=(--mapped)
    say "layout $layout, recompiler"
    rc=0
    F025_BACKEND=recompiler "$H" measure "$RT" "$PKG" "$OUT/core-layout$layout.csv" --n 100 --warmup 3 "${args[@]}" \
      > "$OUT/core-layout$layout.txt" 2> "$LOGS/core-layout$layout.log" || rc=$?
    cat "$OUT/core-layout$layout.txt"
    tail -n 5 "$LOGS/core-layout$layout.log"
    [ "$rc" -eq 0 ] || FAILED=1
    done_
  done
  md "### Layout 2 against layout 3 (F-031 lever 2), recompiler, bounds raised" ""
  md "| Layout | Samples | sync p50 | sync p99 | invoke p50 | invoke gas p50 | actor gas p50 | ack p50 |" \
    "|---|---|---:|---:|---:|---:|---:|---:|"
  local f
  for f in "$OUT"/core-layout2.txt "$OUT"/core-layout3.txt; do
    [ -s "$f" ] || continue
    awk -F, -v run="$(basename "$f" .txt)" '
      /^metric,p50/ { n = $0; sub(/^.*\(n = /, "", n); sub(/,.*$/, "", n) }
      { v[$1 ",50"] = $2; v[$1 ",99"] = $4 }
      END {
        printf "| %s | %s | %s | %s | %s | %s | %s | %s |\n", run, n,
          v["sync_us,50"], v["sync_us,99"], v["invoke_us,50"], v["invoke_gas,50"], v["actor_gas,50"], v["ack_us,50"]
      }' "$f" >> "$SUMMARY"
  done
  for f in "$OUT"/core-layout*.txt; do
    md "<details><summary>$(basename "$f")</summary>" "" '```'
    cat "$f" >> "$SUMMARY"
    md '```' "</details>" ""
  done
}

# The chainspec genesis on the recompiler (F-036, owner ruling genesis-chainspec). Everything is a
# test value: the keys are drawn here, the signer reads them from files (development), and nothing
# leaves the job but the timings.
hex_of() { od -An -tx1 -N"$1" /dev/urandom | tr -d ' \n'; }
wait_for() { # wait_for <log> <pattern> <secs>: 0 when the pattern appears
  local i
  for i in $(seq 1 "$3"); do
    grep -q "$2" "$1" 2>/dev/null && return 0
    sleep 1
  done
  return 1
}
chainspec_genesis() {
  local H M S RT PKG elf G=${GROWN:-1} dir=$W/spec gop=c5ec00000000000000000000000000f1 at=1800000000000
  md "## x86-64 bench: chainspec genesis${LABEL:+ ($LABEL)} (F-036, x$G)" ""
  host_facts
  vos_facts 1
  say "CipherClerk (tools/cipherclerk-patched.sh)"
  retry "$A/tools/cipherclerk-patched.sh"
  done_
  {
    echo '[patch."https://codeberg.org/pandres95/VOS.git"]'
    echo "vos = { path = \"$V/vos\" }"
    echo "vos-pvm = { path = \"$V/pvm/runtime\" }"
    echo "vos-pvm-compiler = { path = \"$V/pvm/compiler\" }"
    echo "vos-protocol = { path = \"$V/vos-protocol\" }"
    echo "vos-pvm-precompiles = { path = \"$V/pvm/proof/precompiles\" }"
  } > "$W/vos-patch.toml"
  say "core host, core guest, aledgent's runtime (JAM profile), migrate and the signer"
  (cd "$A/agents" && cargo build --release -p core-host --config "$W/vos-patch.toml") \
    > "$LOGS/build-core-host.log" 2>&1 || { tail -n 60 "$LOGS/build-core-host.log"; return 1; }
  H=$CARGO_TARGET_DIR/release/aledgent-core-host
  (cd "$A/agents/core" && CARGO_TARGET_DIR=$W/target/core cargo actor --config "$W/vos-patch.toml") \
    > "$LOGS/build-core.log" 2>&1 || { tail -n 60 "$LOGS/build-core.log"; return 1; }
  elf=$(find "$W/target/core/riscv64em-vos/release" -maxdepth 1 -name 'aledgent_core*.elf' | head -n 1)
  PKG=$OUT/core.vos
  "$H" package "$elf" "$PKG" | tee "$LOGS/package.txt"
  # aledgent's runtime against the VOS its own Cargo.toml pins (the composite engine's fork branch),
  # no features: the JAM-compatible profile, checked to call Gray Paper host calls only.
  RT=$OUT/aledgent_runtime.pvm
  (cd "$A/agents/aledgent-runtime" && CARGO_TARGET_DIR=$W/target/rt-jam cargo actor) \
    > "$LOGS/build-rt.log" 2>&1 || { tail -n 60 "$LOGS/build-rt.log"; return 1; }
  ALEDGENT_RUNTIME_GP_ONLY=1 "$H" runtime-pvm "$W/target/rt-jam/riscv64em-vos/release/aledgent_runtime.elf" "$RT" \
    | tee -a "$LOGS/package.txt"
  (cd "$A" && cargo build --release -p aledgent-migrate -p signer) > "$LOGS/build-tools.log" 2>&1 \
    || { tail -n 60 "$LOGS/build-tools.log"; return 1; }
  M=$CARGO_TARGET_DIR/release/migrate
  S=$CARGO_TARGET_DIR/release/aledgent-signer
  done_

  # Amendment 0023 (PR #201): the genesis code's manifest, of these three artefacts, with the
  # profiles they were built with. The chainspec's header names it, and the core host refuses to
  # build or found the genesis under other binaries.
  say "the genesis code's manifest"
  local guest_tc guest_opt host_tc kernel rev
  guest_tc=$(sed -n 's/^channel = "\(.*\)"/\1/p' "$A/agents/core/rust-toolchain.toml")
  guest_opt=$(sed -n 's/^opt-level = "*\([^"]*\)"*/\1/p' "$A/agents/core/Cargo.toml" | head -n 1)
  host_tc=$(cd "$A/agents" && rustc --version)
  kernel=$(grep -A2 '^name = "cipher-clerk"' "$A/Cargo.lock" | sed -n 's/.*#\([0-9a-f]*\)".*/\1/p' | head -n 1)
  rev=${GITHUB_SHA:-$(git -C "$A" rev-parse HEAD 2>/dev/null || echo unknown)}
  mkdir -p "$dir"
  cat > "$dir/code-profile.json" <<JSON
{ "kernel": { "revision": "${kernel:-unknown} with proposals/cipherclerk/0001 (tools/cipherclerk-patched.sh)" },
  "package": { "target": "riscv64em-vos", "toolchain": "$guest_tc", "opt_level": "$guest_opt",
               "features": [], "source_revision": "$rev", "cargo_lock": "$A/agents/core/Cargo.lock" },
  "runtime": { "target": "riscv64em-vos", "toolchain": "$(sed -n 's/^channel = "\(.*\)"/\1/p' "$A/agents/aledgent-runtime/rust-toolchain.toml")",
               "opt_level": "3", "features": [], "source_revision": "$rev",
               "cargo_lock": "$A/agents/aledgent-runtime/Cargo.lock" },
  "native_decide": { "target": "x86_64-unknown-linux-gnu", "toolchain": "$host_tc", "opt_level": "3",
                     "features": [$(composite_feature | tr -d ' ' | sed 's/--features//; s/\([^,]\{1,\}\)/"\1"/g')],
                     "source_revision": "$rev", "cargo_lock": "$A/agents/Cargo.lock" } }
JSON
  "$H" code-manifest --actor "$PKG" --runtime "$RT" --profile "$dir/code-profile.json" --out "$dir/code.bin" \
    > "$OUT/code-manifest.json" 2> "$LOGS/code-manifest.log" || { cat "$LOGS/code-manifest.log"; return 1; }
  done_

  say "the chainspec, the signer and the deployment file"
  mkdir -p "$dir/keys"
  hex_of 32 > "$dir/keys/root.hex"
  { hex_of 31; echo 0a; } | tr -d '\n' > "$dir/keys/clerk.hex"
  hex_of 32 > "$dir/keys/authority.hex"
  hex_of 32 > "$dir/keys/package.hex"
  hex_of 24 > "$dir/keys/core.token"
  hex_of 24 > "$dir/keys/link.token"
  local t0 digest chunks uid
  t0=$(date +%s%N)
  "$M" chainspec build --genesis-op "$gop" --at "$at" --identities $((100000 * G)) \
    --accounts $((100000 * G)) --system-accounts $((200 * G)) --openings $((1000 * G)) \
    --operator-grant-window-limit 100000000000 --code "$dir/code.bin" --out "$dir/full" > "$LOGS/chainspec-build.log" 2>&1
  local build_ms=$((($(date +%s%N) - t0) / 1000000))
  # `inspect`'s one line: the chainspec's own digest (chainspec.json also lists the code's artefacts).
  digest=$("$M" chainspec inspect "$dir/full/chainspec.bin" | grep -o '"digest":"[0-9a-f]*"' | cut -d'"' -f4)
  chunks=$("$M" chainspec inspect "$dir/full/chainspec.bin" | grep -o '"chunks":[0-9]*' | grep -o '[0-9]*$')
  uid=$(id -u)
  cat > "$dir/signer.json" <<JSON
{ "listen": "$dir/signer.sock", "development": true,
  "clients": [ { "name": "core", "uids": [$uid], "token_sha256": ["$("$S" token-sha256 "$dir/keys/core.token")"],
                 "keys": ["keyring", "clerk", "agent-authority", "agent-package"] } ],
  "keys": [
    { "id": "keyring", "scheme": "key_ring", "source": { "file": { "path": "$dir/keys/root.hex" } } },
    { "id": "clerk", "scheme": "clerk", "source": { "file": { "path": "$dir/keys/clerk.hex" } } },
    { "id": "agent-authority", "scheme": "ed25519", "source": { "file": { "path": "$dir/keys/authority.hex" } } },
    { "id": "agent-package", "scheme": "ed25519", "source": { "file": { "path": "$dir/keys/package.hex" } } } ] }
JSON
  deployment() { # deployment <genesis op> <digest> <chunks> <out>
    cat > "$4" <<JSON
{ "profile": "development", "listen": "127.0.0.1:18080", "replica": "bench-0", "require_sync_standby": false, "tokens": [],
  "tick_interval_ms": 1000, "decision_bound_ms": 60000, "core_journal": "$dir/unused.journal",
  "genesis": {
    "assets": [ { "id": "COPM", "precision": 2, "kind": "stable" }, { "id": "TSTA", "precision": 2, "kind": "stable" },
                { "id": "TSTB", "precision": 2, "kind": "stable" }, { "id": "TSTC", "precision": 6, "kind": "stable" } ],
    "operator_verifier": "0f", "deployment": "5eed0000000000000000000000000001",
    "decision_bound_ms": 60000, "review_service_level_ms": 3600000, "genesis_op": "$1", "genesis_at": $at,
    "opening_sources": { "COPM": ["73796e7468657469632d766f6c756d65"] },
    "chainspec": { "digest": "$2", "chunks": $3 } },
  "core_endpoint": "unix:$dir/core.sock",
  "core_link": { "token_sha256": ["$("$S" token-sha256 "$dir/keys/link.token")"], "peer_uids": [$uid] },
  "core_keys": { "signer": { "socket": "$dir/signer.sock", "client": "core", "token_file": "$dir/keys/core.token" },
                 "key_ring": "keyring", "clerk": "clerk", "agent_authority": "agent-authority", "agent_package": "agent-package" } }
JSON
  }
  deployment "$gop" "$digest" "$chunks" "$dir/full.json"
  "$S" serve "$dir/signer.json" > "$LOGS/signer.log" 2>&1 &
  local signer=$!
  wait_for "$LOGS/signer.log" serving 60 || { tail -n 20 "$LOGS/signer.log"; return 1; }
  done_

  say "build-spec, native"
  t0=$(date +%s%N)
  "$H" build-spec --config "$dir/full.json" --chainspec "$dir/full/chainspec.bin" --actor "$PKG" --runtime "$RT" --out "$dir/built" \
    > "$OUT/genesis.json" 2> "$LOGS/build-spec.log" || { tail -n 20 "$LOGS/build-spec.log"; return 1; }
  local spec_ms=$((($(date +%s%N) - t0) / 1000000))
  # The refusal: a runtime that is not the genesis code's is refused before anything is built.
  local refused=no
  printf 'x' | cat "$RT" - > "$dir/other.pvm"
  if ! "$H" build-spec --config "$dir/full.json" --chainspec "$dir/full/chainspec.bin" --actor "$PKG" \
      --runtime "$dir/other.pvm" --out "$dir/refused" > /dev/null 2> "$LOGS/build-spec-refused.log"; then
    grep -q 'not the chainspec.s genesis code' "$LOGS/build-spec-refused.log" && refused=yes
  fi
  local store
  store=$(du -sb "$dir/built/replica" | cut -f1)
  done_

  say "serve: boot to a sealed genesis, and a restart"
  local boot=() k pid line
  for k in 1 2; do
    t0=$(date +%s%N)
    F025_BACKEND=recompiler "$H" serve --config "$dir/full.json" --runtime "$RT" --actor "$PKG" \
      --state "$dir/state" --chainspec "$dir/full/chainspec.bin" --genesis-manifest "$OUT/genesis.json" \
      > "$LOGS/serve-$k.log" 2>&1 &
    pid=$!
    wait_for "$LOGS/serve-$k.log" 'serving on' 7200 || { tail -n 30 "$LOGS/serve-$k.log"; kill "$pid"; return 1; }
    boot+=($((($(date +%s%N) - t0) / 1000000)))
    line=$(grep -o 'verified in [0-9]* ms' "$LOGS/serve-$k.log" | grep -o '[0-9]*')
    boot+=("$line")
    kill "$pid"; wait "$pid" 2>/dev/null || true
  done
  done_

  local vconf vspec vman
  if [ "${VERIFY_FULL:-0}" = 1 ]; then
    say "verify-genesis, on the x$G chainspec itself"
    vconf="$dir/full.json" vspec="$dir/full/chainspec.bin" vman="$OUT/genesis.json"
  else
    say "verify-genesis, on a chainspec of ${VERIFY_IDENTITIES:-2000} identities"
    local vi=${VERIFY_IDENTITIES:-2000} vgop=c5ec00000000000000000000000000f2 vdigest vchunks
    "$M" chainspec build --genesis-op "$vgop" --at "$at" --identities "$vi" --accounts $((vi + vi / 50)) \
      --system-accounts $((vi / 500 + 1)) --openings $((vi / 100 + 1)) --operator-grant-window-limit 100000000000 \
      --code "$dir/code.bin" --out "$dir/small" > "$LOGS/chainspec-small.log" 2>&1
    # `inspect`'s one line: the chainspec's own digest (chainspec.json also lists the code's artefacts).
    vdigest=$("$M" chainspec inspect "$dir/small/chainspec.bin" | grep -o '"digest":"[0-9a-f]*"' | cut -d'"' -f4)
    vchunks=$("$M" chainspec inspect "$dir/small/chainspec.bin" | grep -o '"chunks":[0-9]*' | grep -o '[0-9]*$')
    deployment "$vgop" "$vdigest" "$vchunks" "$dir/small.json"
    "$H" build-spec --config "$dir/small.json" --chainspec "$dir/small/chainspec.bin" --actor "$PKG" --runtime "$RT" --out "$dir/small-built" \
      > "$OUT/genesis-small.json" 2> "$LOGS/build-spec-small.log"
    vconf="$dir/small.json" vspec="$dir/small/chainspec.bin" vman="$OUT/genesis-small.json"
  fi
  # verify-genesis on the engine it picks by itself (the recompiler here), then once per
  # VERIFY_SEGMENTS count (default "2 4"), then on the interpreter unless VERIFY_INTERPRETER=0:
  # the before and after of the engine default and the segments' speed-up, on one runner.
  t0=$(date +%s%N)
  local vrc=0
  "$H" verify-genesis --config "$vconf" --chainspec "$vspec" \
    --manifest "$vman" --runtime "$RT" --actor "$PKG" --dir "$dir" \
    > "$OUT/verify-genesis.json" 2> "$LOGS/verify-genesis.log" || vrc=$?
  local verify_ms=$((($(date +%s%N) - t0) / 1000000))
  local vrows=() k krc kms
  for k in ${VERIFY_SEGMENTS:-2 4}; do
    t0=$(date +%s%N); krc=0
    "$H" verify-genesis --config "$vconf" --chainspec "$vspec" --manifest "$vman" --runtime "$RT" \
      --actor "$PKG" --dir "$dir" --segments "$k" \
      > "$OUT/verify-genesis-k$k.json" 2> "$LOGS/verify-genesis-k$k.log" || krc=$?
    kms=$((($(date +%s%N) - t0) / 1000000))
    vrows+=("| \`verify-genesis --segments $k\` (recompiler, exit $krc) | $kms ms |")
  done
  if [ "${VERIFY_INTERPRETER:-1}" = 1 ]; then
    t0=$(date +%s%N); krc=0
    "$H" verify-genesis --config "$vconf" --chainspec "$vspec" --manifest "$vman" --runtime "$RT" \
      --actor "$PKG" --dir "$dir" --engine interpreter \
      > "$OUT/verify-genesis-interpreter.json" 2> "$LOGS/verify-genesis-interpreter.log" || krc=$?
    kms=$((($(date +%s%N) - t0) / 1000000))
    vrows+=("| \`verify-genesis --engine interpreter\` (the core host's default before, exit $krc) | $kms ms |")
  fi
  kill "$signer" 2>/dev/null || true
  done_

  local records vrecords
  records=$(grep -o '"records": [0-9]*' "$OUT/genesis.json" | grep -o '[0-9]*$')
  vrecords=$(grep -o '"records": *[0-9]*' "$OUT/verify-genesis.json" | grep -o '[0-9]*$' || echo '?')
  md "| Step | Records | Time |" "|---|---:|---:|"
  md "| \`migrate chainspec build\` (x$G synthetic) | $records | ${build_ms} ms |"
  md "| \`build-spec\` (native; leaf store $store B) | $records | ${spec_ms} ms |"
  md "| \`serve\`, first boot to sealed (genesis installed and verified) | $records | ${boot[0]} ms (${boot[1]} ms) |"
  md "| \`serve\`, restart (genesis rebuilt and checked) | $records | ${boot[2]} ms (${boot[3]} ms) |"
  md "| \`verify-genesis\` (chunk by chunk in the PVM, its default engine, exit $vrc) | $vrecords | ${verify_ms} ms |" ""
  md "verify-genesis on $(nproc) vCPU, $vrecords records:" "" "| Run | Time |" "|---|---:|" "${vrows[@]}" ""
  md "Another runtime than the genesis code's refused by \`build-spec\`: **$refused**." ""
  md "Genesis code (amendment 0023's manifest, \`aledgent-core-host code-manifest\`):" '```json'
  cat "$OUT/code-manifest.json" >> "$SUMMARY"
  md '```' ""
  md "Core package: $(cat "$LOGS/package.txt"). Manifest:" '```json'
  cat "$OUT/genesis.json" >> "$SUMMARY"
  md '```'
  return "$vrc"
}

case "$MODE" in
  vos-suites | reservation-timing | mapped-timing | batch-curve | build-core | compare | conformance | chainspec-genesis) ;;
  *)
    echo "unknown mode: $MODE" >&2
    exit 2
    ;;
esac
setup
# A subshell, not `mode || rc=$?`: `set -e` is ignored inside a function called from `||`, and a
# failed build step must stop the mode. Suites and runs record their own failures and go on.
set +e
(
  set -e
  if [ "$MODE" = vos-suites ]; then vos_suites; elif [ "$MODE" = compare ]; then compare
  elif [ "$MODE" = conformance ]; then conformance
  elif [ "$MODE" = chainspec-genesis ]; then chainspec_genesis; else reservation_timing; fi
)
rc=$?
md "Result: exit $rc."
exit "$rc"
