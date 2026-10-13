<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-029 — Book-only assets and their binding

| | |
|---|---|
| Milestone | M-3 — Production V0 (the canary registers its assets book-only, and binds them later) |
| Track | B |
| Home | `crates/ledger-core` (`asset`, `settlement::assets`, `ledger::asset_binding`, `ledger::operator::registry`, `eval`), `services/ingress` (`settlement`, `wire`, `surface`, `query`), `services/settle` (`chain`, `adapter`, `store`), `contracts/settlement` (`Bind`), `deploy/templates`, `e2e/` |
| Depends on | `F-003` (registry), `F-013` (`register_asset`), `F-016`/`F-027` (the adapter, its light client and the call outbox), `F-026` (the contract), `F-017` (the third axis) |
| Realises | `DEC-20` (`PLAN.md` v0.6.0), amendment 0011 R2 |

The directory keeps its first name; the feature was reworked on the owner's rulings of 2026-10-02
(#112 "changes needed"): it first bound every asset to a location at registration, read the precision
from the layer and parsed the name, and held an asset on any metadata drift. All four are gone.

## Goal

An asset is registered **book-only**, with its precision, and moves in the book under issuance
authority (`REQ-AS-7`). Later, online and once, its issuance authority **binds** it to its asset on
Kreivo (`REQ-AS-8`): only if the layer describes it in its own `symbol` and `decimals` fields, and only
at **zero divergence**, which the settlement contract checks at the block that records the binding.
After that, the layer's metadata no longer matters (`REQ-AS-9`).

## Spec traces

`REQ-AS-1`–`AS-3`, `AS-6`–`AS-9`, `REQ-SA-1`, `SA-3`, `REQ-SL-3`, `INV-5`, `INV-11`, `INV-24`, `NFR-9`,
`NFR-17`, `US-F5` (`AC-F5.1`–`AC-F5.5`).

## Design

1. **Registry** (`asset.rs`). `Asset.binding: BookOnly | Binding(location) | Bound(location)`. One
   location, one asset, while binding and after (`by_location`). The binding moves only
   `BookOnly → Binding → Bound`, or `Binding → BookOnly` on a refusal. It is in the digest, the leaf and
   the export (format 10). A deployment's configuration registers book-only assets only; a library
   genesis may declare an asset bound from the start, for a deployment whose position on the layer
   starts empty with the book (the test suites; `NOTES.md` decision 2).
2. **Attestations** (`settlement::assets`). The adapter reads each location its configuration maps,
   at the finalized head over the light client: `Assets::Asset` (status) and `Assets::Metadata`'s
   `symbol` and `decimals` fields. No name, no sufficiency. Absent metadata is "none". The core keeps
   the latest per location and answers an informative verdict (`unregistered`, `book_only` with the
   mismatch that would refuse a binding, `binding`, `bound`). Nothing is held.
3. **`bind_asset`** (`ledger::operator::registry`, `POST /v1/ops/assets/{asset}/bind`). Under a live
   issuance grant over the asset, with a justification; audited. Refused unless the asset is active and
   book-only and the location free (`ERR-NotAuthorised`), and the latest attestation is fresh
   (`ERR-SettlementLayerUnavailable`) and admits it (`ERR-UnsupportedLeg`). It takes the outstanding
   balance: the negation of what every issuance-kind account holds in the asset (the issuance
   counterparty and the opening positions). The asset becomes `Binding`, which refuses its issue and
   redeem with `ERR-NotAuthorised`, and an `AssetBind { reference: the command's operation, asset,
   location, amount }` goes into the settlement store.
4. **The call** (`settlement::envelope`). Every pending binding is in the call outbox as
   `SettlementCall::Bind { asset, location, amount }` under the command's operation reference, no
   expiry, until its outcome is final.
5. **The contract** (`F-026` decision 24). `Call::Bind { asset, amount }` (index 2): refused
   `AlreadyBound` (12) once the asset was bound; refused `HoldingMismatch { holding }` (13) unless the
   contract's holding equals its recorded position plus `amount`; otherwise it adds `amount` to the
   position and writes `InboxEvent::Bound` (4). Inline, never framed. `bound(asset)` reads it.
6. **The adapter** (`services/settle`). Bind calls go on the anchor key's lane, one extrinsic of that
   key in flight at a time (an anchor waits for a bind, a bind for an anchor). Each is stored before it
   is broadcast (`settle_binds`, migration 4), decided at the finalized head as an anchor's is, and
   signed again if it provably did not execute; once one executed it is never signed again. A binding
   at a location the configuration does not map to the asset is never signed (`bind:<ASSET>` alert).
   `Bound` and the refused bind are reported as observations `bound` and `bind_refused`, with the
   reference and the movement.
7. **Finality** (`ledger::asset_binding`). The canonical check that finds the record final decides
   the binding: `Bound` of the asked asset and amount → `Bound(location)`; `BindRefused` → book-only
   again and a `BindRefused` incident; any other record under the reference → an incident, nothing
   decided. A record of a binding already decided changes nothing.
8. **After binding.** External legs are supported only for a bound asset (`require_supported`,
   deposits included). Issuance and redemption go on (`REQ-AS-6`; question `k-01`). Attestations are
   ignored. The third axis (`book_positions`) counts the binding's amount as the asset's starting
   position, from the cut that includes the `Bound` record.

## Out

Moving an asset's existing on-chain supply into the contract: an operation on Kreivo by its
issuer before the binding (question `k-04`). Unbinding. Native conversion (the owner, 2026-10-03:
conversions happen outside aledgent; nothing here assumes one).
