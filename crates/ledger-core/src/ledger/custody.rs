// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Custody switching, credential sets and recovery: the commands of `F-012` (SPEC §8.1,
//! `US-A5`, `US-A7`, `US-A8`, `REQ-AU-3`, `REQ-AU-4`, `REQ-AU-9`, `REQ-AU-11`–`REQ-AU-20`).
//!
//! | Command | Who | Effect |
//! |---|---|---|
//! | `register_credential` | an active member of the set | a member joins the set; its scope is within the registering member's (`REQ-AU-4`) |
//! | `remove_credential` | an active member of the set | a member is removed, at once (`REQ-AU-3`); never the last (`REQ-AU-20`, amendment 0006 item 10) |
//! | `delegate_custody` | the holder, owner authority | owned → delegated (`REQ-AU-12`) |
//! | `reclaim_custody` | the holder, with a proven credential and the mechanisms acknowledged | delegated → owned (`REQ-AU-13`) |
//! | `initiate_recovery` | the operator, on the holder's out-of-band request (operator surface) | a pending recovery, with its delay (`REQ-AU-16`) |
//! | `cancel_recovery` | the holder: any active member covering the account | the pending recovery or succession cancelled |
//! | `complete_recovery` | the operator, carrying the recovery authority's approval (operator surface) | a new credential; the covering ones retired (`REQ-AU-15`) |
//! | `name_successor` | the holder | the account's successor named (`REQ-AU-17`) |
//! | `claim_succession` | the named successor, twice | a pending succession, then the account passes |
//!
//! Every command is idempotent on its operation reference and attributed; its record names the
//! account it touched, which is that account's action log (`REQ-OS-4`, `INV-20`); the service
//! store emits its audit record in the same batch (`F-013`); and it puts a notice for every
//! authority holder of the account in the outbox. The two operator-surface commands take a
//! justification (`ERR-JustificationRequired`) and leave an audit record when refused (`F-013`'s
//! frame, steps 2, 3 and 5; the recovery authority's check is this module's).
//!
//! **Owner authority** over an account is exercised by an active member of its owner's set whose
//! scope covers the account (`REQ-AU-20`, `AC-A5.4`); `F-003`'s evaluator already reads it so for
//! every value-moving command. A holder-held recovery credential is such a member, marked, and
//! acts at once (`REQ-AU-18`, `AC-A8.6`).
//!
//! **The recovery authority** holds no identity in the book and cannot be a caller: it never acts
//! on an account, and no value-moving command can run under it (`REQ-AU-15`, `RISK-9`). Its one
//! act is a signature over one pending recovery ([`crate::custody::approval_message`]), which
//! `complete_recovery` checks against the configured key. The operator can submit that command
//! but cannot make the signature (`AC-A8.1`).

use alloc::collections::BTreeSet;
use alloc::vec;
use alloc::vec::Vec;

use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use super::operator::digest as operator_digest;
use super::{Accepted, Ctx, Ledger, policy_used};
use crate::account::{AccountKind, Custody, LedgerAccount, Time};
use crate::asset::Minor;
use crate::audit::{AuditNote, AuditOutcome, AuditSubject, Justification};
use crate::authority::{AuthorityKind, AuthorityUsed, GrantKind, Subject, effective};
use crate::custody::CustodyWrite;
use crate::custody::{
    ChangeDecision, ChangeFacts, ChangeKind, ChangeState, ChangeTarget, ChangeTrigger,
    MechanismsDigest, Notice, NoticeEvent, PendingChange, PendingId, RecoveryMechanisms, Signature,
    approval_message, next, reclaim_challenge, recovery_rules, store::Closed, verifies,
};
use crate::eval;
use crate::id::{AccountId, CredentialId, IdentityId, Mint, OperationId};
use crate::identity::{IdentityKind, IdentityStatus};
use crate::lifecycle::keys::Canon;
use crate::operator::OperatorOpKind;
use crate::principal::{
    Caller, Credential, CredentialMarker, CredentialStatus, Principal, Scheme, Verifier, resolve,
};
use crate::refusal::Refusal;
use crate::set::NonEmptySet;
use crate::state::{Command, OperationRecord, Write};

/// `register_credential`'s request.
#[derive(Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RegisterCredential {
    /// An everyday credential, or a holder-held recovery credential (`REQ-AU-18`).
    pub marker: CredentialMarker,
    /// The accounts it is limited to; `None` for all of the identity's. Within the registering
    /// credential's own scope (`REQ-AU-4`).
    pub scope: Option<NonEmptySet<AccountId>>,
    /// Its verifier: public material only (`REQ-AU-8`).
    pub verifier: Verifier,
}

impl core::fmt::Debug for RegisterCredential {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The verifier is not printed (`REQ-AU-8`).
        f.debug_struct("RegisterCredential")
            .field("marker", &self.marker)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

impl core::fmt::Debug for InitiateRecovery {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The verifier is not printed (`REQ-AU-8`).
        f.debug_struct("InitiateRecovery")
            .field("account", &self.account)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

/// The most credentials one import carries: Kreivo `Pass`'s own bound on devices per account.
pub const MAX_IMPORTED_CREDENTIALS: usize = 100;

/// One credential of an import (`REQ-MG-7`): public material and the scope it is given.
#[derive(Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ImportedCredential {
    /// Its verifier: public material only (`REQ-AU-8`).
    pub verifier: Verifier,
    /// The accounts it is limited to; `None` for all of the identity's.
    pub scope: Option<NonEmptySet<AccountId>>,
}

/// `import_credentials`'s request (`REQ-MG-7`, amendment 0010).
#[derive(Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ImportCredentials {
    /// The identity whose set takes them.
    pub identity: IdentityId,
    /// The credentials, in order. The `n`th is minted at ordinal `n` of the operation.
    pub credentials: Vec<ImportedCredential>,
}

impl core::fmt::Debug for ImportCredentials {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Verifiers are not printed (`REQ-AU-8`); their schemes are.
        f.debug_struct("ImportCredentials")
            .field("identity", &self.identity)
            .field(
                "schemes",
                &self
                    .credentials
                    .iter()
                    .map(|c| c.verifier.scheme())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// A proof that a holder credential works, over
/// [`reclaim_challenge`]`(account, custody epoch)` (`REQ-AU-13`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CredentialProof {
    /// The credential that proves itself.
    pub credential: CredentialId,
    /// How it proved itself.
    pub proof: Proof,
}

/// How a credential proved itself for a reclaim (`DEC-19`, `F-028`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Proof {
    /// A `key`-scheme signature over the challenge. The core checks it.
    Signature(
        #[cfg_attr(feature = "serde", serde(with = "crate::call::signature_bytes"))] Signature,
    ),
    /// A passkey assertion over the challenge, checked by ingress before the command was logged
    /// (`DEC-19`): the core carries no P-256. Accepted for a `webauthn-es256` credential only;
    /// ingress never takes this mark from a caller.
    PasskeyChecked,
}

/// `reclaim_custody`'s request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ReclaimCustody {
    /// The account.
    pub account: AccountId,
    /// The proof that a holder credential works.
    pub proof: CredentialProof,
    /// The digest of the account's recovery mechanisms, as shown to the holder
    /// ([`Ledger::recovery_mechanisms`], `AC-A5.3`).
    pub acknowledged: Option<MechanismsDigest>,
}

/// `initiate_recovery`'s request.
#[derive(Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct InitiateRecovery {
    /// The owned-custody account to recover.
    pub account: AccountId,
    /// The verifier of the credential the recovery will issue to the holder.
    pub verifier: Verifier,
    /// Its scope: the recovered account among the owner's, or `None` for all of them.
    pub scope: Option<NonEmptySet<AccountId>>,
}

