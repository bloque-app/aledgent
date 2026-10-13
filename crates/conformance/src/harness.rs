// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Harnesses and ports (`F-015` §3, T-015-03).
//!
//! The same cases run on three targets:
//!
//! - [`InProcess`], here: `F-001`'s binding over `MemStore` or `FileStore`,
//!   and `ledger-core`'s [`Ledger`] over a book of the same kind (`F-003`);
//! - `ThroughAgent`, `F-005`'s: the library inside the core's Agent;
//! - `Deployment`, `F-009`'s: the service port only.
//!
//! A target that does not offer a case's port makes the case
//! `NotRunnableHere`, and it cannot back an "enforced" claim there.
//!
//! The kernel and store ports come from `ledger-kernel`'s
//! `conformance-ports` feature, which only this crate may enable
//! (`REQ-CS-5`, `tools/ci/check-conformance-ports.sh`).

use std::collections::BTreeSet;
use std::path::PathBuf;

use cipher_clerk::{Account, AccountId};
use ledger_core::Ledger;
use ledger_core::Refusal;
use ledger_core::account::{Custody, Time};
use ledger_core::asset::{Asset, AssetId, AssetKind, Minor};
use ledger_core::authority::AuthorityKind;
use ledger_core::authority::OperationKind;
use ledger_core::custody::{MechanismsDigest, RecoveryConfig, RecoveryMechanisms};
use ledger_core::eval::{Decision, Request};
use ledger_core::id::{
    AccountId as LedgerAccountId, CredentialId, GrantId, IdentityId, OperationId,
};
use ledger_core::identity::{IdentityKind, IdentityStatus};
use ledger_core::ledger::lifecycle::{FireInput, Fired, Resumed, ReviewEntry, Ticked};
use ledger_core::ledger::{
    Accepted, AssetDecl, Ctx, Founded, Genesis, GrantRequest, IdentityOpened, OpenAccount, Transfer,
};
use ledger_core::lifecycle::store::Operation;
use ledger_core::lifecycle::{KindClass, LifecycleConfig, Shown, Trigger};
use ledger_core::principal::{Caller, Verifier};
use ledger_core::query::trial::{commitment_trial, log_accounts};
use ledger_core::query::{
    Balances, CommitmentTrial, Incident, MovementsRequest, OperationView, Page, Statement,
    StatementRequest, Subject as QuerySubject, TrialBalance,
};
use ledger_core::reservation::BalanceView;
use ledger_core::settlement::SettlementPort;
use ledger_core::state::State;
use ledger_kernel::book::{Applied, BindingFault, Book, Unit};
use ledger_kernel::keys::KeyRing;
pub use ledger_kernel::ports::{KernelPort, StorePort};
use ledger_kernel::store::{FileStore, LeafStore, LogRecord, MemStore};
use ledger_kernel::vault::{FileVault, MemVault, VaultBackend};

use crate::case::Port;

/// The kernel binding's own API (`F-001`): units of posting drafts, the
/// plaintext settled net, and the delta log. Type and architecture cases use
/// it; the service port will sit on top of it.
pub trait BindingPort {
    /// Apply a unit with the host's time (`REQ-RT-7`).
    fn apply(&mut self, unit: &Unit, seed_ts: u64) -> Result<Applied, BindingFault>;
    /// The plaintext net of a kernel account on the settled layer.
    fn settled_net(&self, id: &AccountId) -> Result<Option<i128>, BindingFault>;
    /// The stored kernel account.
    fn kernel_account(&self, id: &AccountId) -> Result<Option<Account>, BindingFault>;
    /// The composite root.
    fn binding_root(&self) -> Result<[u8; 32], BindingFault>;
    /// Every record of the delta log, in commit order.
    fn delta_log(&self) -> Result<Vec<LogRecord>, BindingFault>;
}

impl<S: LeafStore, V: VaultBackend> BindingPort for Book<S, V> {
    fn apply(&mut self, unit: &Unit, seed_ts: u64) -> Result<Applied, BindingFault> {
        Book::apply(self, unit, seed_ts)
    }

    fn settled_net(&self, id: &AccountId) -> Result<Option<i128>, BindingFault> {
        Book::settled_net(self, id)
    }

    fn kernel_account(&self, id: &AccountId) -> Result<Option<Account>, BindingFault> {
        Book::kernel_account(self, id)
    }

    fn binding_root(&self) -> Result<[u8; 32], BindingFault> {
        Book::root(self)
    }

    fn delta_log(&self) -> Result<Vec<LogRecord>, BindingFault> {
        delta_log(self.store()).map_err(BindingFault::Store)
    }
}

/// Every record of a store's delta log, in commit order.
pub fn delta_log<S: LeafStore + ?Sized>(
    store: &S,
) -> Result<Vec<LogRecord>, ledger_kernel::store::StoreError> {
    let Some(last) = store.last_committed()? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(usize::try_from(last).unwrap_or(0) + 1);
    for seq in 0..=last {
        let record = store.log_record(seq)?.ok_or_else(|| {
            ledger_kernel::store::StoreError::Corrupt(format!("delta log has no record {seq}"))
        })?;
        out.push(record);
    }
    Ok(out)
}

