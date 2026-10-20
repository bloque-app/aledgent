// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The service store: the core's hot state for this feature (`DEC-1`, `DEC-4`).
//!
//! It holds the one asset registry, identities, ledger accounts, credential sets, grants, window
//! counters, and the attribution record of every accepted operation. The kernel's leaves live in
//! `F-001`'s leaf store; this is the rest of the book's state.
//!
//! Every change is a [`Write`], applied in all-or-nothing batches by [`State::commit`]. The store
//! enforces its own rules on every write, whatever the caller, so they hold for any change to the
//! application above it (the **store** layer of SPEC §9):
//!
//! - **One registry** (`INV-24`). The store has exactly one registry slot, created with it. An asset
//!   is registered once, with one precision and one kernel tag, and only its status may change,
//!   from active to deprecated. A precision above [`MAX_PRECISION`](crate::asset::MAX_PRECISION)
//!   is refused (`INV-7`).
//! - **Minted identifiers only** (`INV-17`). Every stored identity, account, credential and grant
//!   carries the operation that minted it, and the store refuses a record whose identifier is not
//!   that minting. No identifier can carry caller text.
//! - **Nothing is deleted or rewritten** (`REQ-LA-4`). Records are inserted once. Only statuses
//!   change: `Closed` and `Removed` are absorbing, and a grant is revoked at most once.
//! - **One issuance counterparty.** At most one `Issuance` account exists.
//! - **Bounds hold here too.** A window charge that would pass its limit, or overflow, is refused.
//! - **Attribution** (`INV-20`). An operation record names at least one authority, and an
//!   operation reference is recorded once. A transition of an existing operation (a capture or a
//!   release, `F-004`) is recorded beside it, once per kind, attributed the same way.
//! - **Reservations** (`F-004`, `INV-14`). The open-reservation index changes only by
//!   [`IndexChange`]s, under its own rules: a reservation is opened once and closed once, and a
//!   capture never clears more than was reserved ([`crate::reservation`]).
//! - **Operations** (`F-007`, `INV-15`, `INV-18`, `INV-21`). Operation records change only by
//!   [`OpWrite`]s, under the rules of [`crate::lifecycle::store`]: compare-and-set on the state
//!   left, the transition table, terminal states absorbing, a deadline on every open record, and
//!   one application per transition key.
//! - **Settlement** (`F-010`, `INV-12`). The settlement store changes only by
//!   [`SettlementWrite`]s, under the rules of [`crate::settlement::store`]: a coordinate is
//!   ingested once, and at most one record per position is ever final.
//! - **Audit** (`F-013`, `REQ-AT-1`). Every batch that carries an attribution record also emits
//!   its audit record, in the same batch, onto a hash chain ([`crate::audit::emit`]). A batch
//!   cannot commit without it.
//! - **Operator records** (`F-013`). Operator-surface grants, proposed reversals, incidents and
//!   the adapter's declared support change only by [`OpsWrite`]s, under the rules of
//!   [`crate::operator`]: a reversal is never approved by its proposer (`REQ-OS-3`).
//! - **Postings** (`F-011`, `INV-2`, `REQ-PO-2`). The posting journal changes only by
//!   [`PostingWrite`]s, under the rules of [`crate::query::journal`]: appended in sequence, never
//!   back in time, and only over accounts, reservations and assets the store holds.
//!
//! Its durable home is `F-006`'s service tree: every collection is leaves ([`crate::leaves`]),
//! and the core Agent holds only the tree's root (`F-005`). A store built by [`State::new`] or an
//! import is whole; one built by [`State::partial`] holds only the leaves a witness gave it.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use crate::account::{AccountKind, AccountStatus, Custody, LedgerAccount, Time};
use crate::asset::{
    Asset, AssetBinding, AssetId, AssetKind, AssetRegistry, AssetStatus, Minor, RegistryError,
};
use crate::audit::emit::{self, AuditUndo};
use crate::audit::{AuditLog, AuditNote};
use crate::authority::{
    AuthorityUsed, Charge, Grant, GrantKind, MAX_CHAIN, OperationKind, Subject, WindowKey,
};
use crate::custody::store::{CustodyRefusal, CustodyState, CustodyUndo, CustodyWrite};
use crate::id::{AccountId, CredentialId, GrantId, IdentityId, Mint, OperationId};
use crate::identity::{Identity, IdentityStatus};
use crate::leaves::{Derived, Leaves, Slot, kind};
use crate::lifecycle::keys::TransitionKey;
use crate::lifecycle::store::{
    OpRefusal, OpUndo, OpWrite, Operation, Operations, TransitionRecord, TransitionWrite,
};
use crate::opening::{OpeningRefusal, OpeningUndo, OpeningWrite, OpeningsState};
use crate::operator::{OpsRefusal, OpsState, OpsUndo, OpsWrite};
use crate::principal::{Credential, CredentialStatus};
use crate::query::journal::{self, Journal, JournalRefusal, JournalUndo, PostingWrite};
use crate::reservation::{self, IndexChange, IndexRefusal, ReservationIndex};
use crate::settlement::store::{
    SettlementRefusal, SettlementStore, SettlementUndo, SettlementWrite,
};

/// Which command an operation record is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Command {
    /// The book's genesis: the registry, the operator's identity, the issuance counterparty.
    Genesis,
    /// `open_identity`.
    OpenIdentity,
    /// A change of an identity's status.
    SetIdentityStatus,
    /// `open_account`.
    OpenAccount,
    /// `close_account`.
    CloseAccount,
    /// `grant_authority`.
    GrantAuthority,
    /// `revoke_authority`.
    RevokeAuthority,
    /// `issue`.
    Issue,
    /// `redeem`.
    Redeem,
    /// `transfer`, one leg or many (`F-008`).
    Transfer,
    /// `reserve` (`F-004`).
    Reserve,
    /// `capture`, a transition of a reserved operation (`F-004`).
    Capture,
    /// `release`, a transition of a reserved operation (`F-004`).
    Release,
    /// `withdraw`: an outbound leg, reserved (`F-010`).
    Withdraw,
    /// `submit_outbound`, a transition of a withdrawal (`F-010`).
    SubmitOutbound,
    /// The deployment's settlement configuration (`F-010`).
    ConfigureSettlement,
    /// Grant operator-surface authority (`F-013`).
    GrantOperator,
    /// Revoke operator-surface authority (`F-013`).
    RevokeOperator,
    /// `resolve_operation` (`F-013`).
    ResolveOperation,
    /// Resume an operation past its deadline (`F-007` §9, on `CTR-OPS` by `F-013`).
    Resume,
    /// `propose_reversal` (`F-013`).
    ProposeReversal,
    /// `approve_reversal` (`F-013`).
    ApproveReversal,
    /// `freeze_account` (`F-013`).
    FreezeAccount,
    /// `unfreeze_account` (`F-013`).
    UnfreezeAccount,
    /// `register_asset` (`F-013`).
    RegisterAsset,
    /// Deprecate an asset (`F-013`).
    DeprecateAsset,
    /// `bind_asset` (`F-029`, `DEC-20`, `REQ-AS-8`): bind a book-only asset to the settlement layer.
    BindAsset,
    /// Apply one chunk of the seed genesis committed (`F-036`, `REQ-RC-6`); also the record of the
    /// seed's opening reference.
    ApplyChainSpec,
    /// Grant issuance authority on the operator surface (`F-013`).
    GrantIssuance,
    /// Revoke issuance authority on the operator surface (`F-013`).
    RevokeIssuance,
    /// `declare_adapter_support` (`F-013`).
    DeclareAdapterSupport,
    /// `register_credential` (`F-012`).
    RegisterCredential,
    /// `remove_credential` (`F-012`).
    RemoveCredential,
    /// `delegate_custody` (`F-012`).
    DelegateCustody,
    /// `reclaim_custody` (`F-012`).
    ReclaimCustody,
    /// `initiate_recovery`, on the operator surface (`F-012`).
    InitiateRecovery,
    /// `cancel_recovery`: a pending recovery or succession cancelled (`F-012`).
    CancelRecovery,
    /// `complete_recovery`, on the operator surface (`F-012`).
    CompleteRecovery,
    /// `name_successor` (`F-012`).
    NameSuccessor,
    /// `claim_succession`, either phase (`F-012`).
    ClaimSuccession,
    /// Import of a whole export into an empty deployment (`F-014`, `REQ-RC-3`).
    ImportBook,
    /// `load_opening_balances` (`F-014`, `REQ-RC-4`).
    LoadOpeningBalances,
    /// `anchor_checkpoint`, on the operator surface (`F-017`, `REQ-PR-1`).
    AnchorCheckpoint,
    /// `compare_settlement`, on the operator surface (`F-017`, `REQ-PR-5`).
    CompareSettlement,
    /// `issue_disclosure` (`F-017`, `CTR-DSC`, `REQ-DS-5`).
    IssueDisclosure,
    /// `submit_evidence`: evidence for a `Needs-Review` operation, for the settlement adapter to
    /// verify (`F-013`, `REQ-RC-5` step 2).
    SubmitEvidence,
    /// `import_credentials`, on the operator surface (`F-028`, `REQ-MG-7`).
    ImportCredentials,
    /// `register_integrating_party`, on the operator surface (`REQ-ID-7`, amendment 0011).
    RegisterIntegratingParty,
    /// `tie_principal`, on the operator surface (`REQ-ID-7`, amendment 0011).
    TiePrincipal,
    /// `register_alias`, on the operator surface (`REQ-RC-7`, amendment 0024).
    RegisterAlias,
    /// `reschedule_upgrade`: the way out of a scheduled upgrade, moved (`REQ-RT-20`, `T-037-15`).
    RescheduleUpgrade,
    /// `cancel_upgrade`: the way out of a scheduled upgrade, cancelled (`REQ-RT-20`, `T-037-15`).
    CancelUpgrade,
    /// `upgrade`: the writer's code is to change to a code version from an input
    /// (`REQ-RT-16`, `T-037-04`).
    Upgrade,
}

