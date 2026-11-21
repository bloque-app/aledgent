<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-046 — Settlement-layer format engines

| | |
|---|---|
| Milestone | M-4 — Replicated (lands after `M-3D` and no later than `M-4`, beside the move to `vos-core`; nothing in `M-3` or `M-3D` waits on it) |
| Track | B |
| Home | `services/settle` (the engine seam and its engines in `src/chain`, the fingerprint in `src/chain/surface.rs`, the adapter's choice and its refusal, the binary's `formats` command, the fake layer), `services/settle/kreivo-pin*` (one vector set per engine), `e2e/` (the runtime-upgrade job), `ops/prometheus` (the alert), `ops/runbooks/release` (the runtime-upgrade runbook) |
| Depends on | `F-016` (the adapter), `F-027` (the transaction surface, `T-027-12`, and the layer's confirmation before broadcast, its NOTES decisions 47 and 51), `F-045` (native accounts, the form the engines sign) |
| Unblocks | A Kreivo runtime upgrade that changes the transaction format met by a release shipped before it is enacted, instead of by a release written after the adapter stopped |
| Realises | `REQ-SA-9`, `REQ-SA-2`, `REQ-SA-6`, `REQ-SO-7`, `REQ-SL-4`, `NFR-11` (`AC-E4.3`, `AC-E4.4`) |
| Status | **Planned.** The owner approved the plan on 2026-11-20. No `SPEC.md` change: `REQ-SA-9` already asks for everything here, and this feature changes how the adapter meets it, not what it must do. Nothing is built in the pull request that carries this file |

## The problem

Everything the adapter signs is compiled into it. `chain::kreivo` writes each byte of a Kreivo
transaction by position, from Kreivo's source at pinned revisions, with no runtime metadata, and
`TxFormat` pins the transaction surface each format applies to (`FORMAT_132`, `FORMAT_133`). The adapter
reads live metadata (subxt's dynamic API, `chain::rpc`) for its state reads and to compute the surface's
fingerprint (`chain::surface`, `T-027-12`), but it does not use that metadata to build what it signs.
A client such as PAPI resolves call and extension types from metadata at run time. The adapter's
transactions are fixed when it is built, and the `services/settle/kreivo-pin*` workspaces pin them
against Kreivo's own crates, one runtime at a time.

So a Kreivo runtime upgrade that changes what a transaction carries needs a new adapter release. Until
that release ships, the adapter does what `REQ-SA-9` asks of it: it finds no format for the new surface
(`Refusing::UnknownSurface`) and refuses to serve. This has already happened once. Kreivo `0.18.1`
(spec 136) raised `transaction_version` to 14 because it removed two calls that no transaction of the
adapter carries. The adapter still needed a release that listed 14 in `FORMAT_133` and a third pin
workspace (`kreivo-pin-136`) before it would serve on that runtime.

None of this is a bug. `REQ-SA-9` puts a false stop ahead of a false start, and so does `F-027`'s
decision 46. What this feature changes is **when** the release is written. A change of format on Kreivo
passes a referendum, with days between its approval and its enactment. The adapter should be able to
ship the next format in that window, and switch to it when the upgrade is enacted.

## What the spec already requires

`REQ-SA-9`:

- the adapter decides whether it can keep serving from **the transaction format the layer
  describes**: the extrinsic format version, the parts every transaction must carry and their types,
  and the type of each call the adapter submits. A version number of the layer's rules alone never
  decides it, in either direction;
- before the first transaction signed under rules it has not served, **the layer itself confirms**
  that a transaction the adapter built and signed under them is valid, and the adapter refuses to serve
  until it does;
- an operator MAY name the format for a set of rules in configuration. The confirmation is still
  required.

`AC-E4.3` (an unchanged format keeps serving, with no release or configuration) and `AC-E4.4` (a changed
format, or one the layer does not confirm, refuses) are its tests. `F-027` built both: the surface's
fingerprint (`T-027-12`), and decision 51, `TaggedTransactionQueue_validate_transaction` run at the
block whose runtime the transaction was signed for, before anything signed on a runtime not yet served
is broadcast. `REQ-SO-5` and `REQ-SO-7` fix what an instruction carries and that a restart replays the
same instruction, never a second one. Those are what make a redeploy across an upgrade safe (below).
`F-037` is the book's own upgrade path. An adapter release is not a core upgrade: the adapter holds no
book (`REQ-SA-8`), so a release changes no code version, needs no `Upgrade` input and has no fence.

## Design

### 1. Format engines, compiled in

A **format engine** covers one Kreivo transaction format, end to end:

- **the extension set**: each transaction extension the format carries, in order, with the explicit
  value the adapter gives it (`PassAuthenticate` with no credential, the era, the nonce, the tip, no fee
  asset) and the implicit data it signs over (`spec_version`, `transaction_version`, genesis, the era's
  birth block);
- **the call encodings**: the `Contracts::call` that carries the settlement contract's `execute`. The
  same call carries the lanes' withdrawal batches and the anchor key's anchors and bindings. The
  contract's own message encodings (selectors, the envelope types) are the contract's ABI, not the
  runtime's, so they stay shared and are not part of an engine;
- **the signed payload and the extrinsic framing**: v4 signed today (`F-045`), and whatever later
  version Kreivo moves to;
- **the reads whose shape the runtime decides**: the asset reads (`Assets::Account`, `Assets::Asset`,
  `Assets::Metadata`), the nonce (`System::Account`), the block weights the batch size is cut to, the
  contract's events, and the runtime calls the adapter makes (`ContractsApi_call`,
  `TaggedTransactionQueue_validate_transaction`). The adapter reads no `Timestamp` today: expiry is
  counted in heights (`REQ-SL-4`). An engine whose format needs a timestamp read carries it the same way.

Several engines are compiled into one adapter release. Today's formats become its first engines, and
their bytes do not change: the kreivo-pin vectors and every `services/settle` test pass unchanged.
Engines keep their names (`kreivo-133+native`, and so on), so a runtime already confirmed in the store
(`settle_runtimes`) stays confirmed across the release that brings them in. While the PASS form is still
in the tree (`T-045-06` removes it), its two formats become two engines as they are.

### 2. Choosing an engine: the format fingerprint

The adapter chooses the engine whose **format fingerprint** matches the runtime's, as derived from that
runtime's live metadata:

- the extrinsic format version(s) the metadata declares;
- the ordered list of transaction extensions, each with the **shapes** of its explicit and implicit
  types;
- the shapes of the calls the adapter submits (`Contracts::call`: the pallet's index, the call's
  index, its arguments' names and types).

The fingerprint uses **shapes, not type ids**. A registry renumbers its types between runtimes, so an id
says nothing. The digest stays the one `chain::surface` already computes: structural, sensitive to order
throughout, reading names but not paths or docs. This feature adds the extrinsic version to it.

Neither `spec_version` nor `transaction_version` decides the choice, in either direction (`REQ-SA-9`).
**A native engine's fingerprint leaves `transaction_version` out.** A native transaction signs that
number as implicit data and the runtime checks it, but the number says nothing about the format. Spec 136
is the case in point: a native engine for spec 133's format matches spec 136 with no release. The PASS
engines keep `transaction_version` in their fingerprint for as long as they exist, because the PASS
message order is not in the metadata, and Kreivo bumped `transaction_version` when that order changed.

A fingerprint that matches no engine is the refusal of §6.

### 3. Then the layer confirms

The choice is followed by the existing confirmation (`F-027` decisions 47 and 51). Before the first
transaction signed in an engine on a runtime not served before is broadcast, the runtime validates that
transaction at the block whose runtime it was signed for. Nothing goes out until the result is `Valid`. A
refusal for what the transaction carries stops the adapter on that runtime (`Refusing::Unconfirmed`, the
`runtime:<spec_version>` alert). The operator's mapping (`adapter.formats`, decision 49) keeps its
meaning: it names an engine for a `spec_version` whatever the fingerprint says, and the confirmation is
still required before anything signed in that engine is broadcast.

### 4. subxt's dynamic API where the encoding follows the metadata

Engines encode what the metadata describes through subxt's dynamic API from live metadata: the
`Contracts::call` arguments (`subxt::dynamic::tx`) and each transaction extension by the identifier the
metadata lists. The runtime updater already reloads the metadata when the runtime changes (`chain::rpc`,
`RpcChain::runtime`). An engine then hand-writes only what the metadata cannot tell it:

- the values the adapter chooses for each extension;
- what a signature covers beyond the standard signed payload (the PASS message order, while that form
  exists);
- the contract's ABI;
- Kreivo's own transaction extensions. `PassAuthenticate` (encoded `None`), the fee asset's
  `ChargeAssetTxPayment`, and wrappers such as `DynamicMaxBlockWeight` that carry no data stay
  hand-written, as the engine's own implementations of subxt's `TransactionExtension` trait. Each one's
  value is fixed by the engine and its type is checked by the fingerprint. Only the standard Substrate
  extensions (the spec and transaction versions, genesis, the era, the nonce) use subxt's built-in
  implementations.

**Why this is safe only with an order-sensitive fingerprint.** subxt matches a composite's fields by
name. Its validation hash (`subxt_metadata::utils::validation`) combines them with an XOR and so ignores
their order. That is why `chain::surface` (`services/settle/src/chain/surface.rs:16-21`) rejected subxt's
hash for the positional encoder. Encoding by name from live metadata puts the fields in the order the
metadata gives, but nothing in subxt notices that the order changed. What keeps an engine from signing a
reordered type it was never pinned against is the fingerprint, which stays sensitive to order.

This makes engines smaller. It does not make a single engine accept more runtimes. The fingerprint stays
strict and sensitive to order, so a field reordered under a dynamically encoded call is still a new
fingerprint (decision 46: a false stop over a false start). A new engine is then mostly declarative, which
is what lets it be written, pinned and released in the window a referendum gives.

*Trade-off, stated plainly.* Dynamic encoding moves the bytes' correctness from code written against
Kreivo's source to subxt's encoder working from metadata. The kreivo-pin vectors still decide: an
engine's dynamic bytes must equal the bytes Kreivo's own crates produce, for every vector, or the engine
does not ship.

### 5. No runtime-loaded engines

Engines are compiled in. The adapter does not load them as plugins, scripts or downloaded code. The
adapter sits next to the signer and moves money. Code loaded at run time would need its own signing, its
own verification before load, an audit of each artefact, and a guarantee that every replica runs the
same one. Each of those is a new place where a mistake signs a payout. `F-042` drew the same line for
signature engines ("nothing loads code"). A WASM-sandboxed engine, running in a sandbox with no access to
keys and with its output checked by the adapter, is recorded only as a possible later direction. It is not
designed here.

### 6. The procedure for a Kreivo runtime upgrade

1. **Before the referendum is enacted.** The operator takes the candidate runtime's metadata from a fork
   of the layer with the upgrade applied (a Chopsticks fork, as `F-043`'s rehearsals use) and runs
   `aledgent-settle <config> formats <metadata>`. The command prints the candidate's fingerprint and the
   engine that fits it, or the parts no engine covers, and exits non-zero if none fits.
2. **If no engine fits,** a release that carries the next engine is written, pinned (its kreivo-pin
   vectors) and shipped **before enactment**. It serves the current runtime with the current engine, so
   it can go out at any time. Nothing switches early. The operator rehearses the confirmation on the fork:
   a transaction signed in the new engine is validated there. That rehearsal is evidence for the
   operator. It is not the layer's confirmation, which `REQ-SA-9` still requires from the layer itself.
3. **At enactment,** the adapter switches at **the first finalized block under the new runtime**, after
   the layer confirms the new engine (§3). The runtime it signs for is read at the finalized head, the
   head every era is already born at (`T-027-08`). Between the upgrade's inclusion in the best chain and
   its finality (about 16 blocks at mainnet's median finality lag), nothing new is signed: a transaction
   signed for the old runtime would fail at the new best head, and an upgrade not yet final can still be
   reorganised away. Stored transactions are still broadcast again (§7). Those signed for the old
   runtime are rejected at the new best head (their `spec_version` is in their implicit data). That is
   expected: they are not re-signed, and they end as their era ends (§7). *Today the adapter reads the
   runtime at the best head (`RpcChain::runtime`). This is the one behaviour this feature changes in what
   is already built.*
4. **If no engine fits at enactment** (the release did not ship in time, or an upgrade came without
   lead time), the adapter refuses to serve, as it does today, and raises `format:<spec_version>`, which
   pages at once (`AledgentAdapterNoFormat`, no `for`). The general not-serving page waits five minutes,
   and five minutes is about a withdrawal's whole life (`F-027` decision 52). The alert names the
   fingerprint's parts that differ from the closest engine, so whoever writes the engine knows where to
   start.

The runbook (`ops/runbooks/release/kreivo-runtime-upgrade.md`) carries this procedure, with no
operator's accounts or hosts in it.

### 7. Why a redeploy across the upgrade is safe

A release shipped before enactment replaces a running adapter. Taken one at a time:

- **The core keeps every withdrawal until the adapter acknowledges it, and the adapter stores it
  before acknowledging.** The call outbox holds every withdrawal instruction the executor has not
  acknowledged, and hands the same envelopes under the same references after a restart
  (`crates/ledger-core/src/settlement/envelope.rs:27-30`). An acknowledged withdrawal leaves the outbox
  (`OutboxCall::acknowledged`, `envelope.rs:198-205`). The adapter's intake stores each instruction
  first (`CrashPoint::AfterStore`, `services/settle/src/adapter.rs:2380`) and only then acknowledges it
  (`CrashPoint::AfterAck`, `adapter.rs:2401`). A stop between the two leaves a stored row that the next
  step acknowledges again.
- **The adapter stores the signed bytes before broadcasting, and broadcasts the same bytes after a
  restart.** A batch is signed (`CrashPoint::AfterSign`, `adapter.rs:4209`) and stored, one row per
  operation, all or nothing (`CrashPoint::AfterPersist`, `adapter.rs:4256`). Every operation's key,
  nonce and last valid block are recorded with the core (`record_batch`, `adapter.rs:4260-4262`). Only
  then is the batch broadcast (`CrashPoint::AfterBroadcast`, `adapter.rs:4264`). Each step broadcasts
  every stored, uncertain transaction again, with the same bytes (`adapter.rs:4324`, `REQ-SO-7`).
- **The crash points are tested.** `CrashPoint` (`adapter.rs:127-144`) is the restart matrix
  (`services/settle/tests/restart.rs:47`, `ac_k3_1_the_restart_matrix_loses_no_instruction_…`), and
  `req_so_7_chaos_never_yields_a_second_extrinsic_for_one_operation` (`restart.rs:143`) restarts at
  random and never finds a second transaction for one operation.
- **An anchor stays in the outbox until it is final** (`envelope.rs:27-30`). An anchor transaction that
  provably did not store it is reported, and the core emits the anchor again.
- **A transaction signed for the old runtime and not included before enactment is never re-signed.**
  The old `spec_version` is part of its implicit data, so the new runtime rejects it. Its era ends with
  its key's nonce unspent (`NotExecuted::EraEnded`). Once the finalized head passes the instruction's
  expiry, the core releases it on **both proofs**: the inbox shows it was not included, and the nonce was
  not consumed. `services/settle/tests/contract.rs:644`
  (`t_027_07_a_batch_in_flight_across_the_upgrade_is_released_on_both_proofs`) shows this for
  132 → 133. The user gets the funds back as available, and the withdrawal has to be requested again.
  It is not carried over.
- **What happens to a tombstoned (expired) withdrawal's reserved funds.** They are **released back to
  the payer's available balance**, once the finalized head passes the expiry. The adapter tombstones an
  acknowledged, unsigned instruction when no era can end before its expiry. It first stores
  `NotExecuted::NeverSigned`, then records `SigningRecord::NeverSigned` with the core
  (`adapter.rs:4062-4074`). From then on the core refuses any signing for it (`record_signing`,
  `crates/ledger-core/src/ledger/settlement.rs:849-850`). For a never-signed instruction there is no
  nonce to check, so the release rests on **non-inclusion proven**: the inbox, plus the tombstone the
  core holds (`settlement.rs:1851-1855`). That holds only if the finalized inbox is complete over the
  range, and only if no gap lies within it or, on a chained book, starts below it
  (`settlement.rs:1838`: `gap_within(from, to) || (chained() && gap_starts_below(from))`, which makes it
  unprovable; an incomplete range waits, `:1845-1847`).
  `dispose` then moves the operation `Submitted` → `Expired` once the finalized head is at
  or past the expiry, then `NonInclusionProven`, and releases the reservation (`CHOSEN.release`,
  `settlement.rs:1888-1941`). If the kernel refuses that release, the transitions that move no value are
  kept and the operation is queued for review (`settlement.rs:1956-1970`). If the inbox shows such a gap,
  the operation reaches `Needs-Review` through `NonInclusionUnprovable` instead
  (`settlement.rs:1909-1911`), and nothing is released on a timer (`REQ-OP-9`).
  `services/settle/tests/flow.rs:1136`
  (`ac_e1_4_an_instruction_that_can_no_longer_be_signed_is_tombstoned_and_released`) ends `Released`
  with the full balance available again. Liquidity that comes back later signs nothing for it.
- **While the adapter refuses, nothing is tombstoned and nothing is released.** A step that is not
  serving stops after the lease and the heartbeat (`adapter.rs:1430-1432`). No intake, no observation
  and no submission run, so no finalized head reaches the core. Withdrawals already acknowledged stay
  reserved for as long as the refusal lasts. New ones are refused at submission, because the adapter
  declares that it does not serve. When a release serves again, its first steps tombstone what expired
  in the meantime, and the core releases those withdrawals as above. A refusal holds funds longer than
  usual. It never loses them, and it never pays one out twice.
- **One instance at a time.** The single-active lease (step 1 of `Adapter::step`) means the new release
  takes over only once the old one no longer holds the lease. Two releases never share a nonce lane.

A release that only adds an engine changes no store schema. Rolling it back before enactment is
therefore safe.

## Trade-offs, stated plainly

- **This feature does not keep the adapter serving across a planned change of format.** It makes that
  the usual outcome when a release ships in time. When none does, the adapter refuses, as it does today.
  A guarantee would be a new requirement and would need an amendment. The owner has not asked for one.
- **Signing pauses for every upgrade's finality lag**, even one that keeps the format. That is about
  half a minute on mainnet, against the risk of signing for a runtime that a reorganisation takes away.
  Withdrawals instructed in that window wait. Their expiry is about five minutes, so they are not lost.
- **Leaving `transaction_version` out of the native fingerprint gives up a guard.** A runtime change
  that alters what a transaction means without changing any shape the fingerprint reads no longer stops
  the adapter at the fingerprint. Kreivo would have marked such a change by bumping `transaction_version`.
  Only the layer's confirmation stands in front of it then (§3, `F-027` decision 47). The confirmation
  catches what validation checks: the signature over the payload, the implicit data, a call that does not
  decode. It does not catch what the runtime checks only at dispatch.
- **Withdrawals signed just before enactment and not yet included are released, not retried.** Each
  operation gets one transaction (`F-016`, the ruling of 2026-09-24), and that rule is not reopened here.
  The user requests the withdrawal again.
- **An engine is still code that has to be released.** Lead time comes from governance. An emergency
  upgrade without a referendum leaves the adapter refusing until a release ships.
- **Some changes no engine can absorb**: a runtime that drops `pallet-contracts` for another contracts
  pallet changes the venue and the contract, not only the format. That is other work.

## Out of scope

- Any guarantee that the adapter keeps serving across a planned change of format (it would need an
  amendment, and none is asked for).
- Settlement layers other than Kreivo.
- Runtime-loaded engines, sandboxed or not (§5).
- Changing the one-transaction-per-operation rule, or retrying a withdrawal released across an upgrade.
- The settlement contract, and changes to Kreivo.
