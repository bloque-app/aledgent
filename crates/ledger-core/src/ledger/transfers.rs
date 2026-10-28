// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `transfer`: internal transfers, one leg or many (`F-008`, SPEC §8.1, Epic B, `US-B1`, `US-B3`).
//!
//! A holder moves value to any other ledger account in one posting per leg. A transfer has no
//! reservation and no external leg, so it is final when it is accepted (`REQ-OP-1`, §5.6
//! `Accepted → Settled`) and it never touches the settlement layer (`REQ-SL-5`): the only effects
//! of a transfer are its posting chain, one notification per transition and its attribution
//! record. Nothing on this path names a settlement instruction.
//!
//! ## The command
//!
//! [`Transfer`] carries its [`Leg`]s and an optional deadline. The rest of what `REQ-CS-1` lists
//! travels in the [`Ctx`]: the initiating principal and the credential it presented, whom it acts
//! for ([`crate::principal::Caller`]), and the operation reference that the client key derives
//! (`crate::lifecycle::keys::operation_reference`, `F-007`). The kind is the command itself.
//! Amounts are exact integer minor units; the one decimal conversion is ingress's
//! ([`Ledger::parse_amount`], `F-009`). A single transfer is a unit of one leg.
//!
//! ## The fixed validation order (`F-008` §2)
//!
//! Every check runs before the kernel is called, and the first that fails names the refusal, so
//! the same bad command always gets the same error:
//!
//! 1. more legs than the deployment declares
//!    ([`LifecycleConfig::max_legs`](crate::lifecycle::LifecycleConfig::max_legs)):
//!    `ERR-BatchTooLarge`, before anything is looked up (`AC-B3.3`);
//! 2. the credential: `ERR-CredentialInvalid` or `ERR-IdentityRestricted`; then a repeat of the
//!    same request returns its original outcome, and another request under the same reference is
//!    `ERR-ClientKeyReused` (`F-007`, `REQ-OP-10`);
//! 3. each leg in order: no legs, or an amount of zero, `ERR-ZeroAmount` (`AC-B1.4`); the asset
//!    `ERR-UnknownAsset`, then `ERR-AssetDeprecated`; then what the unit debits or credits one
//!    account in one asset must be expressible in minor units, else `ERR-PrecisionExceeded`;
//! 4. each account, in order of appearance (the debited one first in each leg): exists, and for a
//!    debited account is reached by the principal, else `ERR-UnknownAccount`; not frozen or
//!    closed, else `ERR-AccountFrozen` / `ERR-AccountClosed`; its owner active, else
//!    `ERR-IdentityRestricted`;
//! 5. authority over each debited account for kind `transfer`, through `F-003`'s evaluator:
//!    `ERR-NotAuthorised`, `ERR-AuthorityRevoked` or `ERR-BoundExceeded`. Legs that draw on one
//!    grant are added together for its per-operation and per-window bounds: the unit is one
//!    operation. The authority used is recorded per leg (`REQ-AU-7`);
//! 6. the deadline: the command's, else the deployment's default for transfers, else
//!    `ERR-DeadlineRequired` (`REQ-OP-5`);
//! 7. a Treasury account may not pass its declared limit at any leg (`REQ-LA-2`), and a leg into
//!    its own account may not move more than is available at that leg: `ERR-InsufficientAvailable`.
//!    Every other leg on a Customer or Suspense account is the kernel's to check.
//!
//! ## The posting chain (`F-008` §4–§5, `REQ-KER-10`)
//!
//! Each leg is one kernel transfer with two entries on the settled layer, debit `from` and credit
//! `to`, in the kernel sub-accounts of the leg's asset: the currency tag comes from the registry
//! and the account record, never from the caller. The N legs are N postings of kind `Transfer`
//! under one operation reference, ordinals `0..N`, in one chain: `F-001`'s binding marks every
//! transfer but the last `LINKED`, so the chain is always terminated, and the kernel applies all of
//! them or none. It applies them in order against the staged state, so legs that debit one account
//! in one asset are summed for the check (`AC-B3.2`). Each posting's `external_id` is derived from
//! (operation, `Transfer`, ordinal), so the kernel refuses a replayed leg for ever (`REQ-KER-6`).
//!
//! ## The lifecycle (`F-008` §3, SPEC §5.6)
//!
//! The operation enters at `Received`, is decided (`Received → Accepted`), and in the same commit
//! appends its chain and settles (`Accepted → Settled`). A command refused synchronously —
//! by validation or by the kernel — records nothing, so a corrected retry is decided afresh
//! (`F-007` decision 8). A transfer **durably received first** (opened at `Received` by ingress,
//! `F-009`) is decided the same way, and a refusal is then recorded: `Received → Rejected` for a
//! refusal before the kernel, `Received → Accepted → Rejected` for the kernel's refusal of the
//! chain, before any balance effect. The notification of `Rejected` names the refusal, and a
//! repeat returns it again.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec;
use alloc::vec::Vec;

