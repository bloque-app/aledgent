// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The book: bootstrap, apply, commit (`F-001` §2, §7–§9).
//!
//! [`Book`] ties the pieces together. It holds the leaf store, the openings
//! vault and the core's key ring, and it is the only way postings reach the
//! kernel.
//!
//! **Apply.** A [`Unit`] is an ordered list of [`Step`]s; each step creates
//! some kernel accounts and then runs one chain of postings, all-or-nothing
//! (`REQ-KER-10`). For each step the book takes a savepoint, runs
//! `apply_account_creations` and then `apply_batch` on the chain (every event
//! but the last carries `LINKED`), and drops the savepoint — creations
//! included — if any event fails. Each kernel call gets a seed chosen so that
//! the per-event stamps equal those of one batch over the whole unit. Later
//! steps then see the real post-state of earlier ones.
//!
//! **Results.** The binding never refuses in the kernel's place: the kernel
//! decides, and the binding only labels what it said (`REQ-AR-4`), as in the
//! table of [`EventClass`]. Anything outside that table is a
//! [`BindingFault`]: the whole unit is discarded and nothing is committed.
//!
//! **Commit.** A unit that changed anything yields a `BookDelta`, the vault's
//! changes, and a gapless posting sequence number per applied transfer
//! (`REQ-PO-2`). The vault commits first, then the store commits and appends
//! `(seq, delta, root_after)` to its delta log. On restart, [`Book::open`]
//! rolls the vault back to the store's last committed `seq`.
//!
//! **Batches** (`T-031-19`, `DEC-22` lever 1). Between [`Book::begin_batch`] and
//! [`Book::flush`], each unit is decided exactly as above against the book *and* the units staged
//! before it, but nothing is written: its delta and its vault changes are merged into the batch.
//! [`Book::flush`] commits the whole batch as **one** commit: one vault commit, one store commit,
//! one delta-log record, one root, one fsync. A unit's outcome does not depend on whether it was
//! batched (the kernel never reads the root it decides on), and the merged delta passes the same
//! store rules (`INV-2`, `REQ-PO-2`): what one unit writes never conflicts with what a later unit
//! of the batch writes, because the later one was decided against it. The book's reads of
//! state ([`Book::kernel_account`], [`Book::settled_net`], [`Book::next_posting_seq`]) answer
//! through the batch, as a later unit sees it; [`Book::root`], [`Book::store`] and
//! [`Book::vault`] answer at the last commit, and so does [`Applied::root`] of a staged unit: a
//! caller that needs a root over the batch flushes first.

use alloc::vec::Vec;

use cipher_clerk::kernel::CreateAccount;
use cipher_clerk::state::LedgerState;
use cipher_clerk::{
    Account, AccountId, EventResult, EventStatus, Journal, JournalId, Transfer, TransferId,
    apply_account_creations, apply_batch,
};

use crate::account::KernelAccountSpec;
use crate::build::{BuiltTransfer, build_creation, build_transfer};
use crate::keys::KeyRing;
use crate::openings::{ChainPlan, NetCheck, OpeningBook, simulate_chain};
use crate::posting::PostingDraft;
use crate::store::{
    BookDelta, KeySet, LeafStore, Loaded, LogRecord, PostingRecord, StoreError, SubRoots,
};
use crate::vault::{OpeningVault, Overlay, RowKey, VaultBackend, VaultError};
use crate::working_set::{UnloadedKey, WorkingSet};

use cipher_clerk::crypto::blake2b_256;

/// Domain of the journal-id derivation.
const JOURNAL_ID_DOMAIN: &[u8] = b"aledgent/journal-id/v1";

/// The journal id of the book whose registrar key is `registrar`: one journal
/// per deployment, so one book (`INV-25`).
pub fn journal_id_for(registrar: &cipher_clerk::crypto::AuthKey) -> JournalId {
    let digest = blake2b_256(JOURNAL_ID_DOMAIN, &[&registrar.0]);
    let mut id = [0u8; 16];
    id.copy_from_slice(&digest[..16]);
    JournalId(id)
}

