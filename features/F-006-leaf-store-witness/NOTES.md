<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-006 — implementation notes

Written while implementing `F-006` (issue #6), 2026-09-23. The plan's proposals were followed as
accepted: the service tree, `redb` behind one engine trait, and rebuild with bit-for-bit digests.
Where the plan was silent, the most conservative behaviour was chosen; each such choice is listed
here for review.

## Both ways of running the core (issue #38)

`F-002` did not confirm `DEC-4`; issue #38 asks the owner whether the reservation decision runs in a
VOS Agent or in a native process. The store is built to serve both, with one code path and one
committed delta log:

- **Native core.** `BookStore` implements `F-001`'s `LeafStore`, and `OpeningsStore` implements
  `F-001`'s `VaultBackend`, so `Book<BookStore, OpeningsStore>` runs unchanged. Every commit becomes
  a `DeltaRecord` the store builds itself, archives, and applies. Service-tree puts are staged
  before the commit with `BookStore::stage_service`.
- **Agent core.** The host builds a witness from a read view (`witness::Witness::build`), the core
  verifies it (`Witness::verify`, or CipherClerk's own `verify_transition` on
  `Witness::to_succinct`), and the host applies the record the core committed with
  `BookStore::apply_record`, which checks the chain and both sets of roots instead of building
  them.

Nothing here depends on VOS. The Agent-side carrier is `F-005`'s (below).

## Decisions taken during implementation

### The record and the service tree

1. **`DeltaRecord` lives in `ledger-core::delta`** (plan: "the type lives in `crates/ledger-core`
   so the Agent can build it"). `ops` is the set of operation references the record commits.
   `roots_before` and `roots_after` are the six kernel sub-roots plus the service root; the
   kernel's composite root is untouched, and a separate **book root** folds it with the service
   root: `blake2b-256("aledgent/book-root/v1" ‖ kernel root ‖ service root)`.
2. **The 64 KiB limit applies on both paths.** `DeltaRecord::new` refuses a record over one
   delta-ring row, and the native path refuses the commit with a new
   `WriteRefusal::RecordTooLarge` in `ledger-kernel` (additive). The limit comes from the Agent
   runtime, but keeping it for a native core too keeps the archive's records uniform and
   `ERR-BatchTooLarge` (`F-008`) the same under either ruling of #38. Refuse rather than split.
3. **Service leaves are generic.** A `ServiceLeaf` is `kind ‖ id ‖ facts ‖ body`. The store reads
   only the facts, which it indexes; the body is the library's canonical bytes of the record.
   The key is `blake2b-256("aledgent/service-key/v1" ‖ kind ‖ id)[..16]`, and a key held by
   another record is refused as fatal, like an external-id slot collision. The leaf content is
   the tag `0xA5` then the encoding, hashed with the kernel's `SMT_LEAF`.
   *Amended by `F-005` (T-005-12):* the key is now the kind's code, then the first 15 bytes of
   that hash, so every kind is one subtree of the service tree and a witness can show a kind
   whole. A kind is one byte (`ServiceKind` is a code, not a closed list): the eleven named here
   are the first codes of `ledger_core::leaves::kind`.
4. **Facts make the indexes authenticated.** `OwnedBy` (accounts by identity), `Holder`
   (authority holders by account, the store half of `INV-16`) and `Reserves` (reserved totals,
   `REQ-BA-6`) are part of the leaf content, so the service root covers them and the indexes are
   recomputable from leaves alone. A reserved total that would pass `u64::MAX` or go below zero is
   refused.
5. **Encoding `ledger-core`'s records into service leaves: done by `F-005` (T-005-11 to
   T-005-13).** Every collection of the service store is leaves of its own kind
   (`ledger_core::leaves`); a leaf's body is its canonical key then its canonical value
   (`delta::library_leaf`). The library's leaves state **no facts**: the store's owned, holder
   and reserved indexes stay empty for them, and the library answers those questions from its
   own derived leaves (accounts by owner, grants by subject, reserved totals). `TODO(F-011)`:
   facts, if the host's queries are to read them from the leaf store.

### The trees

6. **Only branching nodes are stored.** A tree of `n` leaves stores `n − 1` branch rows of 98 bytes
   (two child references and the two child hashes at the level below the branch). A lone node's
   hash further up is folded through empty siblings when needed. `F-001`'s `FileStore` stored 128
   node rows per leaf (about 18 KB per leaf, `F-002` §6). Roots, `SmtProof`s and `BatchProof`s are
   checked byte for byte against CipherClerk's `SparseMerkleTree` in property tests.
7. **An upsert only; no delete.** The trees have no removal path (`INV-2`), so the compressed
   structure only grows. *Amended by `F-005` (T-005-12) for the service tree:* an entry of one of
   the library's derived indexes that stops holding (a deadline met, a pending change closed) is
   removed. The leaf keeps an emptied row (the empty sentinel as its hash, nothing stored), so
   the root is the one the tree would have without it, reads answer absent, and its facts leave
   the indexes; a removal of an absent leaf changes nothing. Delta records carry removals
   (`RECORD_VERSION` 2). The kernel's trees still have no removal, and the library's records are
   never removed (`REQ-LA-4`).
8. **Leaf rows are `leaf hash ‖ stored bytes`.** A read re-hashes the content and compares it with
   the stored hash (`F-001` NOTES 5's check, made against the row rather than the 128-node path).
   A row altered hash and content together is not caught by a read; it is caught by the
   rebuild's digest comparison and by the commitment checks.

### The applier and the archive

9. **Archive before store.** A commit is checked, staged, appended to the archive and fsynced,
   and only then written to the store. A crash between the two leaves the archive ahead, and
   `BookStore::open` replays it. So the store's own commits may be non-durable
   (`StoreConfig::durability = Visible`) whenever there is an archive; the default stays durable.
10. **A torn final archive entry is cut off on open; nothing else is repaired.** An incomplete
    last entry is an append that never synced, so it was never acknowledged. A complete entry that
    does not decode canonically or whose log hash does not match is `Corrupt`, always.
11. **A store write that fails after the archive append is a committed record.** The store
    reports the commit, enters a `Behind` state and serves nothing until it is reopened, which
    replays the record. Reporting a failure instead would make the vault revert a commit the
    archive holds.
12. **A `roots_after` mismatch stops the applier** (`Incident`) until the store is reopened. The
    stop is not persisted: after a restart the same record is refused again the same way.
13. **Re-applying a record is a no-op** when its log hash is the one the archive holds for that
    sequence; any other record at an applied sequence is a `Conflict`.

### Reads, witness and openings

14. **History is keyed by kernel account**, `kernel account ‖ posting seq ‖ entry index`, with the
    transfer id, side and layer in the row. A ledger account's history in an asset is its
    `KernelAccountSpec::id()`'s; that mapping and query semantics are `F-011`'s.
15. **The witness has two outputs.** CipherClerk's `SuccinctTransitionWitness` (checked against
    CipherClerk's own `verify_transition`) covers a step's chain; the store's own
    `Witness::verify` also runs the step's account creations, as `Book::apply` does, and fails
    closed on any read outside the witnessed set through `F-001`'s poisoning working set.
    The wire carrier into the Agent is `F-005`'s: a caller blob each for the kernel witness
    (the touched leaves and CipherClerk multiproofs) and the service witness
    (`core_binding::service`).
16. **"Sealed" means a separate file, never reachable from the leaf store's API.** The openings
    store is its own redb file with its own backup; a test scans the leaf store's file, every
    archive segment and every record for every blinding and finds none. Encryption at rest is not
    implemented; with backups it is `F-018`'s, or the owner's to rule.
17. **Balance reads are pinned across the two stores** through the openings store's undo record:
    the openings store commits first, so it is at the leaf view's sequence or one ahead, and one
    ahead the row's previous value is in the undo record. Anything else retries.

### Tests

18. **The flat-cost test counts engine work, not time.** Reads and written rows per apply at 100
    and at 10 000 postings, and on an account holding more than 10 000 entries against cold ones.
    Wall time under a parallel test run is noise; `leaf-bench` measures time.
19. **Crashes are injected as panics** at three points of a commit (before the archive, between
    archive and store, after the store but before the caller hears), in test builds only. The
    crash tests run on a shared in-memory redb backend that survives the "process", so they need
    no fsync.
20. **`ledger-kernel` changes, all additive:** the `rules` module is public so every store runs
    the same write rules; the store rule suite is exported behind a new feature `store-suite`
    (dev-dependencies only) and runs against `BookStore`; `WriteRefusal::RecordTooLarge`.
21. **The conformance case** `inv_6_balances_rebuilt_from_the_delta_log_equal_stored_balances__store`
    needs the binding port: it drives the target's own book, rebuilds `F-006`'s store from the
    target's committed delta log alone, and checks every balance and the `INV-6` commitment check
    at the store.

### Added by `F-005` (T-005-12)

22. **`BookStore::apply_service`**: service-tree changes alone, without a delta record, for the
    host of a core that runs inside an Agent. The host keeps that core's service tree in a store
    of its own (`BookStore::projection_in_memory`); the Agent's delta ring and the host's archive
    of its inputs are its committed transitions, and the tree is rebuilt by replaying them. A
    store that holds records refuses it.
23. **`ReadView::service_leaves_of(kind)`**: every leaf of a kind, by a prefix scan of the kind's
    subtree: what the host witnesses when a command reads a kind whole.

## Measurements (T-006-10)

See `notes/MEASUREMENTS-F006.md`.

## Deferred

- `TODO(F-011)`: the library's service leaves state no facts (decision 5).
- `TODO(F-018)`: schedule the commitment checks and page on findings; replicate and back up the
  archive and the openings store; encryption at rest of the openings store (or the owner's ruling).
- `TODO(F-011)`: query semantics over the history and indexes.
- The 150 M-entry run of `NFR-3` on the target machine (see the measurements note).
