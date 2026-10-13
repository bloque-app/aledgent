<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-005 — Core Agent binding

| | |
|---|---|
| Milestone | M-2 — On VOS, end to end |
| Track | B |
| Home | `agents/core` |
| Depends on | `F-001`, `F-002`, `F-003`, `F-004`, `F-007`, `F-006` (witness path), `DEP-5` (**blocked**), `DEP-1` (through `F-006`), `DEP-2` (tick input only) |
| Unblocks | `F-009`, `F-016`, `F-017`, `F-018` |
| Realises | `DEC-1`, `DEC-4` (Agent side), `DEC-7`, `DEC-10`, `DEC-11` |

## Goal
The ledger-core library runs inside one VOS Local Agent on `saga/agents`. That Agent is the single writer
of the book. Each command commits entries, operation state, evidence, outbox items and the delta record
in one Agent transition. The Agent holds only roots and a small, bounded hot state. Leaves arrive per call
as a witnessed batch from `F-006`. The library's conformance suite passes through the Agent, and one
invocation stays inside the budget `F-002` measured.

## Spec traces
- `REQ-RT-1` — one commit boundary: postings, operation record, evidence, notification enqueue.
- `REQ-RT-3` — deterministic, replayable execution of the book's transitions.
- `REQ-RT-6` — blindings and openings enter as private inputs, never as state.
- `REQ-RT-7` — no clock, randomness or network I/O inside the boundary.
- `REQ-RT-8` — the Agent-state schema is kept small so upgrades go through `F-014` only when needed.
- `REQ-AR-3`, `NFR-8` — no settlement-layer key in the Agent.
- `INV-18` — transitions stay idempotent when the host retries an invocation.
- `INV-25` — one Agent, one book.
- `NFR-15` — completion, which the reservation decision is part of, measured on this host
  (amendment 0007; this line named `NFR-1`'s 50 ms, a misreading).

## Scope
**In:** the Agent crate; the Agent-state layout and its size guard; the command envelope; witness
verification inside the Agent; the private-input oracle; the time input; the outbox ring and the delta
ring; a host-side driver that builds each invocation and applies its result; running the conformance
suite through the Agent.
**Out:** the leaf store, witness builder, delta archive and openings store (`F-006`); command intake,
the received-log, the ticker and notification delivery (`F-009`); the settlement adapter (`F-016`);
replication (`F-021`); backups and restore drills (`F-018`); every ledger rule (`F-003`–`F-014`, in the
library).

## Design

**Target.** VOS `saga/agents` at `7bae926`, as `DEP-5` plans. The Agent profile is **Local**, which has
exactly one replica (`vos-agent-sdk/src/model.rs:468-481`; `DEC-10`). A Local Agent
cannot later widen to replicated, as the acceptance manifest says, so `F-021`
may need a re-genesis through `F-014`.

**VOS limits this binding lives inside** (from the VOS source at the pin unless marked):

| Limit | Value | Source | Effect here |
|---|---|---|---|
| Whole Agent state, all actors, all lanes, inline and rows | 4 MiB | `vos-agent-sdk/src/lib.rs:57`; enforced `vos/src/agent/actor_storage.rs:370-373` | Leaves live outside (`DEC-4`). Hot state has a budget below this |
| State shipped in and out on every invocation | whole image | `vos-agent-sdk/src/runtime.rs:775-812`, `:1058-1061` | Latency grows with hot state, not with the command |
| Inline fields per inner actor machine | 48 KiB | `vos/src/agent/execution.rs:28` | Hot records go in storage rows, not inline fields |
| Rows per lane | 16 384 | `vos/src/agent/standard.rs:42` | Caps outbox ring + delta ring + hot rows together |
| One storage row value or key | 64 KiB | `vos/src/actors/storage.rs:64` | One delta record per row. Sets the ceiling for `ERR-BatchTooLarge` (`F-008`) |
| Invocation message / reply | 16 KiB / 16 KiB | `vos-agent-sdk/src/runtime.rs:14-15` | Command plus witness must fit, or the witness needs another carrier |
| Inner machines per slice | 63 | `docs/architecture.md:67-69` | One actor. No deep call chains |
| Dispatch | serialised per Agent | `vos/src/agent/supervisor_lanes.rs:92`, `:146-148` | Single writer. Throughput is one invocation at a time |
| Cross-lane effects | not atomic | `docs/architecture.md:105-106` | Everything lives in the **Linear** lane |
| Outbound cross-Agent call | none in the ABI | `vos-agent-sdk/src/runtime.rs:1040-1057` | Outbound traffic is an outbox that workers pull (`DEC-7`) |
| Idempotency window | 256 results, 16 MiB (`main`) | `docs/http-ingress.md:73-78` | Permanent dedup is the library's, on client-key leaves and kernel `external_id` |

**Actor shape.** One actor, `aledgent-core`, declared with `#[actor(agent)]` and `#[messages(agent)]`,
the only macro form the Agent model uses. The library is linked into it. Every mutating
command is a `#[msg(linear)]` handler. Reads of hot state are query handlers. The `clerk-*` actors are
not reused: they are unported and one transfer per call. `clerk-witness` is reused for
the witness format.

**Agent state (hot state).** Only fixed-size or bounded data:
- the kernel composite root and its six sub-roots; the service root (`F-006` Proposal);
- the posting sequence, the log head hash, the last archived sequence, the last checkpoint sequence;
- the asset registry (small, bounded);
- the outbox ring (`DEC-7`) and the delta ring (below);
- per-deadline-bucket counts of open operations, so a tick can prove it saw every due operation;
- the age of the oldest non-terminal operation (`REQ-OP-8`).
Anything whose count grows with holders or operations is a leaf in `F-006`. **Proposal:** a hot-state
budget `H`, set from `F-002`'s 1 / 2 / 4 MiB numbers. The Agent refuses to commit a transition that would
push state over `H`, well before VOS's own 4 MiB refusal.

**One command, end to end.**
1. The driver (host side, below) receives a command from `F-009`.
2. It runs the library natively against `F-006`'s read view to find the exact read set.
3. `F-006` builds a witness for that read set against the current roots.
4. The driver invokes the Agent: the command envelope (principal, credential proof, time input, client
   key, body) and the witness as public input; blindings and touched openings as private input.
5. The Agent checks the witness against its committed roots, builds a sparse state from it
   (`SuccinctTransitionWitness`), and runs the library over it.
6. On success it commits one transition: new roots, sequence, operation record changes, evidence,
   outbox items, the audit records of `F-013`, and one delta record in the delta ring. `REQ-RT-1` holds
   because all of these are in one Linear-lane state image.
7. The reply carries the outcome and the new sequence. The driver pulls the delta, hands it to `F-006`,
   and then answers `F-009`.

A read outside the witness fails closed: the succinct witness panics on an unproven read. Nothing commits.

**Principal.** The Agent accepts invocations only from the driver's VOS principal, checked with
`ctx.principal()`. The aledgent principal and its credential proof travel in the
envelope and are checked by the library (`F-003`, `REQ-PN-1`). The VOS principal is transport, not
authority.

**Private inputs.** CipherClerk's `Oracle::next_blinding` is `unimplemented!()` upstream.
So the binding supplies its own `Oracle` impl. It reads blindings and the negativity openings of touched
accounts from the private input. On `saga`, private ingress keeps plaintext out of logs, snapshots and
state (notes/FIT §3). **Proposal:** blindings are derived outside the Agent from a core secret and the
entry id, so the openings store can be re-derived (see `F-006`, `F-014`). *Alternative:* random
blindings, with openings kept only in a sealed store.

**Time and randomness.** The library takes time as an explicit input. The envelope carries it. The Agent
also records `ctx.agent_observed_slot()` (Unix seconds, monotone) for diagnostics. Which
clock is authoritative is `OQ-36`. The binding does not choose. The source is configuration. A build
gate forbids `std::time`, `rand` and network crates in `agents/core`'s guest target (`REQ-RT-7`).

**Delta ring and outbox ring.** Both are Linear-lane rows. The driver reads deltas with a query and
calls `ack_archived(sequence, log_hash)` once `F-006` has archived them. Only then does the Agent trim.
Workers drain the outbox the same way (`DEC-7`). When either ring is full, the Agent refuses new
commands. The command stays `Received` at ingress, and that is an incident past its bound (§5.6,
`REQ-CS-7`). No new error code is needed.

**Failure mapping.** A witness whose root does not match: the driver refreshes and retries. An HTTP 503
or 504 does not prove non-execution (`docs/http-ingress.md:78-80`). So after an unknown
outcome the driver first queries the Agent for the operation reference, and resubmits only if it is
absent. Library refusals come back as the named `ERR-*` (`REQ-CS-4`).

**Upgrade.** `saga` has no state-migration hook. Schema fingerprints fail closed (notes/FIT §3,
`RT-8`). The Agent-state schema is therefore small and versioned. Leaf-store schema changes are a
projection rebuild in `F-006`, not an Agent upgrade.

**If `DEP-5` answers `master`.** The library and the conformance harness do not change (`DEC-1`). The
binding does:
- Actor model: a root service with `#[actor]` / `#[messages]`, at most four application actors per root
  tree (`vos/src/service/mod.rs:152-157`). No `#[msg(linear)]`. Consistency is per root.
- State: the 4 MiB whole-state cap is new on `saga`. On `master`, storage rows
  sit beside the actor's state archive, and the service guest has a 4 MiB nested
  transition buffer (`services/vos-service/src/lib.rs:28-31`). `CommittedMap` keeps an
  SMT root per field and emits batch proofs, as `clerk-ledger` does for all six sub-trees. The leaves could then live in runtime storage, inside the commit boundary. `DEC-4` would
  be revisited, and `F-006` would shrink to history and indexes. `F-002` would re-measure on `master`.
- Limits: 8 192-byte message buffer (`vos/src/actors/lifecycle.rs:35`), 64 KiB slice and private input,
  256 KiB actor heap (`service/mod.rs:198-204`), 256 authenticated storage witnesses and 1 MiB per slice
  (`service/mod.rs:165-176`).
- Outbox: cross-root calls enqueue in the same `Transition`. `DEC-7` could use them.
- Tasks affected: `T-005-01`, `-02`, `-04`, `-07` are rewritten. `-03`, `-05`, `-06`, `-08`–`-10` keep
  their shape.

**What `F-002` must show before this is finalised.**
1. Latency of one mutating Local Agent invocation at 1, 2 and 4 MiB of state. That sets `H`. If even
   1 MiB leaves no room inside `NFR-1`'s 50 ms p99, `DEC-4` is revised under `RISK-2`. (Amendment
   0007: that budget was a misreading; the decision is part of completion, `NFR-15`.)
