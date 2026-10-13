// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The suite through the core Agent (`F-005`, `T-005-09`, `NFR-14`).
//!
//! `F-015` §3 names three targets; this is `ThroughAgent`: the library inside the core Agent. Its
//! service is a **replica** (the library over `F-006`'s stores, recording its calls, as
//! `core_binding::host` keeps it) whose every change is sent to the Agent and checked against the
//! Agent's own transition: the summary of each call, the kernel and service roots, the audit
//! chain's head, the next posting number and the delta-ring entry. Reads are the replica's, as
//! the host answers them (`DEC-4`). A case passes here only if every change it made committed in
//! the Agent exactly as it did natively; any difference panics, and the case fails with it.
//!
//! Two ports:
//!
//! - `through-agent/native`: the Agent's transition natively (`core_binding::host::NativePort`),
//!   the same code the VOS actor runs, without the runtime. Fast; any machine.
//! - `through-agent/vos`: the Agent in VOS, through `aledgent-core-host agent-server`
//!   (`CONFORMANCE_AGENT_HOST=tcp:host:port`): one fresh Agent per case, driven through the
//!   runtime ABI. About two seconds per change on the reference host (`notes/MEASUREMENTS-F005.md`).
//!
//! The kernel, binding and raw store ports are not offered: they are the kernel's and the store's
//! own cases, with no Agent in them. A case that needs them is not runnable here.
//!
//! When the suite checks the replica: before and after every method that changes the service, so
//! that calls made through the settlement, operator and custody sub-ports are sent before the next
//! change; and when the harness is dropped, for the last ones. With `CONFORMANCE_AGENT_BATCH=N`
//! (`T-031-03`) the changes are staged and sent N to an invocation, and checked when sent.

use std::collections::BTreeSet;

use cipher_clerk::{Account, AccountId};
use core_binding::envelope::Committed;
use core_binding::host::{AgentPort, DriveError, Driver, NativePort, Replica};
use core_binding::hot::{Limits, MAX_BUDGET};
use ledger_core::Refusal;
use ledger_core::account::Time;
use ledger_core::asset::{Asset, AssetId, Minor};
use ledger_core::authority::{AuthorityKind, OperationKind};
use ledger_core::custody::RecoveryMechanisms;
use ledger_core::eval::{Decision, Request};
use ledger_core::id::{AccountId as LedgerAccountId, GrantId, IdentityId, OperationId};
use ledger_core::identity::{IdentityKind, IdentityStatus};
use ledger_core::ledger::lifecycle::{FireInput, Fired, Resumed, ReviewEntry, Ticked};
use ledger_core::ledger::{
    Accepted, Ctx, Founded, GrantRequest, IdentityOpened, OpenAccount, Transfer,
};
use ledger_core::lifecycle::store::Operation;
use ledger_core::lifecycle::{KindClass, LifecycleConfig, Shown, Trigger};
use ledger_core::principal::{Caller, Verifier};
use ledger_core::query::{
    Balances, CommitmentTrial, Incident, MovementsRequest, OperationView, Page, Statement,
    StatementRequest, Subject as QuerySubject, TrialBalance,
};
use ledger_core::reservation::BalanceView;
use ledger_core::settlement::SettlementPort;
use ledger_core::state::State;
use ledger_kernel::store::LogRecord;

use crate::harness::{
    BindingPort, ClockPort, Harness, IN_PROCESS_ROOT, KernelPort, ManualClock, SERVICE_GENESIS_OP,
    ServicePort, StorePort, service_genesis,
};

/// The two Agent targets' names.
pub const NATIVE: &str = "through-agent/native";
/// See the module documentation.
pub const VOS: &str = "through-agent/vos";
/// The Agent's transition natively, with the book in its committed maps (`F-031` lever 2): the
/// kernel and the service leaves read from the Agent's own rows, no witness.
pub const MAPPED: &str = "through-agent/mapped";

/// The service through the Agent (module documentation).
pub struct AgentService {
    replica: Replica,
    driver: Driver<Box<dyn AgentPort + Send>>,
    /// Every input the Agent committed, in order.
    pub committed: Vec<Committed>,
    /// Inputs per invocation (`T-031-03`): 1 sends each change as it is made; more stages them
    /// and sends every `batch` in one invocation, the rest when the harness is dropped.
    batch: usize,
}