/// What `claim_succession` did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claimed {
    /// The first claim: a pending succession, which may complete at `effective_at`.
    Pending {
        /// The pending change.
        pending: PendingId,
        /// When the successor may claim again to complete it.
        effective_at: Time,
    },
    /// The second claim, after the delay: the account is the successor's.
    Completed {
        /// The pending change it completed.
        pending: PendingId,
    },
}

/// One pending or past change, as the account view shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PendingView {
    /// Its identity.
    pub id: PendingId,
    /// Recovery or succession.
    pub kind: ChangeKind,
    /// Where it is.
    pub state: ChangeState,
    /// When it was opened.
    pub initiated_at: Time,
    /// When it may complete.
    pub effective_at: Time,
}

/// An account's custody, as its authority holders see it (`INV-20`, `REQ-AU-16`, `F-012` §11).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustodyView {
    /// The account.
    pub account: AccountId,
    /// Its owner.
    pub owner: IdentityId,
    /// Its custody mode.
    pub custody: Custody,
    /// The successor named for it, if one is.
    pub successor: Option<IdentityId>,
    /// Its custody epoch: how many custody changes it has had. A reclaim proof signs it.
    pub epoch: u64,
    /// Every recovery and succession recorded for it, pending ones included, with their
    /// `effective_at`.
    pub changes: Vec<PendingView>,
}

// ---------------------------------------------------------------------- digests

fn head(command: &str, p: &Principal) -> Canon {
    let mut c = Canon::new(command);
    c.bytes(p.identity.as_bytes());
    c.opt(p.on_behalf_of, |c, id| {
        c.bytes(id.as_bytes());
    });
    c
}

fn scope_into(c: &mut Canon, scope: Option<&NonEmptySet<AccountId>>) {
    c.opt(scope, |c, s| {
        c.u64(s.len() as u64);
        for a in s.iter() {
            c.bytes(a.as_bytes());
        }
    });
}

fn marker_code(m: CredentialMarker) -> u8 {
    match m {
        CredentialMarker::Ordinary => 1,
        CredentialMarker::RecoveryHeld => 2,
        CredentialMarker::Imported => 3,
    }
}

// ---------------------------------------------------------------------- rules

/// Whether a credential of scope `new` is within one of scope `of` (`REQ-AU-4`, `INV-22`): an
/// unscoped credential covers every scope; a scoped one covers only scoped subsets of its own.
/// A scoped credential can therefore never register or remove an unscoped one, nor one that
/// names an account it does not cover: widening is not expressible through the set commands.
pub fn scope_within(
    new: Option<&NonEmptySet<AccountId>>,
    of: Option<&NonEmptySet<AccountId>>,
) -> bool {
    match (new, of) {
        (_, None) => true,
        (None, Some(_)) => false,
        (Some(n), Some(o)) => n.is_subset(o),
    }
}

/// Whether a credential of scope `scope` covers `account`.
fn covers(scope: Option<&NonEmptySet<AccountId>>, account: &AccountId) -> bool {
    scope.is_none_or(|s| s.contains(account))
}

fn owner_used(p: &Principal) -> AuthorityUsed {
    AuthorityUsed {
        kind: AuthorityKind::Owner,
        grant: None,
        principal: p.identity,
        credential: p.credential,
        on_behalf_of: p.on_behalf_of,
    }
}

fn subjects(ctx: &Ctx, subjects: Vec<AuditSubject>) -> Write {
    Write::Audit(AuditNote {
        op: ctx.op,
        class: None,
        action: None,
        authorities: Vec::new(),
        subjects,
        justification: None,
        outcome: AuditOutcome::Accepted,
        at: ctx.now,
    })
}