2. Kernel cost per posting inside the PVM, with and without the curve precompile. The estimate is 10–20M
   cycles without it. The precompile is a request to the zkpvm team, not a shipped dependency.
3. Witness size for a reservation and a transfer at 10k / 100k / 1M accounts, against the 16 KiB
   message. If it does not fit, the witness goes as private input, or the upstream ask in `DEP-1` grows.
4. The private-input size limit on `saga`. It is not documented. `main` allows 64 KiB.
5. The cost of verifying the witness inside the Agent: SMT paths of depth 128.

## Test plan
- **Conformance through the Agent.** The `F-015` suite runs against a host trait with two impls: in
  process, and through the Agent on a local `vosx` space. Every `INV-*` and `ERR-*` the library passes
  must pass here too (`NFR-14`).
- **`REQ-RT-1`, crash injection.** Kill the Agent host and the driver at each step of "one command, end
  to end". After restart, the operation record, postings, outbox and delta agree. No acknowledged
  operation is missing (`NFR-4`).
- **`REQ-RT-3`, replay.** Replay a recorded invocation stream from genesis. Every root and sequence must
  match.
- **`REQ-RT-6`.** Scan Agent state images, backups and every exported record for blinding and opening
  bytes. None may appear.
- **`REQ-RT-7`.** The build gate above, plus a test that the same inputs give the same transition on two
  runs.
