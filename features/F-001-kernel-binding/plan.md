<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-001 — Kernel binding

| | |
|---|---|
| Milestone | M-0 — Foundations |
| Track | A |
| Home | `crates/ledger-kernel` |
| Depends on | nothing (CipherClerk at `e9a7b08`) |
| Unblocks | `F-002`, `F-003`, `F-004`, `F-006`, `F-008`, `F-015` (kernel and store cases), `F-005` |
| Realises | `DEC-1` (storage behind a trait, no VOS), `DEC-3` (per-account kernel keys), `DEC-4` (working set in, delta out), `DEC-15` |

## Goal
aledgent can apply balanced postings to one book through CipherClerk, in a plain process, over two
stores: one in memory and one on disk. The store sits behind our own trait, so `F-006` can swap it
without touching the kernel call. The composite root we compute equals CipherClerk's own for the same
kernel calls. Every later feature builds postings through this crate and nothing else.

## Spec traces
- `REQ-KER-1`, `INV-1`: unbalanced postings cannot be built, and the kernel refuses them.
- `REQ-KER-2`: one posting spans several entries and assets, balanced per asset.
- `REQ-KER-3`, `REQ-KER-4`: three balance kinds, and never negative for flagged accounts. This crate provides the mechanism; `F-004` owns the reservation shape.
- `REQ-KER-5`, `INV-2`: append-only, in the kernel and in both stores.
- `REQ-KER-6`: permanent refusal on a repeated submitter key.
- `REQ-KER-7`, `INV-7`: exact integer minor units. No floating point.
- `REQ-KER-10`: all-or-nothing chains.
- `INV-4`: assets kept separate (see Open points).
- `INV-25`: one journal, so one book.
- `REQ-RT-3`, `REQ-RT-6`, `REQ-RT-7`: deterministic apply. Time, blindings and nonces come in as inputs. Openings never enter the book.
- `REQ-PO-2`: a gapless posting sequence.

## Scope
**In:** the CipherClerk pin; kernel account and posting builders; the `LeafStore` trait and its
six sub-trees; an in-memory store and a file-backed store; the working set that the kernel runs over;
the openings vault and oracle; key derivation for kernel accounts; batch apply with chains; the
composite root; classification of kernel results; the committed delta log; test-only raw ports for
`F-015`.

**Out:** the production leaf store, history and indexes, and witness proofs (`F-006`). The VOS Agent
(`F-005`). Who may cause a posting (`F-003`). The reservation shape (`F-004`). Operation states and
mapping to `ERR-*` (`F-007`, `F-008`). Measurement (`F-002`). Disclosure features of the kernel
(`F-017`).

## Design

**1. Pin.** `cipher-clerk` is a git dependency at rev `e9a7b0851f183c14dbd1635dbf0437c86682ad6f`,
the rev both VOS branches pin. It uses `default-features = false`, as the VOS
clerk actors do. The `helpers` feature is a dev-dependency only, for `MemLedger`
as reference. A CI check on the feature graph fails the build if `fmd-experimental`,
`stealth-experimental` or `notes` is enabled (`DEC-11`, `DEF-4`). **Proposal:** a git
pin. *Alternative:* a vendored copy under `vendor/`, which `RISK-3` allows if the pin cannot be held.

**2. The book in kernel terms.** One journal per deployment, bootstrapped off-kernel with
`put_journal`. A second journal write is refused by the store (`INV-25`). A
ledger account holding N assets is N kernel accounts, one per `ledger: u32` tag.
Internal positions are also kernel accounts: one issuance counterparty per asset, and the holding
positions of `F-004`. Every kernel account is credit-normal, so the signed sum per asset is zero
when every posting balances. Flags come from the caller (`F-003`, `F-004`):
`BALANCE_MUST_NOT_BE_NEGATIVE` or none. `ISSUER` is never set, because the kernel never reads it.

```rust
pub struct KernelAccountSpec { role: Role, subject: [u8; 16], asset_tag: u32, flags: AccountFlags }
pub struct PostingDraft {            // only constructor: PostingDraft::balanced(..) -> Result<Self, DraftError>
    meta: PostingMeta,               // posting kind + operation reference (REQ-PO-1)
    entries: Vec<EntryDraft>,        // >= 2; balanced per asset tag; value: NonZeroU64
    closing_debit: bool,             // CLOSING_DEBIT, used by F-004
}
```

**Kernel account ids. Proposal:** the first 16 bytes of `blake2b-256("aledgent/kernel-account/v1" ‖
role ‖ subject ‖ asset_tag)`, deterministic on replay (`REQ-RT-3`). *Alternative:* random ids
supplied as input, plus a mapping table. The builder takes each entry's `ledger` from the account
spec, never from the caller, because the kernel never compares them.

**3. Posting metadata. Proposal.** `tx_template_id` is the 16-byte form of the operation reference.
`user_data` holds a version byte and the posting-kind code (`REQ-PO-1`). `external_id` is
`blake2b-256("aledgent/posting-eid/v1" ‖ operation reference ‖ kind ‖ ordinal)`. A repeated posting of
the same step is then refused by the kernel for ever (`REQ-KER-6`). `F-007` builds
on this for `INV-18`.