/// One all-or-nothing step: accounts to create, then a chain of postings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Step {
    /// Kernel accounts to create before the chain runs. Each must not exist.
    pub creations: Vec<KernelAccountSpec>,
    /// The postings, applied together or not at all.
    pub chain: Vec<PostingDraft>,
}

/// An ordered list of steps, committed together.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Unit {
    /// The steps, in order.
    pub steps: Vec<Step>,
}

/// What the kernel said about one posting, labelled (`F-001` §8).
///
/// | Kernel status | Class |
/// |---|---|
/// | `Created` | [`EventClass::Applied`] |
/// | `BalanceMustNotBeNegative`, plaintext net < 0 | [`EventClass::Insufficient`] |
/// | `BalanceMustNotBeNegative`, plaintext net ≥ 0 | [`BindingFault::Oracle`] ([`BindingFault::OutOfRange`] past `u64::MAX`) |
/// | `ExternalIdAlreadyUsed`, `TransferIdAlreadyExists`, `TransferAlreadyVoided` | [`EventClass::Duplicate`] |
/// | `DebitAccountAlreadyClosed`, `CreditAccountAlreadyClosed` | [`EventClass::Closed`] |
/// | `LinkedEventFailed` | [`EventClass::ChainAborted`] |
/// | anything else | [`BindingFault::Kernel`] |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventClass {
    /// The posting applied and took this place in the posting sequence.
    Applied {
        /// Its posting sequence number (`REQ-PO-2`).
        posting_seq: u64,
        /// Its kernel transfer.
        transfer: TransferId,
        /// The kernel's stamp.
        timestamp: u64,
    },
    /// A flagged account would have gone negative (`F-004`, `F-008` →
    /// `ERR-InsufficientAvailable`).
    Insufficient {
        /// The first flagged account whose plaintext net went below zero.
        account: AccountId,
    },
    /// The kernel has already accepted this posting (`REQ-KER-6`); `F-007`
    /// returns the original outcome.
    Duplicate,
    /// A kernel account on the posting is closed (`F-004` capture and
    /// release guard).
    Closed,
    /// Another posting of the chain was refused, so this one was not applied.
    ChainAborted {
        /// The position in the chain of the posting that was refused.
        cause: usize,
    },
}

/// What happened to one step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepOutcome {
    /// One class per posting of the chain, in order.
    pub events: Vec<EventClass>,
    /// Whether the step applied: its creations and every posting.
    pub applied: bool,
    /// What the kernel itself returned for the chain, unlabelled: kept for
    /// audit and for differential tests against the kernel's reference.
    pub kernel_results: Vec<EventResult>,
}

/// The result of a unit that did not fault.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    /// One outcome per step.
    pub steps: Vec<StepOutcome>,
    /// The commit sequence number, or `None` if nothing changed.
    pub seq: Option<u64>,
    /// The composite root after the unit.
    pub root: [u8; 32],
}

