// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Pending changes, named successors and custody notices in the service store (`F-012` §6, §8,
//! §11), and the store's rules over them.
//!
//! They change only by [`CustodyWrite`]s, applied by [`crate::state::State::commit`] with every
//! other write of the command, all or nothing. The rules hold whoever writes:
//!
//! - a pending change is recorded once, in [`CHANGE_ENTRY`], with a delay
//!   (`effective_at` after `initiated_at`, `REQ-AU-16`), for an account that exists and the
//!   identity that owns it, and a target of its kind;
//! - at most one change of each kind is pending per account;
//! - a change moves only along [`CHANGE_TRANSITIONS`](super::CHANGE_TRANSITIONS), from
//!   `Pending`; `Cancelled` and `Completed` absorb; and it completes only at or after its
//!   `effective_at`, so no command can complete a recovery or succession early;
//! - a custody change is a change (owned ↔ delegated) of an open account that is not the issuance
//!   counterparty, and it advances the account's custody epoch in the same write; an owner change
//!   names another, existing identity;
//! - a successor is an existing identity other than the owner;
//! - a notice is put in the outbox once.

use crate::leaves::{LeafHost, LeafMap, Leaves, kind};

use crate::account::{Custody, Time};
use crate::authority::AuthorityUsed;
use crate::id::{AccountId, CredentialId, IdentityId, OperationId};
use crate::lifecycle::keys::Canon;
use crate::principal::Verifier;
use crate::set::NonEmptySet;

use super::machine::{CHANGE_ENTRY, ChangeState, in_table};

/// A pending change's identity: the reference of the operation that opened it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PendingId(pub OperationId);

/// What kind of change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ChangeKind {
    /// A recovery through the recovery authority (`REQ-AU-15`).
    Recovery,
    /// A named successor's claim (`REQ-AU-17`).
    Succession,
}

/// What a completed change gives.
#[derive(Clone, PartialEq, Eq)]
pub enum ChangeTarget {
    /// A recovery issues this credential into the holder's set.
    Credential {
        /// Its verifier: public material only.
        verifier: Verifier,
        /// The accounts it is limited to, if any.
        scope: Option<NonEmptySet<AccountId>>,
    },
    /// A succession passes the account to this identity.
    Successor(IdentityId),
}

impl core::fmt::Debug for ChangeTarget {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            // The verifier is not printed (`REQ-AU-8`).
            ChangeTarget::Credential { scope, .. } => f
                .debug_struct("Credential")
                .field("scope", scope)
                .finish_non_exhaustive(),
            ChangeTarget::Successor(i) => f.debug_tuple("Successor").field(i).finish(),
        }
    }
}

/// When and by which operation a change left `Pending`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Closed {
    /// The time input of the closing command.
    pub at: Time,
    /// Its operation reference.
    pub by: OperationId,
}

/// A pending recovery or succession (`F-012` §6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingChange {
    /// Its identity.
    pub id: PendingId,
    /// Recovery or succession.
    pub kind: ChangeKind,
    /// The account it is for.
    pub account: AccountId,
    /// The account's owner when it was opened. It completes only while the owner is the same.
    pub owner: IdentityId,
    /// Who opened it, with which authority.
    pub initiated_by: AuthorityUsed,
    /// When.
    pub initiated_at: Time,
    /// When it may complete: `initiated_at` plus the deployment's delay.
    pub effective_at: Time,
    /// What it gives.
    pub target: ChangeTarget,
    /// Where it is.
    pub state: ChangeState,
    /// When and how it left `Pending`.
    pub closed: Option<Closed>,
}

/// The identity of a notice: `H(op, recipient, event)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NoticeId(pub [u8; 32]);

/// What a notice tells an authority holder (`INV-20`, `REQ-AU-11`, `REQ-AU-16`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoticeEvent {
    /// A credential joined the holder's set.
    CredentialRegistered(CredentialId),
    /// A credential left the holder's set.
    CredentialRemoved(CredentialId),
    /// The account's custody became delegated.
    CustodyDelegated,
    /// The account's custody became owned.
    CustodyReclaimed,
    /// A recovery is pending, effective at the time given.
    RecoveryInitiated {
        /// The pending change.
        pending: PendingId,
        /// When it may complete.
        effective_at: Time,
    },
    /// A successor claimed the account; the succession is pending until the time given.
    SuccessionClaimed {
        /// The pending change.
        pending: PendingId,
        /// When it may complete.
        effective_at: Time,
    },
    /// A pending change was cancelled.
    Cancelled(PendingId),
    /// A recovery completed.
    RecoveryCompleted(PendingId),
    /// A succession completed.
    SuccessionCompleted(PendingId),
    /// A successor was named, or the name cleared.
    SuccessorNamed(Option<IdentityId>),
}