**4. The storage trait.** CipherClerk's `LedgerState` is by primary key, infallible, and roots over
six sparse Merkle trees. Our trait is fallible, batch-shaped, and holds the
write rules of `INV-2`:

```rust
pub enum SubTree { Accounts, Transfers, Journals, ExternalIds, Voided, Pending }
pub trait LeafStore {
    type Error: core::fmt::Debug;
    fn subroots(&self) -> Result<[[u8; 32]; 6], Self::Error>;          // composite order
    fn load(&self, keys: &KeySet) -> Result<Loaded, Self::Error>;      // absent keys reported, never dropped
    fn commit(&mut self, seq: u64, d: &BookDelta) -> Result<[[u8; 32]; 6], Self::Error>;
    fn last_committed(&self) -> Result<Option<u64>, Self::Error>;
}
```

| Sub-tree | Key (16 B) | Stored value | Write rule the store enforces |
|---|---|---|---|
| accounts | `AccountId` | rkyv `Account` | insert, then overwrite (balances roll). No delete |
| transfers | `TransferId` | rkyv `Transfer` | insert once. Overwrite refused |
| journals | `JournalId` | rkyv `Journal` | exactly one, at bootstrap. Any later write refused |
| external ids | `external_id_key(eid)` | the full 32-byte id | insert once. A slot holding a different id is a fatal error, never an alias (as clerk-ledger does) |
| voided | `TransferId` | unit | insert once |
| pending | `TransferId` | status code | `Pending`, then `Posted` or `Voided` only. Unused under `DEC-2`, kept so the root stays faithful |

Leaf hashes use CipherClerk's own leaf encoders and `smt_leaf_hash`, at depth 128 with its leaf and
node domains. The root is `composite_root_from_subroots`. There
is no API that removes a leaf. `F-006` implements the same trait.

**5. The working set.** `Loaded` becomes a `WorkingSet`, an overlay that implements `LedgerState`.
Reads come from loaded leaves. Writes go to a `BookDelta`. `root()` returns the root before the
batch. Key discovery walks the batch: every entry's account, every transfer id, external id,
`void_of` and `pending_id`, then the accounts of any referenced transfer. A read of a key that was
not loaded poisons the set. The unit is then discarded with `BindingFault::UnloadedKey`. This
matters because a miss that answered "not seen" would let a duplicate through.

**6. Openings, keys and randomness.** Amounts are Pedersen commitments. The core holds every opening
(`DEC-11`, L0). An `OpeningVault` keeps per (account, layer) plaintext running sums of value and
blinding. It is a private input store, kept apart from the leaf store (`REQ-RT-6`). The oracle
byte-matches on the commitment, like `OpeningsOracle`. The kernel checks
negativity by asking the oracle to open each touched net, so the host must
supply the expected post-state openings. `MemLedger` does this for one transfer at a time. We do it
per chain (step 7).

**Blindings and nonces. Proposal:** a keyed hash over (operation reference, posting ordinal, entry
index) under a private root, and a ChaCha20 RNG seeded per message for signature nonces. Both are
inputs, not host randomness (`REQ-RT-7`). **Keys (`DEC-3`):** `KeyRing` derives each kernel account's
secret, and the registrar's, from a core root that cannot be printed or serialised.

**7. Apply.** `Book::apply(unit, seed_ts) -> Result<Applied, BindingFault>`. A unit is an ordered
list of steps. Each step is `{ creations, chain }`. For each step:

1. Take a savepoint of the working set.
2. Run `apply_account_creations`, then `apply_batch` on the step's chain. All events but the last
   carry `LINKED`.
3. Give each call a seed so that per-event stamps equal those of one batch over all the events
   (`t_i = seed − n + i + 1`).
4. Before the chain runs, register the openings of each flagged account's net after every event, on
   the assumption that the whole chain succeeds.
5. If any event fails, drop the savepoint. The creations of that step go too.

Applying chain by chain is sound because a failed chain rolls back whole. Later steps then see the
real post-state. A property test checks it against a single CipherClerk batch (task T-001-08).

**8. Results.** Each kernel `EventStatus` is classified. The binding never pre-refuses: the kernel
decides, and the binding only labels what the kernel said (`REQ-AR-4`).

| Kernel status | Class | Used by |
|---|---|---|
| `Created` | `Applied { posting_seq }` | all |
| `BalanceMustNotBeNegative` when the plaintext net is < 0 | `Insufficient` | `F-004`, `F-008` → `ERR-InsufficientAvailable` |
| the same when the plaintext net is ≥ 0 | `BindingFault::Oracle` | incident |
| `ExternalIdAlreadyUsed`, `TransferIdAlreadyExists` | `Duplicate` | `F-007` returns the original outcome |
| `DebitAccountAlreadyClosed`, `CreditAccountAlreadyClosed` | `Closed` | `F-004` capture and release guard |
| `LinkedEventFailed`, `LinkedChainOpen` | `ChainAborted(cause)` | all |
| anything else | `BindingFault::Kernel(code)` | incident. The binding never turns it into an `ERR-*` itself. The service chooses the named refusal (`REQ-ER-1`, `F-004` Open points) |