/// Something the binding cannot label: the unit is discarded, nothing is
/// committed, and the operator is told. Never an `ERR-*` by itself; the
/// service chooses the named refusal (`REQ-ER-1`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BindingFault {
    /// The kernel read a key the working set had not loaded.
    UnloadedKey(UnloadedKey),
    /// The openings and the kernel disagree: the kernel refused a net the
    /// vault says is not negative, rejected an opening, or accepted an event
    /// whose effect the vault could not compute.
    Oracle {
        /// Step index.
        step: usize,
        /// Event index within the step's chain.
        event: usize,
    },
    /// A flagged account's balance would pass `u64::MAX` minor units, beyond
    /// what a kernel opening can express (`REQ-KER-7`, `F-001` Open points).
    /// Refused, never wrapped.
    OutOfRange {
        /// Step index.
        step: usize,
        /// Event index within the step's chain.
        event: usize,
        /// The account whose balance would leave the range.
        account: AccountId,
    },
    /// A kernel status outside the classification table.
    Kernel {
        /// Step index.
        step: usize,
        /// Event index within the step's creations or chain.
        event: usize,
        /// Whether the event was an account creation.
        creation: bool,
        /// What the kernel said.
        status: EventStatus,
    },
    /// The leaf store failed or refused a write.
    Store(StoreError),
    /// The vault failed.
    Vault(VaultError),
    /// The store holds no book yet.
    NotBootstrapped,
    /// The store already holds a book.
    AlreadyBootstrapped,
    /// The store's journal is not the one these keys derive.
    ForeignJournal,
    /// The vault is neither at the store's last commit nor one ahead of it.
    VaultDiverged {
        /// The vault's last commit.
        vault: Option<u64>,
        /// The store's last commit.
        store: Option<u64>,
    },
    /// A vault row does not open the store's commitments.
    OpeningMismatch {
        /// The row.
        row: RowKey,
    },
}

impl From<StoreError> for BindingFault {
    fn from(e: StoreError) -> Self {
        BindingFault::Store(e)
    }
}

impl From<VaultError> for BindingFault {
    fn from(e: VaultError) -> Self {
        BindingFault::Vault(e)
    }
}

/// Label one refused chain event (`F-001` §8). `nets` are the nets the
/// simulation expected the kernel to check for that event.
/// `None` when the status is outside the table: the caller turns that into a
/// [`BindingFault`].
pub fn classify_refusal(
    status: EventStatus,
    nets: &[NetCheck],
    cause: usize,
) -> Option<EventClass> {
    match status {
        EventStatus::BalanceMustNotBeNegative => nets
            .iter()
            .find(|n| n.net < 0)
            .map(|n| EventClass::Insufficient { account: n.account }),
        EventStatus::ExternalIdAlreadyUsed
        | EventStatus::TransferIdAlreadyExists
        | EventStatus::TransferAlreadyVoided => Some(EventClass::Duplicate),
        EventStatus::DebitAccountAlreadyClosed | EventStatus::CreditAccountAlreadyClosed => {
            Some(EventClass::Closed)
        }
        EventStatus::LinkedEventFailed => Some(EventClass::ChainAborted { cause }),
        _ => None,
    }
}

/// A step's kernel events, built before anything runs.
struct BuiltStep {
    creations: Vec<CreateAccount>,
    chain: Vec<BuiltTransfer>,
}

/// The units applied since [`Book::begin_batch`] and not yet committed.
#[derive(Clone, Default)]
struct Staged {
    /// Their deltas, merged in order: what one commit will write.
    delta: BookDelta,
    /// Their vault changes, merged: full sums per row.
    overlay: Overlay,
    /// How many units changed something.
    units: usize,
}

impl Staged {
    fn merge(&mut self, delta: BookDelta, overlay: Overlay) {
        let d = &mut self.delta;
        d.accounts.extend(delta.accounts);
        d.transfers.extend(delta.transfers);
        d.journals.extend(delta.journals);
        d.external_ids.extend(delta.external_ids);
        d.voided.extend(delta.voided);
        d.pending.extend(delta.pending);
        d.postings.extend(delta.postings);
        self.overlay = overlay;
        self.units += 1;
    }
}

/// The store as a staged unit sees it: the last commit, under the batch's writes. Read-only.
struct StagedView<'a, S: LeafStore> {
    store: &'a S,
    staged: &'a BookDelta,
}