/// The inputs per invocation of the Agent targets: `CONFORMANCE_AGENT_BATCH`, default 1
/// (`T-031-03`). With more, every change is still checked against the Agent's transition, a
/// batch at a time.
pub fn agent_batch() -> usize {
    std::env::var("CONFORMANCE_AGENT_BATCH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1)
        .max(1)
}

impl AgentService {
    /// Found the suite's service on the Agent behind `port`; `mapped` for a layout-3 Agent.
    pub fn found(port: Box<dyn AgentPort + Send>, now: Time, limits: Limits, mapped: bool) -> Self {
        let found = if mapped {
            Driver::found_mapped
        } else {
            Driver::found
        };
        let (mut driver, replica, _) = found(
            port,
            core_binding::host::HostKeys::local(IN_PROCESS_ROOT, None),
            service_genesis(),
            SERVICE_GENESIS_OP,
            now,
            limits,
        )
        .unwrap_or_else(|e| panic!("through the Agent: genesis: {e:?}"));
        let batch = agent_batch();
        driver.set_batch_limits(core_binding::host::BatchLimits::with_inputs(batch));
        AgentService {
            replica,
            driver,
            committed: Vec::new(),
            batch,
        }
    }

    /// Send what the replica recorded (or stage it, in batches); panic, and so fail the case, on
    /// any difference.
    fn sync(&mut self) {
        let sent = if self.batch == 1 {
            self.driver
                .sync(&mut self.replica, Vec::new())
                .map(|c| c.into_iter().collect())
        } else {
            match self.driver.stage(&mut self.replica, Vec::new()) {
                Ok(_) if self.driver.staged() >= self.batch => self.driver.flush(),
                Ok(_) => Ok(Vec::new()),
                Err(e) => Err(e),
            }
        };
        self.committed_or_panic(sent);
    }

    fn committed_or_panic(&mut self, sent: Result<Vec<Committed>, DriveError>) {
        match sent {
            Ok(c) => {
                if !c.is_empty() {
                    self.driver.archived();
                }
                self.committed.extend(c);
            }
            Err(DriveError::Unsupported(m)) => {
                panic!("through the Agent: `{m}` cannot run inside the Agent")
            }
            Err(e) => panic!("through the Agent: {e:?}"),
        }
    }
}

impl Drop for AgentService {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            self.sync();
            let sent = self.driver.flush();
            self.committed_or_panic(sent);
        }
    }
}

impl ServicePort for AgentService {
    fn founded(&self) -> Founded {
        ServicePort::founded(&self.replica)
    }

    fn open_identity(
        &mut self,
        ctx: &Ctx,
        kind: IdentityKind,
        acts_for: Option<IdentityId>,
        verifier: Verifier,
    ) -> Result<Accepted<IdentityOpened>, Refusal> {
        self.sync();
        let r = ServicePort::open_identity(&mut self.replica, ctx, kind, acts_for, verifier);
        self.sync();
        r
    }

    fn set_identity_status(
        &mut self,
        ctx: &Ctx,
        identity: IdentityId,
        status: IdentityStatus,
    ) -> Result<Accepted<()>, Refusal> {
        self.sync();
        let r = ServicePort::set_identity_status(&mut self.replica, ctx, identity, status);
        self.sync();
        r
    }

    fn open_account(
        &mut self,
        ctx: &Ctx,
        request: OpenAccount,
    ) -> Result<Accepted<LedgerAccountId>, Refusal> {
        self.sync();
        let r = ServicePort::open_account(&mut self.replica, ctx, request);
        self.sync();
        r
    }

    fn close_account(
        &mut self,
        ctx: &Ctx,
        account: LedgerAccountId,
    ) -> Result<Accepted<()>, Refusal> {
        self.sync();
        let r = ServicePort::close_account(&mut self.replica, ctx, account);
        self.sync();
        r
    }

    fn grant_authority(
        &mut self,
        ctx: &Ctx,
        request: GrantRequest,
    ) -> Result<Accepted<GrantId>, Refusal> {
        self.sync();
        let r = ServicePort::grant_authority(&mut self.replica, ctx, request);
        self.sync();
        r
    }

    fn revoke_authority(&mut self, ctx: &Ctx, grant: GrantId) -> Result<Accepted<()>, Refusal> {
        self.sync();
        let r = ServicePort::revoke_authority(&mut self.replica, ctx, grant);
        self.sync();
        r
    }