use ledger_kernel::book::EventClass;
use ledger_kernel::posting::{EntryDraft, PostingDraft, PostingKind, PostingMeta};
use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use super::lifecycle::{Attribution, Plan, Refusals, Stop};
use super::{Accepted, Ctx, Ledger, requests};
use crate::account::{AccountKind, LedgerAccount, Time, check_treasury_limit};
use crate::asset::{Asset, AssetId, Minor, NonZeroMinor};
use crate::authority::{AuthorityUsed, Charge, OperationKind, WindowKey, effective};
use crate::eval::{self, Decision, Request};
use crate::id::{AccountId, GrantId, OperationId};
use crate::lifecycle::keys::{Evidence, RequestDigest, Surface};
use crate::lifecycle::store::{NewOperation, RequestEntry};
use crate::lifecycle::{Facts, KindClass, Origin, Shown, State, Trigger};
use crate::principal::{Principal, resolve};
use crate::refusal::Refusal;
use crate::reservation::UnitBuilder;
use crate::state::{Command, OperationRecord, Write};

/// One leg of a transfer: `amount` of `asset` from `from` to `to`.
///
/// There is no currency tag to set: the kernel's tag of each entry comes from the asset registry
/// and the account record (`F-008` §4, `INV-4`).
///
/// ```compile_fail
/// // A caller cannot name the kernel's currency tag.
/// use ledger_core::ledger::Leg;
/// let _ = Leg { from: todo!(), to: todo!(), asset: todo!(), amount: todo!(), tag: 7 };
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Leg {
    /// The debited ledger account.
    pub from: AccountId,
    /// The credited ledger account.
    pub to: AccountId,
    /// The asset moved.
    pub asset: AssetId,
    /// The amount, in exact minor units (`REQ-AS-4`).
    pub amount: Minor,
}

/// The `transfer` command (`US-B1`, `US-B3`): its legs, in the order the kernel applies them, and
/// its deadline. The initiator, whom it acts for and the client key travel in the [`Ctx`].
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Transfer {
    /// The legs. One leg is a single transfer; more are a multi-leg unit, all or nothing.
    pub legs: Vec<Leg>,
    /// The operation's deadline (`REQ-OP-5`); `None` takes the deployment's default for
    /// transfers.
    pub deadline: Option<Time>,
}

impl Transfer {
    /// A single transfer: a unit of one leg, with the deployment's default deadline.
    pub fn single(from: AccountId, to: AccountId, asset: AssetId, amount: Minor) -> Transfer {
        Transfer {
            legs: vec![Leg {
                from,
                to,
                asset,
                amount,
            }],
            deadline: None,
        }
    }

    /// A multi-leg unit, with the deployment's default deadline.
    pub fn unit(legs: Vec<Leg>) -> Transfer {
        Transfer {
            legs,
            deadline: None,
        }
    }

    /// The same, with its own deadline.
    pub fn until(mut self, deadline: Time) -> Transfer {
        self.deadline = Some(deadline);
        self
    }
}

