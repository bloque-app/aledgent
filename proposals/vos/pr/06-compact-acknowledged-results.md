<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Compact acknowledgements for sequenced invocation identities

An Agent today refuses its 33rd acknowledgement and its 65th Linear invocation, and nothing can make
room again. This PR lets a caller opt in to invocation identities that carry their own position, so
that at capacity the runtime folds the oldest retirement fact into a per-component watermark instead of
refusing. Exactly-once still holds, and an image that never compacts encodes byte for byte as before.

## Problem

The standard AgentRuntime keeps a bounded window of results and retirement facts per result component
(`vos/src/agent/standard.rs` at `2bb78760`):

- `MAX_INVOCATION_RESULTS_PER_LANE = 32` (`:23`) unacknowledged results, and
  `MAX_INVOCATION_ACKNOWLEDGEMENTS_PER_LANE`, also 32 (`:28`), retirement facts (a retirement fact
  records that an invocation identity was acknowledged).
- Facts are never evicted. The comment at `:24-27` says why: a full component "rejects the next
  acknowledgement before retiring its terminal result, preserving exact retry evidence without an
  unbounded tombstone log".
- `commit_clean_acknowledgement` refuses the 33rd acknowledgement with `ResultCapacity`
  (`:4714-4722`). The result it would have retired stays, so after 32 more invocations
  `prepare_execution_state` refuses the next one (`:3027-3031`, also `ResultCapacity`).

The cap is deliberate. Identities are chosen by the caller (`InvocationWork::validate` only requires a
nonzero identity, `vos-agent-sdk/src/runtime.rs:268-282`), and a retained fact is the only thing that
stops a second delivery of the same identity, even under a fresh receipt, from running the work again
(`recover_clean_retirement`, `standard.rs:4507-4536`, returns `DivergentInvocation`).

Measured on one Local Agent driven through the runtime ABI, as `local_journal_driver.rs` does
(`execute_agent_wire`, `:3227-3276`), with a small test actor that writes and acknowledges each
invocation:

| Runtime | Result |
|---|---|
| `saga/agents` at `2bb78760` | 33rd acknowledgement refused (`ResultCapacity`); 65th invocation refused |
| this branch, sequenced identities | 10 000 acknowledged invocations, none refused; Agent state flat at 12 015 B |

## Change

One commit, 4 files, +665 −8; 330 lines are tests.

- **`vos-agent-sdk/src/runtime.rs`: sequenced identities.** New public items, re-exported at the crate
  root:
  - `SEQUENCED_INVOCATION_PREFIX = *b"vos/sequenced/v1"`;
  - `SequencedInvocation { storage: InvocationResultStorage, sequence: NonZeroU64 }`, with
    `new(storage, sequence)` and `invocation_id()`;
  - `SequencedInvocation::classify(id) -> SequencedInvocationId { Unsequenced, Sequenced(_), Malformed }`.

  The 32-byte identity is the 16-byte prefix, one component tag (Control 0, Linear 1, Merge 2,
  Local 3), seven zero bytes and a big-endian nonzero `u64` position. A position therefore has exactly
  one identity. A random identity carries the prefix with probability 2⁻¹²⁸. No wire type changes.
- **`vos/src/agent/standard.rs`: a watermark per result component and compaction at capacity.**
  - `StandardSequencedRetirement { control, linear, merge, local }` (`:1096`): the highest compacted
    position per component. It only grows. `StandardRuntimeState` gains a public field
    `sequenced_retired_through`; the struct already has crate-private fields, so this breaks no
    external construction.
  - In `commit_clean_acknowledgement` (`:4858-4904`), only when a component already holds 32 facts:
    if the acknowledgement being committed is at `watermark + 1`, it goes straight into the watermark;
    else if a retained fact of this component is at `watermark + 1`, that fact is removed, the
    watermark advances and the new fact is retained; otherwise the acknowledgement is refused with
    `ResultCapacity`, exactly as today.
  - In `recover_clean_retirement` (`:4640`, refusal at `:4658-4668`), which every Invoke,
    acknowledgement, error and expiry path already consults: a sequenced identity at or below its
    component's watermark returns `DivergentInvocation`. So does a sequenced identity used from a mode
    whose result component differs from its tag, and a malformed one.
  - Restore (`:2006-2019`, `:2073-2100`) rejects a nonzero watermark on a legacy image or an
    unsupported component, and any retained result, error, continuation or fact at or below its
    component's watermark.