impl<S: LeafStore> LeafStore for StagedView<'_, S> {
    fn subroots(&self) -> Result<SubRoots, StoreError> {
        // The roots at the last commit: a working set's root before is read by nothing that
        // decides (module documentation, Batches).
        self.store.subroots()
    }

    fn load(&self, keys: &KeySet) -> Result<Loaded, StoreError> {
        let mut out = self.store.load(keys)?;
        let d = self.staged;
        for (id, slot) in out.accounts.iter_mut() {
            if let Some(a) = d.accounts.get(id) {
                *slot = Some(a.clone());
            }
        }
        for (id, slot) in out.transfers.iter_mut() {
            if let Some(t) = d.transfers.get(id) {
                *slot = Some(t.clone());
            }
        }
        for (id, slot) in out.journals.iter_mut() {
            if let Some(j) = d.journals.get(id) {
                *slot = Some(j.clone());
            }
        }
        for (id, seen) in out.external_ids.iter_mut() {
            *seen |= d.external_ids.contains(id);
        }
        for (id, seen) in out.voided.iter_mut() {
            *seen |= d.voided.contains(id);
        }
        for (id, slot) in out.pending.iter_mut() {
            if let Some(p) = d.pending.get(id) {
                *slot = Some(*p);
            }
        }
        Ok(out)
    }

    fn commit(&mut self, _seq: u64, _delta: &BookDelta) -> Result<SubRoots, StoreError> {
        Err(StoreError::Io("a staged view is read-only".into()))
    }

    fn last_committed(&self) -> Result<Option<u64>, StoreError> {
        self.store.last_committed()
    }

    fn next_posting_seq(&self) -> Result<u64, StoreError> {
        Ok(self.store.next_posting_seq()? + self.staged.postings.len() as u64)
    }

    fn log_record(&self, seq: u64) -> Result<Option<LogRecord>, StoreError> {
        self.store.log_record(seq)
    }
}

/// The book: leaf store, openings vault, key ring and journal.
pub struct Book<S: LeafStore, V: VaultBackend> {
    store: S,
    vault: OpeningVault<V>,
    keys: KeyRing,
    journal: JournalId,
    staged: Option<Staged>,
}

impl<S: LeafStore, V: VaultBackend> core::fmt::Debug for Book<S, V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Book")
            .field("journal", &self.journal)
            .field("vault", &self.vault)
            .finish_non_exhaustive()
    }
}

impl<S: LeafStore, V: VaultBackend> Book<S, V> {
    /// Create the book in an empty store: write its one journal off-kernel,
    /// with the registrar key these keys derive, as commit 0 (`INV-25`).
    pub fn bootstrap(store: S, vault: V, keys: KeyRing) -> Result<Self, BindingFault> {
        if store.last_committed()?.is_some() {
            return Err(BindingFault::AlreadyBootstrapped);
        }
        let vault = OpeningVault::open(vault)?;
        if vault.last_committed().is_some() {
            return Err(BindingFault::VaultDiverged {
                vault: vault.last_committed(),
                store: None,
            });
        }
        let registrar = keys.registrar_key();
        let journal = journal_id_for(&registrar);
        let mut ws = WorkingSet::new(Default::default(), store.root()?);
        let mut none = OpeningBook::new();
        ws.put_journal(Journal::new(journal, registrar, 0), &mut none);
        let delta = ws.into_delta().map_err(BindingFault::UnloadedKey)?;
        let mut book = Self {
            store,
            vault,
            keys,
            journal,
            staged: None,
        };
        book.commit(0, &Overlay::default(), &delta)?;
        Ok(book)
    }