fn notices<'a>(
    ctx: &'a Ctx,
    recipients: &'a BTreeSet<IdentityId>,
    account: Option<AccountId>,
    event: NoticeEvent,
) -> impl Iterator<Item = Write> + 'a {
    recipients.iter().map(move |r| {
        Write::Custody(CustodyWrite::Notify(Notice::new(
            ctx.op, *r, account, event, ctx.now,
        )))
    })
}

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    // ------------------------------------------------------------------ helpers

    /// The set `p` may change: its own identity's, acting for itself. An agent's set is its own.
    fn own_set(p: &Principal) -> Result<IdentityId, Refusal> {
        if p.kind != IdentityKind::Agent && p.on_behalf_of.is_some() {
            return Err(Refusal::NotAuthorised);
        }
        Ok(p.identity)
    }

    /// Owner authority over `account` from the set (`REQ-AU-20`): `p` is its owner, acting for
    /// itself, with a credential whose scope covers it. Otherwise `ERR-NotAuthorised` if `p`
    /// reaches the account or is the operator, and `ERR-UnknownAccount` if not (`INV-16`).
    fn holder_of(
        &self,
        p: &Principal,
        account: &AccountId,
    ) -> Result<(LedgerAccount, AuthorityUsed), Refusal> {
        let acct = self
            .state
            .account(account)
            .ok_or(Refusal::UnknownAccount)?
            .clone();
        if acct.kind() != AccountKind::Issuance
            && acct.owner() == p.identity
            && p.in_scope(account)
            && p.on_behalf_of.is_none()
        {
            return Ok((acct, owner_used(p)));
        }
        if eval::knows(&self.state, p, &acct) || self.is_operator(p) {
            Err(Refusal::NotAuthorised)
        } else {
            Err(Refusal::UnknownAccount)
        }
    }

    /// Every authority holder of `account` were its custody `custody`: its owner, and the holder
    /// of every live grant that covers it (`INV-20`).
    fn holders_in(
        &self,
        account: &LedgerAccount,
        custody: Custody,
        now: Time,
    ) -> BTreeSet<IdentityId> {
        let mut out = BTreeSet::from([account.owner()]);
        let over_account = self.state.grants_over(Some(account.id()));
        for g in over_account.chain(self.state.grants_over(None)) {
            let over = crate::eval::covers_in(&self.state, g.kind(), g.subject(), account, custody);
            if over && effective(g, now, |id| self.state.grant(id)).is_ok() {
                out.insert(g.principal());
            }
        }
        out
    }

    /// The authority holders of `account` in either custody mode: for a custody change, who had
    /// it and who has it.
    fn holders(&self, account: &LedgerAccount, now: Time) -> BTreeSet<IdentityId> {
        let mut out = self.holders_in(account, Custody::Owned, now);
        out.extend(self.holders_in(account, Custody::Delegated, now));
        out
    }

    /// Whether `v` is the recovery authority's key: it is never a credential of the book.
    fn is_recovery_key(&self, v: &Verifier) -> bool {
        self.config.recovery.recovery_key.is_some_and(|k| k.is(v))
    }

    /// Encumbered value of `account`: always zero in V0, where no encumbrance can exist
    /// (`DEF-11`). `TODO(F-024)`: the encumbrance a reclaim must see lifted.
    fn encumbered(&self, _account: &AccountId) -> Minor {
        Minor::ZERO
    }

    /// Commit `writes` after checking them.
    fn commit_all(&mut self, writes: &[Write]) -> Result<(), Refusal> {
        self.check(writes)?;
        self.commit(writes)
    }

    // ------------------------------------------------------------------ mechanisms

    /// The recovery mechanisms that would apply to a new owned-custody account of `owner`
    /// (`REQ-AU-9`, `AC-A5.3`): what the holder is shown before `open_account` with owned
    /// custody, whose digest that request carries. For the owner itself, or the operator opening
    /// the account for it. `ERR-NotAuthorised` if the deployment does not offer owned custody.
    pub fn recovery_terms(
        &self,
        caller: &Caller,
        owner: &IdentityId,
    ) -> Result<RecoveryMechanisms, Refusal> {
        let p = resolve(&self.state, caller)?;
        if (p.identity != *owner || p.on_behalf_of.is_some()) && !self.is_operator(&p) {
            return Err(Refusal::NotAuthorised);
        }
        if !self.config.recovery.offers_owned() {
            return Err(Refusal::NotAuthorised);
        }
        Ok(RecoveryMechanisms::of(
            &self.config.recovery,
            &self.state,
            owner,
            None,
        ))
    }

    /// The recovery mechanisms that apply to `account` (`REQ-AU-9`, `AC-A5.3`, `F-012` §5): the
    /// recovery authority and its delay, succession if offered (and the named successor), the
    /// holder's holder-held credentials if offered and registered. For whoever reaches the
    /// account (`INV-16`). Its digest is what `reclaim_custody` carries.
    pub fn recovery_mechanisms(
        &self,
        caller: &Caller,
        account: &AccountId,
        now: Time,
    ) -> Result<RecoveryMechanisms, Refusal> {
        let p = resolve(&self.state, caller)?;
        eval::reach_account(&self.state, &p, account, now)?;
        if !self.config.recovery.offers_owned() {
            return Err(Refusal::NotAuthorised);
        }
        let owner = self
            .state
            .account(account)
            .ok_or(Refusal::UnknownAccount)?
            .owner();
        Ok(RecoveryMechanisms::of(
            &self.config.recovery,
            &self.state,
            &owner,
            Some(account),
        ))
    }

    /// The acknowledgement rule of every path that makes an account owned (`REQ-AU-9`,
    /// `AC-A5.3`, `F-012` §5): owned custody is offered, and `acknowledged` is the digest of the
    /// mechanisms that apply now. Missing or stale is `ERR-NotAuthorised`.
    pub(super) fn require_acknowledged(
        &self,
        owner: &IdentityId,
        account: Option<&AccountId>,
        acknowledged: Option<MechanismsDigest>,
    ) -> Result<(), Refusal> {
        if !self.config.recovery.offers_owned() {
            return Err(Refusal::NotAuthorised);
        }
        let expected =
            RecoveryMechanisms::of(&self.config.recovery, &self.state, owner, account).digest();
        if acknowledged != Some(expected) {
            return Err(Refusal::NotAuthorised);
        }
        Ok(())
    }

    // ------------------------------------------------------------------ the credential set

    /// `import_credentials` (SPEC §8.1, `REQ-MG-7`, `REQ-MG-8`, amendment 0010): the operator imports
    /// credentials' public material into an identity's set, each marked
    /// [`CredentialMarker::Imported`]. An operator operation (`REQ-OS-1`): its own grant kind
    /// ([`OperatorOpKind::ImportCredentials`]), a justification, an audit record; and a notice to
    /// every authority holder of the identity's accounts (`INV-20`). The `n`th credential is minted
    /// at ordinal `n` of the operation.
    ///
    /// Which credentials the deployment accepts (for passkeys, its relying parties) is declared at
    /// the boundary that verifies them; the core checks each verifier's shape.
    ///
    /// Refusals, each audited: the frame's (`ERR-NotAuthorised`, `ERR-AuthorityRevoked`,
    /// `ERR-JustificationRequired`); `ERR-NotAuthorised` for an identity that is not active, one
    /// with an account in owned custody (`REQ-MG-8`), a scope naming an account the identity does
    /// not own, or the recovery authority's key; `ERR-CredentialInvalid` for no credential, more
    /// than [`MAX_IMPORTED_CREDENTIALS`], a verifier not well formed for its scheme, or one already
    /// in the set or given twice.
    pub fn import_credentials(
        &mut self,
        ctx: &Ctx,
        request: ImportCredentials,
        justification: &[u8],
    ) -> Result<Accepted<Vec<CredentialId>>, Refusal> {
        let top = self.enter(|| crate::call::Call::ImportCredentials {
            ctx: *ctx,
            request: request.clone(),
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<Vec<CredentialId>>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let n = request.credentials.len();
            let d = operator_digest("import_credentials", &p, justification, |c| {
                c.bytes(request.identity.as_bytes());
                c.u64(n as u64);
                for ic in &request.credentials {
                    ic.verifier.canon_into(c);
                    scope_into(c, ic.scope.as_ref());
                }
            });
            let minted = |op: &OperationId| -> Vec<CredentialId> {
                (0..n)
                    .map(|i| CredentialId::mint(op, u32::try_from(i).unwrap_or(u32::MAX)))
                    .collect()
            };
            if let Some(a) = self.replay(ctx, &p, &d, |r| minted(&r.op))? {
                return Ok(a);
            }
            let command = Command::ImportCredentials;
            let subjects = vec![AuditSubject::Identity(request.identity)];
            let run =
                |l: &Self| -> Result<(AuthorityUsed, Justification, Vec<AccountId>), Refusal> {
                    let (used, j) = l.frame(
                        &p,
                        OperatorOpKind::ImportCredentials,
                        justification,
                        ctx.now,
                    )?;
                    match l.state.identity(&request.identity) {
                        Some(i) if i.status() == IdentityStatus::Active => {}
                        _ => return Err(Refusal::NotAuthorised),
                    }
                    let accounts: Vec<AccountId> =
                        l.state.account_ids_of(&request.identity).copied().collect();
                    // `REQ-MG-8`: no power the operator does not hold already.
                    if l.state.holds_owned_custody(&request.identity) {
                        return Err(Refusal::NotAuthorised);
                    }
                    if n == 0 || n > MAX_IMPORTED_CREDENTIALS {
                        return Err(Refusal::CredentialInvalid);
                    }
                    let held: Vec<&Verifier> = l
                        .state
                        .credentials_of(&request.identity)
                        .filter(|c| c.status() == CredentialStatus::Active)
                        .map(Credential::verifier)
                        .collect();
                    for (i, ic) in request.credentials.iter().enumerate() {
                        if !ic.verifier.well_formed()
                            || held.contains(&&ic.verifier)
                            || request.credentials[..i]
                                .iter()
                                .any(|o| o.verifier == ic.verifier)
                        {
                            return Err(Refusal::CredentialInvalid);
                        }
                        if l.is_recovery_key(&ic.verifier) {
                            return Err(Refusal::NotAuthorised);
                        }
                        if let Some(scope) = &ic.scope
                            && !scope.iter().all(|a| accounts.contains(a))
                        {
                            return Err(Refusal::NotAuthorised);
                        }
                    }
                    Ok((used, j, accounts))
                };
            let (used, j, accounts) = match run(self) {
                Ok(x) => x,
                Err(e) => return Err(self.refused(ctx, &p, command, subjects, e)),
            };
            let mut writes = Vec::new();
            let mut ids = Vec::new();
            for (i, ic) in request.credentials.iter().enumerate() {
                let c = Credential::new(
                    Mint::new(ctx.op, u32::try_from(i).unwrap_or(u32::MAX)),
                    request.identity,
                    CredentialMarker::Imported,
                    ic.scope.clone(),
                    ic.verifier.clone(),
                );
                ids.push(c.id());
                writes.push(Write::PutCredential(c));
            }
            // Every authority holder of the identity's accounts, and the identity (`INV-20`).
            let mut recipients = BTreeSet::from([request.identity]);
            for a in &accounts {
                if let Some(acct) = self.state.account(a) {
                    recipients.extend(self.holders(acct, ctx.now));
                }
            }
            // One notice of the import per recipient (`INV-20`), naming its first credential: a
            // notice is one per (operation, recipient, event kind), and the record lists them all.
            if let Some(first) = ids.first() {
                writes.extend(notices(
                    ctx,
                    &recipients,
                    None,
                    NoticeEvent::CredentialRegistered(*first),
                ));
            }
            self.commit_operator(
                ctx, &p, d, command, used, j, accounts, subjects, writes, ids,
            )
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// The set the identities principal registers into when it acts for an identity it manages
    /// (amendment 0025, clause R2, ruling `pk-register`): `Some((identity, authority))` when
    /// a principal that is not an agent names another identity in `on_behalf_of`, `None`
    /// otherwise. It needs the identities principal's role, an unscoped credential, an ordinary
    /// credential (a holder-held recovery credential is the holder's own act), and an active
    /// identity that is not an agent and holds no account in owned custody (no power the operator
    /// does not hold already, as `REQ-MG-8` says of an import). Otherwise `ERR-NotAuthorised`.
    fn managed_set(
        &self,
        p: &Principal,
        request: &RegisterCredential,
        now: Time,
    ) -> Result<Option<(IdentityId, AuthorityUsed)>, Refusal> {
        let target = match p.on_behalf_of {
            Some(t) if p.kind != IdentityKind::Agent && t != p.identity => t,
            _ => return Ok(None),
        };
        let used = self.identities_principal(p, now)?;
        if !p.unscoped() || request.marker != CredentialMarker::Ordinary {
            return Err(Refusal::NotAuthorised);
        }
        match self.state.identity(&target) {
            Some(i) if i.status() == IdentityStatus::Active && i.kind() != IdentityKind::Agent => {}
            _ => return Err(Refusal::NotAuthorised),
        }
        if target == self.roles()?.operator || self.state.holds_owned_custody(&target) {
            return Err(Refusal::NotAuthorised);
        }
        Ok(Some((target, used)))
    }

    /// `register_credential` (SPEC §8.1, `REQ-AU-20`, `REQ-AU-4`, `REQ-AU-18`, `AC-A5.4`): a new
    /// member of the caller's own set, minted by this operation; or, under amendment 0025's clause
    /// R2, of the set of an identity the identities principal manages (`managed_set`).
    ///
    /// Refusals: `ERR-CredentialInvalid` (no working member presented: a set change needs an
    /// existing credential of the set, or a completed recovery), then `ERR-NotAuthorised` for a
    /// scope wider than the presenting credential's, a scope naming an account the identity does
    /// not own, a holder-held credential the deployment does not offer, the recovery authority's
    /// key as a verifier, or acting for another identity.
    pub fn register_credential(
        &mut self,
        ctx: &Ctx,
        request: RegisterCredential,
    ) -> Result<Accepted<CredentialId>, Refusal> {
        let top = self.enter(|| crate::call::Call::RegisterCredential {
            ctx: *ctx,
            request: request.clone(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<CredentialId>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let mut c = head("register_credential", &p);
            c.u8(marker_code(request.marker));
            scope_into(&mut c, request.scope.as_ref());
            request.verifier.canon_into(&mut c);
            let d = c.digest();
            if let Some(a) = self.replay(ctx, &p, &d, |r| CredentialId::mint(&r.op, 0))? {
                return Ok(a);
            }
            // The caller's own set; or, for the identities principal acting for an identity it
            // manages, that identity's (amendment 0025, `REQ-AU-20`).
            let (identity, used) = match self.managed_set(&p, &request, ctx.now)? {
                Some(x) => x,
                None => (Self::own_set(&p)?, owner_used(&p)),
            };
            // A verifier of the scheme it names (`DEC-19`, `F-028`).
            if !request.verifier.well_formed() {
                return Err(Refusal::CredentialInvalid);
            }
            if request.marker == CredentialMarker::RecoveryHeld && !self.config.recovery.holder_held
            {
                return Err(Refusal::NotAuthorised);
            }
            if !scope_within(request.scope.as_ref(), p.scope.as_ref()) {
                return Err(Refusal::NotAuthorised);
            }
            if let Some(s) = &request.scope {
                let owned = s
                    .iter()
                    .all(|a| self.state.account(a).is_some_and(|x| x.owner() == identity));
                if !owned {
                    return Err(Refusal::NotAuthorised);
                }
            }
            if self.is_recovery_key(&request.verifier) {
                return Err(Refusal::NotAuthorised);
            }
            let credential = Credential::new(
                Mint::new(ctx.op, 0),
                identity,
                request.marker,
                request.scope.clone(),
                request.verifier,
            );
            let id = credential.id();
            let accounts: Vec<AccountId> = match &request.scope {
                Some(s) => s.iter().copied().collect(),
                None => self.state.account_ids_of(&identity).copied().collect(),
            };
            let record = OperationRecord {
                op: ctx.op,
                command: Command::RegisterCredential,
                authorities: vec![used],
                accounts,
                postings: Vec::new(),
                at: ctx.now,
            };
            let mut writes = vec![
                Write::PutCredential(credential),
                Write::PutRecord(record.clone()),
                Self::request_entry(ctx, &p, d, None),
                subjects(ctx, vec![AuditSubject::Identity(identity)]),
            ];
            writes.extend(notices(
                ctx,
                &BTreeSet::from([identity]),
                None,
                NoticeEvent::CredentialRegistered(id),
            ));
            self.commit_all(&writes)?;
            Ok(Accepted {
                value: id,
                record,
                status: None,
            })
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// `remove_credential` (SPEC §8.1, `REQ-AU-20`, `REQ-AU-3`, `AC-A5.4`): a member of the
    /// caller's own set stops working in the commit that records it; the next command it presents
    /// is `ERR-CredentialInvalid`.
    ///
    /// Refusals: `ERR-CredentialInvalid` (no working member presented), then `ERR-NotAuthorised`
    /// for a credential that is not an active member of the caller's set, one wider than the
    /// presenting credential (`REQ-AU-4`), or the set's last active member (`REQ-AU-20`,
    /// amendment 0006 item 10: [`recovery_rules::may_remove`]).
    pub fn remove_credential(
        &mut self,
        ctx: &Ctx,
        credential: CredentialId,
    ) -> Result<Accepted<()>, Refusal> {
        let top = self.enter(|| crate::call::Call::RemoveCredential {
            ctx: *ctx,
            credential,
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<()>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let mut c = head("remove_credential", &p);
            c.bytes(credential.as_bytes());
            let d = c.digest();
            if let Some(a) = self.replay(ctx, &p, &d, |_| ())? {
                return Ok(a);
            }
            let identity = Self::own_set(&p)?;
            let target = self
                .state
                .credential(&credential)
                .filter(|c| c.identity() == identity && c.status() == CredentialStatus::Active)
                .ok_or(Refusal::NotAuthorised)?;
            if !scope_within(target.scope(), p.scope.as_ref()) {
                return Err(Refusal::NotAuthorised);
            }
            let active = self
                .state
                .credentials_of(&identity)
                .filter(|c| c.status() == CredentialStatus::Active)
                .count();
            if !recovery_rules::may_remove(active.saturating_sub(1)) {
                return Err(Refusal::NotAuthorised);
            }
            let accounts: Vec<AccountId> = match target.scope() {
                Some(s) => s.iter().copied().collect(),
                None => self.state.account_ids_of(&identity).copied().collect(),
            };
            let record = OperationRecord {
                op: ctx.op,
                command: Command::RemoveCredential,
                authorities: vec![owner_used(&p)],
                accounts,
                postings: Vec::new(),
                at: ctx.now,
            };
            let mut writes = vec![
                Write::CredentialStatus(credential, CredentialStatus::Removed),
                Write::PutRecord(record.clone()),
                Self::request_entry(ctx, &p, d, None),
                subjects(ctx, vec![AuditSubject::Identity(identity)]),
            ];
            writes.extend(notices(
                ctx,
                &BTreeSet::from([identity]),
                None,
                NoticeEvent::CredentialRemoved(credential),
            ));
            self.commit_all(&writes)?;
            Ok(Accepted {
                value: (),
                record,
                status: None,
            })
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    // ------------------------------------------------------------------ custody

    /// `delegate_custody` (SPEC §8.1, `REQ-AU-11`, `REQ-AU-12`, `AC-A7.1`, `AC-A7.2`): owned →
    /// delegated, with the holder's own credential. Operator authority exists from the next
    /// command: the evaluator reads custody at every check. The change is recorded as the
    /// holder's.
    ///
    /// Refusals: `ERR-CredentialInvalid`; `ERR-UnknownAccount` for an account the caller cannot
    /// reach; `ERR-NotAuthorised` for anyone but the holder — the operator included — and for an
    /// account that is not owned; the account's status.
    pub fn delegate_custody(
        &mut self,
        ctx: &Ctx,
        account: AccountId,
    ) -> Result<Accepted<()>, Refusal> {
        let top = self.enter(|| crate::call::Call::DelegateCustody { ctx: *ctx, account });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<()>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let mut c = head("delegate_custody", &p);
            c.bytes(account.as_bytes());
            let d = c.digest();
            if let Some(a) = self.replay(ctx, &p, &d, |_| ())? {
                return Ok(a);
            }
            let (acct, used) = self.holder_of(&p, &account)?;
            acct.require_open()?;
            if acct.custody() != Custody::Owned {
                return Err(Refusal::NotAuthorised);
            }
            let record = OperationRecord {
                op: ctx.op,
                command: Command::DelegateCustody,
                authorities: vec![used],
                accounts: vec![account],
                postings: Vec::new(),
                at: ctx.now,
            };
            let recipients = self.holders(&acct, ctx.now);
            let mut writes = vec![
                Write::Custody(CustodyWrite::SetCustody {
                    account,
                    custody: Custody::Delegated,
                }),
                Write::PutRecord(record.clone()),
                Self::request_entry(ctx, &p, d, None),
            ];
            writes.extend(notices(
                ctx,
                &recipients,
                Some(account),
                NoticeEvent::CustodyDelegated,
            ));
            self.commit_all(&writes)?;
            Ok(Accepted {
                value: (),
                record,
                status: None,
            })
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// `reclaim_custody` (SPEC §8.1, `REQ-AU-11`, `REQ-AU-13`, `REQ-AU-14`, `AC-A7.3`–`AC-A7.5`,
    /// `AC-A5.3`): delegated → owned. Before it takes effect, a holder credential must be proven to
    /// work: an active member of the owner's set, covering the account, signs
    /// [`reclaim_challenge`]`(account, custody epoch)`. The recovery mechanisms must be
    /// acknowledged. No encumbrance may stand (none can in V0, `DEF-11`).
    ///
    /// Operations already reserved finish under the authority that started them: their capture,
    /// release and submission are checked against the custody they were accepted under
    /// ([`crate::custody::snapshot`]). Delegated grants are not touched (`REQ-AU-14`).
    ///
    /// Refusals: `ERR-CredentialInvalid` for no working credential presented, or a proof that is
    /// not a working member's signature over this challenge; `ERR-UnknownAccount`;
    /// `ERR-NotAuthorised` for anyone but the holder, an account not delegated, a missing or stale
    /// acknowledgement, or a standing encumbrance; the account's status.
    pub fn reclaim_custody(
        &mut self,
        ctx: &Ctx,
        request: ReclaimCustody,
    ) -> Result<Accepted<()>, Refusal> {
        let top = self.enter(|| crate::call::Call::ReclaimCustody { ctx: *ctx, request });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<()>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let account = request.account;
            let mut c = head("reclaim_custody", &p);
            c.bytes(account.as_bytes());
            c.bytes(request.proof.credential.as_bytes());
            c.u8(match request.proof.proof {
                Proof::Signature(_) => 1,
                Proof::PasskeyChecked => 2,
            });
            c.opt(request.acknowledged, |c, d| {
                c.bytes(&d.0);
            });
            let d = c.digest();
            if let Some(a) = self.replay(ctx, &p, &d, |_| ())? {
                return Ok(a);
            }
            let (acct, used) = self.holder_of(&p, &account)?;
            acct.require_open()?;
            if acct.custody() != Custody::Delegated {
                return Err(Refusal::NotAuthorised);
            }
            // The proven credential (`REQ-AU-13`).
            let prover = self
                .state
                .credential(&request.proof.credential)
                .filter(|c| {
                    c.identity() == acct.owner()
                        && c.status() == CredentialStatus::Active
                        && covers(c.scope(), &account)
                })
                .ok_or(Refusal::CredentialInvalid)?;
            // The proof is the one the credential's scheme admits (`DEC-19`): a `key` signature
            // the core checks, or a passkey assertion ingress checked. An API key proves nothing
            // here: it is a bearer secret, not a holder's device (`F-028` plan, open question 2).
            let challenge = reclaim_challenge(&account, self.state.custody().epoch(&account));
            let proven = match (prover.verifier().scheme(), request.proof.proof) {
                (Scheme::Key, Proof::Signature(sig)) => {
                    verifies(prover.verifier().as_bytes(), &challenge, &sig)
                }
                (Scheme::WebAuthnEs256, Proof::PasskeyChecked) => true,
                _ => false,
            };
            if !proven {
                return Err(Refusal::CredentialInvalid);
            }
            self.require_acknowledged(&acct.owner(), Some(&account), request.acknowledged)?;
            if self.encumbered(&account) != Minor::ZERO {
                return Err(Refusal::NotAuthorised);
            }
            let mut authorities = vec![used];
            if request.proof.credential != p.credential {
                authorities.push(AuthorityUsed {
                    credential: request.proof.credential,
                    ..used
                });
            }
            let record = OperationRecord {
                op: ctx.op,
                command: Command::ReclaimCustody,
                authorities,
                accounts: vec![account],
                postings: Vec::new(),
                at: ctx.now,
            };
            let recipients = self.holders(&acct, ctx.now);
            let mut writes = vec![
                Write::Custody(CustodyWrite::SetCustody {
                    account,
                    custody: Custody::Owned,
                }),
                Write::PutRecord(record.clone()),
                Self::request_entry(ctx, &p, d, None),
            ];
            writes.extend(notices(
                ctx,
                &recipients,
                Some(account),
                NoticeEvent::CustodyReclaimed,
            ));
            self.commit_all(&writes)?;
            Ok(Accepted {
                value: (),
                record,
                status: None,
            })
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    // ------------------------------------------------------------------ recovery

    /// `initiate_recovery` (SPEC §8.1, `REQ-AU-15`, `REQ-AU-16`, `AC-A8.2`): on the operator
    /// surface, by the operator on the holder's out-of-band request, with a justification. A
    /// pending recovery of an owned-custody account, effective after the deployment's delay, is
    /// recorded and shown to every authority holder of the account. It names the credential the
    /// recovery will issue. At most one is pending per account.
    ///
    /// Refusals, each audited: `ERR-NotAuthorised` for anyone but the operator's policy
    /// authority; `ERR-JustificationRequired`; `ERR-UnknownAccount`; `ERR-AccountClosed`;
    /// `ERR-NotAuthorised` for an account not owned, a deployment that offers no recovery, a
    /// recovery already pending, a scope that does not name the account or names another
    /// identity's, or the recovery authority's key as the verifier.
    pub fn initiate_recovery(
        &mut self,
        ctx: &Ctx,
        request: InitiateRecovery,
        justification: &[u8],
    ) -> Result<Accepted<PendingId>, Refusal> {
        let top = self.enter(|| crate::call::Call::InitiateRecovery {
            ctx: *ctx,
            request: request.clone(),
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<PendingId>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let d = operator_digest("initiate_recovery", &p, justification, |c| {
                c.bytes(request.account.as_bytes());
                request.verifier.canon_into(c);
                scope_into(c, request.scope.as_ref());
            });
            if let Some(a) = self.replay(ctx, &p, &d, |r| PendingId(r.op))? {
                return Ok(a);
            }
            let command = Command::InitiateRecovery;
            let audit = vec![AuditSubject::Account(request.account)];
            let run = |l: &Self| -> Result<(Justification, PendingChange, LedgerAccount), Refusal> {
                if !l.is_operator(&p) {
                    return Err(Refusal::NotAuthorised);
                }
                let j = Justification::new(justification)?;
                let acct = l
                    .state
                    .account(&request.account)
                    .ok_or(Refusal::UnknownAccount)?
                    .clone();
                acct.require_creditable()?;
                if acct.custody() != Custody::Owned
                    || acct.kind() == AccountKind::Issuance
                    || !l.config.recovery.offers_owned()
                    || l.state
                        .custody()
                        .open(&request.account, ChangeKind::Recovery)
                        .is_some()
                    || l.is_recovery_key(&request.verifier)
                    || !request.verifier.well_formed()
                {
                    return Err(Refusal::NotAuthorised);
                }
                if let Some(s) = &request.scope {
                    let fits = s.contains(&request.account)
                        && s.iter().all(|a| {
                            l.state
                                .account(a)
                                .is_some_and(|x| x.owner() == acct.owner())
                        });
                    if !fits {
                        return Err(Refusal::NotAuthorised);
                    }
                }
                let effective_at = ctx
                    .now
                    .checked_add(l.config.recovery.recovery_delay)
                    .ok_or(Refusal::NotAuthorised)?;
                let pending = PendingChange {
                    id: PendingId(ctx.op),
                    kind: ChangeKind::Recovery,
                    account: request.account,
                    owner: acct.owner(),
                    initiated_by: policy_used(&p),
                    initiated_at: ctx.now,
                    effective_at,
                    target: ChangeTarget::Credential {
                        verifier: request.verifier.clone(),
                        scope: request.scope.clone(),
                    },
                    state: ChangeState::Pending,
                    closed: None,
                };
                Ok((j, pending, acct))
            };
            let (j, pending, acct) = match run(self) {
                Ok(x) => x,
                Err(e) => return Err(self.refused(ctx, &p, command, audit, e)),
            };
            let id = pending.id;
            let event = NoticeEvent::RecoveryInitiated {
                pending: id,
                effective_at: pending.effective_at,
            };
            let record = OperationRecord {
                op: ctx.op,
                command,
                authorities: vec![pending.initiated_by],
                accounts: vec![request.account],
                postings: Vec::new(),
                at: ctx.now,
            };
            let recipients = self.holders_in(&acct, acct.custody(), ctx.now);
            let mut writes = vec![
                Write::Custody(CustodyWrite::Open(pending)),
                Write::PutRecord(record.clone()),
                Self::operator_entry(ctx, &p, d),
                Self::justified(ctx, j, vec![AuditSubject::Identity(acct.owner())]),
            ];
            writes.extend(notices(ctx, &recipients, Some(request.account), event));
            self.commit_all(&writes)?;
            Ok(Accepted {
                value: id,
                record,
                status: None,
            })
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// `cancel_recovery` (SPEC §8.1, `REQ-AU-16`, `AC-A8.2`, `AC-A8.5`): the holder cancels a
    /// pending recovery or succession of its account, with any active member of its set whose
    /// scope covers the account — a holder-held recovery credential included (`AC-A8.6`) — at any
    /// time before it completes. Ordinary use of a credential does not cancel
    /// ([`recovery_rules::use_cancels_pending`]).
    ///
    /// Refusals: `ERR-CredentialInvalid`; `ERR-UnknownOperation` for no such change;
    /// `ERR-UnknownAccount` or `ERR-NotAuthorised` for anyone but the holder;
    /// `ERR-OperationTerminal` for a change already cancelled or completed.
    pub fn cancel_recovery(
        &mut self,
        ctx: &Ctx,
        pending: PendingId,
    ) -> Result<Accepted<()>, Refusal> {
        let top = self.enter(|| crate::call::Call::CancelRecovery { ctx: *ctx, pending });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<()>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let mut c = head("cancel_recovery", &p);
            c.bytes(&pending.0.to_ref().0);
            let d = c.digest();
            if let Some(a) = self.replay(ctx, &p, &d, |_| ())? {
                return Ok(a);
            }
            let change = self
                .state
                .custody()
                .pending(&pending)
                .ok_or(Refusal::UnknownOperation)?
                .clone();
            let (acct, used) = self.holder_of(&p, &change.account)?;
            let facts = ChangeFacts {
                holder: true,
                ..ChangeFacts::default()
            };
            let to = match next(change.state, ChangeTrigger::Cancel, facts) {
                ChangeDecision::Move(e) => e.to,
                ChangeDecision::Refuse(r) => return Err(r),
            };
            let record = OperationRecord {
                op: ctx.op,
                command: Command::CancelRecovery,
                authorities: vec![used],
                accounts: vec![change.account],
                postings: Vec::new(),
                at: ctx.now,
            };
            let mut recipients = self.holders_in(&acct, acct.custody(), ctx.now);
            if let ChangeTarget::Successor(s) = change.target {
                recipients.insert(s);
            }
            let mut writes = vec![
                Write::Custody(CustodyWrite::Close {
                    id: pending,
                    to,
                    closed: Closed {
                        at: ctx.now,
                        by: ctx.op,
                    },
                }),
                Write::PutRecord(record.clone()),
                Self::request_entry(ctx, &p, d, None),
                subjects(ctx, vec![AuditSubject::Operation(pending.0)]),
            ];
            writes.extend(notices(
                ctx,
                &recipients,
                Some(change.account),
                NoticeEvent::Cancelled(pending),
            ));
            self.commit_all(&writes)?;
            Ok(Accepted {
                value: (),
                record,
                status: None,
            })
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// `complete_recovery` (SPEC §8.1, `REQ-AU-15`, `REQ-AU-16`, `REQ-AU-20`, `AC-A8.1`,
    /// `AC-A8.3`, `AC-A8.4`): on the operator surface, with a justification, carrying the
    /// recovery authority's `approval`: its signature over
    /// [`approval_message`]`(pending, account, verifier)`. In one commit the new credential joins
    /// the holder's set, every active member whose scope covers the account is retired, and the
    /// audit record names the initiator, the recovery authority and the submitter. Delegated
    /// grants survive ([`recovery_rules::grants_revoked_on`]).
    ///
    /// Refusals, each audited: `ERR-NotAuthorised` for anyone but the operator's policy
    /// authority; `ERR-JustificationRequired`; `ERR-UnknownOperation`; `ERR-InvalidTransition` for
    /// a succession; `ERR-OperationTerminal`; `ERR-NotAuthorised` without the recovery authority's
    /// approval; `ERR-RecoveryNotElapsed` before `effective_at`; `ERR-NotAuthorised` if the account
    /// changed owner or left owned custody meanwhile; `ERR-AccountClosed`.
    pub fn complete_recovery(
        &mut self,
        ctx: &Ctx,
        pending: PendingId,
        approval: Signature,
        justification: &[u8],
    ) -> Result<Accepted<CredentialId>, Refusal> {
        let top = self.enter(|| crate::call::Call::CompleteRecovery {
            ctx: *ctx,
            pending,
            approval,
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<CredentialId>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let d = operator_digest("complete_recovery", &p, justification, |c| {
                c.bytes(&pending.0.to_ref().0);
            });
            if let Some(a) = self.replay(ctx, &p, &d, |r| CredentialId::mint(&r.op, 0))? {
                return Ok(a);
            }
            let command = Command::CompleteRecovery;
            let audit = vec![AuditSubject::Operation(pending.0)];
            type Planned = (
                Justification,
                PendingChange,
                LedgerAccount,
                Credential,
                Vec<Write>,
            );
            let run = |l: &Self| -> Result<Planned, Refusal> {
                if !l.is_operator(&p) {
                    return Err(Refusal::NotAuthorised);
                }
                let j = Justification::new(justification)?;
                let change = l
                    .state
                    .custody()
                    .pending(&pending)
                    .ok_or(Refusal::UnknownOperation)?
                    .clone();
                let ChangeTarget::Credential { verifier, scope } = &change.target else {
                    return Err(Refusal::InvalidTransition);
                };
                let approved = l.config.recovery.recovery_key.is_some_and(|k| {
                    verifies(
                        &k.0,
                        &approval_message(&change.id, &change.account, verifier),
                        &approval,
                    )
                });
                let facts = ChangeFacts {
                    holder: false,
                    approved,
                    elapsed: ctx.now >= change.effective_at,
                };
                if let ChangeDecision::Refuse(r) =
                    next(change.state, ChangeTrigger::Complete, facts)
                {
                    return Err(r);
                }
                let acct = l
                    .state
                    .account(&change.account)
                    .ok_or(Refusal::UnknownAccount)?
                    .clone();
                acct.require_creditable()?;
                if acct.owner() != change.owner || acct.custody() != Custody::Owned {
                    return Err(Refusal::NotAuthorised);
                }
                let credential = Credential::new(
                    Mint::new(ctx.op, 0),
                    change.owner,
                    CredentialMarker::Ordinary,
                    scope.clone(),
                    verifier.clone(),
                );
                // "The previous one stops working" (`AC-A8.3`): every active member whose scope
                // covers the recovered account. Members scoped only to other accounts are kept.
                let mut effects: Vec<Write> = l
                    .state
                    .credentials_of(&change.owner)
                    .filter(|c| {
                        c.status() == CredentialStatus::Active && covers(c.scope(), &change.account)
                    })
                    .map(|c| Write::CredentialStatus(c.id(), CredentialStatus::Removed))
                    .collect();
                if recovery_rules::grants_revoked_on(ChangeKind::Recovery) {
                    effects.extend(l.grants_of_holder(&acct, change.owner, ctx.now));
                }
                Ok((j, change, acct, credential, effects))
            };
            let (j, change, acct, credential, effects) = match run(self) {
                Ok(x) => x,
                Err(e) => return Err(self.refused(ctx, &p, command, audit, e)),
            };
            let authority = self
                .config
                .recovery
                .authority()
                .ok_or(Refusal::NotAuthorised)?;
            let id = credential.id();
            let record = OperationRecord {
                op: ctx.op,
                command,
                authorities: vec![change.initiated_by, authority.used(), policy_used(&p)],
                accounts: vec![change.account],
                postings: Vec::new(),
                at: ctx.now,
            };
            let recipients = self.holders_in(&acct, acct.custody(), ctx.now);
            let mut writes = vec![Write::PutCredential(credential)];
            writes.extend(effects);
            writes.push(Write::Custody(CustodyWrite::Close {
                id: pending,
                to: ChangeState::Completed,
                closed: Closed {
                    at: ctx.now,
                    by: ctx.op,
                },
            }));
            writes.push(Write::PutRecord(record.clone()));
            writes.push(Self::operator_entry(ctx, &p, d));
            writes.push(Self::justified(
                ctx,
                j,
                vec![
                    AuditSubject::Identity(change.owner),
                    AuditSubject::Operation(pending.0),
                ],
            ));
            writes.extend(notices(
                ctx,
                &recipients,
                Some(change.account),
                NoticeEvent::RecoveryCompleted(pending),
            ));
            self.commit_all(&writes)?;
            Ok(Accepted {
                value: id,
                record,
                status: None,
            })
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// The revocations of the delegated grants `holder` made over `account` that are still
    /// standing: what a change revokes when [`recovery_rules::grants_revoked_on`] says so. Grants
    /// narrowed from them stop with them, as every evaluation walks the chain (`REQ-AU-3`).
    fn grants_of_holder(
        &self,
        account: &LedgerAccount,
        holder: IdentityId,
        now: Time,
    ) -> Vec<Write> {
        self.state
            .grants_over(Some(account.id()))
            .filter(|g| {
                g.kind() == GrantKind::Delegated
                    && g.subject() == Subject::Account(account.id())
                    && g.grantor() == holder
                    && g.revoked().is_none()
            })
            .map(|g| Write::Revoke(g.id(), now))
            .collect()
    }

    // ------------------------------------------------------------------ succession

    /// `name_successor` (SPEC §8.1, `REQ-AU-17`): the holder names the identity that may claim
    /// the account after the deployment's succession delay, or clears the name with `None`.
    ///
    /// Refusals: `ERR-CredentialInvalid`; `ERR-UnknownAccount` or `ERR-NotAuthorised` for anyone
    /// but the holder; `ERR-AccountClosed`; `ERR-NotAuthorised` for a deployment that offers no
    /// succession, a succession already pending, or a successor that is not an active person or
    /// organisation other than the holder.
    pub fn name_successor(
        &mut self,
        ctx: &Ctx,
        account: AccountId,
        successor: Option<IdentityId>,
    ) -> Result<Accepted<()>, Refusal> {
        let top = self.enter(|| crate::call::Call::NameSuccessor {
            ctx: *ctx,
            account,
            successor,
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<()>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let mut c = head("name_successor", &p);
            c.bytes(account.as_bytes());
            c.opt(successor, |c, s| {
                c.bytes(s.as_bytes());
            });
            let d = c.digest();
            if let Some(a) = self.replay(ctx, &p, &d, |_| ())? {
                return Ok(a);
            }
            let (acct, used) = self.holder_of(&p, &account)?;
            acct.require_creditable()?;
            if self.config.recovery.succession_delay.is_none()
                || self
                    .state
                    .custody()
                    .open(&account, ChangeKind::Succession)
                    .is_some()
            {
                return Err(Refusal::NotAuthorised);
            }
            if let Some(s) = successor {
                let fit = self.state.identity(&s).is_some_and(|i| {
                    i.status() == IdentityStatus::Active && i.kind() != IdentityKind::Agent
                }) && s != acct.owner();
                if !fit {
                    return Err(Refusal::NotAuthorised);
                }
            }
            let record = OperationRecord {
                op: ctx.op,
                command: Command::NameSuccessor,
                authorities: vec![used],
                accounts: vec![account],
                postings: Vec::new(),
                at: ctx.now,
            };
            let mut recipients = self.holders_in(&acct, acct.custody(), ctx.now);
            recipients.extend(successor);
            let mut writes = vec![
                Write::Custody(CustodyWrite::Successor { account, successor }),
                Write::PutRecord(record.clone()),
                Self::request_entry(ctx, &p, d, None),
                subjects(
                    ctx,
                    successor.into_iter().map(AuditSubject::Identity).collect(),
                ),
            ];
            writes.extend(notices(
                ctx,
                &recipients,
                Some(account),
                NoticeEvent::SuccessorNamed(successor),
            ));
            self.commit_all(&writes)?;
            Ok(Accepted {
                value: (),
                record,
                status: None,
            })
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// `claim_succession` (SPEC §8.1, `REQ-AU-16`, `REQ-AU-17`, `AC-A8.5`), in two phases. The
    /// named successor's first claim opens a pending succession, effective after the
    /// deployment's delay, which the holder sees and can cancel. Its claim after `effective_at`
    /// completes it: the account's owner becomes the successor, so the previous owner's
    /// credentials no longer cover it, and the delegated grants the previous owner made over it
    /// are revoked ([`recovery_rules::grants_revoked_on`]). An early second claim is
    /// `ERR-RecoveryNotElapsed`.
    ///
    /// The claim acts with an unscoped credential of the successor, for itself, and is recorded
    /// as owner authority claimed by the successor.
    ///
    /// Refusals: `ERR-CredentialInvalid`; `ERR-UnknownAccount` for anyone not named (or
    /// `ERR-NotAuthorised` if it reaches the account); `ERR-NotAuthorised` for a scoped
    /// credential or a deployment that offers no succession; `ERR-AccountClosed`;
    /// `ERR-RecoveryNotElapsed`.
    pub fn claim_succession(
        &mut self,
        ctx: &Ctx,
        account: AccountId,
    ) -> Result<Accepted<Claimed>, Refusal> {
        let top = self.enter(|| crate::call::Call::ClaimSuccession { ctx: *ctx, account });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<Claimed>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let mut c = head("claim_succession", &p);
            c.bytes(account.as_bytes());
            let d = c.digest();
            let custody_records = self.state.custody();
            let replayed = self.replay(ctx, &p, &d, |r| {
                match custody_records.pending(&PendingId(r.op)) {
                    Some(c) => Claimed::Pending {
                        pending: c.id,
                        effective_at: c.effective_at,
                    },
                    None => Claimed::Completed {
                        pending: custody_records
                            .completed_by(&account, &p.identity, &r.op)
                            .unwrap_or(PendingId(r.op)),
                    },
                }
            })?;
            if let Some(a) = replayed {
                return Ok(a);
            }
            let acct = self
                .state
                .account(&account)
                .ok_or(Refusal::UnknownAccount)?
                .clone();
            if self.state.custody().successor(&account) != Some(p.identity) {
                return Err(
                    if eval::knows(&self.state, &p, &acct) || self.is_operator(&p) {
                        Refusal::NotAuthorised
                    } else {
                        Refusal::UnknownAccount
                    },
                );
            }
            if !p.unscoped() || p.on_behalf_of.is_some() {
                return Err(Refusal::NotAuthorised);
            }
            let delay = self
                .config
                .recovery
                .succession_delay
                .ok_or(Refusal::NotAuthorised)?;
            acct.require_creditable()?;
            let claim = owner_used(&p);
            let holders = self.holders_in(&acct, acct.custody(), ctx.now);
            let open = self
                .state
                .custody()
                .open(&account, ChangeKind::Succession)
                .cloned();
            let (value, mut writes, mut recipients, event) = match open {
                None => {
                    let effective_at = ctx.now.checked_add(delay).ok_or(Refusal::NotAuthorised)?;
                    let pending = PendingChange {
                        id: PendingId(ctx.op),
                        kind: ChangeKind::Succession,
                        account,
                        owner: acct.owner(),
                        initiated_by: claim,
                        initiated_at: ctx.now,
                        effective_at,
                        target: ChangeTarget::Successor(p.identity),
                        state: ChangeState::Pending,
                        closed: None,
                    };
                    let id = pending.id;
                    (
                        Claimed::Pending {
                            pending: id,
                            effective_at,
                        },
                        vec![Write::Custody(CustodyWrite::Open(pending))],
                        holders,
                        NoticeEvent::SuccessionClaimed {
                            pending: id,
                            effective_at,
                        },
                    )
                }
                Some(change) => {
                    if change.target != ChangeTarget::Successor(p.identity) {
                        return Err(Refusal::NotAuthorised);
                    }
                    let facts = ChangeFacts {
                        holder: false,
                        approved: true,
                        elapsed: ctx.now >= change.effective_at,
                    };
                    if let ChangeDecision::Refuse(r) =
                        next(change.state, ChangeTrigger::Complete, facts)
                    {
                        return Err(r);
                    }
                    if acct.owner() != change.owner {
                        return Err(Refusal::NotAuthorised);
                    }
                    let mut writes = vec![Write::Custody(CustodyWrite::SetOwner {
                        account,
                        owner: p.identity,
                    })];
                    if recovery_rules::grants_revoked_on(ChangeKind::Succession) {
                        writes.extend(self.grants_of_holder(&acct, change.owner, ctx.now));
                    }
                    writes.push(Write::Custody(CustodyWrite::Close {
                        id: change.id,
                        to: ChangeState::Completed,
                        closed: Closed {
                            at: ctx.now,
                            by: ctx.op,
                        },
                    }));
                    writes.push(Write::Custody(CustodyWrite::Successor {
                        account,
                        successor: None,
                    }));
                    (
                        Claimed::Completed { pending: change.id },
                        writes,
                        holders,
                        NoticeEvent::SuccessionCompleted(change.id),
                    )
                }
            };
            recipients.insert(p.identity);
            let record = OperationRecord {
                op: ctx.op,
                command: Command::ClaimSuccession,
                authorities: vec![claim],
                accounts: vec![account],
                postings: Vec::new(),
                at: ctx.now,
            };
            writes.push(Write::PutRecord(record.clone()));
            writes.push(Self::request_entry(ctx, &p, d, None));
            writes.push(subjects(ctx, vec![AuditSubject::Identity(acct.owner())]));
            writes.extend(notices(ctx, &recipients, Some(account), event));
            self.commit_all(&writes)?;
            Ok(Accepted {
                value,
                record,
                status: None,
            })
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    // ------------------------------------------------------------------ the account view

    /// An account's custody as its authority holders see it (`INV-20`, `REQ-AU-11`,
    /// `REQ-AU-16`, `F-012` §11): the custody mode, every recovery and succession recorded for
    /// it with its `effective_at`, and the named successor. For whoever reaches the account
    /// (`INV-16`).
    pub fn custody_view(
        &self,
        caller: &Caller,
        account: &AccountId,
        now: Time,
    ) -> Result<CustodyView, Refusal> {
        let p = resolve(&self.state, caller)?;
        eval::reach_account(&self.state, &p, account, now)?;
        let acct = self.state.account(account).ok_or(Refusal::UnknownAccount)?;
        let records = self.state.custody();
        Ok(CustodyView {
            account: *account,
            owner: acct.owner(),
            custody: acct.custody(),
            successor: records.successor(account),
            epoch: records.epoch(account),
            changes: records
                .of_account(account)
                .map(|c| PendingView {
                    id: c.id,
                    kind: c.kind,
                    state: c.state,
                    initiated_at: c.initiated_at,
                    effective_at: c.effective_at,
                })
                .collect(),
        })
    }

    /// The custody notices in the outbox, for `recipient` (`F-012` §11). `F-009` delivers them.
    pub fn custody_notices(&self, recipient: &IdentityId) -> Vec<Notice> {
        self.state
            .custody()
            .outbox()
            .filter(|n| n.recipient == *recipient)
            .copied()
            .collect()
    }
}