- **`INV-18`.** Resubmit each command after a simulated unknown outcome. Exactly one state change.
- **Budget.** Record p50 / p99 of the reservation path at state size `H`, in `notes/`: an
  observation, part of completion (`NFR-15`, amendment 0007).

## Exit criterion
The library conformance suite passes through a Local Agent on `saga/agents` `7bae926`. One reservation
invocation, at the hot-state budget `H`, stays within the per-invocation budget derived from `F-002`, at
p99. The numbers are recorded in `notes/`. Not finalised until `DEP-5` is answered.

## Open points
- **`DEP-5` blocks finalisation.** The design above is for `saga`. The `master` delta is listed.
- **`DEP-1` and `RISK-2`.** If VOS plans out-of-image state, most of the witness path goes away.
- **Proposal:** hot-state budget `H`, with the Agent refusing before VOS's own 4 MiB refusal.
- **Proposal:** deterministic blindings from a core secret. It makes openings re-derivable only if every
  entry's plaintext amount is also kept, somewhere a storage reader cannot see (`DEC-11`). And it puts
  one long-lived secret at the centre of privacy. The owner should weigh it against a sealed store.
- **Proposal:** the host-side driver lives in `agents/core` as a separate binary. `DEC-15` names no home
  for it. *Alternative:* put it in `services/ingress`.
- **Proposal:** per-deadline-bucket counts in hot state, so a `tick` cannot silently skip a due operation.
  It touches `F-007`'s deadline model.
- **Witness carrier.** If a witness does not fit the 16 KiB message, the choice is between private input,
  which may weaken third-party replay under `REQ-RT-3`, and an upstream change. The owner decides after
  `F-002`.
- **Clock.** `OQ-36` stays open. The binding reads time as an input from a configured source.