    /// Re-create a book in an empty store and vault from exported leaves (`F-014` T-014-03,
    /// `REQ-RC-3`): `deltas` are committed as commits `0, 1, …` in order, off-kernel, as the
    /// bootstrap writes the journal; the first must hold the book's one journal, the one these
    /// keys derive (`INV-25`). `openings` are the vault's rows, carried by the sealed key-custody
    /// step, and are committed with commit 0.
    ///
    /// Every store rule applies to every delta (`INV-2`, `REQ-KER-6`, `REQ-PO-2`), so the posting
    /// sequence continues exactly where the exported book's did. The caller checks the leaves'
    /// root and the openings before calling: a failure here after commit 0 leaves the store
    /// written, and it must be discarded.
    pub fn import(
        store: S,
        vault: V,
        keys: KeyRing,
        deltas: &[BookDelta],
        openings: &alloc::collections::BTreeMap<RowKey, crate::vault::LayerSums>,
    ) -> Result<Self, BindingFault> {
        if store.last_committed()?.is_some() {
            return Err(BindingFault::AlreadyBootstrapped);
        }
        let vault = OpeningVault::open(vault)?;
        if vault.last_committed().is_some() {
            return Err(BindingFault::VaultDiverged {
                vault: vault.last_committed(),
                store: None,
            });
        }
        let journal = journal_id_for(&keys.registrar_key());
        let first_journal = deltas
            .first()
            .map(|d| d.journals.len() == 1 && d.journals.contains_key(&journal));
        if first_journal != Some(true) || deltas.iter().skip(1).any(|d| !d.journals.is_empty()) {
            return Err(BindingFault::ForeignJournal);
        }
        let mut book = Self {
            store,
            vault,
            keys,
            journal,
            staged: None,
        };
        let none = Overlay::default();
        for (seq, delta) in deltas.iter().enumerate() {
            let overlay = if seq == 0 {
                Overlay {
                    changed: openings.clone(),
                }
            } else {
                none.clone()
            };
            book.commit(seq as u64, &overlay, delta)?;
        }
        Ok(book)
    }

    /// Open an existing book after a restart. If the vault committed a
    /// sequence number the store never did, it is rolled back to the store's.
    pub fn open(store: S, vault: V, keys: KeyRing) -> Result<Self, BindingFault> {
        let last = store.last_committed()?;
        let Some(last) = last else {
            return Err(BindingFault::NotBootstrapped);
        };
        let mut vault = OpeningVault::open(vault)?;
        match vault.last_committed() {
            Some(v) if v == last => {}
            Some(v) if v == last + 1 => vault.revert_last()?,
            v => {
                return Err(BindingFault::VaultDiverged {
                    vault: v,
                    store: Some(last),
                });
            }
        }
        let journal = journal_id_for(&keys.registrar_key());
        let mut keys_wanted = KeySet::default();
        keys_wanted.journals.insert(journal);
        let loaded = store.load(&keys_wanted)?;
        if !matches!(loaded.journals.get(&journal), Some(Some(_))) {
            return Err(BindingFault::ForeignJournal);
        }
        Ok(Self {
            store,
            vault,
            keys,
            journal,
            staged: None,
        })
    }

    /// Start a batch (module documentation, Batches): the units applied from now on are decided
    /// against the book and each other, and committed together by [`Book::flush`]. A batch
    /// already open stays open.
    pub fn begin_batch(&mut self) {
        if self.staged.is_none() {
            self.staged = Some(Staged::default());
        }
    }

    /// Whether a batch is open.
    pub fn batching(&self) -> bool {
        self.staged.is_some()
    }

    /// How many units the open batch holds that changed something.
    pub fn staged_units(&self) -> usize {
        self.staged.as_ref().map_or(0, |s| s.units)
    }

    /// Commit what the open batch holds as one commit; the batch stays open, empty. Returns the
    /// commit's sequence number and root, or `None` when no batch is open or nothing in it
    /// changed the book. On an error nothing of the batch is committed, the batch is closed,
    /// and the book stays at its last commit: the caller decides the batch's units again, or
    /// fails them.
    pub fn flush(&mut self) -> Result<Option<(u64, [u8; 32])>, BindingFault> {
        let Some(staged) = self.staged.take() else {
            return Ok(None);
        };
        if staged.units == 0 {
            self.staged = Some(staged);
            return Ok(None);
        }
        let seq = self.store.last_committed()?.map_or(0, |n| n + 1);
        let root = self.commit(seq, &staged.overlay, &staged.delta)?;
        self.staged = Some(Staged::default());
        Ok(Some((seq, root)))
    }

