<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# VOS 0001 — Compact acknowledged results with sequenced invocation identities

**Draft pull request description, 2026-09-23.** Not yet opened upstream.

| | |
|---|---|
| Branch | `agent/compact-acknowledged-results` on aledgent's VOS fork, one commit on `saga/agents` at `2bb7876` |
| Commit | `de0f12f8` `feat(agent): compact sequenced invocation acknowledgements` |
| Size | 4 files, +665 −8 (`vos-agent-sdk/src/runtime.rs`, `vos/src/agent/standard.rs`, `vos/src/agent/wire.rs`, `vos/src/service/wire.rs`); about 330 lines are tests |
| aledgent task | `T-025-01` (issue #66), `DEC-16` |

Everything below the line is the pull request text.

---

## Problem

The standard AgentRuntime keeps at most 32 positive retirement facts per result component
(`MAX_INVOCATION_ACKNOWLEDGEMENTS_PER_LANE`, `vos/src/agent/standard.rs:23-29`) and refuses the next
acknowledgement once a component is full (`commit_clean_acknowledgement`, `:4711-4721`). Unacknowledged
results are capped at 32 as well. Neither is ever evicted. On one Local Agent driven through the
runtime ABI, the **33rd acknowledgement and the 65th Linear invocation are refused with
`ResultCapacity`**, and nothing in the released code can make room again. An Agent that serves a
stream of Linear writes stops after 64 of them.

The cap is deliberate. The comment at `:24-27` says why: a full component refuses "preserving exact
retry evidence without an unbounded tombstone log". Exactly-once is keyed by the invocation identity,
and identities are caller-chosen: an `AgentInvocationIntent` carries whatever identity its caller
puts in it, and `InvocationWork::validate` only requires it to be nonzero.
A retirement fact is the only thing that stops a second delivery of the same identity from running
again. That includes a second delivery under a *fresh* receipt for the same work: today it finds the
retained fact and is refused as `DivergentInvocation` (`recover_clean_retirement`, `:4507-4536`).
Dropping the fact would turn it into unseen work.

Two cheaper-looking fixes do not hold:

- **Evict a fact once its receipt has expired.** Unseen execution needs a live authorization at a slot
  at or above the Agent's logical high water (`validate_clean_unseen_invocation_slot`, `:2776-2803`),
  so the *expired receipt* can never run the work again. A new receipt for the same identity can,
  and after eviction nothing refuses it. Eviction by expiry would also let `retain_clean_unseen_expiry`
  mint an `ExpiredBeforeExecution` fence ("never ran") for work that did run. And with the CLI's
  one-hour receipts over wall-clock slots, the retained set would still be every invocation in the
  last hour.
- **An authority-signed watermark**, as the management journal does with `decision_sequence` and
  `acknowledged_through` (`:6237-6284`). That is sound, but it changes the receipt contract for
  `InvokeActor` (the SDK requires both fields to be zero today), needs the issuer to allocate
  per-Agent sequences, and still leaves the dedup of identities to the authority.

The evidence, from aledgent's measurement harness (`spikes/vos-host` in `bloque-app/aledgent`, which
drives the runtime ABI exactly as `local_journal_driver.rs:3325-3364` does), is in the table under
"Before and after".

## Design

Keep every existing guarantee for every existing identity, and let a caller *opt in* to identities
that carry their own order, so that old facts can be summarized without being forgotten.

**Sequenced identities** (`vos_agent_sdk::SequencedInvocation`, `vos-agent-sdk/src/runtime.rs`). An
`InvocationId` whose 32 bytes are the prefix `vos/sequenced/v1`, one result-component tag (Control 0,
Linear 1, Merge 2, Local 3), seven zero bytes and a nonzero big-endian position. The identity is a
pure function of `(component, position)`, so a position can be named by exactly one identity. No wire
type changes: work, receipts and acknowledgements are byte-identical. `SequencedInvocation::classify`
tells unsequenced, sequenced and malformed identities apart; a random 32-byte identity carries the
prefix with probability 2⁻¹²⁸.

**A watermark per result component** (`StandardSequencedRetirement`, `standard.rs:1096`). The highest
position whose retirement fact that component has compacted. It only grows.

**Compaction happens only when a component is full**, inside `commit_clean_acknowledgement`
(`standard.rs:4858-4905`). If the component already holds 32 facts, the runtime looks at the next
position (`watermark + 1`):

- if the fact being committed *is* that position, it goes straight into the watermark;
- else if a retained fact is at that position, it is removed and the watermark advances, and the new
  fact is retained;
- otherwise (an unsequenced fact is blocking, or the next position has not been acknowledged yet)
  the acknowledgement is refused with `ResultCapacity`, as today, and nothing changes.

So every component still holds 32 exact facts, as it does now, and what leaves is always the lowest
position. Compaction never runs below capacity, never skips a gap, and never touches an unsequenced
fact.