    fn authorise(&self, ctx: &Ctx, request: &Request) -> Result<Decision, Refusal> {
        ServicePort::authorise(&self.replica, ctx, request)
    }

    fn issue(
        &mut self,
        ctx: &Ctx,
        asset: AssetId,
        destination: LedgerAccountId,
        amount: Minor,
    ) -> Result<Accepted<u64>, Refusal> {
        self.sync();
        let r = ServicePort::issue(&mut self.replica, ctx, asset, destination, amount);
        self.sync();
        r
    }

    fn redeem(
        &mut self,
        ctx: &Ctx,
        asset: AssetId,
        source: LedgerAccountId,
        amount: Minor,
    ) -> Result<Accepted<u64>, Refusal> {
        self.sync();
        let r = ServicePort::redeem(&mut self.replica, ctx, asset, source, amount);
        self.sync();
        r
    }

    fn transfer(&mut self, ctx: &Ctx, transfer: &Transfer) -> Result<Accepted<Vec<u64>>, Refusal> {
        self.sync();
        let r = ServicePort::transfer(&mut self.replica, ctx, transfer);
        self.sync();
        r
    }

    fn parse_amount(&self, asset: &str, decimal: &str) -> Result<Minor, Refusal> {
        ServicePort::parse_amount(&self.replica, asset, decimal)
    }

    fn reach_account(
        &self,
        caller: &Caller,
        account: &LedgerAccountId,
        now: Time,
    ) -> Result<AuthorityKind, Refusal> {
        ServicePort::reach_account(&self.replica, caller, account, now)
    }

    fn reach_identity(
        &self,
        caller: &Caller,
        identity: &IdentityId,
        now: Time,
    ) -> Result<BTreeSet<LedgerAccountId>, Refusal> {
        ServicePort::reach_identity(&self.replica, caller, identity, now)
    }

    fn available(
        &self,
        caller: &Caller,
        account: &LedgerAccountId,
        asset: &AssetId,
        now: Time,
    ) -> Result<i128, Refusal> {
        ServicePort::available(&self.replica, caller, account, asset, now)
    }

    fn trial_balance(&self, asset: &AssetId) -> Result<i128, Refusal> {
        ServicePort::trial_balance(&self.replica, asset)
    }

    fn reserve(
        &mut self,
        ctx: &Ctx,
        payer: LedgerAccountId,
        asset: AssetId,
        amount: Minor,
    ) -> Result<Accepted<u64>, Refusal> {
        self.sync();
        let r = ServicePort::reserve(&mut self.replica, ctx, payer, asset, amount);
        self.sync();
        r
    }

    fn capture(
        &mut self,
        ctx: &Ctx,
        destination: LedgerAccountId,
        cleared: Minor,
    ) -> Result<Accepted<u64>, Refusal> {
        self.sync();
        let r = ServicePort::capture(&mut self.replica, ctx, destination, cleared);
        self.sync();
        r
    }

    fn release(&mut self, ctx: &Ctx) -> Result<Accepted<u64>, Refusal> {
        self.sync();
        let r = ServicePort::release(&mut self.replica, ctx);
        self.sync();
        r
    }

    fn reserve_until(
        &mut self,
        ctx: &Ctx,
        payer: LedgerAccountId,
        asset: AssetId,
        amount: Minor,
        deadline: Option<Time>,
    ) -> Result<Accepted<u64>, Refusal> {
        self.sync();
        let r = ServicePort::reserve_until(&mut self.replica, ctx, payer, asset, amount, deadline);
        self.sync();
        r
    }

    fn operation(&self, op: &OperationId) -> Option<Operation> {
        ServicePort::operation(&self.replica, op)
    }

    fn tick(&mut self, now: Time) -> Result<Ticked, Refusal> {
        self.sync();
        let r = ServicePort::tick(&mut self.replica, now);
        self.sync();
        r
    }

    fn review_queue(&self, now: Time) -> Vec<ReviewEntry> {
        ServicePort::review_queue(&self.replica, now)
    }

    fn oldest_open_age(&self, now: Time) -> Option<Time> {
        ServicePort::oldest_open_age(&self.replica, now)
    }

    fn resume(
        &mut self,
        ctx: &Ctx,
        op: OperationId,
        justification: &[u8],
    ) -> Result<Resumed, Refusal> {
        self.sync();
        let r = ServicePort::resume(&mut self.replica, ctx, op, justification);
        self.sync();
        r
    }