/// The attribution of one accepted command (`REQ-AU-7`, `REQ-PN-1`, `INV-20`). For an operation
/// with a lifecycle, each of its transitions also records the authorities it relied on
/// ([`crate::lifecycle::store::TransitionRecord`]); `F-013` builds the audit record from both.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationRecord {
    /// The operation.
    pub op: OperationId,
    /// What it was.
    pub command: Command,
    /// Every authority it relied on, with the principal that used it. Never empty.
    pub authorities: Vec<AuthorityUsed>,
    /// The ledger accounts it touched.
    pub accounts: Vec<AccountId>,
    /// The posting sequence numbers it wrote (`REQ-PO-2`).
    pub postings: Vec<u64>,
    /// When it was accepted.
    pub at: Time,
}

/// The book's standing roles, set once at genesis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Roles {
    /// The operator's organisation identity. It alone administers identities (`DEP-8`) and grants
    /// operator and issuance authority.
    pub operator: IdentityId,
    /// The issuance counterparty (`REQ-AS-6`).
    pub issuance: AccountId,
}

/// One change to the store.
#[derive(Clone)]
pub enum Write {
    /// Register an asset book-only, with exactly one precision (`REQ-AS-7`).
    RegisterAsset {
        /// Its id.
        id: AssetId,
        /// Its precision.
        precision: u8,
        /// Its kind.
        kind: AssetKind,
    },
    /// Move an asset's binding to the settlement layer (`REQ-AS-8`): asked for, final, or
    /// refused by the layer.
    AssetBinding {
        /// The asset.
        id: AssetId,
        /// Where its binding moves.
        binding: AssetBinding,
    },
    /// Deprecate an asset.
    DeprecateAsset(AssetId),
    /// Insert an identity.
    PutIdentity(Identity),
    /// Change an identity's status.
    IdentityStatus(IdentityId, IdentityStatus),
    /// Insert a ledger account.
    PutAccount(LedgerAccount),
    /// Change an account's status.
    AccountStatus(AccountId, AccountStatus),
    /// Insert a credential into its identity's set.
    PutCredential(Credential),
    /// Change a credential's status.
    CredentialStatus(CredentialId, CredentialStatus),
    /// Insert a grant.
    PutGrant(Grant),
    /// Revoke a grant.
    Revoke(GrantId, Time),
    /// Add to a window counter.
    Charge(Charge),
    /// Record an accepted operation.
    PutRecord(OperationRecord),
    /// Record an accepted transition of an operation already recorded (`F-004`).
    PutTransition(OperationRecord),
    /// Change the open-reservation index (`F-004`).
    Reservation(IndexChange),
    /// Set the book's roles.
    SetRoles(Roles),
    /// Change the operation store (`F-007`).
    Op(OpWrite),
    /// Change the settlement store (`F-010`).
    Settlement(SettlementWrite),
    /// Add to the batch's audit record, or record one of its own (`F-013`).
    Audit(AuditNote),
    /// Change the operator records (`F-013`).
    Ops(OpsWrite),
    /// Record a posting the kernel step applies, in plaintext (`F-011`).
    Posting(PostingWrite),
    /// Change custody, an owner, a pending recovery or succession, a named successor, or put a
    /// custody notice in the outbox (`F-012`).
    Custody(CustodyWrite),
    /// Open the opening position of an asset: an `Issuance` account owned by the operator's
    /// organisation identity, named as that asset's opening position (`F-014`, `REQ-RC-4`).
    OpeningPosition {
        /// The asset.
        asset: AssetId,
        /// The account.
        account: LedgerAccount,
    },
    /// Change the openings record (`F-014`).
    Opening(OpeningWrite),
    /// Change the anchor store: checkpoints, their publication, comparisons and issued
    /// disclosures (`F-017`).
    Anchor(crate::anchor::AnchorWrite),
    /// Change the seed's record: genesis's commitment, a chunk's progress, an alias (`F-036`).
    ChainSpec(crate::chainspec::SpecWrite),
}

/// Why the store refused a write. The whole batch is refused and nothing changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreRefusal {
    /// The registry refused the change (`INV-24`, `INV-7`).
    Registry(RegistryError),
    /// An identifier is not the minting of the operation recorded beside it (`INV-17`).
    NotMinted,
    /// A record with this identifier already exists: records are inserted once.
    Exists,
    /// The record the write names does not exist.
    Missing,
    /// A record it refers to (owner, identity, parent, principal) does not exist.
    Dangling,
    /// A status change out of an absorbing status, or a second revocation.
    Absorbing,
    /// A second issuance counterparty, or a second setting of the roles.
    Single,
    /// A grant whose kind, subject, parent or state is inconsistent.
    MalformedGrant,
    /// A credential scoped to an account its identity does not own.
    ForeignScope,
    /// A window charge past its limit, or past `u64::MAX`.
    BoundExceeded,
    /// An operation record with no authority (`INV-20`).
    Unattributed,
    /// The open-reservation index refused the change (`F-004`, `INV-14`).
    Reservation(IndexRefusal),
    /// The operation store refused the change (`F-007`, `INV-15`, `INV-18`, `INV-21`).
    Operation(OpRefusal),
    /// The settlement store refused the change (`F-010`, `INV-12`).
    Settlement(SettlementRefusal),
    /// The operator records refused the change (`F-013`).
    Ops(OpsRefusal),
    /// The posting journal refused the change (`F-011`).
    Journal(JournalRefusal),
    /// The custody records refused the change (`F-012`).
    Custody(CustodyRefusal),
    /// The openings record refused the change (`F-014`).
    Opening(OpeningRefusal),
    /// The anchor store refused the change (`F-017`).
    Anchor(crate::anchor::AnchorRefusal),
    /// The seed's record refused the change (`F-036`).
    ChainSpec(crate::chainspec::SpecRefusal),
}

/// How to undo one applied write.
/// Whether a posting of `kind` settles a leg through the issuance counterparty: what the positions'
/// tally counts (`REQ-PR-5`).
fn is_settling(kind: ledger_kernel::posting::PostingKind) -> bool {
    matches!(
        kind,
        ledger_kernel::posting::PostingKind::Issue | ledger_kernel::posting::PostingKind::Redeem
    )
}

/// What `lines` moved through the account `issuance`, per line, as value held on the layer: the
/// negation of each line's change.
fn held_by(lines: &[journal::Line], issuance: &AccountId) -> Vec<(AssetId, i128)> {
    lines
        .iter()
        .filter(|l| l.account == *issuance)
        .map(|l| (l.asset, -l.delta()))
        .collect()
}

enum Undo {
    Unregister(AssetId),
    AssetBinding(AssetId, AssetBinding),
    AssetStatus(AssetId, AssetStatus),
    RemoveIdentity(IdentityId),
    RestoreIdentity(Identity),
    RemoveAccount(AccountId),
    /// Take an account reference back: what it named before, if anything.
    UnreferAccount([u8; 32], Option<AccountId>),
    /// Several, taken back last first.
    Many(Vec<Undo>),
    RestoreAccount(LedgerAccount),
    RemoveCredential(CredentialId),
    RestoreCredential(Credential),
    RemoveGrant(GrantId),
    RestoreGrant(Grant),
    Window(WindowKey, Option<Minor>),
    RemoveRecord(OperationId),
    PopTransition(OperationId),
    Reservation(reservation::IndexUndo),
    ClearRoles,
    Op(OpUndo),
    OpRetired(OpUndo, OperationId),
    Settlement(SettlementUndo),
    Audit(AuditUndo),
    Ops(OpsUndo),
    Nothing,
    Journal(JournalUndo),
    Custody(CustodyUndo),
    Owner(LedgerAccount),
    CustodySwitch(LedgerAccount, CustodyUndo),
    Opening(OpeningUndo),
    Position(AccountId, OpeningUndo),
    Anchor(crate::anchor::AnchorUndo),
    ChainSpec(crate::chainspec::SpecUndo),
}

/// A batch applied by [`State::commit_undoable`], and how to take it back while nothing else has
/// been written since.
pub(crate) struct Committed(Vec<Undo>);

