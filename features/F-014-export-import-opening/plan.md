<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-014 — Export, import and opening balances

| | |
|---|---|
| Milestone | M-1 — The book, in a process |
| Track | A |
| Home | `crates/ledger-core` |
| Depends on | `F-001`, `F-003`, `F-007`, `F-011` (observable state), `F-013` (operator frame, audit) |
| Unblocks | `F-018` (restore drill), `F-021` (re-genesis), `M-3` (Phase 1 shadow and cutover under `DEC-14`) |
| Realises | `DEC-14` (the opening-balance mechanism), `DEC-10` (the re-genesis path), `DEC-1` |

## Goal
aledgent can export its complete state at one sequence and import it into an empty deployment. The
imported deployment answers every query exactly as the original did. After a restore, every balance is
re-derived from entries alone and checked. An operator can load opening balances for accounts that
existed before the deployment: one `Opening` posting per (account, asset), and only after the library
itself has confirmed zero divergence against every independent source.

## Spec traces
- `REQ-RC-3` — export and re-import with identical observable state.
- `REQ-RT-8` — an upgrade that needs re-creation relies on `REQ-RC-3` proven at production volume.
- `REQ-RC-4` — one `Opening` posting per (account, asset), against a named opening position, reconciled
  to zero divergence against every independent source before loading. No replay of history.
- `NFR-4`, `INV-6` — no acknowledged operation lost; every balance re-derived from entries.
- `NFR-9` — one minor unit resolution, zero tolerance, the rule the gate applies.
- `REQ-AS-4`, `INV-7` — exact minor units; excess precision rejected, never rounded.
- `INV-5` — each asset still sums to zero after a load.
- `REQ-AT-1` — the load and the import are audited operator interventions.
- `US-K5` — `AC-K5.1`, `AC-K5.2`.

## Scope
**In:** the export format; export at one sequence; import into an empty deployment; the observable-state
comparison harness; balance re-derivation after restore; the opening position; the reconciliation
record and the zero-divergence gate; `load_opening_balances`; a 1M-entry scale test.
**Out:** backups, restore drills and the daily reconciliation job (`F-018`); extracting an operator's existing
book (deployment work); source adapters that read an existing estate (`M-3` migration work);
dispositions for divergent accounts (`RISK-8`, the operator); export and import on the VOS path, which reuse this format through `F-005` and `F-006`.

## Design

**What an export holds.** Everything needed to rebuild the book and every surface, at one sequence `S`:
- kernel leaves of all six sub-trees, in the kernel's canonical bytes;
- service records: identities, ledger accounts, authorities, the public part of credentials, operation
  records with evidence, client keys with their canonical responses, deadlines, undelivered outbox
  items, the asset registry, checkpoint and anchor records;
- counters: the posting sequence, the last `audit_seq` emitted.
It holds no blinding, no opening, and no key (`REQ-RT-6`, `REQ-AU-8`, `NFR-8`). The audit trail is not
in the export. It is retained apart and stays where it is (`REQ-AT-2`).

**Format.** **Proposal:** a manifest plus sections. Each section is a key-sorted, length-prefixed
stream of canonical rkyv records. The manifest holds the format version, the deployment id, `S`, the
kernel composite root, the service root, and per-section counts and digests. *Alternative:* sorted JSON
lines. They are easier to inspect but several times larger. Either way the bytes are deterministic:
two exports of the same state are identical.

**Export at an anchored sequence.** **Proposal:** export runs right after `anchor_checkpoint`, so `S`
equals an anchored sequence. Then the export's root can be checked against the public anchor (`INV-19`,
`REQ-PR-7`), not only against the exporter's word. The export reads one snapshot pinned to `S`
(`REQ-QR-2`).

**Import.**
1. The target must be empty: no leaves, sequence 0. Otherwise the import is refused. Import never merges.
2. Verify the manifest and every section digest.
3. Load the leaves and records through the storage trait. The first journal is written off-kernel, as
   CipherClerk expects of a host.
4. Recompute the kernel composite root and the service root. Both must equal the manifest. Otherwise the
   target is wiped and the import fails.
5. Set the sequence and counters. Record the import as an operator intervention (`REQ-AT-1`).
Import is the only path that writes state outside a command. It is allowed only into an empty
deployment, only from a whole export, and only when the roots match. That keeps §13's rule: no path
changes an existing book.

**Observable-state comparison.** A harness enumerates every identity, account, operation and asset in
both deployments. It runs every §8.2 query: `balances`, `movements` paged to the end, `statement` over a
fixed set of periods, `operation`, `assets`, `trial_balance`, `assurance` and `can_spend` on sampled
amounts. It compares canonical outputs byte for byte, and compares both roots (`AC-K5.2`).

**Re-derivation after restore (`AC-K5.1`).** Three checks, none needing per-entry openings:
1. For each account and layer, the sum of its entry commitments equals the stored `dr` and `cr`
   commitments. Pedersen commitments add up, so this is exact (`INV-6`).
2. The stored plaintext balance, re-committed with its stored blinding, equals that sum. So the openings
   store is checked against the entries too.
3. Every operation reference the ingress acknowledged exists, in a state at least as far as the
   acknowledgement said (`NFR-4`). In process, the harness keeps the acknowledgement log.