    fn lifecycle_config(&self) -> LifecycleConfig {
        ServicePort::lifecycle_config(&self.replica)
    }

    fn receive(
        &mut self,
        ctx: &Ctx,
        kind: OperationKind,
        class: KindClass,
    ) -> Result<Shown, Refusal> {
        self.sync();
        let r = ServicePort::receive(&mut self.replica, ctx, kind, class);
        self.sync();
        r
    }

    fn arrive(
        &mut self,
        op: OperationId,
        kind: OperationKind,
        deadline: Option<Time>,
        now: Time,
    ) -> Result<Shown, Refusal> {
        self.sync();
        let r = ServicePort::arrive(&mut self.replica, op, kind, deadline, now);
        self.sync();
        r
    }

    fn fire(&mut self, ctx: &Ctx, trigger: Trigger, input: FireInput) -> Result<Fired, Refusal> {
        self.sync();
        let r = ServicePort::fire(&mut self.replica, ctx, trigger, input);
        self.sync();
        r
    }

    fn balances(
        &self,
        caller: &Caller,
        account: &LedgerAccountId,
        asset: &AssetId,
        now: Time,
    ) -> Result<BalanceView, Refusal> {
        ServicePort::balances(&self.replica, caller, account, asset, now)
    }

    fn balance_view(
        &self,
        account: &LedgerAccountId,
        asset: &AssetId,
    ) -> Result<BalanceView, Refusal> {
        ServicePort::balance_view(&self.replica, account, asset)
    }

    fn check_holdings(&self) -> Result<(), String> {
        ServicePort::check_holdings(&self.replica)
    }

    fn state(&self) -> &State {
        ServicePort::state(&self.replica)
    }

    fn kernel_account(&self, id: &AccountId) -> Option<Account> {
        ServicePort::kernel_account(&self.replica, id)
    }

    fn book_root(&self) -> [u8; 32] {
        ServicePort::book_root(&self.replica)
    }

    fn book_log(&self) -> Vec<LogRecord> {
        ServicePort::book_log(&self.replica)
    }

    fn settlement_port(&mut self) -> &mut dyn SettlementPort {
        self.sync();
        ServicePort::settlement_port(&mut self.replica)
    }

    fn operator(&mut self) -> &mut dyn crate::ops::OperatorPort {
        self.sync();
        ServicePort::operator(&mut self.replica)
    }

    fn query_balances(
        &self,
        caller: &Caller,
        subject: QuerySubject,
        asset: Option<AssetId>,
        now: Time,
    ) -> Result<Balances, Refusal> {
        ServicePort::query_balances(&self.replica, caller, subject, asset, now)
    }

    fn query_movements(
        &self,
        caller: &Caller,
        request: &MovementsRequest,
        now: Time,
    ) -> Result<Page, Refusal> {
        ServicePort::query_movements(&self.replica, caller, request, now)
    }

    fn query_statement(
        &self,
        caller: &Caller,
        request: &StatementRequest,
        now: Time,
    ) -> Result<Statement, Refusal> {
        ServicePort::query_statement(&self.replica, caller, request, now)
    }

    fn query_operation(
        &self,
        caller: &Caller,
        op: &OperationId,
        now: Time,
    ) -> Result<OperationView, Refusal> {
        ServicePort::query_operation(&self.replica, caller, op, now)
    }

    fn query_assets(&self, caller: &Caller, now: Time) -> Result<Vec<Asset>, Refusal> {
        ServicePort::query_assets(&self.replica, caller, now)
    }

    fn query_trial_balance(
        &self,
        caller: &Caller,
        asset: Option<AssetId>,
        now: Time,
    ) -> Result<TrialBalance, Refusal> {
        ServicePort::query_trial_balance(&self.replica, caller, asset, now)
    }

    fn can_spend(
        &self,
        caller: &Caller,
        account: &LedgerAccountId,
        asset: &AssetId,
        amount: Minor,
        now: Time,
    ) -> Result<(), Refusal> {
        ServicePort::can_spend(&self.replica, caller, account, asset, amount, now)
    }

    fn view_sequence(&self) -> u64 {
        ServicePort::view_sequence(&self.replica)
    }

    fn reconcile(&self) -> Vec<Incident> {
        ServicePort::reconcile(&self.replica)
    }