/// `ledger-core`'s commands and queries, as a principal (SPEC §8).
///
/// `F-003` lands the identity, account, authority and issuance commands, the
/// evaluator, and read reach; later features add theirs. Two internal views,
/// [`ServicePort::state`] and [`ServicePort::kernel_account`], let a case check
/// what a command wrote. They are the suite's, not the command surface's.
pub trait ServicePort {
    /// What genesis created: the operator's identity and first credential,
    /// and the issuance counterparty. The registry is [`service_genesis`]'s.
    fn founded(&self) -> Founded;
    /// `open_identity`.
    fn open_identity(
        &mut self,
        ctx: &Ctx,
        kind: IdentityKind,
        acts_for: Option<IdentityId>,
        verifier: Verifier,
    ) -> Result<Accepted<IdentityOpened>, Refusal>;
    /// Restrict, reinstate or close an identity.
    fn set_identity_status(
        &mut self,
        ctx: &Ctx,
        identity: IdentityId,
        status: IdentityStatus,
    ) -> Result<Accepted<()>, Refusal>;
    /// `open_account`.
    fn open_account(
        &mut self,
        ctx: &Ctx,
        request: OpenAccount,
    ) -> Result<Accepted<LedgerAccountId>, Refusal>;
    /// `close_account`.
    fn close_account(
        &mut self,
        ctx: &Ctx,
        account: LedgerAccountId,
    ) -> Result<Accepted<()>, Refusal>;
    /// `grant_authority`.
    fn grant_authority(
        &mut self,
        ctx: &Ctx,
        request: GrantRequest,
    ) -> Result<Accepted<GrantId>, Refusal>;
    /// `revoke_authority`.
    fn revoke_authority(&mut self, ctx: &Ctx, grant: GrantId) -> Result<Accepted<()>, Refusal>;
    /// The evaluator, with no effect.
    fn authorise(&self, ctx: &Ctx, request: &Request) -> Result<Decision, Refusal>;
    /// `issue`.
    fn issue(
        &mut self,
        ctx: &Ctx,
        asset: AssetId,
        destination: LedgerAccountId,
        amount: Minor,
    ) -> Result<Accepted<u64>, Refusal>;
    /// `redeem`.
    fn redeem(
        &mut self,
        ctx: &Ctx,
        asset: AssetId,
        source: LedgerAccountId,
        amount: Minor,
    ) -> Result<Accepted<u64>, Refusal>;
    /// `transfer`, one leg or many (`F-008`): the legs' posting sequence numbers.
    fn transfer(&mut self, ctx: &Ctx, transfer: &Transfer) -> Result<Accepted<Vec<u64>>, Refusal>;
    /// The one decimal boundary, through the registry.
    fn parse_amount(&self, asset: &str, decimal: &str) -> Result<Minor, Refusal>;
    /// Read reach over an account.
    fn reach_account(
        &self,
        caller: &Caller,
        account: &LedgerAccountId,
        now: Time,
    ) -> Result<AuthorityKind, Refusal>;
    /// Read reach over an identity.
    fn reach_identity(
        &self,
        caller: &Caller,
        identity: &IdentityId,
        now: Time,
    ) -> Result<BTreeSet<LedgerAccountId>, Refusal>;
    /// An account's available balance, for a caller that reaches it.
    fn available(
        &self,
        caller: &Caller,
        account: &LedgerAccountId,
        asset: &AssetId,
        now: Time,
    ) -> Result<i128, Refusal>;
    /// The book's trial balance in one asset.
    fn trial_balance(&self, asset: &AssetId) -> Result<i128, Refusal>;
    /// `reserve` (`F-004`): `ctx.op` names the new reservation.
    fn reserve(
        &mut self,
        ctx: &Ctx,
        payer: LedgerAccountId,
        asset: AssetId,
        amount: Minor,
    ) -> Result<Accepted<u64>, Refusal>;
    /// `capture` (`F-004`): `ctx.op` is the reserved operation's reference.
    fn capture(
        &mut self,
        ctx: &Ctx,
        destination: LedgerAccountId,
        cleared: Minor,
    ) -> Result<Accepted<u64>, Refusal>;
    /// `release` (`F-004`): `ctx.op` is the reserved operation's reference. No amount.
    fn release(&mut self, ctx: &Ctx) -> Result<Accepted<u64>, Refusal>;
    /// `reserve` with its own deadline (`F-007`), or none to take the deployment's default.
    fn reserve_until(
        &mut self,
        ctx: &Ctx,
        payer: LedgerAccountId,
        asset: AssetId,
        amount: Minor,
        deadline: Option<Time>,
    ) -> Result<Accepted<u64>, Refusal>;
    /// An operation's record (`F-007`).
    fn operation(&self, op: &OperationId) -> Option<Operation>;
    /// Advance the lifecycle to `now` (`F-007`).
    fn tick(&mut self, now: Time) -> Result<Ticked, Refusal>;
    /// The review queue at `now` (`F-007`, `AC-C4.1`).
    fn review_queue(&self, now: Time) -> Vec<ReviewEntry>;
    /// The age of the oldest open operation at `now` (`F-007`, `AC-C4.2`).
    fn oldest_open_age(&self, now: Time) -> Option<Time>;
    /// The operator's resume path (`F-007`).
    fn resume(
        &mut self,
        ctx: &Ctx,
        op: OperationId,
        justification: &[u8],
    ) -> Result<Resumed, Refusal>;
    /// The deployment's lifecycle configuration.
    fn lifecycle_config(&self) -> LifecycleConfig;
    /// The lifecycle engine, raw: open a command's operation at `Received` (`ledger-core`'s
    /// `conformance-ports`; `F-009` and `F-010` build the commands that will call it).
    fn receive(
        &mut self,
        ctx: &Ctx,
        kind: OperationKind,
        class: KindClass,
    ) -> Result<Shown, Refusal>;
    /// The lifecycle engine, raw: an unoriginated inbound arrival at `Observed`.
    fn arrive(
        &mut self,
        op: OperationId,
        kind: OperationKind,
        deadline: Option<Time>,
        now: Time,
    ) -> Result<Shown, Refusal>;
    /// The lifecycle engine, raw: fire a trigger with evidence facts. Never reachable by a
    /// principal (`INV-11`); the suite plays the settlement adapter and the operator.
    fn fire(&mut self, ctx: &Ctx, trigger: Trigger, input: FireInput) -> Result<Fired, Refusal>;
    /// Fail the next commit with an internal fault (`REQ-ER-2`). `false` where the target cannot
    /// inject one: the `ERR-Internal` case is then not runnable there.
    fn fault_next_commit(&mut self) -> bool {
        false
    }
    /// How many commands answered `ERR-Internal` so far (`REQ-ER-2`): the incident count.
    fn internal_faults(&self) -> u64 {
        0
    }
    /// An account's balances in one asset, for a caller that reaches it (`F-004`).
    fn balances(
        &self,
        caller: &Caller,
        account: &LedgerAccountId,
        asset: &AssetId,
        now: Time,
    ) -> Result<BalanceView, Refusal>;
    /// The same view, with no authority: the suite's own look at the book.
    fn balance_view(
        &self,
        account: &LedgerAccountId,
        asset: &AssetId,
    ) -> Result<BalanceView, Refusal>;
    /// The open-reservation index against the kernel's holding positions (`INV-6`).
    fn check_holdings(&self) -> Result<(), String>;
    /// The service store, read-only.
    fn state(&self) -> &State;
    /// A kernel account of the service's book, as stored.
    fn kernel_account(&self, id: &AccountId) -> Option<Account>;
    /// The composite root of the service's book.
    fn book_root(&self) -> [u8; 32];
    /// Every record of the service's book's delta log, in commit order.
    fn book_log(&self) -> Vec<LogRecord>;
    /// The settlement boundary's commands (`F-010`).
    fn settlement_port(&mut self) -> &mut dyn SettlementPort;
    /// The operator surface over the same service (`F-013`).
    fn operator(&mut self) -> &mut dyn crate::ops::OperatorPort;
    /// `balances(subject, [asset])` on a fresh view (`F-011`).
    fn query_balances(
        &self,
        caller: &Caller,
        subject: QuerySubject,
        asset: Option<AssetId>,
        now: Time,
    ) -> Result<Balances, Refusal>;
    /// `movements(subject, filters, cursor)` on a fresh view (`F-011`).
    fn query_movements(
        &self,
        caller: &Caller,
        request: &MovementsRequest,
        now: Time,
    ) -> Result<Page, Refusal>;
    /// `statement(subject, period)` on a fresh view (`F-011`).
    fn query_statement(
        &self,
        caller: &Caller,
        request: &StatementRequest,
        now: Time,
    ) -> Result<Statement, Refusal>;
    /// `operation(reference)` on a fresh view (`F-011`).
    fn query_operation(
        &self,
        caller: &Caller,
        op: &OperationId,
        now: Time,
    ) -> Result<OperationView, Refusal>;
    /// `assets()` on a fresh view (`F-011`).
    fn query_assets(&self, caller: &Caller, now: Time) -> Result<Vec<Asset>, Refusal>;
    /// `trial_balance([asset])` on a fresh view (`F-011`).
    fn query_trial_balance(
        &self,
        caller: &Caller,
        asset: Option<AssetId>,
        now: Time,
    ) -> Result<TrialBalance, Refusal>;
    /// `can_spend(account, asset, amount)` on a fresh view (`F-011`).
    fn can_spend(
        &self,
        caller: &Caller,
        account: &LedgerAccountId,
        asset: &AssetId,
        amount: Minor,
        now: Time,
    ) -> Result<(), Refusal>;
    /// The committed posting sequence a fresh view is fixed at (`F-011`).
    fn view_sequence(&self) -> u64;
    /// Every stored balance against the entries that produced it (`INV-6`): the drifts found.
    fn reconcile(&self) -> Vec<Incident>;
    /// The trial balance on commitments, in the service book's leaf store alone, over every
    /// kernel account its delta log wrote (`INV-5` at the store).
    fn store_trial_balance(&self) -> CommitmentTrial;
    /// The recovery mechanisms a new owned-custody account of `owner` would have, as `caller` is
    /// shown them before `open_account` (`F-012`, `AC-A5.3`).
    fn recovery_terms(
        &self,
        caller: &Caller,
        owner: &IdentityId,
    ) -> Result<RecoveryMechanisms, Refusal>;
    /// Custody switching, credential sets and recovery over the same service (`F-012`).
    fn custody(&mut self) -> &mut dyn crate::custody::CustodyPort;
    /// The service as the observable-state comparison asks it (`F-014`, `AC-K5.2`).
    fn observable(&self) -> &dyn ledger_core::export::compare::Observable;
    /// Anchor a checkpoint, export the book as `export.caller` (`F-014`), and import it, as
    /// `import.caller`, into a new, empty in-memory deployment with the same keys, openings and
    /// configuration: the restored service.
    fn export_and_restore(
        &mut self,
        export: &Ctx,
        import: &Ctx,
        sink: &mut dyn ledger_core::audit::AuditSink,
    ) -> Result<Box<dyn ServicePort>, ledger_core::export::ExportError>;
    /// Re-derive every balance from the book's entries, and check every acknowledged operation
    /// is present (`F-014`, `AC-K5.1`).
    fn rederive(
        &self,
        acks: &[ledger_core::export::rederive::Acknowledgement],
    ) -> Result<(), Vec<ledger_core::export::rederive::Finding>>;
}