impl NoticeEvent {
    const fn code(&self) -> u8 {
        match self {
            NoticeEvent::CredentialRegistered(_) => 1,
            NoticeEvent::CredentialRemoved(_) => 2,
            NoticeEvent::CustodyDelegated => 3,
            NoticeEvent::CustodyReclaimed => 4,
            NoticeEvent::RecoveryInitiated { .. } => 5,
            NoticeEvent::SuccessionClaimed { .. } => 6,
            NoticeEvent::Cancelled(_) => 7,
            NoticeEvent::RecoveryCompleted(_) => 8,
            NoticeEvent::SuccessionCompleted(_) => 9,
            NoticeEvent::SuccessorNamed(_) => 10,
        }
    }
}

/// A notice to one authority holder of an affected account, in the outbox. `F-005` makes the
/// outbox durable and `F-009` delivers it, as `F-007`'s lifecycle notifications.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Notice {
    /// Its identity.
    pub id: NoticeId,
    /// The operation that caused it.
    pub op: OperationId,
    /// Whom it is for.
    pub recipient: IdentityId,
    /// The account it is about, if one.
    pub account: Option<AccountId>,
    /// What happened.
    pub event: NoticeEvent,
    /// When.
    pub at: Time,
}

impl NoticeId {
    /// The identity of the notice of `event` for `recipient`, caused by `op`. It names the
    /// event's kind, not its content, so it can be derived without knowing the content: how the
    /// replay of a completing succession claim finds which change it completed
    /// ([`CustodyState::completed_by`]).
    pub fn of(op: &OperationId, recipient: &IdentityId, event: &NoticeEvent) -> NoticeId {
        NoticeId(
            Canon::new("aledgent/custody-notice/v1")
                .bytes(&op.to_ref().0)
                .bytes(recipient.as_bytes())
                .u8(event.code())
                .digest()
                .0,
        )
    }
}

impl Notice {
    /// The notice of `event` for `recipient`, caused by `op`.
    pub fn new(
        op: OperationId,
        recipient: IdentityId,
        account: Option<AccountId>,
        event: NoticeEvent,
        at: Time,
    ) -> Notice {
        Notice {
            id: NoticeId::of(&op, &recipient, &event),
            op,
            recipient,
            account,
            event,
            at,
        }
    }
}

/// One change to the custody records.
#[derive(Clone, Debug)]
pub enum CustodyWrite {
    /// Change an account's custody mode (`REQ-AU-11`).
    SetCustody {
        /// The account.
        account: AccountId,
        /// Its new mode.
        custody: Custody,
    },
    /// Pass an account to another owner: a completed succession (`REQ-AU-17`).
    SetOwner {
        /// The account.
        account: AccountId,
        /// Its new owner.
        owner: IdentityId,
    },
    /// Record a pending change.
    Open(PendingChange),
    /// Move a pending change along the table.
    Close {
        /// The change.
        id: PendingId,
        /// Where it goes.
        to: ChangeState,
        /// When and by what.
        closed: Closed,
    },
    /// Name an account's successor, or clear the name.
    Successor {
        /// The account.
        account: AccountId,
        /// The successor.
        successor: Option<IdentityId>,
    },
    /// Put a notice in the outbox.
    Notify(Notice),
}

/// Why the store refused a custody write. The whole batch is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CustodyRefusal {
    /// A record with this identity already exists.
    Exists,
    /// No such record.
    Missing,
    /// The account, identity or owner it names does not exist or does not match.
    Dangling,
    /// A change of this kind is already pending for the account.
    AlreadyPending,
    /// Not recorded in the entry state, or with no delay, or with a target of the wrong kind.
    NotEntry,
    /// The change is terminal (absorbing).
    Terminal,
    /// The table has no such transition.
    NotInTable,
    /// A completion before `effective_at` (`REQ-AU-16`).
    NotElapsed,
    /// A custody or owner change that changes nothing, or of an account that cannot change.
    NoChange,
}