    /// [`Book::flush`], then close the batch.
    pub fn end_batch(&mut self) -> Result<Option<(u64, [u8; 32])>, BindingFault> {
        let done = self.flush();
        self.staged = None;
        done
    }

    /// Drop the open batch: nothing of it is committed.
    pub fn discard_batch(&mut self) {
        self.staged = None;
    }

    fn view(&self) -> StagedView<'_, S> {
        static EMPTY: BookDelta = BookDelta {
            accounts: alloc::collections::BTreeMap::new(),
            transfers: alloc::collections::BTreeMap::new(),
            journals: alloc::collections::BTreeMap::new(),
            external_ids: alloc::collections::BTreeSet::new(),
            voided: alloc::collections::BTreeSet::new(),
            pending: alloc::collections::BTreeMap::new(),
            postings: Vec::new(),
        };
        StagedView {
            store: &self.store,
            staged: self.staged.as_ref().map_or(&EMPTY, |s| &s.delta),
        }
    }

    /// The posting sequence number the next applied transfer will take, through an open batch.
    pub fn next_posting_seq(&self) -> Result<u64, BindingFault> {
        Ok(self.view().next_posting_seq()?)
    }

    /// The book's one journal.
    pub fn journal(&self) -> JournalId {
        self.journal
    }

    /// The composite root.
    pub fn root(&self) -> Result<[u8; 32], BindingFault> {
        Ok(self.store.root()?)
    }

    /// The last committed sequence number.
    pub fn last_committed(&self) -> Result<Option<u64>, BindingFault> {
        Ok(self.store.last_committed()?)
    }

    /// The leaf store.
    pub fn store(&self) -> &S {
        &self.store
    }

    /// The openings vault.
    pub fn vault(&self) -> &OpeningVault<V> {
        &self.vault
    }

    /// The core's key ring.
    pub fn keys(&self) -> &KeyRing {
        &self.keys
    }

    /// Take the book apart, for a simulated restart.
    pub fn into_parts(self) -> (S, V, KeyRing) {
        (self.store, self.vault.into_backend(), self.keys)
    }

    /// The stored kernel account, if it exists, through an open batch.
    pub fn kernel_account(&self, id: &AccountId) -> Result<Option<Account>, BindingFault> {
        let mut keys = KeySet::default();
        keys.accounts.insert(*id);
        let loaded = self.view().load(&keys)?;
        Ok(loaded.accounts.get(id).cloned().flatten())
    }

    /// The plaintext net of a kernel account on the settled layer, in minor
    /// units, by its normal balance: `None` if the account does not exist.
    pub fn settled_net(&self, id: &AccountId) -> Result<Option<i128>, BindingFault> {
        let Some(account) = self.kernel_account(id)? else {
            return Ok(None);
        };
        let row = RowKey::new(*id, cipher_clerk::Layer::Settled.as_byte());
        let sums = match &self.staged {
            Some(s) => s.overlay.get(&self.vault, &row),
            None => self.vault.sums(&row),
        };
        let net = sums
            .net(account.normal_balance)
            .map_err(|_| BindingFault::OpeningMismatch { row })?;
        Ok(Some(net.0))
    }

    /// Check that every vault row opens the store's commitments for it.
    pub fn verify_openings(&self) -> Result<(), BindingFault> {
        let mut keys = KeySet::default();
        keys.accounts
            .extend(self.vault.rows().map(|(row, _)| row.account));
        let loaded = self.store.load(&keys)?;
        for (row, sums) in self.vault.rows() {
            let Some(Some(account)) = loaded.accounts.get(&row.account) else {
                return Err(BindingFault::OpeningMismatch { row: *row });
            };
            let Some(pair) = account.balances.get(usize::from(row.layer)) else {
                return Err(BindingFault::OpeningMismatch { row: *row });
            };
            if !sums.opens(pair) {
                return Err(BindingFault::OpeningMismatch { row: *row });
            }
        }
        Ok(())
    }

    fn build(&self, unit: &Unit) -> Vec<BuiltStep> {
        unit.steps
            .iter()
            .map(|step| BuiltStep {
                creations: step
                    .creations
                    .iter()
                    .map(|s| build_creation(&self.keys, self.journal, s))
                    .collect(),
                chain: step
                    .chain
                    .iter()
                    .enumerate()
                    .map(|(i, d)| {
                        build_transfer(&self.keys, self.journal, d, i + 1 < step.chain.len())
                    })
                    .collect(),
            })
            .collect()
    }

    /// Apply a unit with the host's time `seed_ts` (`REQ-RT-7`). Steps that
    /// the kernel refuses are reported; steps that apply are committed
    /// together. A [`BindingFault`] discards the whole unit.
    pub fn apply(&mut self, unit: &Unit, seed_ts: u64) -> Result<Applied, BindingFault> {
        let built = self.build(unit);
        let creations: Vec<CreateAccount> = built
            .iter()
            .flat_map(|s| s.creations.iter().cloned())
            .collect();
        let transfers: Vec<Transfer> = built
            .iter()
            .flat_map(|s| s.chain.iter().map(|b| b.transfer.clone()))
            .collect();
        let view = self.view();
        let mut ws = WorkingSet::load_for(&view, &creations, &transfers)?;

        let total = (creations.len() + transfers.len()) as u64;
        let base = seed_ts.saturating_sub(total);
        let mut position: u64 = 0;
        let mut next_posting = view.next_posting_seq()?;
        let mut overlay = self
            .staged
            .as_ref()
            .map(|s| s.overlay.clone())
            .unwrap_or_default();
        let mut oracle = OpeningBook::new();
        let mut postings: Vec<PostingRecord> = Vec::new();
        let mut outcomes = Vec::with_capacity(built.len());

        for (step_index, step) in built.iter().enumerate() {
            let saved_ws = ws.savepoint();
            let saved_overlay = overlay.clone();

            // Creations first.
            let c = step.creations.len() as u64;
            let seed = base.saturating_add(position).saturating_add(c);
            let created = apply_account_creations(&mut ws, &step.creations, &mut oracle, seed);
            position += c;
            if let Some(miss) = ws.miss() {
                return Err(BindingFault::UnloadedKey(miss));
            }
            if let Some((event, r)) = created
                .iter()
                .enumerate()
                .find(|(_, r)| r.status != EventStatus::Created)
            {
                return Err(BindingFault::Kernel {
                    step: step_index,
                    event,
                    creation: true,
                    status: r.status,
                });
            }

            // Then the chain.
            let chain: Vec<Transfer> = step.chain.iter().map(|b| b.transfer.clone()).collect();
            for b in &step.chain {
                for o in &b.openings {
                    oracle.register_opening(o);
                }
            }
            let plan = simulate_chain(&ws, &self.vault, &mut overlay, &mut oracle, &chain);
            let k = chain.len() as u64;
            let seed = base.saturating_add(position).saturating_add(k);
            let results = if chain.is_empty() {
                Vec::new()
            } else {
                apply_batch(&mut ws, &chain, &mut oracle, seed)
            };
            position += k;
            if let Some(miss) = ws.miss() {
                return Err(BindingFault::UnloadedKey(miss));
            }

            match label_chain(step_index, &results, &plan)? {
                None => {
                    let events = results
                        .iter()
                        .zip(&chain)
                        .map(|(r, t)| {
                            let posting_seq = next_posting;
                            next_posting += 1;
                            postings.push(PostingRecord {
                                seq: posting_seq,
                                transfer: t.id,
                            });
                            EventClass::Applied {
                                posting_seq,
                                transfer: t.id,
                                timestamp: r.timestamp,
                            }
                        })
                        .collect();
                    outcomes.push(StepOutcome {
                        events,
                        applied: true,
                        kernel_results: results,
                    });
                }
                Some(events) => {
                    ws.rollback(saved_ws);
                    overlay = saved_overlay;
                    outcomes.push(StepOutcome {
                        events,
                        applied: false,
                        kernel_results: results,
                    });
                }
            }
        }

        let mut delta = ws.into_delta().map_err(BindingFault::UnloadedKey)?;
        delta.postings = postings;
        if delta.is_empty() {
            return Ok(Applied {
                steps: outcomes,
                seq: None,
                root: self.store.root()?,
            });
        }
        let seq = self.store.last_committed()?.map_or(0, |n| n + 1);
        if let Some(staged) = self.staged.as_mut() {
            staged.merge(delta, overlay);
            return Ok(Applied {
                steps: outcomes,
                seq: Some(seq),
                root: self.store.root()?,
            });
        }
        let root = self.commit(seq, &overlay, &delta)?;
        Ok(Applied {
            steps: outcomes,
            seq: Some(seq),
            root,
        })
    }

    /// Commit in the order of `F-001` §9: vault first, then the store. If the
    /// store fails, the vault is reverted so both stay at the last commit.
    fn commit(
        &mut self,
        seq: u64,
        overlay: &Overlay,
        delta: &BookDelta,
    ) -> Result<[u8; 32], BindingFault> {
        self.vault.commit(seq, &overlay.changed)?;
        match self.store.commit(seq, delta) {
            Ok(sub) => Ok(crate::store::composite_root_in(self.store.version(), &sub)),
            Err(e) => {
                self.vault.revert_last()?;
                Err(BindingFault::Store(e))
            }
        }
    }
}

