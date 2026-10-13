<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-007 — Operation lifecycle

> **Amendment 0006 (ruled 2026-09-29).** This plan was written against SPEC v0.3.3's §5.6: thirteen
> states and 23 rows, with `Reversed`. The amended table has twelve states and 28 pairs (38 rows by
> trigger), no row out of a terminal state, a deadline row to `Needs-Review` from every non-terminal
> state, a proved rejection that releases, and an unproven release that goes to review. Its open
> points on `Reversed`, deadlines and `AC-C3.2` are ruled. `NOTES.md` and `T-007-11` record what
> changed.


| | |
|---|---|
| Milestone | M-1 — The book, in a process |
| Track | A |
| Home | `crates/ledger-core` |
| Depends on | `F-001`, `F-003`. `F-004` for the edge tests that append a posting. `DEP-2`, `DEP-10` (planned meanwhile, not blocking) |
| Unblocks | `F-004`, `F-008`, `F-010`, `F-011`, `F-012`, `F-013`, `F-005`, `F-009` |
| Realises | `DEC-1`, `DEC-6` (core side), `DEC-7` (the enqueue half) |

## Goal

Every operation has one record with one state. That state changes only along the thirteen-state table of
SPEC §5.6, and each change is idempotent. Terminal states absorb. Every non-terminal record carries a
deadline, and deadlines advance only from a time the core is given, never from a clock it reads. Two
idempotency keys exist and are never confused: the caller's client key and the service's operation
reference. A test enumerates every state, trigger and fact, and shows that no transition outside the
table can happen.

## Spec traces

- `REQ-OP-1` — the book-effect table decides which posting each edge may append.
- `REQ-OP-2` — capture and release take the reservation's identity, never a caller's amount (with `F-004`).
- `REQ-OP-3`, `INV-18` — a transition is idempotent on (operation, from, to, evidence).
- `REQ-OP-4`, `INV-21` — `Settled`, `Released`, `Rejected`, `Reversed` are terminal and absorbing.
- `REQ-OP-5`, `INV-15`, `ERR-DeadlineRequired` — every non-terminal state has a deadline and an operator path.
- `REQ-OP-6`, `REQ-OP-9` — silence never releases. `Needs-Review` is reachable and queued.
- `REQ-OP-7` — non-terminal replies are labelled non-terminal and non-refunding.
- `REQ-OP-8`, `AC-C4.2` — age of the oldest non-terminal operation.
- `REQ-OP-10`, `REQ-OP-11`, `ERR-ClientKeyReused`, `AC-A1.3` — the two keys.
- `INV-23` — a kernel refusal is never, alone, a terminal outcome for an independently observable leg.
- `AC-C4.1` — a reservation past its deadline appears in a queue with an owner and a service level.
- Supporting: `REQ-RT-1`, `REQ-RT-5`, `REQ-RT-7`, `REQ-EV-1`, `REQ-EV-3`, `REQ-PO-2`, `REQ-KER-6`,
  `REQ-RC-1`, `ERR-InvalidTransition`, `ERR-OperationTerminal`, `ERR-UnknownOperation`.

## Scope

**In:** state and trigger types; the transition table as data; the pure transition function; the
operation record store trait with its store-level guards; client keys and operation references; the
effect list of a transition (postings, notification, audit record) committed as one unit; the global
posting sequence; deadlines, `tick(now)`, the review queue and the oldest-age metric; the model check.

**Out:** reserve, capture and release postings (`F-004`); transfers (`F-008`); observations, finality
and outbound instructions (`F-010`); operator resolve, reversal and the audit store (`F-013`); the durable
`Received` log and the ticker process (`F-009`); the Agent binding and outbox storage (`F-005`).

## Design

**1. Types.** `State` is a fieldless enum of the thirteen states. `Trigger` is a closed, fieldless enum,
one variant per "When" cell of §5.6: `Decide`, `RefuseBeforeEffect`, `SettleWithoutReservation`,
`PostReservation`, `Capture`, `ReleaseSafe`, `HandOffAcknowledged`, `ProvisionalObservation`,
`ExpiryPassedUnobserved`, `DeadlinePassed`, `FinalityAndCanonicalMatch`, `CanonicalMismatch`,
`ReincludableAgain`, `InboundWithdrawn`, `AppendSettling`, `NonInclusionProven`,
`NonInclusionUnprovable`, `OperatorResolve`, `ApprovedReversal`. Each has a `const ALL: [_; N]`, and a
test `match`es every variant with no wildcard, so adding one without updating `ALL` fails to compile.

**2. The table is data.** One constant is the only source of edges:

```rust
pub struct Edge { pub from: State, pub to: State, pub on: Trigger, pub posting: PostingRule }
pub const TRANSITIONS: [Edge; 23] = [ /* one row per (from, to) pair of §5.6 */ ];
pub const ENTRY: [(State, Origin); 2] = [(Received, Origin::Command), (Observed, Origin::UnoriginatedInbound)];
```

