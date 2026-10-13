<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-010 — Evidence and settlement legs

| | |
|---|---|
| Milestone | M-1 — The book, in a process |
| Track | A |
| Home | `crates/ledger-core` (the simulated settlement layer is a `sim` feature of the same crate) |
| Depends on | `F-001`, `F-003`, `F-004`, `F-007` |
| Unblocks | `F-016`, `F-019` (end-to-end run), `F-018` (the third reconciliation axis) |
| Realises | `DEC-1`, `DEC-2` (settling from the holding position), `DEC-7` (instructions through the outbox), `DEC-9` (the simulator has the pallet's shape) |

## Goal

aledgent turns evidence from the settlement layer into operation states by itself. No caller can
assert an outcome. Value that arrives is credited only after two independent facts establish finality.
Value that leaves is reserved before any instruction exists, and leaves the reservation only at
finality. Every coordinate is applied once. An unknown outcome never releases anything. All of this is
proven against an in-process simulated settlement layer, before any chain is involved.

## Spec traces

- `REQ-IN-1`, `INV-12`, `AC-D3.2` — dedup on coordinate, applied at most once.
- `REQ-IN-2`, `INV-11`, `AC-D3.1` — aledgent derives status and ignores any claimed one.
- `REQ-IN-3`, `AC-D1.2`, `AC-E1.2` — `Final` needs finality passed **and** canonical match.
- `REQ-IN-4`, `AC-D1.3` — re-verify every cycle, `Orphaned` on mismatch, no reversal needed.
- `REQ-IN-5` — completeness accounting (the library side of `US-D2`).
- `REQ-IN-6`, `AC-D3.3` — one authoritative source. Others corroborate.
- `REQ-IN-7` — observed, final and settled stay distinct everywhere.
- `REQ-OP-1`, `INV-10`, `AC-D1.1` — no entry against the issuance counterparty before `Final`.
- `REQ-SO-1`, `INV-13`, `AC-E1.1` — no outbound leg without a reservation first.
- `REQ-SO-2`–`SO-4`, `AC-E1.5` — aledgent assembles instructions. Unknown is non-terminal.
- `REQ-OP-2`, `AC-E1.3`, `ERR-AmountMismatch` — the difference is posted explicitly.
- `REQ-SA-4`, `REQ-OP-9`, `AC-E1.4` — three dispositions. Release only on proof.
- `INV-23` — a kernel refusal is never, alone, the outcome of an observable leg.
- `AC-D1.4`, `REQ-SI-2`, `REQ-SI-3` — credit arrivals aledgent did not start.
- Supporting: `REQ-SI-1`, `REQ-SI-4`, `REQ-SI-5`, `REQ-SO-5`–`SO-7`, `REQ-SA-1`, `REQ-SA-6`, `REQ-SA-7`,
  `REQ-SL-3`, `REQ-SL-4`, `REQ-SL-7`–`SL-9`, `ERR-ObservationDuplicate`, `ERR-ObservationUnassociated`,
  `ERR-EvidenceInsufficient`, `ERR-UnsupportedLeg`, `ERR-ReleaseUnsafe`.

## Scope

**In:** the `CTR-SET` types inside the library; `record_observation` and re-verification reports;
association; two-fact finality; orphaning; inbound settling; `submit_outbound` and instruction assembly;
the mapping of the three dispositions onto §5.6; amount mismatch; the support declaration check;
completeness accounting; the simulated settlement layer and a test driver that plays the adapter.

**Out:** the real adapter, its signer, its light client and its inbox cursor (`F-016`); liquidity parking
and `AC-E3.*` (`F-016`); `AC-D2.*` against a real chain (`F-016`); the pallet (`F-019`); reserve postings
(`F-004`); the lifecycle table itself (`F-007`); operator resolution of `Needs-Review` (`F-013`).

## Design

**1. The simulated settlement layer (`sim` feature).** It mirrors the pallet of `DEC-9`: one sequenced
inbox per instance, with `Deposit { account_ref, asset, amount }` and
`Withdraw { operation_ref, to, asset, amount, outcome }` records and `Anchor` records. Each record has a
native coordinate `(position, record_digest)` (`REQ-SL-8`, `REQ-SL-9`). The simulator keeps a best head
and a finalised head that move apart, as Kreivo's do: the best head advances about every 0.6 s, and
finality follows from the relay chain with no upper bound. A test can fork the
unfinalised tail, which replaces records above a position. A test can also prune history older than a window, as the public
endpoints do beyond about 4,096 blocks. The window lets `REQ-SA-7` gaps happen.
Instructions carry an expiry position (`REQ-SL-4`). The simulator never includes one after its expiry.
It is deterministic and seeded, so every failing run can be replayed. **Proposal:** a `sim` feature
inside `ledger-core`. *Alternative:* a separate crate `crates/settle-sim`. That is cleaner to reuse in
`F-016`, but it adds a path that `DEC-15` does not list.

**2. `CTR-SET` types.** `Observation { source, coordinate, kind, payload, observed_finality,
operation_ref?, account_ref? }` (`REQ-SI-1`). There is no field for an outcome, a status or a verdict.
The `payload` is the layer's own bytes and error text, kept opaque. The library never branches on it
(`REQ-PO-4`). `CanonicalCheck { source, coordinate, finalised_through, still_matches }` is a separate
report of two facts about a coordinate already observed. The verdict returned to the submitter is one of
`Applied(state)`, `Ignored`, `AlreadyApplied`, `NotAssociated` (`REQ-SI-5`).

**3. Ingest.** `record_observation` runs in this order:
- The source must be the configured authoritative one. Otherwise the observation is stored as
  corroboration and causes no transition (`REQ-IN-6`).
- The coordinate is inserted into a unique set in the store (`INV-12`, store layer). A seen coordinate
  gives `ERR-ObservationDuplicate` with verdict `AlreadyApplied`, which is distinct from a failure
  (`REQ-SI-4`).
- Association: first by `operation_ref` (`REQ-SI-3`); else by `account_ref`, resolved through the
  account-reference index (`REQ-SL-7`); else `ERR-ObservationUnassociated`.
- For an originated leg: `Submitted → Observed`. For an unoriginated deposit: a new record enters at
  `Observed` (`REQ-SI-2`), with the operation reference derived from the coordinate (`F-007`).

The settling posting of a coordinate carries `external_id = H("aledgent/coord/v1", coordinate)`. A second
settlement of the same coordinate is then also refused by the kernel, so `INV-12` holds
at kernel strength as well as in the store.

**4. Two-fact finality (`REQ-IN-3`, `REQ-IN-4`, `REQ-SL-3`).** Each observation cycle, the adapter sends a
`CanonicalCheck` for every coordinate still `Observed`. `Observed → Final` needs
`finalised_through ≥ coordinate.position` **and** `still_matches`. Each is a separate field with no
default. `still_matches = false` gives `Observed → Orphaned`. The library has no confirmation-depth
setting at all, so finality cannot be approximated by counting later records. The check report is
idempotent through the `F-007` transition key. The key uses `(coordinate, finalised_through)`, not the
coordinate alone.

**5. `INV-10` by capability.** `Observed → Final` is the only code that can build a `FinalityWitness`.
The `Issue` and `Redeem` posting builders take one by value. No other path can write an entry against the
issuance counterparty for an external leg. A compile-fail test tries to build the witness elsewhere.

**6. Inbound settling.** `Final → Settled` appends `Issue`: debit the issuance counterparty, credit the
destination's available value, in the same commit as `Final`. This credits the account whether or not
aledgent started anything (`AC-D1.4`). If the kernel refuses (for example, the destination is closed),
the record stays `Final` and joins the review queue (`INV-23`, `F-007` point 9). Inbound `Orphaned` goes
to `Released` with no posting, because nothing was reserved or posted (`REQ-OP-1`, `AC-D1.3`).

**7. Outbound.** `OutboundInstruction::new` takes a `&ReservationHandle`. Only `F-004`'s reserve path can
make one, and it is bound to the operation reference. There is no other constructor (`INV-13`,
`REQ-SO-1`). The support declaration `(operation kind, asset)` is checked at `Received → Accepted`, before
any reservation, so an unsupported leg is refused at submission with `ERR-UnsupportedLeg` (`REQ-SA-1`).
`submit_outbound` assembles `{ operation_ref, asset, amount, destination, expiry_position,
reservation: true }` (`REQ-SO-5`, `REQ-SO-2`) and appends it to the outbox. The state stays `Reserved`
until the adapter durably acknowledges receipt. That acknowledgement is the `HandOffAcknowledged` trigger
of `Reserved → Submitted` (`REQ-SO-6`). After a restart the outbox delivers the same bytes under the same
identity (`REQ-SO-7`). The operation deadline must be later than the expiry plus a finality margin, so
expiry is always decided before the deadline. `submit_outbound` checks this.

**8. Dispositions (`REQ-SA-4`).**
- *Executed:* a `Withdraw` record at a coordinate → `Observed` → `Final` → `Settled`, with `Redeem`
  taken from the holding position (`DEC-2`).
- *Provably not executed:* once `finalised_through ≥ expiry_position` with no observation,
  `Submitted → Expired`. Non-inclusion is proven when the ingested ranges cover the whole interval from
  submission to expiry with no gap (point 10). Then `Expired → Released`, and `F-004` appends `Release`
  (`AC-E1.4`). If a gap covers any part of it, the result is `Expired → Needs-Review` (`REQ-OP-9`).
- *Unknown:* nothing moves. The deadline takes `Submitted` or `Observed` to `Needs-Review`. Nothing is
  released on silence (`AC-E1.5`, `REQ-SO-4`).

A release requested while the leg can still execute is refused with `ERR-ReleaseUnsafe`.

**9. Amount mismatch (`REQ-OP-2`, `AC-E1.3`).** The executed amount comes from the observation. It is
compared with the reservation, never used to choose what is released. On a difference, the operation
records `ERR-AmountMismatch` as a named condition, raises an incident, and settles in one chain.
**Proposal:** executed < reserved: `Redeem` the executed amount and release the rest to the payer.
Executed > reserved: `Redeem` the reserved amount, plus a posting of the excess from the deployment's
`Suspense` account to the issuance counterparty (`REQ-LA-2`). *Alternative:* no posting, and the
operation goes to `Needs-Review`. That keeps the difference visible only as an incident, and `AC-E1.3`
requires it posted.

**10. Completeness (`REQ-IN-5`, library side of `REQ-SA-7`).** The store keeps a range set of inbox
positions ingested, and a list of explicit gaps, each with the range and the reason. A rescan submits
`(range, digests of every record in it)`. The library compares that with what it ingested. Any difference
is an incident with the missing coordinates, not a silent repair. The unassociated bucket counts
separately. Positions `F-016` skipped are recorded as gaps and are never marked as ingested.

**11. Vocabulary (`REQ-IN-7`).** The record keeps `observed_at`, `final_at` and `settled_at` as three
fields. Surfaces show the three states by their own names.

## Test plan

- `AC-D1.1`: a provisional deposit. No posting, balance unchanged, `INV-10` holds. Simulator test.
- `AC-D1.2`: the finalised head passes the deposit and the record matches. One `Issue` posting.
- `AC-D1.3`: fork the tail after observation. `Orphaned`, then `Released`, with no posting at any point.
- `AC-D1.4`: a deposit to a tracked account that aledgent did not originate is credited.
- `AC-D3.1`: an observation whose payload says "settled" before finality. The state stays `Observed`.
- `AC-D3.2`: the same coordinate twice. The second gets `AlreadyApplied`, and one posting exists. Also a
  direct kernel replay, which is refused.
- `AC-D3.3`: two sources report one event. Only the authoritative one causes a transition.
- `AC-E1.1`: the withdrawal path has no instruction without a reservation. A compile-fail test tries to
  build an instruction without a handle.
- `AC-E1.2`: the reservation stays until `Final`, then `Redeem`.
- `AC-E1.3`: executed amount ± 1 minor unit. `ERR-AmountMismatch` recorded, difference posted, trial
  balance zero.
- `AC-E1.4`: the simulator drops the instruction, expiry passes, the ranges are complete. `Released`.
- `AC-E1.5`: no evidence at all. `Needs-Review` at the deadline, no release. With a gap over the expiry
  interval: `Expired → Needs-Review`.
- `INV-23`: a closed destination at `Final`. The record stays open, never `Rejected`.
- Property test: random forks, delays, drops, duplicates and gaps from a seeded simulator. After every
  step, `INV-10`–`INV-13` hold and no `Customer` balance is negative.

## Exit criterion

`AC-D1.1`–`AC-D1.4`, `AC-D3.1`–`AC-D3.3` and `AC-E1.1`–`AC-E1.5` pass against the simulated settlement
layer. `INV-10`, `INV-11`, `INV-12`, `INV-13` and `INV-23` pass in the conformance suite. The seeded fork
and gap property run finds no violation in 10k runs.

## Open points

- **Final failure observation.** `REQ-OP-9` allows a release on "a final failure observation". §5.6 has no
  edge from `Observed` or `Final` to `Released`. Meanwhile, an included-but-failed withdrawal is stored as
  evidence and does not move to `Observed`. The deadline then takes it to `Needs-Review`, and an operator
  resolves it with that evidence. An amendment should add the edge or say this is intended.
- **Re-verification against `INV-12`.** The observation type carries `finality`, which suggests one
  coordinate is reported more than once. `INV-12` says a coordinate is applied once. This plan separates
  the one ingest (deduplicated) from repeated `CanonicalCheck` reports (idempotent per check). That is a
  reading, for the owner to confirm.
- **Unassociated deposits.** Value that arrives with an unknown `account_ref` is on the settlement layer
  but has no destination. The spec does not say whether it goes to `Suspense`. Meanwhile it is recorded,
  counted as unassociated and raised as an incident. The book-versus-layer axis of `NFR-9` will show the
  difference until someone decides.
- **Proposal:** the posting rule for amount mismatch (point 9). **Proposal:** the `sim` feature (point 1).
  **Proposal:** the `account_ref` is `H("aledgent/acctref/v1", deployment, account id)`. It is
  deterministic, contains no personal data and is not derived from the identity (`REQ-ID-5`, `INV-17`).
  *Alternative:* an issued reference kept in a lookup table, which allows rotation.
- **Two clocks.** Expiry is a settlement-layer position. Deadlines use injected time (`DEC-6`). `OQ-36` is
  open. The rule "deadline later than expiry plus a margin" needs a position-to-time estimate, which
  `F-016` measures.
- **`OQ-30`:** the simulator supports several assets. Whether V0 settles more than one is not decided
  here.