/// The service store. See the module documentation for its rules.
pub struct State {
    registry: Slot<AssetRegistry, { kind::REGISTRY }>,
    identities: Leaves<IdentityId, Identity, { kind::IDENTITY }>,
    accounts: Leaves<AccountId, LedgerAccount, { kind::LEDGER_ACCOUNT }>,
    by_owner: Leaves<IdentityId, BTreeSet<AccountId>, { kind::ACCOUNTS_BY_OWNER }>,
    /// The accounts in owned custody each identity owns, closed ones included: derived from
    /// `accounts`, never exported, and absent for an identity with none. Whether an identity holds
    /// an account in owned custody (amendment 0025 R2, `REQ-MG-8`) is one point read of it, not a
    /// read of every account the identity owns: an identity may own hundreds of accounts, each a
    /// leaf the Agent would be given (`REQ-RT-1`; a verify-genesis halt of 2026-10-11).
    owned_custody: Leaves<IdentityId, BTreeSet<AccountId>, { kind::OWNED_CUSTODY_BY_OWNER }>,
    credentials: Leaves<CredentialId, Credential, { kind::CREDENTIAL }>,
    /// Each identity's credential set, by id: derived from `credentials`, changed in the same
    /// write, never exported (`F-012` follow-up, issue #50).
    sets: Leaves<IdentityId, BTreeSet<CredentialId>, { kind::CREDENTIAL_SET }>,
    grants: Leaves<GrantId, Grant, { kind::GRANT }>,
    /// The grants each identity holds: derived from `grants` for queries only, not a leaf. A
    /// principal may hold a grant over each of a hundred thousand accounts (amendment 0025 R4),
    /// so a leaf of them all would bring the Agent every one of its ids with each grant it is
    /// given; a command reads the grants over its subject instead ([`State::held`]).
    by_principal: Derived<BTreeMap<IdentityId, BTreeSet<GrantId>>>,
    /// The grants over each account (`Some`), and over every delegated account (`None`):
    /// derived from `grants`, never exported, so a custody change reads the grants over its
    /// account, not every grant of the book (`F-005`).
    by_subject: Leaves<Option<AccountId>, BTreeSet<GrantId>, { kind::GRANTS_BY_SUBJECT }>,
    /// The grants over each asset and each identity: derived from `grants`, never exported, so
    /// an issuance or an account opening reads the grants over its asset or its owner, not every
    /// grant its principal holds (`REQ-RT-1`; a halt of 2026-10-10).
    by_asset_or_identity: Leaves<Subject, BTreeSet<GrantId>, { kind::GRANTS_BY_ASSET_OR_IDENTITY }>,
    /// The ledger account each account reference names (`REQ-SL-7`): derived from `accounts`,
    /// never exported, so an observation reads the account it names, not every account
    /// (`F-005`). The issuance counterparty and the opening positions are never named.
    by_ref: Leaves<[u8; 32], AccountId, { kind::ACCOUNT_BY_REFERENCE }>,
    windows: Leaves<WindowKey, Minor, { kind::WINDOW }>,
    records: Leaves<OperationId, OperationRecord, { kind::ATTRIBUTION }>,
    transitions: Leaves<OperationId, Vec<OperationRecord>, { kind::ATTRIBUTED_TRANSITIONS }>,
    reservations: ReservationIndex,
    roles: Slot<Option<Roles>, { kind::ROLES }>,
    operations: Operations,
    settlement: SettlementStore,
    audit: AuditLog,
    ops: OpsState,
    journal: Journal,
    custody: CustodyState,
    openings: OpeningsState,
    anchoring: crate::anchor::AnchorStore,
    /// The seed's record (`F-036`).
    chainspec: crate::chainspec::SpecState,
    /// While a seed chunk is being applied: the operator's seeding passes the seal's guard
    /// (`crate::principal`). Never stored: it is set and cleared inside one call.
    applying_spec: bool,
    /// While a seed chunk is being applied: what every commit did, to take the whole chunk back
    /// if one of its records is refused. Never stored.
    spec_undo: Option<Vec<Undo>>,
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

mod custody;

/// A grant's key in the grants-by-subject index: its account, or `None` for every delegated
/// account; a grant over an asset or an identity is in [`asset_or_identity_key`]'s index instead.
fn subject_key(g: &Grant) -> Option<Option<AccountId>> {
    match g.subject() {
        Subject::Account(a) => Some(Some(a)),
        Subject::DelegatedAccounts => Some(None),
        Subject::Asset(_) | Subject::Identity(_) => None,
    }
}

/// Take `g` out of the index of the grants each identity holds.
fn unindex_holder(index: &mut Derived<BTreeMap<IdentityId, BTreeSet<GrantId>>>, g: &Grant) {
    let index = index.write();
    if let Some(set) = index.get_mut(&g.principal()) {
        set.remove(&g.id());
        if set.is_empty() {
            index.remove(&g.principal());
        }
    }
}

/// A grant's key in the index of grants over an asset or an identity: its subject; a grant over
/// an account is in [`subject_key`]'s index instead.
fn asset_or_identity_key(g: &Grant) -> Option<Subject> {
    match g.subject() {
        s @ (Subject::Asset(_) | Subject::Identity(_)) => Some(s),
        Subject::Account(_) | Subject::DelegatedAccounts => None,
    }
}

fn check_mint<T: PartialEq>(id: T, minted: Mint, mint: fn(&OperationId, u32) -> T) -> bool {
    id == mint(&minted.op, minted.ordinal)
}

impl State {
    /// An empty store, with its one, empty registry.
    pub fn new() -> State {
        State {
            registry: Slot::default(),
            identities: Leaves::new(),
            accounts: Leaves::new(),
            by_owner: Leaves::new(),
            owned_custody: Leaves::new(),
            credentials: Leaves::new(),
            sets: Leaves::new(),
            grants: Leaves::new(),
            by_principal: Derived::default(),
            by_subject: Leaves::new(),
            by_asset_or_identity: Leaves::new(),
            by_ref: Leaves::new(),
            windows: Leaves::new(),
            records: Leaves::new(),
            transitions: Leaves::new(),
            reservations: ReservationIndex::new(),
            roles: Slot::default(),
            operations: Operations::new(),
            settlement: SettlementStore::new(),
            audit: AuditLog::new(),
            ops: OpsState::new(),
            journal: Journal::new(),
            custody: CustodyState::new(),
            openings: OpeningsState::new(),
            anchoring: crate::anchor::AnchorStore::new(),
            chainspec: crate::chainspec::SpecState::new(),
            applying_spec: false,
            spec_undo: None,
        }
    }

    /// Apply a batch, all or nothing.
    pub fn commit(&mut self, writes: &[Write]) -> Result<(), StoreRefusal> {
        let Committed(undo) = self.commit_undoable(writes)?;
        if let Some(log) = self.spec_undo.as_mut() {
            log.extend(undo);
        }
        Ok(())
    }

    /// Start applying a seed chunk (`F-036`): the seal's guard lets the operator's seeding
    /// through, and every commit is kept so that the chunk can be taken back whole.
    pub(crate) fn begin_spec_chunk(&mut self) {
        self.applying_spec = true;
        self.spec_undo = Some(Vec::new());
    }

    /// End a seed chunk: keep what it committed, or take all of it back.
    pub(crate) fn end_spec_chunk(&mut self, keep: bool) {
        self.applying_spec = false;
        if let Some(undo) = self.spec_undo.take()
            && !keep
        {
            self.rollback(undo);
        }
    }

    /// Whether the book refuses every command but its seeding (`F-036`, `REQ-RC-6`): genesis
    /// committed a seed and its last chunk has not sealed it. Reading it is a witnessed read of the
    /// seed's record, so the core Agent decides it on the same leaf as the host.
    pub fn spec_open(&self) -> bool {
        !self.applying_spec && self.chainspec.progress().is_some_and(|p| p.open())
    }

    /// The seed's record.
    pub fn chainspec(&self) -> &crate::chainspec::SpecState {
        &self.chainspec
    }

    /// Apply a batch, all or nothing, keeping what it takes to undo it: the lifecycle commits
    /// its service-store writes before the kernel step, and takes them back if the step does not
    /// apply (`F-007` §6).
    pub(crate) fn commit_undoable(&mut self, writes: &[Write]) -> Result<Committed, StoreRefusal> {
        self.commit_faulty(writes, None)
            .map_err(|e| e.unwrap_or(StoreRefusal::Operation(OpRefusal::Missing)))
    }

    /// [`State::commit_undoable`], with a fault injected after `fault` writes have applied: the
    /// batch is rolled back and `Err(None)` returned. For the fault-injection tests of `F-007`.
    pub(crate) fn commit_faulty(
        &mut self,
        writes: &[Write],
        fault: Option<usize>,
    ) -> Result<Committed, Option<StoreRefusal>> {
        let mut undo = Vec::with_capacity(writes.len());
        for (i, w) in writes.iter().enumerate() {
            if fault == Some(i) {
                self.rollback(undo);
                return Err(None);
            }
            match self.apply(w) {
                Ok(u) => undo.push(u),
                Err(e) => {
                    self.rollback(undo);
                    return Err(Some(e));
                }
            }
        }
        // The batch's audit record, emitted in the same batch (`F-013` §2, `REQ-AT-1`).
        match emit::derive(writes) {
            Ok(drafts) => undo.extend(drafts.into_iter().map(|d| Undo::Audit(self.audit.push(d)))),
            Err(e) => {
                self.rollback(undo);
                return Err(Some(e));
            }
        }
        if fault == Some(writes.len()) {
            self.rollback(undo);
            return Err(None);
        }
        Ok(Committed(undo))
    }