**Opening position.** **Proposal:** one account per asset, kind `Issuance`, owned by the operator's
identity, named as the opening position for that asset. Kind `Issuance` has no negative limit
(`REQ-LA-2`). Its balance is minus the total loaded, so `INV-5` holds.
*Alternative:* post openings against the issuance counterparty itself.

**Opening posting.** Kind `Opening`. It debits the opening position and credits the account's
available balance, in exact minor units of a registered asset. The kernel `external_id` is
`H("aledgent/opening/v1" ‖ account ‖ asset)`. The kernel refuses a second use permanently
(`REQ-KER-6`), so "one per (account, asset)" is kernel-strength. **Proposal:** the
library also refuses an opening for an (account, asset) that already has any entry. Openings are not
`IMPORTED` transfers: they carry the load time. The source's as-of time is kept in the reconciliation
record. No historical operation is replayed (`REQ-RC-4`).

**Reconciliation record and gate.**
```
OpeningSet { rows: [(AccountId, AssetId, Amount)], source_map_digest }
Reconciliation { set_digest, as_of, required_sources, sources: [SourceExtract], recorded_by, at }
SourceExtract { source_id, extract_digest, rows: [(source_ref, AssetId, Amount)] }
```
`required_sources` per asset is deployment configuration: the list of independent sources that must
agree. The library checks, itself, with no "passed" flag taken on trust:
- every required source is present;
- each source's rows map, through the source map, onto exactly the set's (account, asset) keys, with
  nothing missing and nothing extra in either direction;
- every amount equals the set's amount exactly, in minor units: the difference is 0;
- every amount is exact in the asset's precision. A value that needs rounding fails the gate, it is not
  rounded (`REQ-AS-4`).
The source map from the old system's references to `AccountId`s is part of the set digest, so it cannot
change after reconciliation. Converting source decimals into minor units happens in the source adapter,
the one boundary `REQ-AS-4` allows.

**`load_opening_balances`.** Operator authority of its own kind, through `F-013`'s frame. Input: the
set, its reconciliation record, a client key, a justification. The gate runs first. Then the postings go
in batches, each an atomic linked chain (`REQ-KER-10`) sized under the batch limit of `F-008`. Every
posting carries the load's reference. A resumed load skips postings already made: the kernel's
`external_id` sees them. The load completes when the posted total per asset equals the set total and
the opening position equals minus that total. The reconciliation record is kept as a service record
and in the audit trail, so the gate can be re-checked later. Loads are per set, so `DEC-14`'s per-asset,
clean-accounts-first phases are several loads.

## Test plan
- `AC-K5.2`: export a random book of 1M entries, import it into an empty deployment, and run the
  comparison harness. No difference. A second export of the import is byte-identical to the first.
- `AC-K5.1`: restore from a store copy, run the three re-derivation checks. Then corrupt one balance,
  one entry and one opening in turn. Each is caught.
- Import refusals: a non-empty target, a bad section digest, a root mismatch.
- `REQ-RC-4`: a set that matches every source loads. One minor unit of difference in one row of one
  source blocks the whole set. A missing row, an extra row, a missing required source, and an amount
  with excess precision each block it.
- `REQ-RC-4`, one per (account, asset): a second opening for a pair is refused at the kernel, even with
  a new client key.
- `INV-5`: after a load, `trial_balance` is exactly zero per asset.
- `REQ-AT-1`: the load and the import each leave an audit record with principal and justification.
- Scale: export, import and compare at 1M entries. Opening load of 500k rows. Times recorded in `notes/`.

## Exit criterion
`AC-K5.1` and `AC-K5.2` pass on a book of 1M entries, in a plain process. The zero-divergence gate
blocks every seeded divergence in the test set and admits the clean set.

## Open points
- **Secrets across a re-genesis.** An imported deployment must keep signing kernel transfers for
  existing accounts (`DEC-3` derives their keys from a core root) and must know the openings. Neither is
  in the export. They must cross by a sealed key-custody step. Whether that sealed bundle is an
  "exported record" under `REQ-RT-6` is for the owner. `F-005`'s deterministic-blinding Proposal would
  reduce the openings to one secret.
- **`REQ-RT-8` and volume.** 1M entries meets `PLAN.md`'s exit. It does not prove `REQ-RC-3` "at
  production volume". That proof is needed before any upgrade or `F-021` re-genesis relies on it.
- **No operation kind for a load.** §5 lists no operation kind for an opening load, but every posting
  names an operation. This plan gives the load its own reference. The owner may want an amendment.
- **Error codes.** No `ERR-*` names "divergence not zero" or "opening already loaded". **Proposal:**
  `ERR-EvidenceInsufficient` for a failed gate, and `ERR-InvalidTransition` for an opening on a pair
  that already has entries. *Alternative:* an amendment adding named codes (`REQ-ER-1` keeps the list
  closed).
- **Open holds in the source.** A hold in the old book at cutover cannot become a reservation, since it
  has no operation record. Loading it as available would let it be spent twice. The set must be taken
  when that (asset, account class) has no open holds. This is an `M-3` cutover rule under `DEC-14`.
- **`DEC-14` and `Suspense`.** `DEC-14` sends divergent accounts to `Suspense` with a signed-off
  disposition. The gate has no override: a row that differs between sources does not load. How a
  disposition becomes a figure every source agrees on is open under `RISK-8`. This feature does not
  decide it.
- **`OQ-60`** stays open. The load is operator-only here and not offered to integrating parties.
- **Proposal:** the export format, export at an anchored sequence, and the opening position's kind.
