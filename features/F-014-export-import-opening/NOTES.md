<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-014 — implementation notes

Written while implementing `F-014` (issue #14), 2026-09-23, on top of `F-012` (PR #46). The plan's
proposals were followed as accepted: a manifest plus key-sorted sections, export at an anchored
sequence, the opening position as an `Issuance` account, and the error codes of the plan's Open
points. Where the plan was silent, the more conservative behaviour was chosen (refuse rather than
allow); each such choice is listed here for review.

## What landed

| Where | What |
|---|---|
| `crates/ledger-core/src/export/` | The canonical encoding (`canon.rs`, `records.rs`), sections and manifest (`mod.rs`), the kernel's leaves and the sealed openings (`kernel.rs`), the anchor interface (`anchor.rs`, `TODO(F-017)`), the observable-state comparison (`compare.rs`, T-014-04) and re-derivation after a restore (`rederive.rs`, T-014-05) |
| `crates/ledger-core/src/state/image.rs`, `lifecycle/store/canon.rs`, `settlement/canon.rs`, `settlement/store/canon.rs`, `custody/store/canon.rs` | Each store's records as sections, and its rebuild from them |
| `crates/ledger-core/src/ledger/operator/export.rs` | `export_book`, `import_book`, `checkpoint` (T-014-01 to T-014-03) |
| `crates/ledger-core/src/opening.rs` | `OpeningSet`, `SourceExtract`, `Reconciliation`, the zero-divergence gate `reconcile`, the openings record (T-014-06, T-014-07) |
| `crates/ledger-core/src/ledger/operator/opening.rs` | `load_opening_balances` (T-014-08) |
| `crates/ledger-kernel/src/book/mod.rs` | `Book::import` (additive) |
| `crates/ledger-kernel/src/posting.rs` | `PostingDraft::with_external_id`, `external_id`, `transfer_id` (additive) |
| `crates/conformance/src/tools/book.rs` | A random book driven through the command surface, filling every section |
| `crates/conformance/src/cases/f014.rs` | `AC-K5.1`, `AC-K5.2`, run on both in-process stores |
| `crates/conformance/src/bin/f014-scale.rs` | The scale run (T-014-09); results in `notes/MEASUREMENTS-F014.md`: `AC-K5.1` and `AC-K5.2` at 800 000 entries, an opening load of 500 000 rows |
| `crates/ledger-core/tests/export_import.rs`, `tests/opening.rs`, `src/ledger/tests/opening.rs` | The tests below |

## Amendment 0006 (ruled 2026-09-29)

- **Item 7: the operation kind *opening load*.** Ruled. The load stays what was built: an operator
  command (`Command::LoadOpeningBalances`, operator kind `LoadOpeningBalances`) with an operation
  reference of its own, which every opening posting carries; SPEC §8.1 now names its kind. It has
  no lifecycle record, like the other commands decided at once.
- **Item 8: `ERR-Internal`.** An import that fails after it began writing is still
  `ExportError::Partial`, not an `ERR-*`: import is an operator's host procedure, not a command of
  §8.1. A binding fault during a load is `ERR-Internal`, counted as its incident (`REQ-ER-2`), as
  everywhere.

## Decisions taken during implementation

### The format (T-014-01)

1. **A hand-written canonical encoding, not derived rkyv.** The kernel's leaves are the kernel's
   own rkyv archives (the bytes its leaves hash). The service store's records are written field by
   field by `Canonical` impls: big-endian fixed-width integers, length-prefixed bytes and
   sequences, presence bytes, tags this crate owns, maps and sets in key order. Decoding refuses
   anything but that one form (unknown tag, keys out of order, trailing bytes), so a decoded value
   re-encodes to the same bytes. Deriving rkyv on some fifty service types across every feature's
   modules would have tied their layouts to the archive format; the hand-written form fails to
   compile when a feature adds a field or a variant (every struct is destructured and every enum
   matched exhaustively), which is how `F-012`'s custody records were found and added.
2. **Records are rebuilt through their constructors.** Identities, accounts, credentials and grants
   are written as their minting and rebuilt with `new`, so `INV-17` holds for an imported record as
   for a new one. Sub-stores with write rules (the reservation index, the operator records, the
   journal, the openings record) are rebuilt by replaying their records through those rules. The
   operation store, the settlement store and the custody records are restored field by field
   (their histories cannot be replayed: a completed succession changed the owner a new change would
   be checked against), and their indexes derived again. After the rebuild the store is re-encoded
   and its service root compared with the manifest's.
3. **The service root is the digest of the service sections**, in order.
   `TODO(F-005)`: once the service store lives in `F-006`'s service tree, the manifest carries that
   tree's root instead.
4. **The kernel's leaves are folded from the committed delta log** up to the export's commit, the
   source of truth `DEC-4` names, and their composite root is recomputed with CipherClerk's
   `composite_state_root` and compared with the store's before anything is exported.
5. **No blinding, no opening, no key.** A test scans an export for every blinding the vault holds,
   and for the key root, and finds none (`REQ-RT-6`, `REQ-AU-8`, `NFR-8`). Verifiers are exported:
   they are public material (`REQ-AU-8`, `F-003`).

### Export at an anchored sequence (T-014-02)

6. **`S` is the posting sequence**, not the commit sequence: an import commits the leaves in chunks,
   so its commit numbering differs, while the posting sequence continues exactly. The anchor
   interface (`Anchors`, `TODO(F-017)`) answers the book root anchored at a posting sequence;
   `Checkpoints` is an in-process stand-in that records what it is told and proves nothing to a
   third party.
7. **Export is a privileged read** (`REQ-PV-4`): it reads every fact of the book past the authority
   checks of `REQ-PV-1`. It goes through the one instrumented mechanism under the `PrivilegedRead`
   operator kind, with a justification, and its record reaches the audit store before anything is
   returned. `tests/privileged_reads.rs` counts it (`ReadKind::Export`).
8. **The audit trail is not exported** (`REQ-AT-2`). An export needs the audit outbox shipped
   (`AuditNotShipped` otherwise); the manifest carries the chain's head; an import needs the audit
   store's head to be that head, and continues the chain. **Stated plainly:** the plan says a second
   export of the import is byte-identical to the first. Every section, both roots and the sequence
   are; the manifest's audit head is not, because the import records itself on the audit trail
   (`REQ-AT-1`) and the second export's own read is recorded too. The test checks exactly this.

### Import (T-014-03)

9. **Every check before the first write**: an empty store and vault; the layout and every section
   digest; the deployment id (another would derive other operation references); the anchor at `S`;
   both roots recomputed; the sealed secrets (the keys derive the book's journal, the openings open
   every exported commitment and every commitment has its opening); the audit head; the caller's
   operator kind `ImportBook` in the imported book and a justification. A refusal gives the store
   and vault back untouched. Only a store or vault failure after the first write is
   `ExportError::Partial`: the target must be discarded (`F-018`'s drill).
10. **A refused import is not audited**: there is no deployment state yet to hold its record. The
    accepted import's record goes on the audit chain only, as an operator intervention, not into any
    section.
11. **The sealed key-custody step** is, in process, the `Openings` value (the vault's rows) and the
    same key root. It has no byte encoding here: whether its sealed wire form is an "exported record"
    under `REQ-RT-6` is the owner's (plan Open points).
12. **Leaves are committed in chunks of 24 KiB of encoded leaves**, under one delta-ring row, so
    `F-006`'s `BookStore` takes them; the openings with commit 0.

### Comparison and re-derivation (T-014-04, T-014-05)

13. **The comparison renders every reply with `Debug`** and compares the strings, accepted or
    refused. It includes `F-012`'s reads (custody view, recovery mechanisms, custody notices).
    `assurance()` is not on the query surface yet (`F-015`, `F-017`).
14. **Re-derivation adds the service store's own drift check** (`F-011`'s `reconcile`). A book with
    pending-transfer statuses is reported, not modelled: aledgent's postings never make one
    (`DEC-2`). In process the acknowledgement log is the caller's; a reply whose state is another
    operation's (an approval) is acknowledged as a command.

### Opening balances (T-014-06 to T-014-08)

15. **The opening position** is an `Issuance` account owned by the operator's organisation identity,
    one per asset, opened by the first load of that asset through a new write
    (`Write::OpeningPosition`): the store's "one issuance counterparty" rule is unchanged for
    `PutAccount`, and a second `Issuance` account can exist only as a named position. The evaluator
    already refuses every operation on an `Issuance` account, so a position moves only by openings.
16. **The kernel key is the pair's**: `blake2b-256("aledgent/opening/v1" ‖ account ‖ asset)`, set with
    `PostingDraft::with_external_id`; the transfer id follows it. The posting's operation stays the
    load's, so the journal and every movement name the load. A unit test sends a second opening of a
    pair, built by the library under a new operation, straight to the kernel: `Duplicate`.
17. **The library refuses first**: a pair loaded by another load, or with any entry already (its
    kernel sub-account exists), is `ERR-InvalidTransition` (plan Proposal).
18. **Source figures are decimal strings**, converted by the one decimal boundary
    (`Minor::from_decimal`) inside the gate: a figure that would need rounding is a divergence, never
    rounded. The extract's digest is recomputed.
19. **Required sources are deployment configuration** (`LifecycleConfig::opening_sources`). An asset
    with none cannot be loaded (`Divergence::NoSourcesRequired`).
20. **A zero row** is reconciled and posts nothing; the pair counts as loaded, and a later opening of
    it is refused.
21. **Negative opening balances cannot be expressed** (rows are `Minor`, the decimal boundary takes no
    sign), so a negative balance in a source book cannot be loaded. For the owner.
22. **The load's attribution record names the opening positions and no postings**: a load of 500 000
    rows would otherwise make one record, and one audit record, of a million subjects. The posting
    sequence numbers are in the load's record (`OpeningLoad::posted`) and every posting names the load,
    so `movements` shows them; `operation(load)` shows none.
23. **Batches are `max_legs` postings, one linked chain and one commit each** (`REQ-KER-10`).
    `max_batches` bounds one call. The same request under the same client key resumes; another
    request under it is `ERR-ClientKeyReused`. A chain the kernel refuses (an account closed between
    calls) stops the load, audited, and it stays incomplete. Completion checks the journal's posted
    total per asset against the set's, and each position against minus everything ever loaded in its
    asset.

### Shared files (all additive)

24. `ledger-kernel`: `Book::import`; `PostingDraft::with_external_id`, `external_id`, `transfer_id`
    (a mirror's `void_of` and the key now share one boxed field, so a draft is no larger).
25. `ledger-core`: `Command::ImportBook` (39) and `LoadOpeningBalances` (40), after `F-012`'s 30–38;
    `ReadKind::Export` (2); `OperatorOpKind::ImportBook` (12) and `LoadOpeningBalances` (13);
    `Write::OpeningPosition`, `Write::Opening`; `State::identities`, `credentials`, `openings`;
    `LedgerAccount::treasury_limits`; `AuditLog::continuing`, `head_hash`; `OpsState::grants`;
    `Journal::append_entry` (`append` now calls it); `operator_authority_in` (the frame's step 1 over
    a store, which `operator_authority` calls); `LifecycleConfig::opening_sources`.
26. `conformance`: `ServicePort::observable`, `export_and_restore`, `rederive`. The in-process restore
    imports into memory whatever the original store kind.
27. `services/ingress`: the two new operator kinds on the wire (`import_book`,
    `load_opening_balances`), so a grant of them can be made and shown through ingress; `openapi.json`
    regenerated. Without them ingress's mapping would have shown either kind as `resume`.
28. `F-012`'s custody records are four sections: changes, successors, custody epochs (the reclaim
    proof binds the epoch) and notices.

## Tests

| Test | Covers |
|---|---|
| `tests/export_import.rs`: `ac_k5_2_export_and_reimport_give_identical_observable_state` | `AC-K5.2` on a random book with every section filled, through bytes; the second export's sections and roots byte-identical |
| `ac_k5_2_the_imported_book_takes_commands_as_the_original_does` | Client-key entries cross (a repeat replays); a new command gives the same outcome; the harness sees a book that moved on alone |
| `two_exports_of_the_same_state_are_byte_identical_and_hold_no_secret` | T-014-01 |
| `t_014_02_the_manifest_root_is_the_anchored_root` | T-014-02; a stale anchor and a caller without the kind are refused |
| `t_014_03_import_refuses_a_non_empty_target_a_bad_digest_and_a_root_mismatch` | T-014-03, and a forgery consistent with itself, wrong openings, foreign keys, another deployment, no authority, another audit store |
| `req_at_1_the_import_is_on_the_audit_trail` | `REQ-AT-1`, `REQ-PV-4` |
| `ac_k5_1_a_restored_book_rederives_and_every_seeded_corruption_is_caught` | `AC-K5.1`: a balance, an entry, an opening and a missing acknowledged operation, each caught |
| `ac_k5_1_a_file_store_copy_restores_and_rederives` | `AC-K5.1` on `FileStore` |
| `tests/opening.rs` (8 tests), `ledger::tests::opening` | `REQ-RC-4`: the clean set loads (`INV-5`, `REQ-AT-1`); every seeded divergence blocks; one opening per pair; resumed and stopped loads; zero rows; the operator kind |
| `ledger-kernel`: `req_rc_3_import_*`, `req_ker_6_a_key_of_its_own_*` | the kernel additions |
| `conformance` `AC-K5.1`, `AC-K5.2` | the service port, both in-process stores |

## Deferred

- `TODO(F-017)`: anchoring itself; `Checkpoints` stands in for it.
- `TODO(F-005)`: the service root as `F-006`'s service tree root; export and import on the Agent path.
- `F-018`: backups, restore drills, discarding a partial import's target.
- `TODO(F-009)`: `load_opening_balances` is not routed through ingress. An opening set can be large, and
  comes with every source's extract, which is an operator tool's input, not a request
  body; export and import are host procedures, like the privileged read ingress does not route.
  The operator kinds are on the wire.
- Streaming the export section by section: the format allows it; the in-process API builds it in
  memory (see `notes/MEASUREMENTS-F014.md`).
- The owner: the sealed key-custody bundle's form (`REQ-RT-6`); negative opening balances; how a
  `Suspense` disposition becomes a figure every source agrees on (`RISK-8`); `OQ-60`.
- `REQ-RT-8`: the export and re-import are proven here at 800 000 entries, not 1 M: the 3 GB limit on
  generated data on the shared host did not fit two 1 M-entry books (`notes/MEASUREMENTS-F014.md`,
  which also extrapolates to 1 M). Not at production volume either.