/// The acknowledgement an `open_account` request for `owner` carries in `custody` (`F-012`,
/// `AC-A5.3`): for owned custody, the digest of the recovery mechanisms `caller` is shown; for
/// delegated, none.
pub fn acknowledgement(
    s: &dyn ServicePort,
    caller: &Caller,
    owner: IdentityId,
    custody: Custody,
) -> Option<MechanismsDigest> {
    (custody == Custody::Owned).then(|| {
        s.recovery_terms(caller, &owner)
            .expect("the in-process deployment offers owned custody")
            .digest()
    })
}

impl<S: LeafStore, V: VaultBackend> ServicePort for Ledger<S, V> {
    fn founded(&self) -> Founded {
        let roles = self.state().roles().expect("genesis set the roles");
        Founded {
            operator: roles.operator,
            operator_credential: CredentialId::mint(&SERVICE_GENESIS_OP, 1),
            issuance: roles.issuance,
        }
    }

    fn open_identity(
        &mut self,
        ctx: &Ctx,
        kind: IdentityKind,
        acts_for: Option<IdentityId>,
        verifier: Verifier,
    ) -> Result<Accepted<IdentityOpened>, Refusal> {
        Ledger::open_identity(self, ctx, kind, acts_for, verifier)
    }

    fn set_identity_status(
        &mut self,
        ctx: &Ctx,
        identity: IdentityId,
        status: IdentityStatus,
    ) -> Result<Accepted<()>, Refusal> {
        Ledger::set_identity_status(self, ctx, identity, status)
    }

