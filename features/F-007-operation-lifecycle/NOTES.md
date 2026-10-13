<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-007 — implementation notes

Written while implementing `F-007` (issue #7), 2026-09-23. The plan's proposals were followed as
accepted: the deterministic operation reference (§7) and a per-kind default deadline in deployment
configuration (§8). Where the plan was silent, the more conservative behaviour was chosen (refuse
rather than allow, keep reserved rather than release); each such choice is listed here for review.

## What landed

| Where | What |
|---|---|
| `crates/ledger-core/src/lifecycle/table.rs` | `State`, `Trigger`, `Origin`, `KindClass`, `PostingRule`, `Edge`, `TRANSITIONS` (23 rows), `ENTRY`; the SPEC-sync tests (T-007-01) |
| `crates/ledger-core/src/lifecycle/next.rs` | `next()`, `Facts`, `Decision`, `Lifecycle`, `StateClass`, `Shown`, `past_deadline()` (T-007-02, T-007-10) |
| `crates/ledger-core/src/lifecycle/model.rs` | The model check: soundness, completeness, reachability, absorption, no posting kind twice (T-007-03) |
| `crates/ledger-core/src/lifecycle/store.rs`, `state.rs` | The operation store and its rules, `OperationStore` (T-007-04) |
| `crates/ledger-core/src/lifecycle/keys.rs`, `ledger/requests.rs` | Client keys, operation references, request digests, transition keys, notification identities (T-007-05) |
| `crates/ledger-core/src/ledger/lifecycle.rs` | The engine: `commit_plan`, replay, `fire`, `tick`, the review queue, `oldest_open_age`, `resume` (T-007-05 to T-007-08) |
| `crates/ledger-core/src/ledger/reservations.rs` | `reserve`, `capture`, `release` as lifecycle operations; `reserve_until` |
| `crates/ledger-core/src/ledger/tests/lifecycle.rs` | Engine tests: fault injection, the replayed posting, deadlines, `resume`, `INV-23` |
| `crates/ledger-core/tests/lifecycle_walks.rs` | 10 000 random walks, each step applied twice (T-007-09) |
| `crates/conformance/src/cases/f007.rs` | Every `F-007` case, the 23 edge witnesses, and the walks (T-007-09) |

## Amendment 0006 (ruled 2026-09-29)

The owner ruled on amendment 0006 on 2026-09-29, and the table follows §5.6 as amended (SPEC
v0.4.0). What changed here:

- **(a) One constant, rows by trigger.** `lifecycle::table::TRANSITIONS` is still the only source
  of edges, now 38 rows over 28 pairs: a pair whose "When" cell names several conditions has one
  row per trigger, and every row of a pair appends the same kind of posting (a test checks it). The
  store checks the row `(from, to, trigger)`, not only the pair. The `exceptional` flag is gone.
- **(b) No `Reversed`** (items 1 and 4). The state (code 13), `Trigger::ApprovedReversal` (code 19)
  and `Resolution::Reverse` are withdrawn; their codes are never reused. Nothing leaves a terminal
  state: `INV-21` holds at the store with no exception, and the known gap that pinned
  `Settled → Reversed` is replaced by `inv_21_no_row_leaves_a_terminal_state__store`. A reversal is
  an operation of its own, `KindClass::Reversal`, settled by `Trigger::ApproveReversal`
  (`Accepted → Settled`, one `Reversal` posting; `F-013`).
- **(c) Deadlines** (item 5). `past_deadline()` fires `DeadlinePassed` → `Needs-Review` from every
  non-terminal state but `Needs-Review` itself; terminal states carry none. `Received`'s deadline is
  the decision bound: past it the operation moves and is reported as an incident too. `Needs-Review`
  keeps its service level; past it the operation stays and the queue shows it overdue. `resume`
  stays the operator's forward path before a deadline passes.
- **(d) A proved rejection** (item 3): `Trigger::ProvedRejection` releases an outbound leg from
  `Reserved`, `Submitted`, `Observed`, `Orphaned` or `Expired` when `Facts::failure_proven` holds
  (`F-010` derives it).
- **(e) An unproven release** (item 11): `Trigger::UnsafeReleaseRefused` moves an outbound leg from
  the same five states to `Needs-Review`, appending nothing; `release` takes it whenever it answers
  `ERR-ReleaseUnsafe`, with the refusal on the move's notification. A repeat, with the operation
  already in `Needs-Review`, is a no-op (`INV-18`).

## Decisions taken during implementation

### Store

1. **The operation store is part of the service store, not of `F-001`'s leaf stores.** The plan
   says the in-memory and file stores of `F-001` implement `OperationStore`. Those stores hold
   kernel leaves only. `F-003` put every other record in the service store (`state::State`), in
   memory on both in-process targets, with its durable home in `F-005` and `F-006` (`F-003`
   decision 1). The operation store follows: `State` implements `OperationStore`, its writes are
   `Write::Op`, and one batch covers a transition, its reservation-index changes, window charges
   and attribution records. The store-layer cases run on it through `Port::ServiceStore`, on both
   targets. `F-005`/`F-006` give it a durable form. The kernel half of a transition (the posting,
   its sequence number, its `external_id`) is on both `F-001` stores.
2. **Store rules.** The rules are compare-and-set on `from`, table membership, terminal refusal,
   a required deadline and a unique key, as planned. The store also refuses a posting kind the
   pair's book effect does not allow (`REQ-OP-1` at the store). It refuses a key that is not the
   derivation of (operation, from, to, evidence). A repeated key is a no-op in a raw batch too. The
   store also keeps the deadline index, the open-since index, the clock's high-water mark (which
   never goes back), the outbox and the client-key entries.