/// How to undo one applied custody write (the records here; the account-level writes are undone
/// by the service store itself).
pub(crate) enum CustodyUndo {
    RemovePending(PendingId),
    RestorePending(PendingChange),
    Successor(AccountId, Option<IdentityId>),
    RemoveNotice(NoticeId),
    Epoch(AccountId, Option<u64>),
}

/// The custody records of the service store. Each collection is leaves of the service tree
/// (`crate::leaves`).
#[derive(Default)]
pub struct CustodyState {
    pending: Leaves<PendingId, PendingChange, { kind::CUSTODY_PENDING }>,
    open: Leaves<(AccountId, ChangeKind), PendingId, { kind::CUSTODY_OPEN }>,
    successors: Leaves<AccountId, IdentityId, { kind::SUCCESSOR }>,
    epochs: Leaves<AccountId, u64, { kind::CUSTODY_EPOCH }>,
    outbox: Leaves<NoticeId, Notice, { kind::CUSTODY_NOTICE }>,
}

impl LeafHost for CustodyState {
    fn leaf_maps<'a>(&'a self, out: &mut alloc::vec::Vec<&'a dyn LeafMap>) {
        out.push(&self.pending);
        out.push(&self.open);
        out.push(&self.successors);
        out.push(&self.epochs);
        out.push(&self.outbox);
    }
    fn leaf_maps_mut<'a>(&'a mut self, out: &mut alloc::vec::Vec<&'a mut dyn LeafMap>) {
        out.push(&mut self.pending);
        out.push(&mut self.open);
        out.push(&mut self.successors);
        out.push(&mut self.epochs);
        out.push(&mut self.outbox);
    }
}

impl CustodyState {
    /// Empty.
    pub fn new() -> CustodyState {
        CustodyState::default()
    }

    /// A pending change, in whatever state.
    pub fn pending(&self, id: &PendingId) -> Option<&PendingChange> {
        self.pending.get(id)
    }

    /// The change of `kind` pending for `account`, if one is.
    pub fn open(&self, account: &AccountId, kind: ChangeKind) -> Option<&PendingChange> {
        self.open
            .get(&(*account, kind))
            .and_then(|id| self.pending.get(id))
    }

    /// The succession change that the claim `op` of `claimant` completed on `account`, if it
    /// completed one. Point reads only (finding F18's class, `F-032`): the completion's notice to
    /// the claimant, which names the change, then the change, which must be `account`'s and
    /// closed by `op`. The outbox is never pruned (notices are delivered, not removed), so the
    /// notice is there for as long as the claim's record is. Before, the replay looked for the
    /// change among every change ever recorded ([`CustodyState::of_account`]), so its witness grew
    /// with every recovery and succession of the book.
    pub fn completed_by(
        &self,
        account: &AccountId,
        claimant: &IdentityId,
        op: &OperationId,
    ) -> Option<PendingId> {
        let id = NoticeId::of(
            op,
            claimant,
            &NoticeEvent::SuccessionCompleted(PendingId(*op)),
        );
        let NoticeEvent::SuccessionCompleted(pending) = self.outbox.get(&id)?.event else {
            return None;
        };
        self.pending(&pending)
            .filter(|c| c.account == *account && c.closed.is_some_and(|x| x.by == *op))
            .map(|c| c.id)
    }