The 23 rows are the (from, to) pairs of §5.6 once its "or" cells are expanded. `PostingRule` encodes the
book-effect table: `None`, `Reserve`, `SettleOneOf{Transfer, Capture, Redeem, Issue}`,
`ReleaseIfReserved`, `Reversal`. A test reads `SPEC.md` §5.6 through `include_str!`, parses the From and
To columns, and asserts the parsed pair set equals the `TRANSITIONS` pair set. A spec amendment that
changes the table then fails the build until the constant follows.

**3. One function moves state.** `fn next(s: State, t: Trigger, f: &Facts) -> Decision`, with
`Decision = Move(State, PostingRule) | Stay(Reason) | Refuse(ErrorCode)`. `Facts` is a small struct of
booleans and enums that the caller computes from the record, the evidence and the book: origin, kind
class (no reservation / reservation only / outbound / inbound), reservation exists, deadline passed,
expiry passed, finality passed, canonical match, non-inclusion provable, kernel accepts, operator
justification present, two distinct approvers. `next` looks up the row for `(s, t)`, checks its guard
over `Facts`, and returns `Move` only for a row in `TRANSITIONS`. There is no other writer of `State`: the
field is private and has no setter.

**4. Terminal typing.** A record's lifecycle is
`enum Lifecycle { Open { state: State, deadline: Time }, Closed { state: State, at: Time } }`. `Open`
cannot be built without a deadline, and `Closed` only for the four terminal states. So `INV-15` holds by
construction in the service. The store holds it again (point 5).

**5. Store trait (`INV-18`, `INV-21`, `INV-15` at the store layer).** `OperationStore::apply_transition(
op, from, to, key, effects)` must, inside one storage transaction: compare-and-set on `from`; refuse any
`(from, to)` not in `TRANSITIONS`; refuse any write to a record whose state is terminal; refuse an `Open`
record with no deadline; and insert `key` into a unique set. A second call with the same `key` returns the
first result and writes nothing. `key = H("aledgent/transition/v1", op, from, to, evidence_digest)`. For
a deadline trigger, the evidence digest is the deadline value, not the tick time, so repeated ticks
produce one key. The in-memory and file stores of `F-001` implement the trait. `F-006` implements it at
scale.

**6. Effects commit as one unit (`REQ-RT-1`).** A transition returns an `Effects` value: zero or one
posting chain, one notification, one audit record. The commit assigns each posting the next value of the
global, gapless posting sequence (`REQ-PO-2`). It stamps each kernel transfer with
`external_id = H("aledgent/posting/v1", op, key, index)`, so the kernel refuses a second copy permanently
(`REQ-KER-6`). The notification
identity is `H(op, key)`, with no timestamp (`REQ-EV-3`). Postings, the state change, the evidence
reference, the outbox item and the transition key commit together or not at all.

**7. Two keys (`REQ-OP-10`, `REQ-OP-11`).** The client-key store maps
`(initiating principal, surface, client key)` to `(request digest, operation reference, canonical
reply)`. This domain is the one `DEP-10` plans until `OQ-6` is answered. Same key and same digest: return
the stored reply. Same key and a different digest: `ERR-ClientKeyReused`. No key: derive
`H("aledgent/ck/v1", canonical request without transport fields)` and return it in the reply. No
timestamp or retry counter enters any key. **Proposal:** the operation reference is
`H("aledgent/opref/v1", deployment, principal, surface, client key)`. It is then deterministic, so
ingress can mint it at `Received` (`DEC-5`) and the core can recompute and check it. It needs no
randomness inside the boundary (`REQ-RT-7`). It is opaque and never parsed. An unoriginated inbound
arrival takes `H("aledgent/opref/inbound/v1", coordinate)`. *Alternative:* a counter in core state,
mapped at ingress by a lookup table. It is simpler to read but needs a round trip before `Received`.
The derivation functions live in `ledger-core` and `F-009` calls the same code.

**8. Deadlines from injected time (`DEC-6`, `REQ-RT-5`, `REQ-RT-7`).** The library never reads a clock.
`tick(now)` is a command. The core keeps a clock high-water mark and ignores a `now` below it. A
deadline index `BTreeSet<(Time, OperationReference)>` is walked in order up to `now`, with at most
`max_fires_per_tick` transitions per call. The rest wait for the next tick, so one invocation stays in
the `F-002` budget. On a due deadline, `next(s, DeadlinePassed, …)` moves `Submitted` and `Observed` to
`Needs-Review`. (`Expired → Needs-Review` is driven by `NonInclusionUnprovable`, not by the deadline.)
For every other non-terminal state the table has no deadline edge, so nothing moves. The record joins the **review queue** instead. The queue is a projection: every
`Needs-Review` record, plus every `Open` record past its deadline. Each entry carries an owner and a
service-level time from deployment configuration (`AC-C4.1`, `REQ-OP-6`). `oldest_open_age(now)` reads
the minimum of a second index on `opened_at` (`REQ-OP-8`). The deadline is set at acceptance from the
command. A deployment default per operation kind fills a missing one. No command and no default:
`ERR-DeadlineRequired`. `Received` carries the decision bound of `REQ-CS-7` as its deadline. Passing it
raises an incident, not a transition.