**Refusal of compacted identities** is added in the one place every path already asks
(`recover_clean_retirement`, `standard.rs:4655-4670`). When no retained fact matches and the identity
is sequenced at or below its component's watermark, it returns `DivergentInvocation`, the same answer
a retained divergent fact gives. The Invoke recovery path, `recover_clean_execution`,
`retain_clean_invocation_error`, `retain_clean_unseen_expiry` (through it) and the ACK path all
consult it before anything can run or be retained. A sequenced identity used from a mode whose result
component differs from its own tag, or a malformed one, is refused the same way.

**Restore** (`standard.rs:2003-2019`, `:2073-2100`) rejects a nonzero watermark on a legacy image or an
unsupported component, and rejects any retained result, error, continuation or fact that names a
position at or below its component's watermark.

**Encoding** (`vos/src/agent/wire.rs:464-510`). Each component gains an optional trailing section,
`SQRT` magic, version 1, the watermark, after the existing optional `SCER` errors section. It is
written only when the watermark is nonzero, and a zero watermark in it is non-canonical. **Every image
that never compacts encodes exactly as before**, so existing images, retained journals and golden
state tests are unaffected. `Decoder::peek` (`vos/src/service/wire.rs`) is the only helper added.

## Guarantees preserved

- **Exactly-once.** A sequenced identity `(c, p)` can run only if no retained record names it and
  `p > watermark(c)`. Once it has run, a result is retained until acknowledged; the acknowledgement
  becomes a retained fact; the fact leaves only by compaction, which requires `p = watermark(c) + 1`
  and sets `watermark(c) = p`. The watermark never decreases. So after the first execution, either a
  retained record names the identity or `p ≤ watermark(c)`, and both refuse it, under any receipt,
  including a freshly signed one (tested). Since the identity is a function of `(c, p)`, no other
  identity can reach the same position, and the component tag stops an Ordered-scope identity from
  running once in Control and again in Linear. Unsequenced identities are untouched.
- **Replay determinism.** Compaction is a pure function of the prior state and the acknowledgement:
  it depends only on the component's count, its watermark and the retained facts' identities. No
  clock, host input or iteration-order dependence. Checkpoint and reopen is byte-identical at every
  1 000th invocation of the long test, and the compiled runtime matches the native source on the
  existing compiled-runtime gates.
- **The idempotency window.** Each component keeps 32 exact retirement facts, as today; only the
  lowest position is ever summarized, so in a stream acknowledged in order the 32 most recent
  retirements keep exact retry evidence. An Invoke retry of an older, compacted identity gets
  `DivergentInvocation`, which is what an Invoke retry of any acknowledged identity already gets. The
  one visible difference: an *ACK* retry of a compacted identity also gets `DivergentInvocation`
  instead of the exact acknowledgement. The runtime can no longer prove which work was retired at
  that position, so it does not claim it.
- **No false non-execution fence.** `retain_clean_unseen_expiry` cannot mint `ExpiredBeforeExecution`
  for a compacted identity (tested).
- **Hostile state.** Restore rejects every inconsistent combination listed above (tested).
- **No ABI or artifact change.** The r19 ABI, the SDK wire and the bundled `agent_runtime.pvm` are
  unchanged by the source change. The runtime program identity changes when the artifact is rebuilt
  from this source; repinning `support/production-artifacts.toml` is the maintainer's release step and
  is not in this branch.

## Tests

In VOS's own suite (`vos/src/agent/wire.rs`, next to
`clean_acknowledgement_capacity_preserves_facts_and_result_across_restart`):

- `sequenced_acknowledgements_compact_past_capacity_without_readmitting_work`: **10 000 sequenced
  Linear invocations, each acknowledged**, on one Agent, with checkpoint/reopen every 1 000. After each
  one, zero results are retained, `min(n, 32)` facts are retained and the watermark is `n − 32`.
  Every retired identity is refused as divergent on Invoke; the oldest is refused on Invoke, on ACK,
  under a re-signed receipt and on the expiry-fence path, without changing the state; the newest keeps
  its exact ACK retry; an unsequenced fact at capacity is retained exactly; hostile watermarks are
  rejected on restore.
- `sequenced_compaction_waits_for_gaps_and_refuses_foreign_identities`: a gap at the next position
  keeps the component refusing; filling the gap retires the new fact straight into the watermark;
  an identity from another component and a malformed prefix are refused.
- `sequenced_retirement_section_is_absent_until_used_and_canonical`: no `SQRT` section while unused;
  a zero watermark is non-canonical.

Suites run (Linux `aarch64` container, `nightly-2025-05-09`):

