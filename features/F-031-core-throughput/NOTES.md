<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-031 — implementation notes

Decisions taken where the plan left room, each the conservative reading. Questions only the owner can
settle (`c-01` to `c-12`) are summarised at the end.

## Decisions

1. **"Commands per invocation" was one.** The brief named `MAX_CALLS` 32 as today's cap. That constant is
   the settlement contract's calls per batch (`services/settle`, `contracts/settlement`, #102). The core
   took one input per invocation. Lever 1 adds the batch; the contract's cap is unchanged.
2. **Numbers.** Amendment 0011 took `NFR-17`, `DEC-20`, `DEC-21` and `F-030`. This feature uses amendment
   0012, `DEC-22`, `F-031`, `NFR-18` to `NFR-20` and `RISK-13`, `DEP-11`.
3. **Shared from genesis, one voter** (ruled 2026-10-03, `c-11`). The core is created with the Shared
   profile and one voter, then three when VOS has membership change: no re-genesis, and `DEC-21`'s
   single node is that one voter.
4. **The committed map is ours.** `core_binding::committed` implements the same tree as VOS's
   `committed.rs` over the actor's rows, so it runs natively and in tests, and stays Apache-2.0 OR MIT. Its
   roots are CipherClerk's. Converging on VOS's type is a question for @olanod (VOS's maintainer) (`c-12`).
5. **`service` and `book-kernel` stay one actor** until @olanod confirms an inline call chain commits as one
   Linear transition (alignment proposal §7, Q3).

## Owner questions (all with a default applied)

| Id | Question | Default |
|---|---|---|
| `c-01` | `NFR-18`'s numbers | *Ruled 2026-10-03:* no target; measured and reported by the stress suite; aim 10k/s, 1M better |
| `c-02` | `NFR-19`'s credit lag | 2 s at p99 |
| `c-03` | `NFR-20`'s revocation lag | 1 s at p99 |
| `c-04` | Closing an account with credits in transit | `Closing` until every shard confirms none in flight |
| `c-05` | A multi-leg unit across partitions | Reserve every debit leg, then capture all or release all |
| `c-06` | Which accounts are pools, and who rebalances | Issuer escrows, fee collectors, treasuries, by the operator |
| `c-07` | Partition by account or by identity (`REQ-BA-5`) | By account, as ruled; in transit shown |
| `c-08` | `US-F2`, `REQ-AS-5`, `DEF-7` withdrawn with `OQ-17` | Withdrawn |
| `c-09` | 2¹⁶ partitions, ranges per shard, the directory in the principals Agent | As stated |
| `c-10` | Principals: resolved by ingress and stamped, revocations fed to shards | As stated |
| `c-11` | Shared with one voter at first deployment, against `DEC-21`'s single node | *Ruled 2026-10-03:* Shared, one voter; restructure now |
| `c-12` | Our committed map against VOS's `committed.rs` | Ours, byte-compatible; ask @olanod |

## Lever 1: batched core invocations

Notes of `T-031-02` to `T-031-05` (branch `f-031-batching`, 2026-10-02/03). Where the brief was silent
the conservative reading was taken; each such choice is listed here.

### What landed

| Task | Where | What |
|---|---|---|
| `T-031-02` | `crates/core-binding` (`envelope`, `transition`, `hot`) | `Envelope::Batch`, `Reply::Batch`, `AgentRefusal::Skipped` (appended). The Agent applies a batch's inputs in order over an overlay of its rows, each exactly as `input()` applies one alone, with a prefix commit. `MAX_BATCH_INPUTS` = 256. `Limits::DEFAULT`: ring 16 → 64, budget 26 → 32 KiB |
| `T-031-04` | `services/ingress` (`boundary`, `remote`, `service`, `store`, `config`) | `Core::apply_batch` (default: one by one, stopping at the first failure); `Request::ApplyBatch` / `Response::Replies`; the forwarder takes up to `core_batch` undecided rows (default 32, 1 = one at a time) and decides the answered prefix in order; `Store::next_undecided_batch` (default: one row; MemStore and PgStore in forwarding order); `DeploymentConfig::core_batch` |
| `T-031-03` | `crates/core-binding` (`host`, `core`), `crates/conformance`, `agents/core-host` (`serve`) | `Driver::stage` / `Driver::flush` (`sync` = both), `BatchLimits`, size-adaptive staging; `AgentCore::apply_batch` with the rebuild mapping and the singles fallback; `CONFORMANCE_AGENT_BATCH`; `serve --batch N` |
| `T-031-05` | `agents/core-host` (`measure`), `.github/x86-bench`, `x86-bench.yml` | `measure --batch N`; the bench's `batches` and `batch_gas` inputs and the batched tables |

### Decisions and deviations

1. **Packs (deviation from `Envelope::Batch(Vec<InputEnvelope>)`).** VOS hands an application actor
   at most **four caller blobs** per invocation (`vos/src/agent/execution.rs`,
   `MAX_EXECUTION_BLOBS = 4` at `de0f12f`), and every input brings three. So the envelope is
   `Batch(BatchEnvelope { inputs, packs })`: the blobs of a run of consecutive inputs travel in one
   caller blob, a `Pack` (each blob with the key its input names it by), and `packs` names at most
   four of them with how many inputs each covers. Each `InputEnvelope` is unchanged: its own three
   blob keys and its own witness, built against the state after the input before it. The pack is
   checked against its own key by the runtime; inside it the Agent matches each input's blobs by
   key and length (it does not recompute the runtime's content hash, which is port-specific).
2. **The guest's heap bounds a pack.** The Agent fetches one pack when it reaches its first input
   and drops it after its last, so the 256 KiB heap holds one pack, the input it is running, the
   message and the buffered writes. `BatchLimits::max_pack_bytes` = 46 KiB (a quarter of
   `max_blob_bytes`). Natively, a 20-input batch peaks at 151 KB with its message, against 95 KB
   for one input (`tests/heap.rs`, `req_rt_1_a_full_batch_fits_the_guests_heap`); the test holds it
   under three quarters of the heap.
3. **VOS's actor gas bounds a batch at three reservations.** `MAX_EXECUTION_GAS` is 1 000 M per
   actor invocation at `de0f12f`, and a reservation takes about 295 M of actor gas. So the batch
   is also bounded by `max_actor_gas / input_gas` (330 M, above every ordinary command's p99). The
   default sets no gas bound (a native port has none); `aledgent-core-host` sets VOS's constant.
   A heavier input can still run a batch out of gas: nothing of it commits, and `AgentCore`
   re-drives its inputs one at a time. Lifting the bound is a VOS change (to be proposed upstream
   with `proposals/vos/0003`'s program limit); the bench measures both.
4. **The reply limit is 8 KiB, not 16.** VOS keeps an application actor's reply to
   `MAX_EXECUTION_REPLY_BYTES` = 8 KiB (the protocol's 16 KiB is the outer bound).
   `max_reply_bytes` = 7 KiB, estimated from each input's summaries at its widest.
5. **Ring and budget.** Ring 64 holds two host batches of 32. Budget 32 KiB, not more: 64 ordinary
   entries are about 17 KiB, and VOS refuses a lane image over 48 KiB in one execution
   (`MAX_EXECUTION_STATE_TOTAL_BYTES`), which the guest holds in its heap beside the batch.
6. **Stage assumes commit.** `stage` applies the input's service leaves and pins the next view at
   once; a refused or ambiguous flush leaves the host ahead of the Agent, and the host rebuilds
   (`Driver::recover`), as it already did after a refused input.
7. **No input is answered before one ahead of it.** `Core::apply_batch`'s default, `AgentCore`'s
   singles fallback and the forwarder all stop at the first failure; everything after it answers
   `Unavailable` and runs nowhere, and the forwarder retries from there. The singles fallback
   re-drives only the failed batch's remaining inputs, and stops at the first that fails alone.
8. **Archive entries.** `AgentCore` now archives the entries `Driver::take_entries` gives after a
   successful flush, and nothing else; after a rebuild only `rebuild` archives (what the Agent
   committed beyond the log). The single-input path pushed `last_entry` again after a rebuild that
   had already archived it (an input committed, then every retry of its answer failed), which
   would have broken the archive's chain at the next start.
9. **A remote batch of one is an `apply`.** `RemoteCore::apply_batch` sends one input as
   `Request::Apply`, so a host without `ApplyBatch` still answers `core_batch = 1`. A host that
   predates `ApplyBatch` answers a larger batch `Unavailable`; ingress and the core host are
   deployed together, so this is not handled further.
10. **`tests/agent_core.rs` was not running.** It needs core-binding's `ingress` feature, which
    nothing in the workspace enables, so `cargo test --workspace` skipped it, and its two tests had
    gone stale with amendment 0007 (a reservation is acknowledged `202 Received`). They are fixed
    (they wait for the decision, and count the Agent's reservations instead of its inputs, since
    ticks and deliveries commit in the background), and CI runs the file.

11. **Reservations still go first.** One row at a time, a reservation logged while a row was
    retried went next, ahead of every queued transfer (`T-009-04`). With a batch in flight, the
    forwarder keeps retrying only the rows the core may have run (up to its last ambiguous answer);
    the rest go back to the queue, which is read again, so a reservation logged meanwhile goes
    before them. `surface.rs`'s `reservations_are_forwarded_before_earlier_transfers` caught this.
12. **Layout 3 is not batched.** Lever 2's committed-map Agent (`crate::mapped`, #133) is driven one
    input per invocation: on a mapped driver `stage` flushes anything staged and sends the input at
    once, and its result comes with the next `flush`. Batching layout-3 inputs is not designed here.

### Tests

| Test | Verifies |
|---|---|
| `tests/batch.rs` `req_rt_1_inv_18_a_batch_commits_exactly_as_its_inputs_one_by_one` | N inputs in one invocation = N singles: summaries, roots, next postings, log hashes, ring entries, hot row |
| `req_rt_1_a_refused_input_commits_the_inputs_before_it_and_skips_the_rest` | Prefix commit: 1..k-1 commit, k refused, the rest `Skipped`; the Agent equals two singles |
| `inv_18_a_batch_resent_after_an_unknown_outcome_changes_the_book_once`, `inv_18_a_batch_refused_midway_and_resent_commits_its_prefix_once` | Re-sent batches answer `Duplicate` for what committed |
| `req_cs_7_staging_flushes_before_a_bound_is_passed` | Size-adaptive staging: blobs, packs, count, actor gas, message |
| `t_031_02_*` | Malformed batches refused whole; a missing pack; pack encoding |
| `tests/heap.rs` `req_rt_1_a_full_batch_fits_the_guests_heap` | A default batch fits the guest's heap |
| `tests/agent_core.rs` `req_cs_7_*`, `inv_18_a_batch_whose_answer_is_lost_commits_once` | `AgentCore::apply_batch` = one by one; a failed batch commits its prefix, then the rest alone; a faulty input fails alone and nothing after it runs; the archive survives a restart |
| ingress `t_031_04_*` | The forwarder decides in log order with batches and a mid-batch failure; the socket batch; the stores' order |
| conformance with `CONFORMANCE_AGENT_BATCH=8` | The suite through the native Agent, eight inputs per invocation (CI step) |

### Measured

Run 37099849992 of the x86-64
bench (`reservation-timing`, `batches="1 8 32"`, `batch_gas` 8 000 M), on this branch at `7b2608b`:
AMD EPYC Genoa, 16 CPUs, VOS `3140c3d` with `de0f12f` picked, the recompiler, 192 reservations after
3 of warm-up. The core is now 2.64 MB of program (lever 2 linked), so one reservation costs 2 061 M of
invoke gas, not §9's 1 747 M. Observations, not requirements (`NFR-18` has no target, ruled
2026-10-03). Details and reproduction: `notes/MEASUREMENTS-F031.md` §1.

| VOS actor gas | Batch | Invocations | Round p50 | Round p99 | Invoke gas p50 / invocation | Actor gas p50 / invocation | Inputs/s (round p50) | Inputs/s (run) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| stock, 1 000 M | 1 | 192 | 292 ms | 330 ms | 2 062 M | 295 M | 3.42 | 3.38 |
| stock | 8 | 72 | 1.13 s | 1.19 s | 2 667 M | 882 M | 7.10 | 7.05 |
| stock | 32 | 66 | 4.02 s | 4.06 s | 2 672 M | 885 M | 7.96 | 7.99 |
| raised, 8 000 M | 1 | 192 | 295 ms | 324 ms | 2 062 M | 295 M | 3.39 | 3.37 |
| raised | 8 | 24 | 531 ms | 572 ms | 4 195 M | 2 360 M | 15.08 | 15.01 |
| raised | 32 | 12 | 1.62 s | 1.80 s | 5 418 M | 3 536 M | 19.69 | 19.43 |

- **Stock VOS: 2.3 times**, bounded by the actor gas at three reservations per invocation (3, 3, 2 for
  a batch of 8; 3 each for 32).
- **With only `MAX_EXECUTION_GAS` raised: 4.4 times at 8 and 5.8 times at 32.** At 32 the packs bind
  (20 and 12 inputs per invocation, 178 KB of caller blobs at most): the heap, state, message, reply
  and availability bounds are VOS's own.
- Against the stress suite's curve (#127, one input carrying K commands, every bound raised): 3.6,
  13.9 and 13.1 inputs/s at 1, 8 and 32. Batching separate inputs reaches 15.1 and 19.7 with only the
  gas raised, because each input keeps its own small witness and no per-command bound is passed.
- The lockstep run (227 executions) found both engines identical, and every batch committed with its
  roots equal to the replica's.

### Deferred

- Lifting VOS's 1 000 M actor gas per invocation (an upstream change, to propose with
  `proposals/vos/0003`'s program limit; without it a batch holds three reservations).
- A gas estimate learned from the runtime's reports instead of a fixed 330 M per input.

## T-031-19: the store commit, batched and cut

Measured and explained in `notes/MEASUREMENTS-F031.md` §6.

Decisions, each the conservative reading:

1. **A batch is one commit, not one commit per unit in one transaction.** The merged delta passes
   the same write rules (`INV-2`, `REQ-PO-2`), and the delta log gets one record per batch. A
   unit's outcome never depends on the batch (the kernel does not read the root it decides on);
   a reader of the delta log sees fewer, larger records.
2. **Reads through the batch, roots at the last commit.** `Book::kernel_account`,
   `Book::settled_net` and `Book::next_posting_seq` answer as a later unit sees the book, which
   `ledger-core` needs mid-command. `Book::root`, `Book::store` and `Book::vault` answer at the last
   commit. A checkpoint (on the tick or on demand) and an export commit an open batch first, so
   a batched run anchors and exports exactly what the same commands one by one do: the in-process
   core decides in batches and replays its journal one input at a time (`REQ-RT-3`). Without it
   a checkpoint taken inside a batch covered only the commit before the batch, and a replay made
   a different one (found by `ops`' restore drill, a flaky `ac_k5_1` on #142).
3. **A failed batch commit loses the batch.** The in-process core stops and replays its journal
   on the next start (`REQ-RT-3`); every input of the batch answers `Ambiguous` and is retried
   under its key (`INV-18`). With a test fault armed, the inputs go one by one as before.
4. **`FileStore`'s format moves forward only.** A file written before `T-031-19` holds every node
   and reads the same under this build (deep nodes are computed, not read); a file written by
   this build lacks the deep nodes and cannot be read by an earlier one. No production book
   exists.
5. **`Ledger::begin_batch` and `end_batch` join the reviewed write surface** (`REQ-CS-5`): they
   group the kernel's commits and write nothing themselves.