**9. Commit and sequence.** A successful unit yields a `BookDelta`, a vault delta and a gapless
`posting_seq` per applied transfer (`REQ-PO-2`). Kernel stamps are not gapless: failed events use a
slot. The vault commits `seq` first. The store then commits and appends `(seq, delta, root_after)`
to an append-only delta log. On restart the vault rolls back to `store.last_committed()`. The delta
log is the source of truth `DEC-4` names, and the store is a projection rebuilt from it.

**10. Stores.** `MemStore` holds six CipherClerk `SparseMerkleTree`s and a map of leaves.
`FileStore` — **Proposal:** one redb file with a leaf table per sub-tree, a node table keyed by
(sub-tree, depth, prefix) updated along the path on each write, and the delta log, fsynced on every
commit. *Alternative:* SQLite, or an append-only log with an in-memory rebuild. It is the reference
for durability semantics. `F-006` chooses the production engine.

**11. Ports for `F-015`.** A `conformance-ports` feature exposes a raw `KernelPort`, which submits a
hand-built `Transfer`, and a raw `StorePort`, which attempts any write. Production crates must not
enable the feature. A CI check enforces this, because the command surface MUST NOT offer arbitrary
posting (`REQ-CS-5`).

## Test plan
- **`INV-1` (kernel):** through `KernelPort`, an unbalanced transfer, a single-entry transfer and a
  cross-asset transfer that does not balance per asset are refused with `UnbalancedTransfer`. A
  compile-fail test shows that `PostingDraft` cannot be built unbalanced.
- **`INV-2` (kernel and store):** a void is a new transfer. Through `StorePort`, overwriting a
  transfer, external id or voided leaf is refused, on both stores, and again after reopening
  `FileStore`.
- **`INV-3` (kernel part):** a raw overdraw of a flagged account is refused with
  `BalanceMustNotBeNegative`.
- **`INV-4`:** per-asset zero-sum through the kernel. The builder refuses an entry whose asset
  differs from its account's. The layer at which this is enforced is an Open point.
- **`INV-7`:** the `Minor` and `NonZeroU64` types. Clippy denies float arithmetic, and a grep gate
  rejects `f32` and `f64` in `crates/`.
- **`REQ-KER-6`:** a repeated `external_id` is refused, including after a `FileStore` restart.
- **`REQ-KER-10`:** a chain whose last event fails leaves an empty delta, and its creations are
  dropped.
- **Differential property test:** 10 000 random units of creations and chains, applied to `MemStore`
  and `FileStore` and, call by call, to `MemLedger` with a full-knowledge oracle. Results and
  composite roots are equal at every step. They also equal `composite_state_root` over the full leaf
  sets.
- **Crash injection** between the vault commit and the store commit. After recovery, the root and
  the vault agree with the last committed `seq`.
- **Working-set miss:** a batch run with one key withheld poisons the set and commits nothing.

## Exit criterion
From PLAN §4: *kernel conformance tests pass in-process, and the root matches CipherClerk's reference
for the same batch.* Made concrete, in `cargo test` on both stores:

- The `F-015` cases for `INV-1`, `INV-2` (kernel and store), `INV-3` (kernel), `INV-4`, `INV-7` and
  `INV-25` pass.
- The differential test passes for 10 000 cases.
- Committed golden vectors (fixed units with fixed seeds) reproduce their recorded roots.

## Open points
- **`INV-4` layer.** The kernel never compares an entry's currency with its account's. Our builder check is service-level. `REQ-AR-6` requires `INV-4` to be kernel- or
  store-enforced. `RISK-3` plans an upstream patch. Until then the owner chooses between
  two options: carry the one-line check in a pinned fork, or accept that the declaration cannot yet
  claim `INV-4` at kernel strength. Not decided here.
- **Signing inside the core.** This was found in CipherClerk source at the pin. `sign` and `SecretKey` sit behind the `signing` feature, which enables
  `rand_core/getrandom` (`Cargo.toml` features, `src/crypto/mod.rs:19-20`). `DEC-3` needs the core to
  sign. `F-001` signs in-process with a supplied RNG. Whether this builds for the PVM target is for
  `F-002` to confirm and `F-005` to settle, and it may need an upstream split of the feature.
- **Where openings live durably.** They are private inputs (`REQ-RT-6`), yet the core needs them after
  a restart. `F-001` keeps them in a separate vault. Their production home is for `F-005` and `F-006`.
- **Range.** Values are `u64` minor units. `REQ-KER-7` asks for room above the largest holdings in
  §11, and §11 states none. This is a concern for assets with many decimals (notes/FIT §2). `F-003`
  records precision. The owner should confirm that `u64` is enough.
- **Proposals to review:** the git pin, redb, the id and posting-metadata derivations, blinding and
  nonce derivation, and chain-by-chain apply.
- Exposure: `RISK-3` (API churn). `DEP-4` (a later kernel primitive would change `F-004`, not this
  crate's trait).