    fn open_account(
        &mut self,
        ctx: &Ctx,
        request: OpenAccount,
    ) -> Result<Accepted<LedgerAccountId>, Refusal> {
        Ledger::open_account(self, ctx, request)
    }

    fn close_account(
        &mut self,
        ctx: &Ctx,
        account: LedgerAccountId,
    ) -> Result<Accepted<()>, Refusal> {
        Ledger::close_account(self, ctx, account)
    }

    fn grant_authority(
        &mut self,
        ctx: &Ctx,
        request: GrantRequest,
    ) -> Result<Accepted<GrantId>, Refusal> {
        Ledger::grant_authority(self, ctx, request)
    }

    fn revoke_authority(&mut self, ctx: &Ctx, grant: GrantId) -> Result<Accepted<()>, Refusal> {
        Ledger::revoke_authority(self, ctx, grant)
    }

    fn authorise(&self, ctx: &Ctx, request: &Request) -> Result<Decision, Refusal> {
        Ledger::authorise(self, ctx, request)
    }

    fn issue(
        &mut self,
        ctx: &Ctx,
        asset: AssetId,
        destination: LedgerAccountId,
        amount: Minor,
    ) -> Result<Accepted<u64>, Refusal> {
        Ledger::issue(self, ctx, asset, destination, amount)
    }

    fn redeem(
        &mut self,
        ctx: &Ctx,
        asset: AssetId,
        source: LedgerAccountId,
        amount: Minor,
    ) -> Result<Accepted<u64>, Refusal> {
        Ledger::redeem(self, ctx, asset, source, amount)
    }

    fn transfer(&mut self, ctx: &Ctx, transfer: &Transfer) -> Result<Accepted<Vec<u64>>, Refusal> {
        Ledger::transfer(self, ctx, transfer)
    }

    fn parse_amount(&self, asset: &str, decimal: &str) -> Result<Minor, Refusal> {
        Ledger::parse_amount(self, asset, decimal)
    }

    fn reach_account(
        &self,
        caller: &Caller,
        account: &LedgerAccountId,
        now: Time,
    ) -> Result<AuthorityKind, Refusal> {
        Ledger::reach_account(self, caller, account, now)
    }

    fn reach_identity(
        &self,
        caller: &Caller,
        identity: &IdentityId,
        now: Time,
    ) -> Result<BTreeSet<LedgerAccountId>, Refusal> {
        Ledger::reach_identity(self, caller, identity, now)
    }

    fn available(
        &self,
        caller: &Caller,
        account: &LedgerAccountId,
        asset: &AssetId,
        now: Time,
    ) -> Result<i128, Refusal> {
        Ledger::available(self, caller, account, asset, now)
    }

    fn trial_balance(&self, asset: &AssetId) -> Result<i128, Refusal> {
        Ledger::trial_balance(self, asset)
    }

    fn reserve(
        &mut self,
        ctx: &Ctx,
        payer: LedgerAccountId,
        asset: AssetId,
        amount: Minor,
    ) -> Result<Accepted<u64>, Refusal> {
        Ledger::reserve(self, ctx, payer, asset, amount)
    }

    fn capture(
        &mut self,
        ctx: &Ctx,
        destination: LedgerAccountId,
        cleared: Minor,
    ) -> Result<Accepted<u64>, Refusal> {
        Ledger::capture(self, ctx, destination, cleared)
    }

    fn release(&mut self, ctx: &Ctx) -> Result<Accepted<u64>, Refusal> {
        Ledger::release(self, ctx)
    }

    fn reserve_until(
        &mut self,
        ctx: &Ctx,
        payer: LedgerAccountId,
        asset: AssetId,
        amount: Minor,
        deadline: Option<Time>,
    ) -> Result<Accepted<u64>, Refusal> {
        Ledger::reserve_until(self, ctx, payer, asset, amount, deadline)
    }

    fn operation(&self, op: &OperationId) -> Option<Operation> {
        Ledger::operation(self, op).cloned()
    }