**9. Resume path (`REQ-OP-5`, `REQ-RC-1`) and `INV-23`.** `resume(op)` re-evaluates the forward rows of
the current state with fresh `Facts`. It never adds an edge. `F-013` exposes it on `CTR-OPS` with its
own authority and justification. When the kernel refuses a settling posting on an edge driven by
evidence (`Final → Settled` for an inbound leg), `next` returns `Stay(KernelRefused)`. It never returns
`Rejected`. The record stays open and queued (`INV-23`). `Accepted → Rejected` is the only edge a
kernel refusal can take, and only for a kind class with no external leg observed yet.

**10. Caller view (`REQ-OP-7`).** `StateClass::of(State)` returns `Terminal`, `OpenNonRefunding` or
`AwaitingHuman` (for `Needs-Review`). `F-011`'s `operation(reference)` and every notification show the
class next to the state. Until `OQ-35` is answered, every state is shown to callers.

## Test plan

- **Model check (exit criterion), unit level, no kernel.**
  (a) *Soundness:* for every `State × Trigger × Facts` (13 × 19 × 2^12 cases, about one million calls,
  under a second), every `Move(s')` has `(s, s')` in `TRANSITIONS`.
  (b) *Completeness:* every row is produced by at least one case.
  (c) *Reachability:* a breadth-first search from `ENTRY` over (state, facts), with `next` as the
  successor function, reaches exactly the 23 pairs and all 13 states, and no other pair.
  (d) *Absorption (`INV-21`):* from a terminal state `next` never moves, except the row flagged in the
  Open points.
  (e) *Store guard:* all 169 `(from, to)` pairs are sent straight to `apply_transition`. Exactly the
  table's pairs are accepted.
  Hand-written, with no external model-checking crate. The space is small enough to enumerate.
- **Edge witnesses:** one named test per row, `t_5_6_<from>_<to>`, driving the real library with the
  `F-001` kernel and `F-004` postings. Each asserts the posting kind of §5.6's book-effect table.
- **Property tests:** random walks of commands, evidence and ticks, each step applied twice. After every
  step: `INV-15`, `INV-18` (the second apply changes nothing, and the composite root is unchanged),
  `INV-21`, and every recorded pair is in the table.
- **Acceptance tests:** `AC-C4.1`, `AC-C4.2`, the key half of `AC-A1.3`, and the repeat half of
  `AC-C2.1`. `INV-23`: a kernel refusal injected at `Final → Settled` leaves the record open.
- These tests go into the `F-015` conformance suite under the invariant ids they name.

## Exit criterion

Every transition in §5.6 has a named test. The model check shows by enumeration that no other transition
is reachable, and the store refuses every pair outside the table. The SPEC-sync test passes.
`INV-15`, `INV-18`, `INV-21`, `INV-23`, `AC-C4.1` and `AC-C4.2` pass in the conformance suite.

This feature also closes two criteria that earlier features cannot test alone: `AC-A1.3` from `F-003`
(a repeated client key returns the same account) and the repeat half of `AC-C2.1` from `F-004`.

## Open points

- **`Settled → Reversed` against `REQ-OP-4` and `INV-21`.** §5.6 lists the edge and says no other
  transition exists. `REQ-OP-4` and `INV-21` say `Settled` is terminal and absorbing. Both cannot hold.
  Meanwhile the edge is encoded as §5.6 states, flagged `exceptional`. The absorption test excludes only
  that row, marked `[PENDING OWNER RULING]`. An amendment is needed.
- **`AC-C3.2` against §5.6.** The AC moves an operation to `Needs-Review` when a release is unsafe. The
  table allows `Submitted → Needs-Review` only when the deadline has passed. Meanwhile an unsafe release
  is refused with `ERR-ReleaseUnsafe`, the state is unchanged, and the record joins the review queue. The
  `AC-C3.2` test is marked pending.
- **`Received` and `INV-15`.** §5.6 says no deadline has started at `Received`. `INV-15` requires one on
  every non-terminal operation. Meanwhile `Received` carries the `REQ-CS-7` bound as its deadline. This
  is a reading, for the owner to confirm.
- **Operator paths for `Accepted`, `Reserved`, `Final`, `Orphaned` past deadline.** These have no edge
  to `Needs-Review`, and `resolve_operation` needs `Needs-Review`. Meanwhile `resume` is their operator
  path (`REQ-OP-5` names a resume path; §8.1 has no command for it). A `Reserved` operation on an
  owned-custody account has no operator who may release it.
- **`Needs-Review → Reversed`** has no settled posting to reverse. The plan assumes it reverses the
  `Reserve` posting (`REQ-PO-3`). `F-013` to confirm.
- **Proposal:** the deterministic operation reference (point 7). **Proposal:** a per-kind default
  deadline in deployment configuration (point 8). Alternative: every command carries one.
- **`DEP-2`:** the ticker is a liveness dependency (`RISK-7`). **`OQ-36`:** injected time is the time
  used here. That is a plan choice, not an answer to the question.