### One commit (T-007-06)

3. **Service store first, kernel last.** `commit_plan` reads the posting sequence numbers the chain
   will take from the book (global and gapless; this process is the only writer). It writes them
   into the records, applies every service-store write as one **undoable** batch, then applies the
   one kernel step, and undoes the batch if the step is refused or faults. So "a fault injected
   between any two writes leaves nothing committed" holds for every point: each write of the batch,
   and before the kernel step. The kernel step is last and atomic itself. A kernel step that
   applied with other sequence numbers than predicted is unreachable with one writer; it is reported
   as a fault and not undone, because the kernel step cannot be taken back. In process, the service
   store is memory, so the pair is durable together only under the Agent's single commit (`F-005`).
4. **`external_id` stays `F-001`'s.** The plan stamps each transfer with
   `H("aledgent/posting/v1", op, key, index)`. `F-001` already derives it from
   `(operation, kind, ordinal)` and pins that with golden vectors. The model check shows no posting
   kind repeats on any path through §5.6, so `(operation, kind, 0)` is unique per posting. The
   kernel therefore refuses a replayed posting for ever (`REQ-KER-6`, tested). Switching to the key
   would change `F-001`'s vectors for no gain.
5. **One notification per transition**, including `Received → Accepted`. Its identity is
   `H(op, transition key)`, and it carries the state, its class, the outcome where terminal, and the
   refusal for a move into `Rejected`. The outbox is in the service store; `F-005` makes it durable
   and `F-009` delivers it.
6. **The audit record is the transition record.** It holds the key, from, to, trigger, evidence,
   posting kind, posting sequence numbers, authorities and time. `F-003`'s `OperationRecord`, and
   `F-004`'s `PutTransition` for capture and release, stay as the command's attribution record.
   They are not folded into the operation record. `F-013` builds the audit store from both.

### Keys and repeats (T-007-05)

7. **Every command is idempotent on its operation reference.** Each command's request has a
   canonical digest (`ledger::requests`, public for `F-009`). It covers the command, the initiating
   identity, whom it acts for, and every field that changes what the command does, never the time
   or the credential. The first acceptance stores `(op, initiator, surface, digest, reply state)`.
   A repeat of the same request by the same principal returns the original outcome, rebuilt from
   the records: every identifier a command returns was minted from its reference. Anything else
   under that reference is `ERR-ClientKeyReused`. This replaces `F-003`'s blanket
   `ERR-ClientKeyReused` and closes `AC-A1.3`.
8. **A refused command records nothing.** A command refused synchronously (validation, authority,
   the kernel) writes no operation, so a corrected retry with the same key is decided afresh. The
   `Received → Rejected` edge is for a command durably received first (`F-009`). The conformance
   port writes it, and its witness takes it.