    fn tick(&mut self, now: Time) -> Result<Ticked, Refusal> {
        Ledger::tick(self, now)
    }

    fn review_queue(&self, now: Time) -> Vec<ReviewEntry> {
        Ledger::review_queue(self, now)
    }

    fn oldest_open_age(&self, now: Time) -> Option<Time> {
        Ledger::oldest_open_age(self, now)
    }

    fn resume(
        &mut self,
        ctx: &Ctx,
        op: OperationId,
        justification: &[u8],
    ) -> Result<Resumed, Refusal> {
        Ledger::resume(self, ctx, op, justification)
    }

    fn lifecycle_config(&self) -> LifecycleConfig {
        Ledger::config(self).clone()
    }

    fn receive(
        &mut self,
        ctx: &Ctx,
        kind: OperationKind,
        class: KindClass,
    ) -> Result<Shown, Refusal> {
        Ledger::port_receive(self, ctx, kind, class)
    }

    fn arrive(
        &mut self,
        op: OperationId,
        kind: OperationKind,
        deadline: Option<Time>,
        now: Time,
    ) -> Result<Shown, Refusal> {
        Ledger::port_arrive(self, op, kind, deadline, now)
    }

    fn fire(&mut self, ctx: &Ctx, trigger: Trigger, input: FireInput) -> Result<Fired, Refusal> {
        Ledger::port_fire(self, ctx, trigger, input)
    }

    fn fault_next_commit(&mut self) -> bool {
        Ledger::port_fault_next_commit(self);
        true
    }

    fn internal_faults(&self) -> u64 {
        Ledger::internal_faults(self).0
    }

    fn balances(
        &self,
        caller: &Caller,
        account: &LedgerAccountId,
        asset: &AssetId,
        now: Time,
    ) -> Result<BalanceView, Refusal> {
        Ledger::balances(self, caller, account, asset, now)
    }

    fn balance_view(
        &self,
        account: &LedgerAccountId,
        asset: &AssetId,
    ) -> Result<BalanceView, Refusal> {
        Ledger::balance_view(self, account, asset)
    }

    fn check_holdings(&self) -> Result<(), String> {
        Ledger::check_holdings(self).map_err(|m| format!("{m:?}"))
    }

    fn state(&self) -> &State {
        Ledger::state(self)
    }

    fn kernel_account(&self, id: &AccountId) -> Option<Account> {
        self.book().kernel_account(id).expect("the store reads")
    }

    fn book_root(&self) -> [u8; 32] {
        self.book().root().expect("the store reads")
    }

    fn book_log(&self) -> Vec<LogRecord> {
        delta_log(self.book().store()).expect("the store reads")
    }

    fn settlement_port(&mut self) -> &mut dyn SettlementPort {
        self
    }

    fn operator(&mut self) -> &mut dyn crate::ops::OperatorPort {
        self
    }

    fn query_balances(
        &self,
        caller: &Caller,
        subject: QuerySubject,
        asset: Option<AssetId>,
        now: Time,
    ) -> Result<Balances, Refusal> {
        self.view(now)?.balances(caller, subject, asset)
    }

    fn query_movements(
        &self,
        caller: &Caller,
        request: &MovementsRequest,
        now: Time,
    ) -> Result<Page, Refusal> {
        self.view(now)?.movements(caller, request)
    }

    fn query_statement(
        &self,
        caller: &Caller,
        request: &StatementRequest,
        now: Time,
    ) -> Result<Statement, Refusal> {
        self.view(now)?.statement(caller, request)
    }

    fn query_operation(
        &self,
        caller: &Caller,
        op: &OperationId,
        now: Time,
    ) -> Result<OperationView, Refusal> {
        self.view(now)?.operation(caller, op)
    }

    fn query_assets(&self, caller: &Caller, now: Time) -> Result<Vec<Asset>, Refusal> {
        self.view(now)?.assets(caller)
    }

    fn query_trial_balance(
        &self,
        caller: &Caller,
        asset: Option<AssetId>,
        now: Time,
    ) -> Result<TrialBalance, Refusal> {
        self.view(now)?.trial_balance(caller, asset)
    }

    fn can_spend(
        &self,
        caller: &Caller,
        account: &LedgerAccountId,
        asset: &AssetId,
        amount: Minor,
        now: Time,
    ) -> Result<(), Refusal> {
        self.view(now)?.can_spend(caller, account, asset, amount)
    }

    fn view_sequence(&self) -> u64 {
        self.view(0).expect("the store reads").sequence()
    }

    fn reconcile(&self) -> Vec<Incident> {
        Ledger::reconcile(self).expect("the store reads")
    }

    fn store_trial_balance(&self) -> CommitmentTrial {
        let store = self.book().store();
        let accounts = log_accounts(store).expect("the store reads");
        commitment_trial(store, &accounts).expect("the store reads")
    }

    fn recovery_terms(
        &self,
        caller: &Caller,
        owner: &IdentityId,
    ) -> Result<RecoveryMechanisms, Refusal> {
        Ledger::recovery_terms(self, caller, owner)
    }

    fn custody(&mut self) -> &mut dyn crate::custody::CustodyPort {
        self
    }

    fn observable(&self) -> &dyn ledger_core::export::compare::Observable {
        self
    }