/// The name of the kernel's refusal of a transfer's chain (`F-008` §6, `REQ-ER-1`). In a failed
/// chain the kernel rewrites every event but the failing one to `LinkedEventFailed`; the engine
/// reads that one event, so the failing leg's own status decides the error.
///
/// | Kernel status, as `F-001` labels it | Refusal |
/// |---|---|
/// | `BalanceMustNotBeNegative` ([`EventClass::Insufficient`]) | `ERR-InsufficientAvailable` |
/// | `DebitAccountAlreadyClosed`, `CreditAccountAlreadyClosed` ([`EventClass::Closed`]) | `ERR-AccountClosed` |
/// | `ExternalIdAlreadyUsed`, `TransferIdAlreadyExists` ([`EventClass::Duplicate`]) | `ERR-ClientKeyReused` |
/// | `LinkedEventFailed` alone, or an applied event | `ERR-Internal`: unreachable (`REQ-ER-2`) |
///
/// A duplicate is a replay. A repeat of an accepted request returns its stored outcome before the
/// kernel is reached; a posting the kernel has seen that the service store does not know is not
/// a repeat it can answer, so the reference is refused as used (`REQ-KER-6`). Every other kernel
/// status — `AccountNotFound` included, since the binding creates every missing sub-account in
/// the same step (`INV-9`) — is a binding fault: nothing commits, and the command answers
/// `ERR-Internal`, with the fault kept for the operator and an incident raised (`REQ-ER-2`,
/// amendment 0006 item 8).
pub(super) fn kernel_refusal(class: &EventClass) -> Refusal {
    match class {
        EventClass::Insufficient { .. } => Refusal::InsufficientAvailable,
        EventClass::Closed => Refusal::AccountClosed,
        EventClass::Duplicate => Refusal::ClientKeyReused,
        EventClass::Applied { .. } | EventClass::ChainAborted { .. } => Refusal::Internal,
    }
}

/// The chain of a transfer: one balanced `Transfer` posting per leg, ordinals `0..N`, in leg
/// order. Every entry is on its account's sub-account of the leg's asset.
pub(super) fn chain_of(
    op: &OperationId,
    legs: &[(LedgerAccount, LedgerAccount, Asset, NonZeroMinor)],
) -> Result<Vec<PostingDraft>, Refusal> {
    let mut chain = Vec::with_capacity(legs.len());
    for (i, (from, to, asset, amount)) in legs.iter().enumerate() {
        let ordinal = u32::try_from(i).map_err(|_| Refusal::BatchTooLarge)?;
        let meta = PostingMeta::new(PostingKind::Transfer, op.to_ref(), ordinal);
        let draft = PostingDraft::balanced(
            meta,
            vec![
                EntryDraft::debit(from.sub_account(asset), amount.get()),
                EntryDraft::credit(to.sub_account(asset), amount.get()),
            ],
        )
        .map_err(|_| Refusal::NotAuthorised)?;
        chain.push(draft);
    }
    debug_assert_eq!(chain.len(), legs.len());
    debug_assert!(
        chain
            .iter()
            .enumerate()
            .all(|(i, d)| d.meta().ordinal as usize == i && !d.closes_debited()),
        "one terminated chain: ordinals 0..N, nothing closes"
    );
    Ok(chain)
}

/// Why validation stopped.
enum Halt {
    /// A named refusal of the command.
    Refuse(Refusal),
    /// A binding fault while reading the book: fail closed, decide nothing (`F-008` Open points).
    Fault(Refusal),
}

impl From<Refusal> for Halt {
    fn from(r: Refusal) -> Halt {
        Halt::Refuse(r)
    }
}

/// A transfer that passed validation.
struct Checked {
    legs: Vec<(LedgerAccount, LedgerAccount, Asset, NonZeroMinor)>,
    used: Vec<AuthorityUsed>,
    charges: Vec<Charge>,
    accounts: Vec<AccountId>,
    deadline: Time,
}

