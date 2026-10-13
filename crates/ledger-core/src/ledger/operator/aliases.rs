// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Aliases after genesis and the grants on an account (`REQ-RC-7`, amendment 0024; owner ruling
//! `system-accounts-operator`; `ops/runbooks/system-accounts.md`).
//!
//! - `register_alias` (SPEC §8.1): the operator names a ledger account by (source, reference), as a
//!   seed's aliases do (`REQ-RC-6`), after genesis. It is how a system account gets its role
//!   (`issuer/fee`, …), which services resolve instead of storing the account's identifier. The
//!   operator's policy authority alone, as for `tie_principal`: it configures the deployment, so no
//!   grant gives it. An alias names one account, for good: the book keeps only its key, a digest
//!   of the source and the reference (`INV-17`), and never re-points or removes it.
//! - `alias(source, reference)` (SPEC §8.2): the account an alias names, to a principal that
//!   reaches that account (`INV-16`); to anyone else, as to an alias that does not exist,
//!   `ERR-UnknownAccount`.
//! - `grants(account)` (SPEC §8.2): every grant whose subject is the account, live and revoked, to
//!   the operator's policy authority or the account's owner. The recipe's check that each service
//!   holds exactly the grants it should becomes a read.
//! - `grants(identity)` (SPEC §8.2, amendment 0026): the grants whose subject is an identity, to
//!   the principal that holds them; every one to the identity itself, the operator's policy
//!   authority and the identities principal. A holder learns the id of its grant over an identity, which it names as the
//!   `parent` of the account grants it makes under it (amendment 0025 R4).
//!
//! An identifier the initial state left out (`ERR-IdentifierArchived`, `REQ-RC-7`) is ingress's to
//! answer: the archived list is the deployment's, fixed with its genesis, and never reaches the
//! book.

use alloc::vec;
use alloc::vec::Vec;

use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use super::digest;
use crate::account::{AccountKind, AccountStatus, Time};
use crate::audit::{AuditSubject, Justification};
use crate::authority::{AuthorityKind, Grant, Subject};
use crate::chainspec::{SpecWrite, alias_key};
use crate::eval;
use crate::id::{AccountId, IdentityId};
use crate::ledger::{Accepted, Ctx, Ledger, policy_used};
use crate::opening::OpeningSource;
use crate::principal::{Caller, resolve};
use crate::refusal::Refusal;
use crate::state::{Command, Write};