- **`vos/src/agent/wire.rs`: encoding.** An optional trailing section per component, after the
  existing optional `SCER` errors section: `SQRT`, a `u16` version 1 and the `u64` watermark, 14 bytes,
  written only when the watermark is nonzero (`:464-510`, written at `:824` and `:1491-1495`). A zero
  watermark inside the section is non-canonical.
- **`vos/src/service/wire.rs`:** a crate-private `Decoder::peek` (`:130`).

## Design choices and alternatives

- **Evict a fact when its receipt expires.** Rejected. The expired receipt can no longer run the work,
  but a new receipt for the same identity can, and after eviction nothing refuses it. It would also let
  `retain_clean_unseen_expiry` mint an `ExpiredBeforeExecution` fence ("never ran") for work that ran.
  And with the CLI's one-hour receipts, the retained set is still every invocation of the last hour.
- **An authority-signed watermark**, as the management journal does with `decision_sequence` and
  `acknowledged_through` (`standard.rs:6225-6284`). Sound, but it changes the `InvokeActor` receipt
  contract (both fields must be zero today, `vos-agent-sdk/src/authority.rs:2315-2320`), makes the
  issuer allocate per-Agent sequences, and still leaves de-duplication to the authority.
- **An explicit `sequence` field on `InvocationWork`.** Cleaner to read, but an ABI bump. Encoding the
  position in the identity needs no wire change and is opt-in by construction.
- **Compaction only at capacity, only the lowest position.** Each component still keeps 32 exact facts,
  compaction never skips a gap, and it is a pure function of the prior state and the acknowledgement:
  no clock, no host input.

## Compatibility

- **Existing Agents, images and journals:** unchanged. The `SQRT` section is absent while the watermark
  is zero, so every image that never compacts encodes and decodes exactly as before. No ABI, SDK wire or
  receipt change.
- **Opt-in:** only identities that carry the prefix are affected. In an Agent that never uses them,
  behaviour is unchanged.
- **Mixed Agents.** In an Agent that uses both kinds, an unsequenced acknowledgement at capacity now
  succeeds when the fact at `watermark + 1` is sequenced: it compacts that fact and takes its slot. An
  unsequenced fact at the front still blocks, as today.
- **One visible difference.** An acknowledgement retry of a compacted identity returns
  `DivergentInvocation`, not the exact acknowledgement: the runtime can no longer prove which work
  retired there. An Invoke retry of any acknowledged identity already returned `DivergentInvocation`.
  In a stream acknowledged in order, the 32 most recent retirements keep exact retry evidence.
- **Forward only.** Once a component compacts, its image carries `SQRT`, which a runtime without this
  change rejects. There is no downgrade after the first compaction.
- **Migrating a caller.** Build identities with
  `SequencedInvocation::new(mode.result_storage(), position).invocation_id()`. The caller keeps one
  counter per result component, **starting at 1 and never skipping a position**: a missing position
  blocks compaction until it is invoked (or fenced) and acknowledged. There is no allocator helper in
  `vosx` or the hosts yet.
- **Artifact.** `vosx/blobs/agent_runtime.pvm` and `support/production-artifacts.toml` are not rebuilt
  or repinned; the runtime's ProgramId changes when you rebuild it from this source.

## Tests

New, in `vos/src/agent/wire.rs`:

- `sequenced_acknowledgements_compact_past_capacity_without_readmitting_work` (`:15202`,
  `feature = "pvm"`): 10 000 sequenced Linear invocations, each acknowledged, with checkpoint and
  reopen every 1 000 (byte-identical). After each: no retained results, `min(n, 32)` facts,
  watermark `n − 32`. Retired identities are refused on Invoke; the oldest on Invoke, on
  acknowledgement, under a re-signed receipt and on the expiry-fence path, without changing the state;
  hostile watermarks are rejected on restore.
- `sequenced_compaction_waits_for_gaps_and_refuses_foreign_identities` (`:15362`, `pvm`).
- `sequenced_retirement_section_is_absent_until_used_and_canonical` (`:15440`); it checks the Control
  component's encoding.