/// How the command meets the operation it names.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Path {
    /// No operation: decided at once, and a refusal records nothing.
    Synchronous,
    /// A transfer durably received first, still at `Received`: its decision is recorded.
    Received,
}

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    /// `transfer` (SPEC §8.1, `US-B1`, `US-B3`, `REQ-KER-10`): move value between ledger accounts,
    /// one `Transfer` posting per leg, all legs or none, final on acceptance. Returns the posting
    /// sequence numbers of the legs, in order, and the state `Settled`.
    ///
    /// Refusals, in the fixed order of the module documentation: `ERR-BatchTooLarge`,
    /// `ERR-CredentialInvalid`, `ERR-IdentityRestricted`, `ERR-ClientKeyReused`, `ERR-ZeroAmount`,
    /// `ERR-UnknownAsset`, `ERR-AssetDeprecated`, `ERR-PrecisionExceeded`, `ERR-UnknownAccount`,
    /// `ERR-AccountFrozen`, `ERR-AccountClosed`, `ERR-IdentityRestricted`, `ERR-NotAuthorised`,
    /// `ERR-AuthorityRevoked`, `ERR-BoundExceeded`, `ERR-DeadlineRequired`, then
    /// `ERR-InsufficientAvailable` (a Treasury limit, a leg into its own account, or the kernel)
    /// and the kernel's others (`F-008` §6). A refused transfer changes nothing in the book
    /// (`AC-B1.2`, `AC-B3.1`). A repeat of an accepted request returns the original outcome and writes
    /// nothing (`REQ-OP-3`).
    pub fn transfer(
        &mut self,
        ctx: &Ctx,
        transfer: &Transfer,
    ) -> Result<Accepted<Vec<u64>>, Refusal> {
        let top = self.enter(|| crate::call::Call::Transfer {
            ctx: *ctx,
            transfer: transfer.clone(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<Vec<u64>>, Refusal> = (|| {
            // 1. The declared maximum, before any lookup (AC-B3.3).
            if transfer.legs.len() > self.config.max_legs {
                return Err(Refusal::BatchTooLarge);
            }
            // 2. The credential, then a repeat.
            let p = resolve(&self.state, &ctx.caller)?;
            let digest = requests::transfer(&p, transfer);
            if let Some(a) = self.replay(ctx, &p, &digest, |r| r.postings.clone())? {
                return Ok(a);
            }
            if let Some(e) = self.state.operations().request(&ctx.op) {
                // Decided before without an attribution record: a received transfer that was
                // rejected. The same request gets the same refusal.
                return Err(if e.initiator == p.identity && e.digest == digest {
                    self.rejection_of(&ctx.op)
                } else {
                    Refusal::ClientKeyReused
                });
            }
            let path = match self.state.operations().get(&ctx.op) {
                None => Path::Synchronous,
                Some(o)
                    if o.state() == State::Received
                        && o.kind() == OperationKind::Transfer
                        && o.class() == KindClass::NoReservation
                        && o.initiator() == Some(p.identity) =>
                {
                    Path::Received
                }
                Some(_) => return Err(Refusal::ClientKeyReused),
            };
            // 3.–7. Validation.
            let checked = match self.check_transfer(ctx, &p, transfer) {
                Ok(c) => c,
                Err(Halt::Fault(r)) => return Err(r),
                Err(Halt::Refuse(r)) => {
                    if path == Path::Received {
                        self.reject_received(ctx, &p, digest, KindClass::NoReservation, r, None)?;
                    }
                    return Err(r);
                }
            };
            // The chain, the lifecycle and the record, in one commit.
            let mut u = UnitBuilder::new();
            for draft in chain_of(&ctx.op, &checked.legs)? {
                u.post(draft);
            }
            let evidence = Evidence::request(&digest);
            let mut facts = Facts::new(Origin::Command, KindClass::NoReservation);
            facts.accepts = true;
            let deadline = Some(checked.deadline);
            let accepted = self
                .plan_move(
                    ctx.op,
                    State::Received,
                    Trigger::Decide,
                    &facts,
                    evidence,
                    deadline,
                    ctx.now,
                    checked.used.clone(),
                )
                .map_err(stop_refusal)?;
            let settled = self
                .plan_move(
                    ctx.op,
                    State::Accepted,
                    Trigger::SettleWithoutReservation,
                    &facts,
                    evidence,
                    deadline,
                    ctx.now,
                    checked.used.clone(),
                )
                .map_err(stop_refusal)?;
            let record = OperationRecord {
                op: ctx.op,
                command: Command::Transfer,
                authorities: checked.used.clone(),
                accounts: checked.accounts.clone(),
                postings: Vec::new(),
                at: ctx.now,
            };
            let open = match path {
                Path::Synchronous => Some(NewOperation {
                    reference: ctx.op,
                    kind: OperationKind::Transfer,
                    class: KindClass::NoReservation,
                    origin: Origin::Command,
                    initiator: Some(p.identity),
                    state: State::Received,
                    deadline: Some(self.received_deadline(ctx.now)),
                    at: ctx.now,
                }),
                Path::Received => None,
            };
            let plan = Plan {
                unit: Some(u),
                writes: checked.charges.iter().copied().map(Write::Charge).collect(),
                open,
                transitions: vec![accepted, settled],
                attribution: Attribution::Put(record),
                request: Some(entry(ctx, &p, digest, State::Settled)),
                refusals: Refusals::Transfer,
            };
            match self.commit_plan(ctx.now, plan) {
                Ok(done) => Ok(Accepted {
                    value: done.postings,
                    record: done.record.ok_or(Refusal::NotAuthorised)?,
                    status: Some(Shown::of(State::Settled)),
                }),
                Err(f) if f.by_kernel && path == Path::Received => {
                    // Accepted, then refused by the kernel before any balance effect.
                    self.reject_received(
                        ctx,
                        &p,
                        digest,
                        KindClass::NoReservation,
                        f.refusal,
                        Some((checked.deadline, checked.used)),
                    )?;
                    Err(f.refusal)
                }
                Err(f) => Err(f.refusal),
            }
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// Take a transfer in at `Received` (`F-009`, `DEC-5`, `REQ-CS-2`, `REQ-CS-7`): the core's
    /// half of an acknowledgement that ingress gave before the core decided. It opens the
    /// operation `ctx.op` at `Received`, kind `Transfer`, attributed to the initiating principal
    /// (`REQ-PN-1`), with the decision bound as its deadline. It moves no value and decides
    /// nothing: [`Ledger::transfer`] under the same `ctx.op` then decides it, and records a
    /// refusal as `Received → Rejected` with a notification that names it (`REQ-EV-1`).
    ///
    /// Idempotent: the same principal taking in the same reference again gets the state the
    /// operation is in now and writes nothing. A reference opened by anyone else, or for anything
    /// else, is `ERR-ClientKeyReused`. The credential is checked first (`ERR-CredentialInvalid`,
    /// `ERR-IdentityRestricted`); the leg count too, so a unit that could never be decided is not
    /// opened (`ERR-BatchTooLarge`).
    pub fn receive_transfer(&mut self, ctx: &Ctx, transfer: &Transfer) -> Result<Shown, Refusal> {
        let top = self.enter(|| crate::call::Call::ReceiveTransfer {
            ctx: *ctx,
            transfer: transfer.clone(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Shown, Refusal> = (|| {
            if transfer.legs.len() > self.config.max_legs {
                return Err(Refusal::BatchTooLarge);
            }
            let p = resolve(&self.state, &ctx.caller)?;
            match self.state.operations().get(&ctx.op) {
                None => self.receive(ctx, OperationKind::Transfer, KindClass::NoReservation),
                Some(o)
                    if o.kind() == OperationKind::Transfer
                        && o.class() == KindClass::NoReservation
                        && o.origin() == Origin::Command
                        && o.initiator() == Some(p.identity) =>
                {
                    Ok(o.shown())
                }
                Some(_) => Err(Refusal::ClientKeyReused),
            }
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// Steps 3 to 7 of the fixed order: everything the kernel is not asked.
    fn check_transfer(&mut self, ctx: &Ctx, p: &Principal, t: &Transfer) -> Result<Checked, Halt> {
        // 3. Each leg: amount, asset. Then every sum the unit makes on one account and asset
        //    must be expressible in minor units.
        if t.legs.is_empty() {
            return Err(Refusal::ZeroAmount.into());
        }
        let mut amounts = Vec::with_capacity(t.legs.len());
        let mut assets = Vec::with_capacity(t.legs.len());
        for leg in &t.legs {
            amounts.push(NonZeroMinor::new(leg.amount)?);
            assets.push(*self.state.registry().active(&leg.asset)?);
        }
        let mut sides: BTreeMap<(AccountId, AssetId, bool), u128> = BTreeMap::new();
        for leg in &t.legs {
            for (account, debit) in [(leg.from, true), (leg.to, false)] {
                let sum = sides.entry((account, leg.asset, debit)).or_insert(0);
                *sum += u128::from(leg.amount.get());
                if *sum > u128::from(u64::MAX) {
                    return Err(Refusal::PrecisionExceeded.into());
                }
            }
        }
        // 4. Each account, in order of appearance: exists and is reached, then its status, then
        //    its owner's.
        let mut accounts: Vec<AccountId> = Vec::new();
        let mut seen: BTreeSet<(AccountId, bool)> = BTreeSet::new();
        for leg in &t.legs {
            for (id, debited) in [(leg.from, true), (leg.to, false)] {
                if !seen.insert((id, debited)) {
                    continue;
                }
                let acct = self.state.account(&id).ok_or(Refusal::UnknownAccount)?;
                if debited && !eval::knows(&self.state, p, acct) {
                    return Err(Refusal::UnknownAccount.into());
                }
                acct.require_open()?;
                self.state
                    .identity(&acct.owner())
                    .ok_or(Refusal::NotAuthorised)?
                    .require_active()?;
                if !accounts.contains(&id) {
                    accounts.push(id);
                }
            }
        }
        // 5. Authority over each debited account, leg by leg; then the legs that draw on one
        //    grant, together.
        let mut decisions: Vec<Decision> = Vec::with_capacity(t.legs.len());
        for (leg, amount) in t.legs.iter().zip(&amounts) {
            let to = self.state.account(&leg.to).ok_or(Refusal::UnknownAccount)?;
            if to.kind() == AccountKind::Issuance {
                // Value enters and leaves the issuance counterparty only by issue and redeem.
                return Err(Refusal::NotAuthorised.into());
            }
            decisions.push(eval::authorise_principal(
                &self.state,
                p,
                &Request {
                    kind: OperationKind::Transfer,
                    account: leg.from,
                    asset: Some(leg.asset),
                    amount: Some(*amount),
                    counterparty: Some(leg.to),
                },
                ctx.now,
                self.generation(),
            )?);
        }
        let charges = self.bounds_together(t, &decisions, ctx.now)?;
        // 6. The deadline.
        let deadline = self.deadline_for(OperationKind::Transfer, t.deadline, ctx.now)?;
        // 7. The limits the kernel does not check, leg by leg, as the kernel applies them.
        let mut legs = Vec::with_capacity(t.legs.len());
        for ((leg, amount), asset) in t.legs.iter().zip(&amounts).zip(&assets) {
            let from = self
                .state
                .account(&leg.from)
                .ok_or(Refusal::UnknownAccount)?
                .clone();
            let to = self
                .state
                .account(&leg.to)
                .ok_or(Refusal::UnknownAccount)?
                .clone();
            legs.push((from, to, *asset, *amount));
        }
        self.check_limits(&legs)?;
        Ok(Checked {
            legs,
            used: decisions.iter().map(|d| d.used).collect(),
            charges,
            accounts,
            deadline,
        })
    }

    /// The bounds of the unit as one operation: legs that draw on one grant are added together
    /// for its per-operation bound, and every window counter is charged once with their sum
    /// (`REQ-AG-2`). Returns the merged window charges.
    fn bounds_together(
        &self,
        t: &Transfer,
        decisions: &[Decision],
        now: Time,
    ) -> Result<Vec<Charge>, Refusal> {
        let mut per_grant: BTreeMap<(GrantId, AssetId), u128> = BTreeMap::new();
        for (leg, d) in t.legs.iter().zip(decisions) {
            if let Some(g) = d.used.grant {
                *per_grant.entry((g, leg.asset)).or_insert(0) += u128::from(leg.amount.get());
            }
        }
        for ((g, _), sum) in &per_grant {
            let grant = self.state.grant(g).ok_or(Refusal::NotAuthorised)?;
            let eff = effective(grant, now, |id| self.state.grant(id))
                .map_err(|_| Refusal::NotAuthorised)?;
            if eff
                .per_operation
                .is_some_and(|max| *sum > u128::from(max.minor().get()))
            {
                return Err(Refusal::BoundExceeded);
            }
        }
        let mut windows: BTreeMap<WindowKey, (u128, Charge)> = BTreeMap::new();
        for c in decisions.iter().flat_map(|d| d.charges.iter()) {
            let slot = windows.entry(c.key).or_insert((0, *c));
            slot.0 += u128::from(c.amount.get());
        }
        let mut merged = Vec::with_capacity(windows.len());
        for (key, (sum, c)) in windows {
            let after = u128::from(self.state.used(&key).get()) + sum;
            if after > u128::from(c.limit.minor().get()) {
                return Err(Refusal::BoundExceeded);
            }
            let amount = u64::try_from(sum).map_err(|_| Refusal::BoundExceeded)?;
            merged.push(Charge {
                key,
                amount: Minor::new(amount),
                limit: c.limit,
            });
        }
        Ok(merged)
    }

    /// The limits the kernel does not check, leg by leg in the order the kernel applies them,
    /// against the balances the unit starts from:
    ///
    /// - a Treasury account stays within its declared limit (`REQ-LA-2`): the kernel does not
    ///   check an unflagged account;
    /// - a leg into its own account moves nothing, so the kernel sees no net debit and would
    ///   accept any amount; it is held to what is available at that leg, as the kernel holds
    ///   every other leg (`AC-B1.2`, `NOTES.md`).
    ///
    /// Either past its limit is `ERR-InsufficientAvailable`.
    fn check_limits(
        &mut self,
        legs: &[(LedgerAccount, LedgerAccount, Asset, NonZeroMinor)],
    ) -> Result<(), Halt> {
        // The watched positions: every Treasury account of the unit, and every account a leg
        // pays back into itself.
        let mut nets: BTreeMap<(AccountId, AssetId), i128> = BTreeMap::new();
        for (from, to, asset, _) in legs {
            for acct in [from, to] {
                let watched = acct.kind() == AccountKind::Treasury || from.id() == to.id();
                let key = (acct.id(), asset.id());
                if !watched || nets.contains_key(&key) {
                    continue;
                }
                let net = match self.net(acct, asset) {
                    Ok(n) => n,
                    Err(f) => return Err(Halt::Fault(self.fault(f))),
                };
                nets.insert(key, net);
            }
        }
        if nets.is_empty() {
            return Ok(());
        }
        // A watched account's net before the first leg that watches it must include every leg
        // before: walk the unit from the start, keeping every watched net current.
        for (from, to, asset, amount) in legs {
            let value = i128::from(amount.minor().get());
            if let Some(net) = nets.get(&(from.id(), asset.id())).copied() {
                if from.kind() == AccountKind::Treasury {
                    check_treasury_limit(from, &asset.id(), net, amount.minor())?;
                } else if from.id() == to.id() && net < value {
                    return Err(Refusal::InsufficientAvailable.into());
                }
            }
            if let Some(net) = nets.get_mut(&(from.id(), asset.id())) {
                *net -= value;
            }
            if let Some(net) = nets.get_mut(&(to.id(), asset.id())) {
                *net += value;
            }
        }
        Ok(())
    }
}

/// The client-key entry of a decided transfer, whose reply showed `reply`.
fn entry(ctx: &Ctx, p: &Principal, digest: RequestDigest, reply: State) -> RequestEntry {
    RequestEntry {
        op: ctx.op,
        initiator: p.identity,
        surface: Surface::Command,
        digest,
        reply: Some(reply),
    }
}

/// A planned move that did not happen is a refusal here: the command supplies every fact the
/// move needs.
pub(super) fn stop_refusal(s: Stop) -> Refusal {
    match s {
        Stop::Refuse(r) => r,
        Stop::Stay(_) => Refusal::InvalidTransition,
    }
}