| Command | Result |
|---|---|
| `cargo test -p vos --lib --features 'agent-runtime storage network http-ingress'` | 1 781 passed, 17 ignored, **7 failed — the same 7 fail on unmodified `2bb7876`**: two need the recompiler (`x86_64` only), five need prebuilt artifacts (`package_admission.rs:754`, `clean_bootstrap.rs:10723`/`:10760`, `driver.rs:5934`) |
| the same, `--ignored compiled_runtime compiled_shared_genesis`, with `AGENT_RUNTIME_CANDIDATE_ELF` built from this branch | 4 passed |
| `cargo test -p vos --lib --features '… experimental-state-blocks'` on `agent::wire`, `agent::replay`, `agent::state_block*`, `agent::execution`, `agent::actor_storage` | 222 passed, 26 ignored |
| `cargo test -p vos-agent-sdk`, with and without `experimental-state-blocks` | 247 and 248 passed |

## Before and after

aledgent's harness, one Local Agent with the `state-blob` actor, through the runtime ABI. "Before" is
the standard runtime built from `2bb7876` by the same pipeline as "after"
(`cargo actor` in `services/agent-runtime`, then `vos_pvm_compiler::link_elf_spi`, as
`vosx agent-runtime-pvm` does). That build is not byte-identical to the bundled
`agent_runtime.pvm`, which is pinned to the older source revision `2ccfacb`.

**Capacity** (`vos-host retention`, `state-blob` `touch`, each invocation Invoke then Acknowledge):

| Runtime | Result |
|---|---|
| before | the 33rd acknowledgement is refused (`ResultCapacity`); the 65th invocation is refused |
| after, sequenced identities | **10 000 acknowledged invocations** (plus the setup fill), none refused; Agent state constant at 12 015 B from the first thousand to the last |

**Cost per invocation** (`vos-host s4`, 200 invocations per size, p50 gas, caller time p50 / p99 for
Invoke plus Acknowledge in process, `aarch64` Linux VM on an M2 Pro). Before, filling 2 and 4 MiB
takes more invocations than 32 acknowledgements allow, so 31 results stay unacknowledged there; after,
every fill is acknowledged and nothing is retained, while the component holds its 32 most recent
facts.

| Rows | Before: retained results | Invoke gas | Ack gas | Caller ms | After: retained results | Invoke gas | Ack gas | Caller ms |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 64 KiB | 0 | 99.6 M | 43.2 M | 189.1 / 210.0 | 0 | 102.0 M | 45.5 M | 192.7 / 219.3 |
| 1 MiB | 0 | 346.6 M | 252.4 M | 930.4 / 968.3 | 0 | 348.4 M | 254.1 M | 930.3 / 955.8 |
| 2 MiB | 31 | 823.0 M | 664.7 M | 2 182.5 / 2 243.7 | 0 | 612.6 M | 477.6 M | 1 737.7 / 1 786.6 |
| ≈ 3.9 MiB | 31 | 1 283.0 M | 1 060.7 M | 3 637.4 / 3 722.4 | 0 | 1 111.0 M | 898.7 M | 3 163.3 / 3 250.3 |

**The per-result term.** The control run (before, 31 unacknowledged results at 64 KiB, 100
invocations) costs 271.6 M gas per Invoke and 205.6 M per Acknowledge, 622.0 / 653.3 ms at the caller:
**5.5 M and 5.2 M gas per retained result per execution**, consistent with the receipt
re-verification on restore (not profiled).
After the change nothing forces results to stay retained, so the term is zero for a caller that
acknowledges each result: at 2 and 4 MiB that is −210 M / −172 M gas per Invoke and −187 M / −162 M
per Acknowledge (the results and their bytes), and 20 % / 13 % at the caller. Holding 32 facts
costs about 2.4 M gas per execution (the 64 KiB row), roughly 75 k per fact.

This change does not move the fixed cost of an invocation or the per-byte cost of state; those are
other work.

## For the maintainer to decide

1. **Opt-in identity layout versus an explicit field.** This branch encodes the position in the
   identity so no wire type changes. The alternative is a `sequence` field on `InvocationWork` (an ABI
   bump), or the authority-signed watermark above.
2. **The ACK retry of a compacted identity.** It returns `DivergentInvocation`. A distinct
   `InvocationError::Retired` would say it more precisely, at the cost of a wire tag.
3. **Who uses sequenced identities.** One Agent-wide sequence per component suits a single writer (an
   ingress process, a system actor). Many independent callers would need a sequence namespace each,
   for instance per credential, with its own watermark; that is a follow-up, not in this branch.
4. **The per-result signature re-check.** Restore re-verifies the receipt signature of every retained
   *unacknowledged* result on every execution (`standard.rs:1837-1862`); the measured cost is about
   5 M gas each. It is part of the hostile-state restore, so this branch leaves it. With compaction a caller acknowledges every
   result and none accumulate, so in practice the term disappears (see the table).
5. **Repinning the bundled runtime** after review.