    fn export_and_restore(
        &mut self,
        export: &Ctx,
        import: &Ctx,
        sink: &mut dyn ledger_core::audit::AuditSink,
    ) -> Result<Box<dyn ServicePort>, ledger_core::export::ExportError> {
        use ledger_core::export::Checkpoints;
        use ledger_core::ledger::operator::{ImportTarget, openings_of};
        let mut anchors = Checkpoints::new();
        anchors.anchor(self);
        let e = self.export_book(export, &anchors, b"restore drill", sink)?;
        let target = ImportTarget {
            store: MemStore::new(),
            vault: MemVault::new(),
            keys: KeyRing::from_root(IN_PROCESS_ROOT),
            openings: openings_of(self),
            config: self.config().clone(),
        };
        let (restored, _) =
            Ledger::import_book(&e, target, &anchors, import, b"restore drill", sink)
                .map_err(|f| f.error)?;
        Ok(Box::new(restored))
    }

    fn rederive(
        &self,
        acks: &[ledger_core::export::rederive::Acknowledgement],
    ) -> Result<(), Vec<ledger_core::export::rederive::Finding>> {
        Ledger::rederive(self, acks)
    }
}

/// The operation reference every in-process service's genesis uses.
pub const SERVICE_GENESIS_OP: OperationId =
    OperationId::from_ref(ledger_kernel::posting::OperationRef([0xE0; 16]));

/// The in-process deployment's identity, which operation references are derived under.
pub const SERVICE_DEPLOYMENT: [u8; 16] = [0xD0; 16];

/// The in-process deployment's decision bound for `Received` (`REQ-CS-7`), in time units.
pub const SERVICE_DECISION_BOUND: Time = 60;

/// The in-process deployment's review service level (`AC-C4.1`), in time units.
pub const SERVICE_REVIEW_LEVEL: Time = 3_600;

/// The in-process deployment's default deadline for a reservation, from acceptance.
pub const SERVICE_RESERVE_DEADLINE: Time = 1_000;

/// The lifecycle configuration of every in-process service: a default deadline for
/// reservations only, so every other kind of operation must carry its own.
pub fn service_lifecycle() -> LifecycleConfig {
    LifecycleConfig::new(
        SERVICE_DEPLOYMENT,
        SERVICE_DECISION_BOUND,
        SERVICE_REVIEW_LEVEL,
    )
    .with_default_deadline(OperationKind::Reserve, SERVICE_RESERVE_DEADLINE)
    .with_recovery(service_recovery())
}

/// The in-process deployment's recovery delay (`F-012`), in time units.
pub const SERVICE_RECOVERY_DELAY: Time = 500;

/// The in-process deployment's succession delay (`F-012`), in time units.
pub const SERVICE_SUCCESSION_DELAY: Time = 2_000;

/// The recovery authority of every in-process service: `services/recovery` over a test key.
/// A test key of the suite's, never a default of a production crate (`REQ-AU-8`).
pub fn service_recovery_authority() -> recovery::RecoveryService {
    let mut secret = [0x5A; 32];
    secret[31] = 0;
    recovery::RecoveryService::new(
        recovery::key::SigningKey::from_bytes(secret).expect("a canonical test key"),
    )
}

/// The recovery configuration of every in-process service (`F-012`): owned custody offered,
/// [`service_recovery_authority`]'s key, short delays, succession and the holder-held credential
/// offered.
pub fn service_recovery() -> RecoveryConfig {
    RecoveryConfig::owned_custody(service_recovery_authority().public_key())
        .with_recovery_delay(SERVICE_RECOVERY_DELAY)
        .with_succession_delay(Some(SERVICE_SUCCESSION_DELAY))
        .with_holder_held(true)
}

/// The registry every in-process service starts from: `USD` (precision 2),
/// `EUR` (2), `BTC` (8), and `OLD` (2), registered deprecated; and
/// [`service_lifecycle`].
pub fn service_genesis() -> Genesis {
    let asset = |id: &str, precision, kind, deprecated| AssetDecl {
        id: AssetId::parse(id).expect("a valid asset id"),
        // Bound from genesis: the suite's in-process deployments settle in every asset
        // (F-029 NOTES); book-only assets are F-029's own cases.
        location: Some(ledger_core::asset::AssetLocation::parse(&format!("Test{id}")).unwrap()),
        precision,
        kind,
        deprecated,
    };
    Genesis {
        assets: vec![
            asset("USD", 2, AssetKind::Fiat, false),
            asset("EUR", 2, AssetKind::Fiat, false),
            asset("BTC", 8, AssetKind::Native, false),
            asset("OLD", 2, AssetKind::Fiat, true),
        ],
        operator_verifier: Verifier::new(vec![0x0F]),
        lifecycle: service_lifecycle(),
        chainspec: None,
        spec: None,
    }
}

/// Time as an input (`REQ-RT-7`): cases never read a clock, they are given
/// one.
pub trait ClockPort {
    /// The current host time, in the unit the binding stamps with.
    fn now(&self) -> u64;
    /// Move time forward.
    fn advance(&mut self, by: u64);
}

/// A clock that moves only when told.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ManualClock(pub u64);

impl ClockPort for ManualClock {
    fn now(&self) -> u64 {
        self.0
    }

    fn advance(&mut self, by: u64) {
        self.0 = self.0.saturating_add(by);
    }
}

/// What a case runs against. A fresh harness is built for every case run, so
/// no case sees another's state.
pub trait Harness {
    /// The target's name, as the report records it.
    fn target(&self) -> &str;

    /// Raw kernel transfers through a bootstrapped book, `ledger-core`
    /// bypassed.
    fn kernel(&mut self) -> Option<&mut dyn KernelPort>;

    /// The kernel binding's API over the **same** book as [`Harness::kernel`].
    fn binding(&mut self) -> Option<&mut dyn BindingPort>;