9. **Capture and release repeats come from the transition key.** Their evidence is the request's
   digest. An identical repeat derives the same key and returns the original outcome, which closes
   the repeat half of `AC-C2.1`. A *different* capture or release of an ended reservation is
   `ERR-OperationTerminal`, as `F-004` had it: it is another transition of a terminal operation,
   not a reuse of a client key.
10. **`issue`, `redeem` and the commands that move no value** (identities, accounts, grants) get
    the repeat rule but no lifecycle record. They are decided and complete at once. §5.6's
    `Accepted → Settled` allows `Transfer` only, and which lifecycle kind issuance is belongs to
    `F-010`/`F-013` (amendment 0006 item 7 asks for kinds for the rest). `Accepted` replies carry
    `status: None` for them.
11. **The surface of a request entry is `Command`** in process: the library does not know which
    surface a command arrived on. `F-009` passes it.

### Deadlines (T-007-07)

12. **Configuration.** `LifecycleConfig` travels in `Genesis`: the deployment's id (for operation
    references), the decision bound of `REQ-CS-7`, per-kind default deadlines, the review service
    level, and `max_fires_per_tick` (256 by default). The in-process harness configures a default
    for reservations only (1 000 time units), so `ERR-DeadlineRequired` is reachable for every
    other kind. `reserve` takes the default. `reserve_until` takes the command's deadline.
13. **One deadline per operation.** It is set at acceptance and kept across states. `Received`
    carries the decision bound. `Needs-Review` takes the time it was entered plus the review service
    level. A terminal state has none.
14. **The tick.** It walks the index of deadlines not yet fired, in order, at most
    `max_fires_per_tick`. A due deadline either fires its edge, or marks the operation overdue,
    which takes it out of the index. So a stuck operation never blocks the walk, and repeated ticks
    fire nothing twice. A state change clears the mark: an overdue `Reserved` handed off to
    `Submitted` with the same, passed, deadline moves to `Needs-Review` at the next tick.
15. **`tick` trusts the time it is given.** That is `DEC-6`: time is an input. The high-water
    mark means a `now` too far ahead would stick: later, truthful ticks below it are ignored, and
    deadlines would fire early. `tick` is therefore not a principal's command. Its only caller is
    the runtime's scheduled work (`REQ-RT-5`: the ticker of `F-009`, the Agent's timer of `F-005`),
    which must hand it a time from the host clock. `OQ-36` is untouched.
16. **The review queue** holds every `Needs-Review` operation, every open operation past its
    deadline, and every flagged one. Each entry names the owner and the time it must be resolved
    by: its deadline in `Needs-Review`, otherwise its deadline plus the service level. The owner is
    the operator's organisation identity. Nothing else can own a queue in V0 (`DEP-8`), and `F-013`
    may route.
17. **An unsafe release moves the operation to `Needs-Review`** (amendment 0006 item 11; it was a
    flag while the item was open). The refusal and the move are recorded together; nothing is
    released. `AC-C3.2` is a case again.

### Resume and kernel refusals (T-007-08)

18. **`resume` is the operator's.** It needs the operator's policy authority and a non-empty
    justification (`ERR-JustificationRequired` otherwise), and it takes no evidence. It
    re-evaluates, in table order, the forward rows of the current state whose trigger is
    `DeadlinePassed`, `ReleaseSafe` or `AppendSettling`, and applies the first that moves. These are
    the rows whose facts aledgent holds and whose posting the engine builds. Evidence-driven rows
    wait for `F-010`'s facts, and `Transfer`, `Issue` and `Reversal` rows wait for their feature. A
    release needs the operator's authority over the payer, which does not exist over owned custody
    (`REQ-AU-6`, `ERR-UnknownAccount`). This is the plan's Open point made concrete: nobody may
    release an owned-custody reservation past its deadline, except its holder.
19. **A kernel refusal on an edge that appends a posting moves nothing** (`Stay(KernelRefused)`).
    For an operation with an external leg, it also flags the operation into the review queue
    (`INV-23`). Only the edges into `Rejected`, before any balance effect, are taken on a refusal
    alone.