/// Label a chain's results. `Ok(None)`: every event applied. `Ok(Some(..))`:
/// the chain was refused, with one class per event.
fn label_chain(
    step: usize,
    results: &[EventResult],
    plan: &ChainPlan,
) -> Result<Option<Vec<EventClass>>, BindingFault> {
    let failed = results.iter().position(|r| {
        r.status != EventStatus::Created && r.status != EventStatus::LinkedEventFailed
    });
    let Some(cause) = failed else {
        if let Some(event) = plan.unknown_from {
            // The kernel accepted an event whose effect the vault cannot
            // compute: the vault would diverge from the book.
            return Err(BindingFault::Oracle { step, event });
        }
        return Ok(None);
    };
    let nets = plan.nets.get(cause).map(Vec::as_slice).unwrap_or(&[]);
    let class =
        classify_refusal(results[cause].status, nets, cause).ok_or_else(|| {
            match results[cause].status {
                EventStatus::BalanceMustNotBeNegative => {
                    match nets.iter().find(|n| n.net > i128::from(u64::MAX)) {
                        Some(n) => BindingFault::OutOfRange {
                            step,
                            event: cause,
                            account: n.account,
                        },
                        None => BindingFault::Oracle { step, event: cause },
                    }
                }
                EventStatus::BlindingMismatch => BindingFault::Oracle { step, event: cause },
                status => BindingFault::Kernel {
                    step,
                    event: cause,
                    creation: false,
                    status,
                },
            }
        })?;
    Ok(Some(
        (0..results.len())
            .map(|i| {
                if i == cause {
                    class
                } else {
                    EventClass::ChainAborted { cause }
                }
            })
            .collect(),
    ))
}

#[cfg(feature = "conformance-ports")]
pub mod ports;

#[cfg(test)]
mod tests;

#[cfg(all(test, feature = "std"))]
mod batch_tests;

#[cfg(all(test, feature = "std"))]
mod crash_tests;

#[cfg(all(test, feature = "std"))]
mod differential;

#[cfg(all(test, feature = "std"))]
mod cost_tests;
