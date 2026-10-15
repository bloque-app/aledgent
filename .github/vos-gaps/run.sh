#!/usr/bin/env bash
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
# The tests of one VOS checkout (VOS_URL @ VOS_REF) for .github/workflows/vos-gaps.yml.
#
# Runs on a GitHub-hosted Linux runner (VOS's multi-node code is Linux-only), with everything
# under /mnt/w:
#
#   /mnt/w/vos      the VOS checkout under test
#   /mnt/w/target   every cargo build
#   /mnt/w/out      what the job uploads: summary.md and logs/
#
# Two passes: every non-ignored test of `vos --lib` and `vosx` (regressions against upstream),
# then the gap branch's own tests, ignored ones included, one at a time. KEY (gap1 ... gap5, or
# base for upstream `saga/agents`) selects the second pass. Exit 1 if any suite failed.
# shellcheck disable=SC2016
set -euo pipefail

W=/mnt/w
V=$W/vos
OUT=$W/out
LOGS=$OUT/logs
SUMMARY=$OUT/summary.md
mkdir -p "$LOGS" "$W/tmp"

export CARGO_TARGET_DIR=$W/target
export TMPDIR=$W/tmp
export CARGO_INCREMENTAL=0
export CARGO_TERM_COLOR=never
export CARGO_TERM_PROGRESS_WHEN=never
export CARGO_NET_RETRY=10
export CARGO_NET_GIT_FETCH_WITH_CLI=true
# Keep debug assertions (test profile) but make the physical multi-node tests
# fast enough for a 4-core runner. Per-package opt-level overrides still apply.
export CARGO_PROFILE_DEV_OPT_LEVEL=1
export CARGO_PROFILE_DEV_DEBUG=line-tables-only
export CARGO_PROFILE_TEST_DEBUG=line-tables-only
export RUST_MIN_STACK=16777216
export RUST_BACKTRACE=1

FEATURES='agent-runtime storage network http-ingress'

md() { printf '%s\n' "$@" >> "$SUMMARY"; }
retry() {
  local n=1 delay=10
  until "$@"; do
    [ "$n" -ge 5 ] && { echo "failed after $n attempts: $*" >&2; return 1; }
    sleep "$delay"; n=$((n + 1)); delay=$((delay * 2))
  done
}

echo "::group::checkout $VOS_URL $VOS_REF"
rm -rf "$V"
git init -q "$V"
git -C "$V" remote add origin "$VOS_URL"
retry git -C "$V" fetch -q --depth 1 origin "$VOS_REF"
git -C "$V" checkout -q FETCH_HEAD
SHA=$(git -C "$V" rev-parse HEAD)
echo "::endgroup::"
md "## $KEY: \`$VOS_REF\` @ \`${SHA:0:12}\`" "" "\`$VOS_URL\` · $(git -C "$V" log -1 --format='%s')" ""
md "Runner: $(nproc) cpus, $(grep MemTotal /proc/meminfo | awk '{print int($2/1048576)" GiB"}') RAM." ""

echo "::group::toolchains"
for f in "$V/rust-toolchain.toml" "$V/actors/system-authority/rust-toolchain.toml" \
  "$V/services/agent-runtime/rust-toolchain.toml" "$V/examples/agent-runtimes/custom-linear/rust-toolchain.toml"; do
  [ -f "$f" ] && sed -n 's/^channel *= *"\(.*\)"/\1/p' "$f"
done | { cat; echo nightly-2026-03-20; } | grep -E '^nightly-[0-9]{4}-' | sort -u | while read -r tc; do
  retry rustup toolchain install "$tc" --profile minimal --component rust-src
done
echo "::endgroup::"

FAILED=0
# suite <label> <log> <env...> -- <cargo args...>
suite() {
  local label=$1 name=$2; shift 2
  local envs=()
  while [ "$1" != -- ]; do envs+=("$1"); shift; done
  shift
  local log=$LOGS/$name.log rc=0 started=$SECONDS
  echo "::group::$label"
  (cd "$V" && env "${envs[@]}" cargo "$@") > "$log" 2>&1 || rc=$?
  tail -n 60 "$log"
  echo "::endgroup::"
  local took=$((SECONDS - started))
  [ "$rc" -eq 0 ] || FAILED=1
  local status="passed"; [ "$rc" -eq 0 ] || status="**FAILED** (exit $rc)"
  md "#### $label — $status, ${took} s" "" "\`${envs[*]} cargo $*\`" ""
  local results
  results=$(grep -E '^test result:' "$log" || true)
  if [ -n "$results" ]; then md '```' "$results" '```' ""; fi
  local failures
  failures=$(awk '/^failures:$/ {f=1; next} /^test result:/ {f=0} f && /^    [A-Za-z_][A-Za-z0-9_:]*$/ {print $1}' "$log" | sort -u)
  if [ -n "$failures" ]; then md "Failed: $(echo "$failures" | sed 's/.*/`&`/' | paste -sd' ' -)" ""; fi
  if [ "$rc" -ne 0 ] && [ -z "$results" ]; then md '```' "$(grep -E '^error' "$log" | head -20)" '```' ""; fi
}

# The candidate Authority ELF the physical tests load, built from this checkout.
echo "::group::authority ELF"
if (cd "$V/actors/system-authority" && CARGO_TARGET_DIR=$W/target/agent-state-authority \
    cargo +nightly-2026-03-20 actor --locked --features experimental-state-blocks) > "$LOGS/build-authority.log" 2>&1; then
  export AUTHORITY_CANDIDATE_ELF=$W/target/agent-state-authority/riscv64em-vos/release/system_authority.elf
  md "Authority ELF built from this checkout: \`$(sha256sum "$AUTHORITY_CANDIDATE_ELF" | cut -c1-16)…\`" ""