    /// Take the book apart and try to bootstrap a second book over the same
    /// store and vault (`INV-25`). The book is consumed. `None` if the target
    /// cannot express it.
    fn bootstrap_again(&mut self) -> Option<Result<(), BindingFault>>;

    /// Raw writes to an **empty** leaf store of the target's kind, separate
    /// from the book's, so a case can write its own bootstrap.
    fn store(&mut self) -> Option<&mut dyn StorePort>;

    /// Close and reopen the raw store, keeping what it committed. `false` if
    /// the target cannot.
    fn reopen_store(&mut self) -> bool;

    /// `ledger-core`'s commands and queries.
    fn service(&mut self) -> Option<&mut dyn ServicePort>;

    /// Raw writes to an **empty** `ledger-core` service store, separate from
    /// the service's own, so a store case can write what it needs (`F-003`).
    /// `None` if the target has none.
    fn service_store(&mut self) -> Option<&mut State> {
        None
    }

    /// The clock the case hands to the code under test.
    fn clock(&mut self) -> &mut dyn ClockPort;

    /// Whether the case that just ran went through this harness at all. A case that builds its
    /// own deployment (`F-009`'s and later cases) passes whatever the target; on a target that
    /// exists to put the library somewhere else (the Agent, `F-005`) such a pass proves nothing
    /// about it, and the runner reports it as not runnable there.
    fn exercised(&self) -> bool {
        true
    }

    /// Whether the target offers `port`.
    fn offers(&mut self, port: Port) -> bool {
        match port {
            Port::Kernel => self.kernel().is_some(),
            Port::Binding => self.binding().is_some(),
            Port::Store => self.store().is_some(),
            Port::Service => self.service().is_some(),
            Port::ServiceStore => self.service_store().is_some(),
            Port::Build => true,
        }
    }
}

/// The in-process targets' store kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreKind {
    /// `MemStore` and `MemVault`.
    Mem,
    /// `FileStore` and `FileVault` in a temporary directory.
    File,
}

/// A book the in-process harness can hand out as either port.
trait InProcessBook {
    fn as_kernel(&mut self) -> &mut dyn KernelPort;
    fn as_binding(&mut self) -> &mut dyn BindingPort;
    fn bootstrap_again(self: Box<Self>) -> Result<(), BindingFault>;
}

impl<S: LeafStore + 'static, V: VaultBackend + 'static> InProcessBook for Book<S, V> {
    fn as_kernel(&mut self) -> &mut dyn KernelPort {
        self
    }

    fn as_binding(&mut self) -> &mut dyn BindingPort {
        self
    }

    fn bootstrap_again(self: Box<Self>) -> Result<(), BindingFault> {
        let (store, vault, keys) = self.into_parts();
        Book::bootstrap(store, vault, keys).map(|_| ())
    }
}

/// The raw store of an in-process harness.
enum RawStore {
    Mem(Box<MemStore>),
    File {
        store: Option<Box<FileStore>>,
        path: PathBuf,
    },
}

/// The core root every in-process book derives its keys from.
pub const IN_PROCESS_ROOT: [u8; 32] = [0xC5; 32];

/// `F-001`'s binding in this process, over `MemStore` or `FileStore`.
pub struct InProcess {
    kind: StoreKind,
    name: &'static str,
    dir: Option<tempfile::TempDir>,
    book: Option<Box<dyn InProcessBook>>,
    book_consumed: bool,
    store: Option<RawStore>,
    service: Option<Box<dyn ServicePort>>,
    service_store: Option<Box<State>>,
    clock: ManualClock,
}

impl InProcess {
    /// The two in-process targets, by report name.
    pub const TARGETS: [(&'static str, StoreKind); 2] = [
        ("in-process/mem-store", StoreKind::Mem),
        ("in-process/file-store", StoreKind::File),
    ];

    /// A fresh harness: nothing is created until a case asks for a port.
    pub fn new(kind: StoreKind) -> InProcess {
        let name = match kind {
            StoreKind::Mem => InProcess::TARGETS[0].0,
            StoreKind::File => InProcess::TARGETS[1].0,
        };
        InProcess {
            kind,
            name,
            dir: None,
            book: None,
            book_consumed: false,
            store: None,
            service: None,
            service_store: None,
            clock: ManualClock(0),
        }
    }

    fn dir(&mut self) -> PathBuf {
        self.dir
            .get_or_insert_with(|| tempfile::tempdir().expect("a temporary directory"))
            .path()
            .to_path_buf()
    }

    fn book(&mut self) -> Option<&mut Box<dyn InProcessBook>> {
        if self.book.is_none() && !self.book_consumed {
            let keys = KeyRing::from_root(IN_PROCESS_ROOT);
            let book: Box<dyn InProcessBook> = match self.kind {
                StoreKind::Mem => Box::new(
                    Book::bootstrap(MemStore::new(), MemVault::new(), keys)
                        .expect("an in-memory book bootstraps"),
                ),
                StoreKind::File => {
                    let dir = self.dir();
                    Box::new(
                        Book::bootstrap(
                            FileStore::open(dir.join("book.redb")).expect("the book file opens"),
                            FileVault::open(dir.join("vault.redb")).expect("the vault file opens"),
                            keys,
                        )
                        .expect("a file-backed book bootstraps"),
                    )
                }
            };
            self.book = Some(book);
        }
        self.book.as_mut()
    }
}

impl Harness for InProcess {
    fn target(&self) -> &str {
        self.name
    }

    fn kernel(&mut self) -> Option<&mut dyn KernelPort> {
        self.book().map(|b| b.as_kernel())
    }