    /// Every change recorded for `account`, in identity order. It reads every change of the book:
    /// for queries only, never inside a recorded call (finding F18's class, `F-032`).
    pub fn of_account<'a>(
        &'a self,
        account: &'a AccountId,
    ) -> impl Iterator<Item = &'a PendingChange> + 'a {
        self.pending.values().filter(move |c| c.account == *account)
    }

    /// The custody epoch of `account`: how many custody changes it has had. A reclaim proof
    /// binds it ([`super::reclaim_challenge`]).
    pub fn epoch(&self, account: &AccountId) -> u64 {
        self.epochs.get(account).copied().unwrap_or(0)
    }

    /// Advance `account`'s custody epoch: part of every custody change, in its batch.
    pub(crate) fn bump(&mut self, account: &AccountId) -> CustodyUndo {
        let before = self.epochs.get(account).copied();
        self.epochs
            .insert(*account, before.unwrap_or(0).saturating_add(1));
        CustodyUndo::Epoch(*account, before)
    }

    /// The successor named for `account`.
    pub fn successor(&self, account: &AccountId) -> Option<IdentityId> {
        self.successors.get(account).copied()
    }

    /// The outbox.
    pub fn outbox(&self) -> impl Iterator<Item = &Notice> {
        self.outbox.values()
    }

    /// Apply a write to the records here, under the rules of the module documentation. The
    /// account-level facts come from the service store: `owner_of` is an account's owner, if it
    /// exists; `identity` whether an identity exists.
    pub(crate) fn apply(
        &mut self,
        w: &CustodyWrite,
        owner_of: impl Fn(&AccountId) -> Option<IdentityId>,
        identity: impl Fn(&IdentityId) -> bool,
    ) -> Result<CustodyUndo, CustodyRefusal> {
        match w {
            CustodyWrite::Open(c) => {
                if self.pending.contains_key(&c.id) {
                    return Err(CustodyRefusal::Exists);
                }
                if c.state != CHANGE_ENTRY || c.closed.is_some() || c.effective_at <= c.initiated_at
                {
                    return Err(CustodyRefusal::NotEntry);
                }
                match (c.kind, &c.target) {
                    (ChangeKind::Recovery, ChangeTarget::Credential { .. }) => {}
                    (ChangeKind::Succession, ChangeTarget::Successor(s)) => {
                        if !identity(s) || *s == c.owner {
                            return Err(CustodyRefusal::Dangling);
                        }
                    }
                    _ => return Err(CustodyRefusal::NotEntry),
                }
                if owner_of(&c.account) != Some(c.owner) {
                    return Err(CustodyRefusal::Dangling);
                }
                if self.open.contains_key(&(c.account, c.kind)) {
                    return Err(CustodyRefusal::AlreadyPending);
                }
                self.open.insert((c.account, c.kind), c.id);
                self.pending.insert(c.id, c.clone());
                Ok(CustodyUndo::RemovePending(c.id))
            }
            CustodyWrite::Close { id, to, closed } => {
                let before = self.pending.get(id).ok_or(CustodyRefusal::Missing)?.clone();
                if before.state.terminal() {
                    return Err(CustodyRefusal::Terminal);
                }
                if !in_table(before.state, *to) {
                    return Err(CustodyRefusal::NotInTable);
                }
                if *to == ChangeState::Completed && closed.at < before.effective_at {
                    return Err(CustodyRefusal::NotElapsed);
                }
                let mut after = before.clone();
                after.state = *to;
                after.closed = Some(*closed);
                self.open.remove(&(before.account, before.kind));
                self.pending.insert(*id, after);
                Ok(CustodyUndo::RestorePending(before))
            }
            CustodyWrite::Successor { account, successor } => {
                let owner = owner_of(account).ok_or(CustodyRefusal::Dangling)?;
                if let Some(s) = successor {
                    if !identity(s) || *s == owner {
                        return Err(CustodyRefusal::Dangling);
                    }
                }
                let before = match successor {
                    Some(s) => self.successors.insert(*account, *s),
                    None => self.successors.remove(account),
                };
                Ok(CustodyUndo::Successor(*account, before))
            }
            CustodyWrite::Notify(n) => {
                if self.outbox.contains_key(&n.id) {
                    return Err(CustodyRefusal::Exists);
                }
                if !identity(&n.recipient) {
                    return Err(CustodyRefusal::Dangling);
                }
                self.outbox.insert(n.id, *n);
                Ok(CustodyUndo::RemoveNotice(n.id))
            }
            // Applied by the service store, which holds the accounts.
            CustodyWrite::SetCustody { .. } | CustodyWrite::SetOwner { .. } => {
                Err(CustodyRefusal::NoChange)
            }
        }
    }

    pub(crate) fn undo(&mut self, u: CustodyUndo) {
        match u {
            CustodyUndo::RemovePending(id) => {
                if let Some(c) = self.pending.remove(&id) {
                    self.open.remove(&(c.account, c.kind));
                }
            }
            CustodyUndo::RestorePending(c) => {
                if c.state == ChangeState::Pending {
                    self.open.insert((c.account, c.kind), c.id);
                }
                self.pending.insert(c.id, c);
            }
            CustodyUndo::Successor(account, before) => match before {
                Some(s) => {
                    self.successors.insert(account, s);
                }
                None => {
                    self.successors.remove(&account);
                }
            },
            CustodyUndo::RemoveNotice(id) => {
                self.outbox.remove(&id);
            }
            CustodyUndo::Epoch(account, before) => match before {
                Some(e) => {
                    self.epochs.insert(account, e);
                }
                None => {
                    self.epochs.remove(&account);
                }
            },
        }
    }
}

/// The records in an export, and their rebuild (`F-014`).
mod canon;