    fn store_trial_balance(&self) -> CommitmentTrial {
        ServicePort::store_trial_balance(&self.replica)
    }

    fn recovery_terms(
        &self,
        caller: &Caller,
        owner: &IdentityId,
    ) -> Result<RecoveryMechanisms, Refusal> {
        ServicePort::recovery_terms(&self.replica, caller, owner)
    }

    fn custody(&mut self) -> &mut dyn crate::custody::CustodyPort {
        self.sync();
        ServicePort::custody(&mut self.replica)
    }

    fn observable(&self) -> &dyn ledger_core::export::compare::Observable {
        ServicePort::observable(&self.replica)
    }

    fn export_and_restore(
        &mut self,
        export: &Ctx,
        import: &Ctx,
        sink: &mut dyn ledger_core::audit::AuditSink,
    ) -> Result<Box<dyn ServicePort>, ledger_core::export::ExportError> {
        self.sync();
        let r = ServicePort::export_and_restore(&mut self.replica, export, import, sink);
        self.sync();
        r
    }

    fn rederive(
        &self,
        acks: &[ledger_core::export::rederive::Acknowledgement],
    ) -> Result<(), Vec<ledger_core::export::rederive::Finding>> {
        ServicePort::rederive(&self.replica, acks)
    }
}

/// Where the Agent runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Where {
    /// The transition natively.
    Native,
    /// The transition natively, over the Agent's committed maps (layout 3, `F-031` lever 2).
    Mapped,
    /// VOS, through `aledgent-core-host agent-server`.
    Vos,
}

/// The `ThroughAgent` harness (module documentation).
pub struct ThroughAgent {
    at: Where,
    service: Option<AgentService>,
    clock: ManualClock,
}

impl ThroughAgent {
    /// A fresh harness: the Agent is founded when a case first asks for the service.
    pub fn new(at: Where) -> Self {
        ThroughAgent {
            at,
            service: None,
            clock: ManualClock(0),
        }
    }

    fn port(&self) -> (Box<dyn AgentPort + Send>, Limits) {
        match self.at {
            // Natively the guest's heap does not bind: the budget is VOS's own ceiling.
            Where::Native | Where::Mapped => (
                Box::new(NativePort::new()),
                Limits {
                    budget: MAX_BUDGET,
                    ..Limits::DEFAULT
                },
            ),
            Where::Vos => {
                let endpoint = std::env::var("CONFORMANCE_AGENT_HOST").unwrap_or_else(|_| {
                    panic!("through-agent/vos needs CONFORMANCE_AGENT_HOST=tcp:host:port")
                });
                let port = core_binding::host::RemotePort::connect(&endpoint)
                    .unwrap_or_else(|e| panic!("through-agent/vos: {endpoint}: {e}"));
                (
                    Box::new(port),
                    Limits {
                        budget: MAX_BUDGET,
                        ..Limits::DEFAULT
                    },
                )
            }
        }
    }
}

impl Harness for ThroughAgent {
    fn target(&self) -> &str {
        match self.at {
            Where::Native => NATIVE,
            Where::Mapped => MAPPED,
            Where::Vos => VOS,
        }
    }

    fn kernel(&mut self) -> Option<&mut dyn KernelPort> {
        None
    }

    fn binding(&mut self) -> Option<&mut dyn BindingPort> {
        None
    }

    fn bootstrap_again(&mut self) -> Option<Result<(), ledger_kernel::book::BindingFault>> {
        None
    }

    fn store(&mut self) -> Option<&mut dyn StorePort> {
        None
    }

    fn reopen_store(&mut self) -> bool {
        false
    }

    fn service(&mut self) -> Option<&mut dyn ServicePort> {
        if self.service.is_none() {
            let (port, limits) = self.port();
            let mapped = matches!(self.at, Where::Mapped);
            self.service = Some(AgentService::found(port, self.clock.now(), limits, mapped));
        }
        self.service.as_mut().map(|s| s as &mut dyn ServicePort)
    }

    fn clock(&mut self) -> &mut dyn ClockPort {
        &mut self.clock
    }

    fn exercised(&self) -> bool {
        self.service.is_some()
    }

    // Answered without founding the Agent, so that [`Harness::exercised`] says whether the case
    // itself asked for the service.
    fn offers(&mut self, port: crate::case::Port) -> bool {
        matches!(port, crate::case::Port::Service | crate::case::Port::Build)
    }
}