20. **The generic engine is crate-private.** `fire` takes evidence facts as input, so it must never
    be reachable by a principal (`INV-11`). `receive`, `arrive` and `fire` are `pub(crate)`, and
    are exposed as `port_receive`, `port_arrive` and `port_fire` only under `ledger-core`'s
    `conformance-ports`. No production crate may enable that feature (the existing CI check covers
    it).

### Guards `next()` chose where §5.6 is loose

21. `Reserved → Released` for an outbound leg requires proof that it cannot execute
    (`non_inclusion_proven`); for a reservation with no external leg it is always safe.
    `Orphaned → Submitted` requires an originated outbound leg whose expiry has not passed.
    `Orphaned → Released` on `InboundWithdrawn` requires an unoriginated inbound arrival.
    `Accepted → Settled` on `ApproveReversal` requires a reversal operation, a justification and
    two approvers (`REQ-OS-3`).
    `Needs-Review → Settled` appends `Capture`, `Redeem` or `Issue` by kind class, and needs a
    reservation for the first two. When the facts point at another row (for example, finality
    without a canonical match), `next()` stays rather than choosing one.
22. `Facts` carries a `Resolution` for `OperatorResolve`, because the trigger is fieldless and has
    two targets. The model check enumerates it: 2 × 5 × 2¹⁰ × 2 fact combinations, about five
    million calls.

## Conformance (T-007-09)

- Run cases in `crates/conformance/src/cases/f007.rs`, all green on both in-process stores:
  - `INV-15` at the store and at the service;
  - `INV-18` at the store, the random walks, and the 38 edge witnesses;
  - `INV-21` at the store: two checks, no gap (amendment 0006);
  - `INV-23` at the service;
  - the six errors, and `ERR-Internal` (`REQ-ER-2`, through the `port_fault_next_commit` port);
  - `AC-A1.3`, the repeat half of `AC-C2.1`, `AC-C3.2`, `AC-C4.1` and `AC-C4.2`.
- The **edge witnesses** `t_5_6_<from>_<to>__service` (with `_<trigger>` where a pair has several
  rows) are registered under `INV-18`, tracing
  `REQ-OP-1` and `REQ-OP-3`; §5.6's rows have no catalogue id of their own. Each witness reads the
  posting's kind back from the book's own delta log.
- The suite's walk case runs 200 walks per target (`CONFORMANCE_F007_WALKS` changes it).
  `ledger-core`'s `tests/lifecycle_walks.rs` runs 10 000 walks, as 400 books of 25 walks on
  `MemStore`, in about six seconds (`F007_WALKS` changes it). They cover 23 of the 28 pairs. The
  other five are the two into `Rejected`, `Received` and `Accepted` to `Needs-Review` (decided at
  once, before any deadline), and `Accepted → Settled`, whose postings only `F-008` and `F-013`
  build; the witnesses take those. Since amendment 0006 the walker leaves the rows to
  `Needs-Review` and `Released` to ticks and releases three draws in four, gives deadlines up to
  240 and walks up to sixteen steps: with more exits from every state, walks otherwise stop before
  `Final` or a second `Submitted`.
- **Assurance:** at `m0` the declaration counts `INV-15` and `INV-18` as enforced, and `INV-21` at
  the store now that no row leaves a terminal state.
- `F-004`'s `INV-14` case asserted that a repeated release of a released reservation is
  `ERR-OperationTerminal`. It now asserts that for a different command only; an identical repeat
  returns the original outcome.

## Deferred

- `TODO(F-009)`: the durable `Received` log and the ticker process; passing the surface and the
  client key; returning a derived client key in the reply.
- `TODO(F-010)`: outbound and inbound commands on the engine, and the evidence facts
  (observation, finality, canonical match, expiry, non-inclusion) that `fire` takes. `INV-23`'s
  observation-side case.
- `TODO(F-008)`: transfers as `Accepted → Settled` with their `Transfer` posting.
- `TODO(F-013)`: `resume` and operator resolution on `CTR-OPS` with their authority and audit;
  the `Reversal` posting; the audit store over the transition records.
- `TODO(F-011)`: `operation(reference)` on the query surface, with the state's class.
- `F-005`, `F-006`: the durable form of the operation store and the outbox, and the Agent's single
  commit around both stores.