```sh
cargo test -p vos --lib --features agent-runtime agent::wire::tests::sequenced_
cargo test -p vos --lib --features 'agent-runtime storage network http-ingress'
cargo test -p vos-agent-sdk
```

Run on Linux aarch64, `nightly-2025-05-09`:

| Command | Result |
|---|---|
| `cargo test -p vos --lib --features 'agent-runtime storage network http-ingress'` | 1 781 passed, 17 ignored, 7 failed; the same 7 fail on unmodified `2bb78760` |
| the same with `--ignored compiled_runtime compiled_shared_genesis` and `AGENT_RUNTIME_CANDIDATE_ELF` built from this branch | 4 passed |
| `vos` with `experimental-state-blocks`, filtered to `agent::wire`, `replay`, `state_block*`, `execution`, `actor_storage` | 222 passed, 26 ignored |
| `cargo test -p vos-agent-sdk`, with and without `experimental-state-blocks` | 247 and 248 passed |

**Not run:** the 7 failures above are two tests that need the x86-64 recompiler and five that need
prebuilt artifacts (`package_admission.rs:754`, `clean_bootstrap.rs:10723` and `:10760`,
`driver.rs:5934`); this branch has not been run on x86-64. None of the four compiled-runtime gates
exercises compaction, so the compiled runtime has not been tested on the new path.

**Cost per invocation** (one Local Agent, 200 invocations per state size, p50). Before, the 2 MiB and
≈ 3.9 MiB fills needed more invocations than 32 acknowledgements allow, so 31 results stayed
unacknowledged; after, every fill is acknowledged:

| Agent state | Before: Invoke / Ack gas | After: Invoke / Ack gas |
|---:|---:|---:|
| 64 KiB | 99.6 M / 43.2 M | 102.0 M / 45.5 M |
| 2 MiB | 823.0 M / 664.7 M | 612.6 M / 477.6 M |
| ≈ 3.9 MiB | 1 283.0 M / 1 060.7 M | 1 111.0 M / 898.7 M |

Each retained unacknowledged result costs about 5.5 M gas per Invoke, most likely the receipt
re-verification on restore (`standard.rs:1837-1862`). A caller that acknowledges everything pays none
of it. Holding 32 facts costs about 2.4 M gas per execution.

## Relation to the other pull requests

All five VOS pull requests (#6 to #10) are one commit each on `saga/agents` at `2bb78760`, still the tip
(checked 2026-09-29), and each applies alone. This one shares `vos-agent-sdk/src/runtime.rs` and
`vos/src/agent/wire.rs` with #7, and `vos/src/agent/standard.rs` with #8, in different regions.
`git merge-tree` merges it cleanly with each of #7, #8, #9 and #10, and #6 + #7 + #8 + #9 and
#6 + #7 + #9 + #10 merge cleanly together. It can go first. (#8 and #10 conflict with each other; see
those PRs.) Our x86-64 measurements for #10 run with this commit applied, because our host uses
sequenced identities.

## Questions for you

1. **Position in the identity** (no wire change), an explicit `sequence` field on `InvocationWork`
   (ABI bump), or an authority-signed watermark?
2. **The acknowledgement retry of a compacted identity:** keep `DivergentInvocation`, or add a distinct
   `InvocationError::Retired` (a new wire tag)?
3. **Mixed Agents:** is it acceptable that an unsequenced acknowledgement at capacity can compact a
   sequenced fact, or should unsequenced facts only ever be refused at capacity?
4. **Many callers.** One sequence per component suits a single writer. Many independent callers would
   need a namespace each (for example per credential). A follow-up?
5. **The Merge component under the Shared profile:** compaction assumes every replica sees the same
   acknowledgement order. Is that guaranteed there?
6. **Receipt re-verification on every restore**, about 5 M gas per retained result, error or
   continuation. Keep it?
7. Repinning `vosx/blobs/agent_runtime.pvm` and `support/production-artifacts.toml` after review.

## Why we need it

We are building a confidential double-entry ledger that runs as a VOS Agent, deciding each
balance-affecting command within a completion budget. Its commands arrive as one ordered stream of Linear
invocations from a single ingress. Without this change the Agent stops after 64 of them, for good. With
it, our core Agent ran 1 250 committed invocations (1 000 ledger operations) with the runtime state flat
at 11.9 KB.