/// The longest reference an alias takes, in bytes. A prior system's identifier or a role name is
/// far shorter; the bound keeps a command's body small.
pub const MAX_ALIAS_REFERENCE: usize = 256;

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    /// `register_alias` (SPEC §8.1, `REQ-RC-7`): name `account` by (`source`, `reference`).
    ///
    /// Refusals, each audited: `ERR-NotAuthorised` for anyone but the operator's policy authority,
    /// or for an alias that already names an account (this one or another: an alias is never
    /// re-pointed); `ERR-UnknownAccount` for an account the book does not hold, or the issuance
    /// counterparty; `ERR-AccountClosed`; `ERR-MalformedCommand` for an empty reference or one over
    /// [`MAX_ALIAS_REFERENCE`]; `ERR-JustificationRequired`.
    pub fn register_alias(
        &mut self,
        ctx: &Ctx,
        source: OpeningSource,
        reference: &[u8],
        account: AccountId,
        justification: &[u8],
    ) -> Result<Accepted<()>, Refusal> {
        let top = self.enter(|| crate::call::Call::RegisterAlias {
            ctx: *ctx,
            source,
            reference: reference.to_vec(),
            account,
            justification: justification.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<()>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let key = alias_key(&source, reference);
            let d = digest("register_alias", &p, justification, |c| {
                c.bytes(&key);
                c.bytes(account.as_bytes());
            });
            if let Some(a) = self.replay(ctx, &p, &d, |_| ())? {
                return Ok(a);
            }
            let command = Command::RegisterAlias;
            let subjects = vec![AuditSubject::Account(account)];
            let run = |l: &Self| -> Result<crate::audit::Justification, Refusal> {
                if !l.is_operator(&p) {
                    return Err(Refusal::NotAuthorised);
                }
                let j = Justification::new(justification)?;
                if reference.is_empty() || reference.len() > MAX_ALIAS_REFERENCE {
                    return Err(Refusal::MalformedCommand);
                }
                let acct = l.state.account(&account).ok_or(Refusal::UnknownAccount)?;
                if acct.kind() == AccountKind::Issuance {
                    return Err(Refusal::UnknownAccount);
                }
                if acct.status() == AccountStatus::Closed {
                    return Err(Refusal::AccountClosed);
                }
                if l.state.chainspec().alias(&key).is_some() {
                    return Err(Refusal::NotAuthorised);
                }
                Ok(j)
            };
            let j = match run(self) {
                Ok(x) => x,
                Err(e) => return Err(self.refused(ctx, &p, command, subjects, e)),
            };
            self.commit_operator(
                ctx,
                &p,
                d,
                command,
                policy_used(&p),
                j,
                Vec::new(),
                subjects,
                vec![Write::ChainSpec(SpecWrite::Alias { key, account })],
                (),
            )
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// `alias(source, reference)` (SPEC §8.2, `REQ-RC-7`): the account the alias names, for a
    /// caller that reaches it (`INV-16`). An alias that does not exist, and one whose account the
    /// caller cannot reach, are both `ERR-UnknownAccount`. A read; nothing is recorded.
    pub fn alias(
        &self,
        caller: &Caller,
        source: &OpeningSource,
        reference: &[u8],
        now: Time,
    ) -> Result<AccountId, Refusal> {
        let p = resolve(&self.state, caller)?;
        let account = self
            .state
            .chainspec()
            .alias(&alias_key(source, reference))
            .ok_or(Refusal::UnknownAccount)?;
        eval::reach_account(&self.state, &p, &account, now).map_err(|_| Refusal::UnknownAccount)?;
        Ok(account)
    }

    /// `grants(account)` (SPEC §8.2, `REQ-RC-7`): every grant whose subject is `account`, live and
    /// revoked, in identifier order, for the operator's policy authority or the account's owner.
    /// Anyone else: `ERR-NotAuthorised` if it reaches the account by a grant, `ERR-UnknownAccount`
    /// if it does not reach it at all (`INV-16`). A read; nothing is recorded.
    pub fn grants_on(
        &self,
        caller: &Caller,
        account: &AccountId,
        now: Time,
    ) -> Result<Vec<Grant>, Refusal> {
        let p = resolve(&self.state, caller)?;
        if !self.is_operator(&p) {
            match eval::reach_account(&self.state, &p, account, now)? {
                AuthorityKind::Owner => {}
                _ => return Err(Refusal::NotAuthorised),
            }
        }
        if self.state.account(account).is_none() {
            return Err(Refusal::UnknownAccount);
        }
        Ok(self.state.grants_over(Some(*account)).cloned().collect())
    }

    /// `grants(identity)` (SPEC §8.2, amendment 0026): the grants whose subject is `identity`,
    /// live and revoked, in identifier order. The identity itself, with an unscoped credential
    /// (`a0026-q1` (a), as an account's owner reads `grants(account)`), the operator's policy
    /// authority and the identities principal (`REQ-ID-6`) get every one; any other principal,
    /// with an unscoped credential, gets the ones it holds, and `ERR-NotAuthorised` if it holds
    /// none. A read; nothing is recorded.
    ///
    /// The holder's read is an index point read (the grants it holds); the others walk every
    /// grant, as no index is kept by identity subject.
    pub fn grants_over_identity(
        &self,
        caller: &Caller,
        identity: &IdentityId,
        now: Time,
    ) -> Result<Vec<Grant>, Refusal> {
        let p = resolve(&self.state, caller)?;
        let over = |g: &&Grant| g.subject() == Subject::Identity(*identity);
        let itself = p.identity == *identity && p.unscoped();
        if itself || self.identities_principal(&p, now).is_ok() {
            return Ok(self.state.all_grants().filter(over).cloned().collect());
        }
        if !p.unscoped() {
            return Err(Refusal::NotAuthorised);
        }
        let held: Vec<Grant> = self
            .state
            .grants_of(&p.identity)
            .filter(over)
            .cloned()
            .collect();
        if held.is_empty() {
            return Err(Refusal::NotAuthorised);
        }
        Ok(held)
    }
}