else
  tail -n 40 "$LOGS/build-authority.log"; FAILED=1
  md "Authority ELF **did not build**; candidate tests fail." ""
fi
echo "::endgroup::"

# Fixtures that `just test` builds first and a few lib tests read.
fixture=$V/examples/agent-runtimes/custom-linear
if grep -qs scripted-fixture "$fixture/Cargo.toml"; then
  if (cd "$fixture" && CARGO_TARGET_DIR=$W/target/agent-recovery-artifacts/scripted cargo actor --locked --features scripted-fixture) \
      > "$LOGS/build-recovery-fixture.log" 2>&1; then
    export AGENT_SCRIPTED_RUNTIME_ELF=$W/target/agent-recovery-artifacts/scripted/riscv64em-vos/release/custom_linear_agent_runtime.elf
  else
    md "Scripted recovery fixture did not build (logs/build-recovery-fixture.log)." ""
  fi
fi
(cd "$V" && CARGO_TARGET_DIR=$V/target cargo build --locked -p echo-extension) > "$LOGS/build-echo-extension.log" 2>&1 || true

VOSLIB=(test --locked -p vos --features "$FEATURES" --lib)
VOSX=(test --locked -p vosx)

# Upstream's own failures. These five `vos --lib` tests fail on the unmodified upstream
# `saga/agents` (e6f2bb45, the `base` row, run on 2026-10-03), with no gap applied, so they say
# nothing about a gap branch. They are skipped in the full suite, and listed in each summary,
# so that a branch's result is its own. All five are physical tests of the clean bootstrap
# (`agent::clean_bootstrap::tests::physical`):
#
# - native_local_authorization_clock_advance_preserves_receipt_and_intent: a Local Agent's
#   authorisation, across an advance of the clock, keeps its receipt and its intent;
# - native_local_create_submission_is_canonical_signed_and_runtime_bound: a Local Agent's
#   create submission is canonical, signed and bound to its runtime;
# - native_shared_system_owner_survives_route_retirement_and_fails_closed_on_poison: a Shared
#   Agent's system owner survives the retirement of its route, and fails closed on a poisoned
#   input;
# - physical_current_abi_shared_bootstrap_restarts_after_every_phase_without_duplicate_slots and
# - physical_current_abi_shared_bootstrap_restarts_before_every_phase_without_duplicate_slots:
#   a Shared Agent's bootstrap on the current ABI, restarted after (before) each of its phases,
#   never takes a slot twice.
#
# Re-enable the `base` row in the workflow when upstream's tip moves, and drop from this list
# whatever passes there.
BASELINE_FAILURES=(
  agent::clean_bootstrap::tests::physical::native_local_authorization_clock_advance_preserves_receipt_and_intent
  agent::clean_bootstrap::tests::physical::native_local_create_submission_is_canonical_signed_and_runtime_bound
  agent::clean_bootstrap::tests::physical::native_shared_system_owner_survives_route_retirement_and_fails_closed_on_poison
  agent::clean_bootstrap::tests::physical::physical_current_abi_shared_bootstrap_restarts_after_every_phase_without_duplicate_slots
  agent::clean_bootstrap::tests::physical::physical_current_abi_shared_bootstrap_restarts_before_every_phase_without_duplicate_slots
)
SKIPS=()
for t in "${BASELINE_FAILURES[@]}"; do SKIPS+=(--skip "$t"); done
md "Skipped in the full vos suite, because they fail on the unmodified baseline too: $(printf '`%s` ' "${BASELINE_FAILURES[@]##*::}")" ""

# 1. Every non-ignored test: regressions against the baseline.
suite "vos --lib, all non-ignored" vos-lib "X=1" -- "${VOSLIB[@]}" -- --test-threads=2 "${SKIPS[@]}"
suite "vosx, all non-ignored" vosx "X=1" -- "${VOSX[@]}" -- --test-threads=2

# 2. The branch's own tests, ignored ones included, one at a time.
case "$KEY" in
  base | gap1)
    suite "vosx fixed-roster and enrollment (include ignored)" vosx-roster "X=1" -- \
      "${VOSX[@]}" -- --include-ignored --test-threads=1 fixed_roster node_enrollment
    ;;
  gap2)
    suite "vosx Shared create/install/invoke (include ignored)" vosx-shared "X=1" -- \
      "${VOSX[@]}" -- --include-ignored --test-threads=1 candidate_shared_agent_create_install_invoke
    suite "vos shared_replica (include ignored)" vos-shared-replica VOS_AGENT_PROFILE_REFINE_MACHINES=1 -- \
      "${VOSLIB[@]}" -- --include-ignored --test-threads=1 agent::clean_bootstrap::tests::physical::shared_replica::
    suite "vos common_checkpoint replica join (include ignored)" vos-replica-join "X=1" -- \
      "${VOSLIB[@]}" -- --include-ignored --test-threads=1 candidate_shared_replica_join
    ;;
  gap3)
    suite "vos signed ChangeReplicas (include ignored)" vos-change-replicas "X=1" -- \
      "${VOSLIB[@]}" -- --include-ignored --test-threads=1 fresh_joiners_ v2_joiner_ signed_change_replicas committee_change committee_prepare one_voter_to_three observer_attaches
    ;;
  gap4)
    suite "vos Local export/import (include ignored)" vos-local-export "X=1" -- \
      "${VOSLIB[@]}" -- --include-ignored --test-threads=1 physical_local_export local_sdk_host::
    ;;
  gap5)
    suite "vos checkpoint archive (include ignored)" vos-archive "X=1" -- \
      "${VOSLIB[@]}" -- --include-ignored --test-threads=1 checkpoint_archive
    ;;
esac

df -h /mnt | tail -1
exit "$FAILED"