    /// Take back a batch [`State::commit_undoable`] applied. Nothing may have been written since.
    pub(crate) fn undo(&mut self, c: Committed) {
        self.rollback(c.0);
    }

    /// Whether a batch would commit, without changing anything.
    pub fn check(&mut self, writes: &[Write]) -> Result<(), StoreRefusal> {
        let mut undo = Vec::with_capacity(writes.len());
        let mut result = Ok(());
        for w in writes {
            match self.apply(w) {
                Ok(u) => undo.push(u),
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        self.rollback(undo);
        result?;
        emit::derive(writes).map(|_| ())
    }

    fn rollback(&mut self, undo: Vec<Undo>) {
        for u in undo.into_iter().rev() {
            match u {
                Undo::Unregister(id) => self.registry.get_mut().unregister_last(&id),
                Undo::AssetBinding(id, before) => {
                    self.registry.get_mut().restore_binding(&id, before)
                }
                Undo::AssetStatus(id, s) => self.registry.get_mut().restore_status(&id, s),
                Undo::RemoveIdentity(id) => {
                    self.identities.remove(&id);
                }
                Undo::RestoreIdentity(i) => {
                    self.identities.insert(i.id(), i);
                }
                Undo::RemoveAccount(id) => {
                    if let Some(a) = self.accounts.remove(&id) {
                        self.by_owner.remove_from(&a.owner(), &id);
                        if a.custody() == Custody::Owned {
                            self.owned_custody.remove_from(&a.owner(), &id);
                        }
                    }
                }
                Undo::Many(us) => self.rollback(us),
                Undo::UnreferAccount(r, before) => match before {
                    Some(id) => {
                        self.by_ref.insert(r, id);
                    }
                    None => {
                        self.by_ref.remove(&r);
                    }
                },
                Undo::RestoreAccount(a) => {
                    self.accounts.insert(a.id(), a);
                }
                Undo::RemoveCredential(id) => {
                    if let Some(c) = self.credentials.remove(&id) {
                        self.unindex_credential(&c);
                    }
                }
                Undo::RestoreCredential(c) => {
                    self.credentials.insert(c.id(), c);
                }
                Undo::RemoveGrant(id) => {
                    if let Some(g) = self.grants.remove(&id) {
                        unindex_holder(&mut self.by_principal, &g);
                        if let Some(key) = subject_key(&g) {
                            self.by_subject.remove_from(&key, &id);
                        }
                        if let Some(key) = asset_or_identity_key(&g) {
                            self.by_asset_or_identity.remove_from(&key, &id);
                        }
                    }
                }
                Undo::RestoreGrant(g) => {
                    self.grants.insert(g.id(), g);
                }
                Undo::Window(key, before) => match before {
                    Some(v) => {
                        self.windows.insert(key, v);
                    }
                    None => {
                        self.windows.remove(&key);
                    }
                },
                Undo::RemoveRecord(op) => {
                    self.records.remove(&op);
                }
                Undo::PopTransition(op) => {
                    if let Some(list) = self.transitions.get_mut(&op) {
                        list.pop();
                        if list.is_empty() {
                            self.transitions.remove(&op);
                        }
                    }
                }
                Undo::Reservation(u) => self.reservations.undo(u),
                Undo::ClearRoles => {
                    self.roles.set(None);
                }
                Undo::Op(u) => self.operations.undo(u),
                Undo::OpRetired(u, op) => {
                    self.operations.undo(u);
                    self.settlement.unretire(op);
                }
                Undo::Settlement(u) => self.settlement.undo(u),
                Undo::Audit(u) => self.audit.undo(u),
                Undo::Ops(u) => self.ops.undo(u),
                Undo::Nothing => {}
                Undo::Journal(u) => self.journal.undo(u),
                Undo::Custody(u) => self.custody.undo(u),
                Undo::Owner(before) => self.restore_owner(before),
                Undo::CustodySwitch(before, epoch) => {
                    self.custody.undo(epoch);
                    self.index_custody(&before);
                    self.accounts.insert(before.id(), before);
                }
                Undo::Opening(u) => self.openings.undo(u),
                Undo::Anchor(u) => self.anchoring.undo(u),
                Undo::ChainSpec(u) => self.chainspec.undo(u),
                Undo::Position(id, u) => {
                    self.openings.undo(u);
                    if let Some(a) = self.accounts.remove(&id) {
                        self.by_owner.remove_from(&a.owner(), &id);
                    }
                }
            }
        }
    }

    fn apply(&mut self, w: &Write) -> Result<Undo, StoreRefusal> {
        match w {
            Write::RegisterAsset {
                id,
                precision,
                kind,
            } => {
                self.registry
                    .get_mut()
                    .register(*id, *precision, *kind)
                    .map_err(StoreRefusal::Registry)?;
                Ok(Undo::Unregister(*id))
            }
            Write::AssetBinding { id, binding } => {
                let before = self
                    .registry
                    .get_mut()
                    .set_binding(id, *binding)
                    .map_err(StoreRefusal::Registry)?;
                Ok(Undo::AssetBinding(*id, before))
            }
            Write::DeprecateAsset(id) => {
                let before = self
                    .registry
                    .get_mut()
                    .deprecate(id)
                    .map_err(StoreRefusal::Registry)?;
                Ok(Undo::AssetStatus(*id, before))
            }
            Write::PutIdentity(i) => {
                if !check_mint(i.id(), i.minted(), IdentityId::mint) {
                    return Err(StoreRefusal::NotMinted);
                }
                if self.identities.contains_key(&i.id()) {
                    return Err(StoreRefusal::Exists);
                }
                if i.status() != IdentityStatus::Active {
                    return Err(StoreRefusal::Absorbing);
                }
                if let Some(p) = i.acts_for() {
                    if !self.identities.contains_key(&p) {
                        return Err(StoreRefusal::Dangling);
                    }
                }
                self.identities.insert(i.id(), *i);
                Ok(Undo::RemoveIdentity(i.id()))
            }
            Write::IdentityStatus(id, status) => {
                let before = *self.identities.get(id).ok_or(StoreRefusal::Missing)?;
                if before.status() == IdentityStatus::Closed && *status != IdentityStatus::Closed {
                    return Err(StoreRefusal::Absorbing);
                }
                self.identities.insert(*id, before.with_status(*status));
                Ok(Undo::RestoreIdentity(before))
            }
            Write::PutAccount(a) => {
                if !check_mint(a.id(), a.minted(), AccountId::mint) {
                    return Err(StoreRefusal::NotMinted);
                }
                if self.accounts.contains_key(&a.id()) {
                    return Err(StoreRefusal::Exists);
                }
                if !self.identities.contains_key(&a.owner()) {
                    return Err(StoreRefusal::Dangling);
                }
                if a.status() != AccountStatus::Active {
                    return Err(StoreRefusal::Absorbing);
                }
                if a.kind() == AccountKind::Issuance
                    && self
                        .accounts
                        .values()
                        .any(|x| x.kind() == AccountKind::Issuance)
                {
                    return Err(StoreRefusal::Single);
                }
                self.by_owner.entry_or_default(a.owner()).insert(a.id());
                if a.custody() == Custody::Owned {
                    self.owned_custody
                        .entry_or_default(a.owner())
                        .insert(a.id());
                }
                self.accounts.insert(a.id(), a.clone());
                if a.kind() != AccountKind::Issuance {
                    // Two accounts naming one reference is a hash collision: the smaller
                    // identifier keeps it, as a scan in identifier order would find it.
                    let r = crate::id::account_reference(&a.id());
                    let before = self.by_ref.get(&r).copied();
                    if before.is_none_or(|b| a.id() < b) {
                        self.by_ref.insert(r, a.id());
                        return Ok(Undo::Many(alloc::vec![
                            Undo::RemoveAccount(a.id()),
                            Undo::UnreferAccount(r, before),
                        ]));
                    }
                }
                Ok(Undo::RemoveAccount(a.id()))
            }
            Write::AccountStatus(id, status) => {
                let before = self.accounts.get(id).ok_or(StoreRefusal::Missing)?.clone();
                if before.status() == AccountStatus::Closed && *status != AccountStatus::Closed {
                    return Err(StoreRefusal::Absorbing);
                }
                self.accounts
                    .insert(*id, before.clone().with_status(*status));
                Ok(Undo::RestoreAccount(before))
            }
            Write::PutCredential(c) => {
                if !check_mint(c.id(), c.minted(), CredentialId::mint) {
                    return Err(StoreRefusal::NotMinted);
                }
                if self.credentials.contains_key(&c.id()) {
                    return Err(StoreRefusal::Exists);
                }
                if !self.identities.contains_key(&c.identity()) {
                    return Err(StoreRefusal::Dangling);
                }
                if c.status() != CredentialStatus::Active {
                    return Err(StoreRefusal::Absorbing);
                }
                if let Some(scope) = c.scope() {
                    let owned = self.by_owner.get(&c.identity());
                    if !scope.iter().all(|a| owned.is_some_and(|o| o.contains(a))) {
                        return Err(StoreRefusal::ForeignScope);
                    }
                }
                self.credentials.insert(c.id(), c.clone());
                self.sets.entry_or_default(c.identity()).insert(c.id());
                Ok(Undo::RemoveCredential(c.id()))
            }
            Write::CredentialStatus(id, status) => {
                let before = self
                    .credentials
                    .get(id)
                    .ok_or(StoreRefusal::Missing)?
                    .clone();
                if before.status() == CredentialStatus::Removed
                    && *status != CredentialStatus::Removed
                {
                    return Err(StoreRefusal::Absorbing);
                }
                self.credentials
                    .insert(*id, before.clone().with_status(*status));
                Ok(Undo::RestoreCredential(before))
            }
            Write::PutGrant(g) => {
                self.check_grant(g)?;
                self.by_principal
                    .write()
                    .entry(g.principal())
                    .or_default()
                    .insert(g.id());
                if let Some(key) = subject_key(g) {
                    self.by_subject.entry_or_default(key).insert(g.id());
                }
                if let Some(key) = asset_or_identity_key(g) {
                    self.by_asset_or_identity
                        .entry_or_default(key)
                        .insert(g.id());
                }
                self.grants.insert(g.id(), g.clone());
                Ok(Undo::RemoveGrant(g.id()))
            }
            Write::Revoke(id, at) => {
                let before = self.grants.get(id).ok_or(StoreRefusal::Missing)?.clone();
                if before.revoked().is_some() {
                    return Err(StoreRefusal::Absorbing);
                }
                self.grants
                    .insert(*id, before.clone().with_revoked(Some(*at)));
                Ok(Undo::RestoreGrant(before))
            }
            Write::Charge(c) => {
                if !self.grants.contains_key(&c.key.grant) {
                    return Err(StoreRefusal::Dangling);
                }
                let before = self.windows.get(&c.key).copied();
                let after = before
                    .unwrap_or(Minor::ZERO)
                    .checked_add(c.amount)
                    .ok_or(StoreRefusal::BoundExceeded)?;
                if after > c.limit.minor() {
                    return Err(StoreRefusal::BoundExceeded);
                }
                self.windows.insert(c.key, after);
                Ok(Undo::Window(c.key, before))
            }
            Write::PutRecord(r) => {
                if r.authorities.is_empty() {
                    return Err(StoreRefusal::Unattributed);
                }
                if self.records.contains_key(&r.op) {
                    return Err(StoreRefusal::Exists);
                }
                self.records.insert(r.op, r.clone());
                Ok(Undo::RemoveRecord(r.op))
            }
            Write::PutTransition(r) => {
                if r.authorities.is_empty() {
                    return Err(StoreRefusal::Unattributed);
                }
                if !self.records.contains_key(&r.op) {
                    return Err(StoreRefusal::Dangling);
                }
                let list = self.transitions.entry_or_default(r.op);
                if list.iter().any(|t| t.command == r.command) {
                    if list.is_empty() {
                        self.transitions.remove(&r.op);
                    }
                    return Err(StoreRefusal::Exists);
                }
                list.push(r.clone());
                Ok(Undo::PopTransition(r.op))
            }
            Write::Reservation(change) => {
                if let IndexChange::Open(r) = change {
                    if !self.accounts.contains_key(&r.payer.account) {
                        return Err(StoreRefusal::Dangling);
                    }
                    if self.registry.get().get(&r.asset).is_err() {
                        return Err(StoreRefusal::Dangling);
                    }
                }
                let u = self
                    .reservations
                    .apply(change)
                    .map_err(StoreRefusal::Reservation)?;
                Ok(Undo::Reservation(u))
            }
            Write::SetRoles(roles) => {
                if self.roles.get().is_some() {
                    return Err(StoreRefusal::Single);
                }
                if !self.identities.contains_key(&roles.operator) {
                    return Err(StoreRefusal::Dangling);
                }
                match self.accounts.get(&roles.issuance) {
                    Some(a) if a.kind() == AccountKind::Issuance => {}
                    _ => return Err(StoreRefusal::Dangling),
                }
                self.roles.set(Some(*roles));
                Ok(Undo::ClearRoles)
            }
            Write::Op(w) => {
                let u = self.operations.apply(w).map_err(StoreRefusal::Operation)?;
                // An operation that ends takes its instruction, if any, out of the live ones.
                if let OpWrite::Transition(t) = w
                    && t.to.is_terminal()
                    && self.settlement.retire(&t.op)
                {
                    return Ok(Undo::OpRetired(u, t.op));
                }
                Ok(Undo::Op(u))
            }
            Write::Settlement(w) => self
                .settlement
                .apply(w)
                .map(Undo::Settlement)
                .map_err(StoreRefusal::Settlement),
            Write::Anchor(w) => self
                .anchoring
                .apply(w)
                .map(Undo::Anchor)
                .map_err(StoreRefusal::Anchor),
            // Read by the batch's audit emission, after every write has applied.
            Write::Audit(_) => Ok(Undo::Nothing),
            Write::Ops(w) => {
                let identities = &self.identities;
                let grants = &self.grants;
                self.ops
                    .apply(
                        w,
                        |i| identities.contains_key(i),
                        |g| grants.contains_key(g),
                    )
                    .map(Undo::Ops)
                    .map_err(StoreRefusal::Ops)
            }
            Write::Posting(p) => {
                let lines = journal::lines_of(
                    &p.draft,
                    self.registry.get(),
                    &self.accounts,
                    &self.reservations,
                )
                .map_err(StoreRefusal::Journal)?;
                // A settling posting of an operation the positions' tally already counts: added
                // to the tally as it is written (finding F18's class, `REQ-PR-5`).
                let held = if is_settling(p.draft.meta().kind)
                    && self.settlement.is_counted(&p.operation)
                {
                    self.roles
                        .get()
                        .map(|r| held_by(&lines, &r.issuance))
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                let u = self
                    .journal
                    .append(p, lines)
                    .map_err(StoreRefusal::Journal)?;
                Ok(match self.settlement.tally_posting(&held) {
                    Some(t) => Undo::Many(alloc::vec![Undo::Journal(u), Undo::Settlement(t)]),
                    None => Undo::Journal(u),
                })
            }
            Write::Custody(w) => self.apply_custody(w),
            Write::OpeningPosition { asset, account } => {
                // An opening position is the one `Issuance` account besides the issuance
                // counterparty: owned by the operator's organisation identity, in delegated
                // custody, minted, new, one per registered asset (`F-014`). Never in owned
                // custody: the owned-custody index does not hold positions (T-032-23).
                if !check_mint(account.id(), account.minted(), AccountId::mint) {
                    return Err(StoreRefusal::NotMinted);
                }
                if self.accounts.contains_key(&account.id()) {
                    return Err(StoreRefusal::Exists);
                }
                let roles = self.roles.get().ok_or(StoreRefusal::Dangling)?;
                if account.kind() != AccountKind::Issuance
                    || account.owner() != roles.operator
                    || account.custody() != Custody::Delegated
                    || account.status() != AccountStatus::Active
                {
                    return Err(StoreRefusal::Opening(OpeningRefusal::Position));
                }
                if self.registry.get().get(asset).is_err() {
                    return Err(StoreRefusal::Dangling);
                }
                let u = self
                    .openings
                    .put_position(*asset, account.id())
                    .map_err(StoreRefusal::Opening)?;
                self.by_owner
                    .entry_or_default(account.owner())
                    .insert(account.id());
                self.accounts.insert(account.id(), account.clone());
                Ok(Undo::Position(account.id(), u))
            }
            Write::ChainSpec(w) => {
                if let crate::chainspec::SpecWrite::Alias { account, .. } = w {
                    match self.accounts.get(account) {
                        Some(a) if a.kind() != AccountKind::Issuance => {}
                        _ => return Err(StoreRefusal::Dangling),
                    }
                }
                self.chainspec
                    .apply(w)
                    .map(Undo::ChainSpec)
                    .map_err(StoreRefusal::ChainSpec)
            }
            Write::Opening(w) => {
                if let OpeningWrite::Begin { load, .. } = w {
                    for r in load.set.rows() {
                        if !self.accounts.contains_key(&r.account)
                            || self.registry.get().get(&r.asset).is_err()
                        {
                            return Err(StoreRefusal::Dangling);
                        }
                    }
                }
                self.openings
                    .apply(w)
                    .map(Undo::Opening)
                    .map_err(StoreRefusal::Opening)
            }
        }
    }

    fn check_grant(&self, g: &Grant) -> Result<(), StoreRefusal> {
        if !check_mint(g.id(), g.minted(), GrantId::mint) {
            return Err(StoreRefusal::NotMinted);
        }
        if self.grants.contains_key(&g.id()) || self.ops.grant(&g.id()).is_some() {
            return Err(StoreRefusal::Exists);
        }
        if !self.identities.contains_key(&g.principal())
            || !self.identities.contains_key(&g.grantor())
        {
            return Err(StoreRefusal::Dangling);
        }
        if g.revoked().is_some() {
            return Err(StoreRefusal::MalformedGrant);
        }
        // `OpenAccount` belongs to a grant over an identity alone (amendment 0025).
        if !matches!(g.subject(), Subject::Identity(_))
            && g.ops().contains(&OperationKind::OpenAccount)
        {
            return Err(StoreRefusal::MalformedGrant);
        }
        match (g.kind(), g.subject()) {
            (GrantKind::Issuance, Subject::Asset(a)) => {
                if self.registry.get().get(&a).is_err() {
                    return Err(StoreRefusal::Dangling);
                }
            }
            (GrantKind::Delegated | GrantKind::Operator, Subject::Account(a)) => {
                if !self.accounts.contains_key(&a) {
                    return Err(StoreRefusal::Dangling);
                }
            }
            (GrantKind::Operator, Subject::DelegatedAccounts) => {}
            // Amendment 0025: over an identity, a delegated grant to open its accounts, and
            // nothing else.
            (GrantKind::Delegated, Subject::Identity(i)) => {
                if !self.identities.contains_key(&i) {
                    return Err(StoreRefusal::Dangling);
                }
                if !g.ops().contains(&OperationKind::OpenAccount)
                    || g.ops().contains(&OperationKind::Issue)
                    || g.parent().is_some()
                {
                    return Err(StoreRefusal::MalformedGrant);
                }
            }
            _ => return Err(StoreRefusal::MalformedGrant),
        }
        // A parent of the same kind and subject, within the chain limit.
        // Or, once, a delegated grant over an account narrowed from a grant over its owner
        // (amendment 0025 R4).
        let mut depth = 1;
        let mut subject = g.subject();
        let mut link = g.parent();
        while let Some(p) = link {
            let parent = self.grants.get(&p).ok_or(StoreRefusal::Dangling)?;
            if let (GrantKind::Delegated, Subject::Account(a), Subject::Identity(i)) =
                (g.kind(), subject, parent.subject())
            {
                if self.accounts.get(&a).is_some_and(|acct| acct.owner() == i) {
                    subject = parent.subject();
                }
            }
            if parent.kind() != g.kind() || parent.subject() != subject {
                return Err(StoreRefusal::MalformedGrant);
            }
            depth += 1;
            if depth > MAX_CHAIN {
                return Err(StoreRefusal::MalformedGrant);
            }
            link = parent.parent();
        }
        Ok(())
    }

    /// The one asset registry.
    pub fn registry(&self) -> &AssetRegistry {
        self.registry.get()
    }

    /// An active asset, or the refusal (`ERR-UnknownAsset`, `ERR-AssetDeprecated`).
    pub fn active_asset(&self, id: &AssetId) -> Result<&Asset, crate::Refusal> {
        self.registry.get().active(id)
    }

    /// An identity.
    pub fn identity(&self, id: &IdentityId) -> Option<&Identity> {
        self.identities.get(id)
    }

    /// Every identity, by id (`F-014`: the comparison of two books enumerates them).
    pub fn identities(&self) -> impl Iterator<Item = &Identity> {
        self.identities.values()
    }

    /// Every credential, by id: public material only (`REQ-AU-8`).
    pub fn credentials(&self) -> impl Iterator<Item = &Credential> {
        self.credentials.values()
    }

    /// A ledger account.
    pub fn account(&self, id: &AccountId) -> Option<&LedgerAccount> {
        self.accounts.get(id)
    }

    /// Whether an identity owns an account in owned custody, closed or not: one point read
    /// ([`State`]'s `owned_custody`), however many accounts the identity owns.
    pub fn holds_owned_custody(&self, owner: &IdentityId) -> bool {
        self.owned_custody.get(owner).is_some_and(|s| !s.is_empty())
    }

    /// The identifiers of every account an identity owns, in identifier order: one point read of
    /// the owner's index, never of the accounts themselves.
    pub fn account_ids_of(&self, owner: &IdentityId) -> impl Iterator<Item = &AccountId> {
        self.by_owner.get(owner).into_iter().flatten()
    }

    /// Keep the owned-custody index of `account`'s owner as `account` stands: in it when in owned
    /// custody, out of it otherwise. Touches the index only when the account is, or was, in it.
    fn index_custody(&mut self, account: &LedgerAccount) {
        let (owner, id) = (account.owner(), account.id());
        if account.custody() == Custody::Owned {
            self.owned_custody.entry_or_default(owner).insert(id);
        } else {
            self.owned_custody.remove_from(&owner, &id);
        }
    }

    /// Every account an identity owns.
    pub fn accounts_of(&self, owner: &IdentityId) -> impl Iterator<Item = &LedgerAccount> {
        self.by_owner
            .get(owner)
            .into_iter()
            .flatten()
            .filter_map(|id| self.accounts.get(id))
    }

    /// Every ledger account.
    pub fn all_accounts(&self) -> impl Iterator<Item = &LedgerAccount> {
        self.accounts.values()
    }

    /// A credential.
    pub fn credential(&self, id: &CredentialId) -> Option<&Credential> {
        self.credentials.get(id)
    }

    /// Every credential of an identity's set, in whatever status, by id (`F-012`). A point
    /// lookup in the set index, not a scan of the book's credentials (issue #50).
    pub fn credentials_of<'a>(
        &'a self,
        identity: &'a IdentityId,
    ) -> impl Iterator<Item = &'a Credential> + 'a {
        self.sets
            .get(identity)
            .into_iter()
            .flatten()
            .filter_map(|id| self.credentials.get(id))
    }

    fn unindex_credential(&mut self, c: &Credential) {
        if let Some(set) = self.sets.get_mut(&c.identity()) {
            set.remove(&c.id());
            if set.is_empty() {
                self.sets.remove(&c.identity());
            }
        }
    }

    /// Whether the set index equals a recomputation from the credentials alone: every
    /// credential under its identity, nothing else, no empty set. For the tests of issue #50.
    #[cfg(test)]
    pub(crate) fn credential_sets_consistent(&self) -> bool {
        let mut derived: alloc::collections::BTreeMap<IdentityId, BTreeSet<CredentialId>> =
            alloc::collections::BTreeMap::new();
        for c in self.credentials.values() {
            derived.entry(c.identity()).or_default().insert(c.id());
        }
        derived.iter().eq(self.sets.iter())
    }

    /// Whether the owned-custody index equals a recomputation from the accounts alone: every
    /// account in owned custody under its owner, nothing else, no empty set. For its tests.
    #[cfg(test)]
    pub(crate) fn owned_custody_consistent(&self) -> bool {
        let mut derived: alloc::collections::BTreeMap<IdentityId, BTreeSet<AccountId>> =
            alloc::collections::BTreeMap::new();
        for a in self.accounts.values() {
            if a.custody() == Custody::Owned {
                derived.entry(a.owner()).or_default().insert(a.id());
            }
        }
        derived.iter().eq(self.owned_custody.iter())
    }

    /// The custody records: pending recoveries and successions, named successors, and custody
    /// notices (`F-012`).
    pub fn custody(&self) -> &CustodyState {
        &self.custody
    }

    /// A grant.
    pub fn grant(&self, id: &GrantId) -> Option<&Grant> {
        self.grants.get(id)
    }

    /// Every grant, in identifier order (`F-012`: the authority holders of an account).
    pub fn all_grants(&self) -> impl Iterator<Item = &Grant> {
        self.grants.values()
    }

    /// Every grant over `account` (an account subject), or with `None` every grant over every
    /// delegated account, in identifier order: an index point read (`F-005`).
    pub fn grants_over(&self, account: Option<AccountId>) -> impl Iterator<Item = &Grant> {
        self.by_subject
            .get(&account)
            .into_iter()
            .flatten()
            .filter_map(|id| self.grants.get(id))
    }

    /// The ledger account an account reference names (`REQ-SL-7`), never the issuance
    /// counterparty nor an opening position: an index point read (`F-005`).
    pub fn account_by_reference(&self, r: &[u8; 32]) -> Option<&LedgerAccount> {
        self.by_ref.get(r).and_then(|id| self.accounts.get(id))
    }

    /// The grants `principal` holds over `subject`, in identifier order: index point reads of
    /// the grants over the subject, never of every grant the principal holds, so what a command
    /// is given to evaluate does not grow with the principal's grants (`REQ-RT-1`). A principal
    /// may hold a grant over each of a hundred thousand accounts (amendment 0025 R4); the grants
    /// over one account, asset or identity are few. For an account subject, only the grants over
    /// that account itself: the ones over every delegated account are
    /// `held(principal, Subject::DelegatedAccounts)`.
    pub fn held(&self, principal: &IdentityId, subject: Subject) -> Vec<&Grant> {
        let ids = match subject {
            Subject::Account(a) => self.by_subject.get(&Some(a)),
            Subject::DelegatedAccounts => self.by_subject.get(&None),
            Subject::Asset(_) | Subject::Identity(_) => self.by_asset_or_identity.get(&subject),
        };
        ids.into_iter()
            .flatten()
            .filter_map(|id| self.grants.get(id))
            .filter(|g| g.principal() == *principal)
            .collect()
    }

    /// The grants `principal` holds that may reach `account`: those over it and those over every
    /// delegated account, in identifier order ([`State::held`]). Which of them cover it is the
    /// evaluation's (`crate::eval`).
    pub fn held_over_account(&self, principal: &IdentityId, account: &AccountId) -> Vec<&Grant> {
        let mut v = self.held(principal, Subject::Account(*account));
        v.extend(self.held(principal, Subject::DelegatedAccounts));
        v.sort_by_key(|g| g.id());
        v
    }

    /// Every grant an identity holds. For queries: a command evaluates with [`State::held`], whose
    /// reads do not grow with the principal's grants.
    pub fn grants_of(&self, principal: &IdentityId) -> impl Iterator<Item = &Grant> {
        self.by_principal
            .read()
            .get(principal)
            .into_iter()
            .flatten()
            .filter_map(|id| self.grants.get(id))
    }

    /// A window counter.
    pub fn used(&self, key: &WindowKey) -> Minor {
        self.windows.get(key).copied().unwrap_or(Minor::ZERO)
    }

    /// The record of an accepted operation.
    pub fn record(&self, op: &OperationId) -> Option<&OperationRecord> {
        self.records.get(op)
    }

    /// The book's roles, once set.
    pub fn roles(&self) -> Option<Roles> {
        *self.roles.get()
    }

    /// The accepted transitions of an operation, in order (`F-004`).
    pub fn transitions(&self, op: &OperationId) -> &[OperationRecord] {
        self.transitions.get(op).map_or(&[], Vec::as_slice)
    }

    /// Every posting sequence number `op` wrote: its record's, its transition commands', and its
    /// lifecycle transitions' (an arrival settled on evidence has only the last).
    pub fn posting_seqs_of(&self, op: &OperationId) -> BTreeSet<u64> {
        let mut seqs: BTreeSet<u64> = BTreeSet::new();
        if let Some(r) = self.record(op) {
            seqs.extend(r.postings.iter().copied());
        }
        for t in self.transitions(op) {
            seqs.extend(t.postings.iter().copied());
        }
        if let Some(o) = self.operations().get(op) {
            for t in o.transitions() {
                seqs.extend(t.postings.iter().copied());
            }
        }
        seqs
    }

    /// What `op`'s settling postings moved through the issuance counterparty, per asset, as the
    /// book's record of value held on the settlement layer counts it (`REQ-PR-5`): a deposit's
    /// `Issue` adds what it credited, an outbound leg's `Redeem` takes away what it executed.
    /// Reads `op`'s own postings only.
    pub(crate) fn held_by_operation(&self, op: &OperationId) -> Vec<(AssetId, i128)> {
        let Some(issuance) = self.roles().map(|r| r.issuance) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for seq in self.posting_seqs_of(op) {
            let Some(p) = self.journal.get(seq) else {
                continue;
            };
            if p.operation != *op || !is_settling(p.kind) {
                continue;
            }
            out.extend(held_by(&p.lines, &issuance));
        }
        out
    }

    /// The positions' tally from nothing up to inbox position `k`: every operation with a final
    /// deposit or withdrawal record below `k` and what its settling postings moved, and every
    /// binding decided `Bound` by a final record below `k` (amendment 0011, `REQ-AS-8`). A scan of
    /// the inbox below `k`: how an import checks the tally it was given, never a recorded call's.
    pub(crate) fn tally_from_scratch(&self, k: u64) -> BTreeMap<AssetId, i128> {
        let mut held: BTreeMap<AssetId, i128> = BTreeMap::new();
        let store = &self.settlement;
        for op in store.counted_below(k) {
            for (a, d) in self.held_by_operation(&op) {
                *held.entry(a).or_default() += d;
            }
        }
        let Some(instance) = store.config().map(|c| c.instance) else {
            return held;
        };
        for b in store.asset_binds() {
            if b.status == crate::settlement::assets::BindStatus::Bound
                && b.coordinate
                    .is_some_and(|c| c.instance == instance && c.seq < k)
            {
                *held.entry(b.asset).or_default() += i128::from(b.amount.get());
            }
        }
        held
    }

    /// The open-reservation index (`F-004`).
    pub fn reservations(&self) -> &ReservationIndex {
        &self.reservations
    }

    /// The operation store (`F-007`).
    pub fn operations(&self) -> &Operations {
        &self.operations
    }

    /// The settlement store (`F-010`).
    pub fn settlement(&self) -> &SettlementStore {
        &self.settlement
    }

    /// The audit outbox and the head of the audit chain (`F-013`).
    pub fn audit(&self) -> &AuditLog {
        &self.audit
    }

    /// The audit store holds record `seq`: it leaves the outbox (`F-013`). Not a change to the
    /// book: the record is already committed, and now also shipped.
    pub(crate) fn audit_acknowledge(&mut self, seq: u64) {
        self.audit.acknowledge(seq);
    }

    /// The audit store holds every record through `head`: they leave the outbox (`F-013`,
    /// [`AuditLog`]). Not a change to the book. `Err(seq)`: the store holds another record there.
    pub(crate) fn audit_acknowledge_through(
        &mut self,
        head: (u64, [u8; 32]),
    ) -> Result<usize, u64> {
        self.audit.acknowledge_through(head)
    }

    /// The operator records (`F-013`).
    pub fn ops(&self) -> &OpsState {
        &self.ops
    }

    /// The integrating party `principal` is tied to, if any (`REQ-ID-7`).
    pub fn party_of(&self, principal: &IdentityId) -> Option<crate::party::PartyId> {
        self.ops.tie(principal).map(|t| t.party)
    }

    /// The client-key domain of `principal` (`REQ-OP-10`): its integrating party, or itself where
    /// it is tied to none.
    pub fn key_domain(&self, principal: &IdentityId) -> crate::party::KeyDomain {
        crate::party::KeyDomain::of(*principal, self.party_of(principal))
    }

    /// Every attribution record: operations' and their transitions', in reference order
    /// (`INV-20`, `REQ-OS-4`). `F-011` indexes them by account.
    pub fn records(&self) -> impl Iterator<Item = &OperationRecord> {
        self.records
            .values()
            .chain(self.transitions.values().flatten())
    }

    /// The posting journal (`F-011`). Queries read it through [`crate::query::QueryStore`],
    /// which filters every read by the caller's scope.
    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    /// The opening positions and loads (`F-014`).
    pub fn openings(&self) -> &OpeningsState {
        &self.openings
    }

    /// The anchor store: checkpoints, comparisons and issued disclosures (`F-017`).
    pub fn anchoring(&self) -> &crate::anchor::AnchorStore {
        &self.anchoring
    }
}

/// The service store's sections of an export, and its rebuild from them (`F-014`).
mod image;

/// The service store as leaves of the service tree (`F-005`).
mod tree;
pub use tree::{LeafEntry, Touched};

/// What applying a transition did (`INV-18`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Applied {
    /// The transition was new, and was applied with its effects.
    New(TransitionRecord),
    /// Its key was already recorded: the first result, and nothing written.
    Repeated(TransitionRecord),
}

/// The operation store's interface (`F-007` §5, T-007-04). The service store implements it for
/// both in-process targets; `F-005` and `F-006` implement it durably.
pub trait OperationStore {
    /// An operation's record.
    fn operation(&self, op: &OperationId) -> Option<&Operation>;