    fn binding(&mut self) -> Option<&mut dyn BindingPort> {
        self.book().map(|b| b.as_binding())
    }

    fn bootstrap_again(&mut self) -> Option<Result<(), BindingFault>> {
        self.book()?;
        let book = self.book.take()?;
        self.book_consumed = true;
        Some(book.bootstrap_again())
    }

    fn store(&mut self) -> Option<&mut dyn StorePort> {
        if self.store.is_none() {
            self.store = Some(match self.kind {
                StoreKind::Mem => RawStore::Mem(Box::default()),
                StoreKind::File => {
                    let path = self.dir().join("raw.redb");
                    RawStore::File {
                        store: Some(Box::new(
                            FileStore::open(&path).expect("the raw store file opens"),
                        )),
                        path,
                    }
                }
            });
        }
        match self.store.as_mut()? {
            RawStore::Mem(s) => Some(&mut **s),
            RawStore::File { store, .. } => store.as_deref_mut().map(|s| s as &mut dyn StorePort),
        }
    }

    fn reopen_store(&mut self) -> bool {
        match self.store.as_mut() {
            None => false,
            // Nothing to reopen in memory: the state is the state.
            Some(RawStore::Mem(_)) => true,
            Some(RawStore::File { store, path }) => {
                *store = None;
                *store = Some(Box::new(
                    FileStore::open(&*path).expect("the raw store file reopens"),
                ));
                true
            }
        }
    }

    fn service(&mut self) -> Option<&mut dyn ServicePort> {
        if self.service.is_none() {
            // A book of its own, of the target's kind. The service owns it, so
            // it is not the book the kernel and binding ports hand out.
            let keys = KeyRing::from_root(IN_PROCESS_ROOT);
            let now = self.clock.now();
            let service: Box<dyn ServicePort> = match self.kind {
                StoreKind::Mem => {
                    let book = Book::bootstrap(MemStore::new(), MemVault::new(), keys)
                        .expect("an in-memory book bootstraps");
                    let (ledger, _) =
                        Ledger::genesis(book, service_genesis(), SERVICE_GENESIS_OP, now)
                            .expect("genesis");
                    Box::new(ledger)
                }
                StoreKind::File => {
                    let dir = self.dir();
                    let book = Book::bootstrap(
                        FileStore::open(dir.join("service-book.redb")).expect("the book opens"),
                        FileVault::open(dir.join("service-vault.redb")).expect("the vault opens"),
                        keys,
                    )
                    .expect("a file-backed book bootstraps");
                    let (ledger, _) =
                        Ledger::genesis(book, service_genesis(), SERVICE_GENESIS_OP, now)
                            .expect("genesis");
                    Box::new(ledger)
                }
            };
            self.service = Some(service);
        }
        match &mut self.service {
            Some(s) => Some(&mut **s),
            None => None,
        }
    }

    fn service_store(&mut self) -> Option<&mut State> {
        Some(self.service_store.get_or_insert_with(Box::default))
    }

    fn clock(&mut self) -> &mut dyn ClockPort {
        &mut self.clock
    }
}

/// A target the runner can build a fresh harness for.
#[derive(Clone, Copy, Debug)]
pub struct Target {
    /// The name the report records.
    pub name: &'static str,
    /// Build a fresh harness.
    pub make: fn() -> Box<dyn Harness>,
    /// Whether the target offers every port. A case a partial target cannot run (the Agent
    /// targets have no kernel or store port of their own, `F-005`) is judged on the complete
    /// targets, which a profile still requires it of; on the partial one it is reported as not
    /// runnable, never as passing.
    pub complete: bool,
}

/// Every target this build knows: the two in-process stores, and the core Agent natively and in
/// VOS (`F-005`, [`crate::agent`]). `F-009` adds `Deployment`.
pub fn all_targets() -> Vec<Target> {
    vec![
        Target {
            name: InProcess::TARGETS[0].0,
            make: || Box::new(InProcess::new(StoreKind::Mem)),
            complete: true,
        },
        Target {
            name: InProcess::TARGETS[1].0,
            make: || Box::new(InProcess::new(StoreKind::File)),
            complete: true,
        },
        Target {
            name: crate::agent::NATIVE,
            make: || Box::new(crate::agent::ThroughAgent::new(crate::agent::Where::Native)),
            complete: false,
        },
        Target {
            name: crate::agent::MAPPED,
            make: || Box::new(crate::agent::ThroughAgent::new(crate::agent::Where::Mapped)),
            complete: false,
        },
        Target {
            name: crate::agent::VOS,
            make: || Box::new(crate::agent::ThroughAgent::new(crate::agent::Where::Vos)),
            complete: false,
        },
    ]
}

/// The targets a run uses: `CONFORMANCE_TARGETS` (names, comma-separated), or the default:
/// both in-process stores and the core Agent's transition natively. `through-agent/vos` needs
/// an Agent host (`CONFORMANCE_AGENT_HOST`) and takes about two seconds per change, so it runs
/// only when named.
pub fn targets() -> Vec<Target> {
    let wanted: Vec<String> = match std::env::var("CONFORMANCE_TARGETS") {
        Ok(v) => v.split(',').map(|s| s.trim().to_owned()).collect(),
        Err(_) => vec![
            InProcess::TARGETS[0].0.to_owned(),
            InProcess::TARGETS[1].0.to_owned(),
            crate::agent::NATIVE.to_owned(),
            crate::agent::MAPPED.to_owned(),
        ],
    };
    all_targets()
        .into_iter()
        .filter(|t| wanted.iter().any(|w| w == t.name))
        .collect()
}