    /// The transition recorded under `key`, with its operation.
    fn transition_by_key(&self, key: &TransitionKey) -> Option<(&Operation, &TransitionRecord)>;

    /// Apply `t` and `effects` as one all-or-nothing batch, under the store's rules: compare-and-set
    /// on `t.from`, table membership, terminal refusal, a required deadline, and a unique key. A
    /// key already recorded returns the first result and writes nothing, effects included.
    fn apply_transition(
        &mut self,
        t: TransitionWrite,
        effects: &[Write],
    ) -> Result<Applied, StoreRefusal>;
}

impl OperationStore for State {
    fn operation(&self, op: &OperationId) -> Option<&Operation> {
        self.operations.get(op)
    }

    fn transition_by_key(&self, key: &TransitionKey) -> Option<(&Operation, &TransitionRecord)> {
        self.operations.by_key(key)
    }

    fn apply_transition(
        &mut self,
        t: TransitionWrite,
        effects: &[Write],
    ) -> Result<Applied, StoreRefusal> {
        if let Some((_, first)) = self.operations.by_key(&t.key) {
            return Ok(Applied::Repeated(first.clone()));
        }
        let key = t.key;
        let mut writes = Vec::with_capacity(effects.len() + 1);
        writes.push(Write::Op(OpWrite::Transition(t)));
        writes.extend(effects.iter().cloned());
        self.commit(&writes)?;
        let (_, record) = self
            .operations
            .by_key(&key)
            .ok_or(StoreRefusal::Operation(OpRefusal::Missing))?;
        Ok(Applied::New(record.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::{MAX_PRECISION, NonZeroMinor};
    use crate::authority::{Bounds, GrantTerms, OperationKind, Window};
    use crate::identity::IdentityKind;
    use crate::principal::{CredentialMarker, Verifier};
    use crate::set::NonEmptySet;
    use alloc::collections::BTreeMap;
    use core::num::NonZeroU64;
    use ledger_kernel::posting::OperationRef;

    fn mint(n: u8, ordinal: u32) -> Mint {
        Mint::new(OperationId::from_ref(OperationRef([n; 16])), ordinal)
    }

    fn usd() -> AssetId {
        AssetId::parse("USD").unwrap()
    }

    fn seeded() -> (State, Identity, LedgerAccount) {
        let mut s = State::new();
        let person = Identity::new(mint(1, 0), IdentityKind::Person, None).unwrap();
        let acct = LedgerAccount::new(
            mint(2, 0),
            person.id(),
            AccountKind::Customer,
            Custody::Owned,
            0,
            BTreeMap::new(),
        )
        .unwrap();
        s.commit(&[
            Write::RegisterAsset {
                id: usd(),
                precision: 2,
                kind: AssetKind::Fiat,
            },
            Write::PutIdentity(person),
            Write::PutAccount(acct.clone()),
        ])
        .unwrap();
        (s, person, acct)
    }

    /// `F-014`, T-032-23: the store opens an opening position only in delegated custody, so the
    /// owned-custody index, which positions never enter, cannot miss one.
    #[test]
    fn an_opening_position_is_in_delegated_custody() {
        let mut s = State::new();
        let operator = Identity::new(mint(1, 0), IdentityKind::Organisation, None).unwrap();
        let issuance = LedgerAccount::new(
            mint(2, 0),
            operator.id(),
            AccountKind::Issuance,
            Custody::Delegated,
            0,
            BTreeMap::new(),
        )
        .unwrap();
        s.commit(&[
            Write::RegisterAsset {
                id: usd(),
                precision: 2,
                kind: AssetKind::Fiat,
            },
            Write::PutIdentity(operator),
            Write::PutAccount(issuance.clone()),
            Write::SetRoles(Roles {
                operator: operator.id(),
                issuance: issuance.id(),
            }),
        ])
        .unwrap();
        let position = |custody| {
            LedgerAccount::new(
                mint(3, 0),
                operator.id(),
                AccountKind::Issuance,
                custody,
                0,
                BTreeMap::new(),
            )
            .unwrap()
        };
        assert_eq!(
            s.commit(&[Write::OpeningPosition {
                asset: usd(),
                account: position(Custody::Owned),
            }]),
            Err(StoreRefusal::Opening(OpeningRefusal::Position))
        );
        assert!(s.account(&position(Custody::Owned).id()).is_none());
        assert!(!s.holds_owned_custody(&operator.id()));
        s.commit(&[Write::OpeningPosition {
            asset: usd(),
            account: position(Custody::Delegated),
        }])
        .unwrap();
        assert_eq!(
            s.openings().position(&usd()),
            Some(position(Custody::Delegated).id())
        );
        assert!(s.owned_custody_consistent());
    }

    #[test]
    fn a_refused_batch_changes_nothing() {
        let (mut s, person, _) = seeded();
        let other = Identity::new(mint(3, 0), IdentityKind::Person, None).unwrap();
        let before = s.registry().digest();
        assert_eq!(
            s.commit(&[
                Write::PutIdentity(other),
                Write::RegisterAsset {
                    id: AssetId::parse("EUR").unwrap(),
                    precision: 2,
                    kind: AssetKind::Fiat
                },
                Write::PutIdentity(person),
            ]),
            Err(StoreRefusal::Exists)
        );
        assert!(s.identity(&other.id()).is_none());
        assert_eq!(s.registry().digest(), before);
        assert_eq!(s.registry().len(), 1);
    }

    #[test]
    fn inv_24_one_registry_one_precision() {
        let (mut s, _, _) = seeded();
        for spelling in ["USD", "usd", "Usd"] {
            assert_eq!(
                s.commit(&[Write::RegisterAsset {
                    id: AssetId::parse(spelling).unwrap(),
                    precision: 6,
                    kind: AssetKind::Stable
                }]),
                Err(StoreRefusal::Registry(RegistryError::AlreadyRegistered(
                    usd()
                )))
            );
        }
        assert_eq!(s.registry().get(&usd()).unwrap().precision(), 2);
        assert_eq!(
            s.commit(&[Write::RegisterAsset {
                id: AssetId::parse("XX").unwrap(),
                precision: MAX_PRECISION + 1,
                kind: AssetKind::Other
            }]),
            Err(StoreRefusal::Registry(RegistryError::PrecisionOutOfRange(
                MAX_PRECISION + 1
            )))
        );
    }

    #[test]
    fn inv_17_only_minted_identifiers_are_stored() {
        let (mut s, person, acct) = seeded();
        let forged = Identity::new(mint(4, 0), IdentityKind::Person, None)
            .unwrap()
            .forged(IdentityId::mint(&mint(5, 0).op, 0));
        assert_eq!(
            s.commit(&[Write::PutIdentity(forged)]),
            Err(StoreRefusal::NotMinted)
        );
        let forged_acct = acct.clone().forged(AccountId::mint(&mint(9, 0).op, 1));
        assert_eq!(
            s.commit(&[Write::PutAccount(forged_acct)]),
            Err(StoreRefusal::NotMinted)
        );
        let g = Grant::new(
            mint(6, 0),
            person.id(),
            GrantTerms {
                kind: GrantKind::Delegated,
                principal: person.id(),
                subject: Subject::Account(acct.id()),
                ops: NonEmptySet::one(OperationKind::Transfer),
                bounds: Bounds::new(None, None, Some(NonEmptySet::one(usd())), None).unwrap(),
                parent: None,
                expires: None,
            },
            0,
        );
        assert_eq!(
            s.commit(&[Write::PutGrant(
                g.clone().forged(GrantId::mint(&mint(7, 0).op, 0))
            )]),
            Err(StoreRefusal::NotMinted)
        );
        s.commit(&[Write::PutGrant(g)]).unwrap();
    }

    #[test]
    fn req_la_4_closed_is_absorbing_and_nothing_is_deleted() {
        let (mut s, person, acct) = seeded();
        s.commit(&[Write::AccountStatus(acct.id(), AccountStatus::Closed)])
            .unwrap();
        assert_eq!(
            s.commit(&[Write::AccountStatus(acct.id(), AccountStatus::Active)]),
            Err(StoreRefusal::Absorbing)
        );
        assert_eq!(
            s.account(&acct.id()).unwrap().status(),
            AccountStatus::Closed
        );
        assert_eq!(
            s.commit(&[Write::PutAccount(acct.clone())]),
            Err(StoreRefusal::Exists)
        );
        s.commit(&[Write::IdentityStatus(person.id(), IdentityStatus::Closed)])
            .unwrap();
        assert_eq!(
            s.commit(&[Write::IdentityStatus(person.id(), IdentityStatus::Active)]),
            Err(StoreRefusal::Absorbing)
        );
    }

    #[test]
    fn credentials_are_scoped_only_to_their_identitys_accounts() {
        let (mut s, person, acct) = seeded();
        let stranger = Identity::new(mint(8, 0), IdentityKind::Person, None).unwrap();
        s.commit(&[Write::PutIdentity(stranger)]).unwrap();
        let foreign = Credential::new(
            mint(8, 1),
            stranger.id(),
            CredentialMarker::Ordinary,
            Some(NonEmptySet::one(acct.id())),
            Verifier::new(alloc::vec![1]),
        );
        assert_eq!(
            s.commit(&[Write::PutCredential(foreign)]),
            Err(StoreRefusal::ForeignScope)
        );
        let own = Credential::new(
            mint(1, 1),
            person.id(),
            CredentialMarker::Ordinary,
            Some(NonEmptySet::one(acct.id())),
            Verifier::new(alloc::vec![2]),
        );
        s.commit(&[Write::PutCredential(own.clone())]).unwrap();
        s.commit(&[Write::CredentialStatus(own.id(), CredentialStatus::Removed)])
            .unwrap();
        assert_eq!(
            s.commit(&[Write::CredentialStatus(own.id(), CredentialStatus::Active)]),
            Err(StoreRefusal::Absorbing)
        );
    }

    #[test]
    fn grants_are_revoked_once_and_charges_stay_within_bounds() {
        let (mut s, person, acct) = seeded();
        let w = Window {
            length: NonZeroU64::new(10).unwrap(),
            limit: NonZeroMinor::new(Minor::new(5)).unwrap(),
        };
        let g = Grant::new(
            mint(6, 0),
            person.id(),
            GrantTerms {
                kind: GrantKind::Delegated,
                principal: person.id(),
                subject: Subject::Account(acct.id()),
                ops: NonEmptySet::one(OperationKind::Transfer),
                bounds: Bounds::new(None, Some(w), None, None).unwrap(),
                parent: None,
                expires: None,
            },
            0,
        );
        // Delegated authority over an asset is malformed.
        let mut bad = g.clone();
        bad = Grant::new(
            mint(6, 1),
            person.id(),
            GrantTerms {
                kind: GrantKind::Delegated,
                principal: person.id(),
                subject: Subject::Asset(usd()),
                ops: NonEmptySet::one(OperationKind::Issue),
                bounds: bad.bounds().clone(),
                parent: None,
                expires: None,
            },
            0,
        );
        assert_eq!(
            s.commit(&[Write::PutGrant(bad)]),
            Err(StoreRefusal::MalformedGrant)
        );
        s.commit(&[Write::PutGrant(g.clone())]).unwrap();
        let key = WindowKey {
            grant: g.id(),
            window: 0,
            asset: usd(),
        };
        let charge = |n| {
            Write::Charge(Charge {
                key,
                amount: Minor::new(n),
                limit: w.limit,
            })
        };
        s.commit(&[charge(3)]).unwrap();
        assert_eq!(s.commit(&[charge(3)]), Err(StoreRefusal::BoundExceeded));
        s.commit(&[charge(2)]).unwrap();
        assert_eq!(s.used(&key), Minor::new(5));
        s.commit(&[Write::Revoke(g.id(), 1)]).unwrap();
        assert_eq!(
            s.commit(&[Write::Revoke(g.id(), 2)]),
            Err(StoreRefusal::Absorbing)
        );
        assert_eq!(s.grant(&g.id()).unwrap().revoked(), Some(1));
    }
}
