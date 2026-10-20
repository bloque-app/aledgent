// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The upgrade-authority mode (`T-037-05`; `REQ-RT-18`, `REQ-RT-20`, ruling `a0023-q2`,
//! `DEC-26` (b′); amendment 0034 `[PENDING AMENDMENT 0034]`).
//!
//! The mode decides who may authorise an upgrade of the writer's code, a migration of its
//! construction, and a change of the mode itself. It is part of the writer's committed state, and
//! it is the **only** authority over the writer's code: whatever a platform holds to install
//! programs (the core's package key on `saga`, the node's admin token on `vos-core`) is a host key,
//! never an authority over the book (amendment 0033 (b′)).
//!
//! - **Members** ([`Member`]). A mode names each principal **with one of its credentials**, by the
//!   digest of that credential's verifier ([`verifier_digest`]), and a principal acts under the
//!   mode only with that credential (amendment 0034). A credential the operator adds to a named
//!   principal later (an import, `REQ-MG-7`) has another verifier, so it gains the operator
//!   nothing (`REQ-MG-8`). The operator can still restrict or close a named identity: that can
//!   only stop the principal acting, never let anyone else act for it. It can delay an upgrade,
//!   never authorise one.
//! - **Modes** ([`Rule`], with an optional [`Mode::delay`]): one member ([`Rule::Single`]); a
//!   proposer and a different approver ([`Rule::ProposeApprove`]); `k` distinct members out of
//!   `n` ([`Rule::Threshold`]). The **delay** is counted in the book's own clock, its inputs,
//!   because time comes in only as an input (`REQ-RT-7`): at least `delay` inputs lie strictly
//!   between the input that authorises a change and the input it applies from ([`check_from`]).
//!   It is at most [`MAX_DELAY`], so a mode can always be satisfied within the book's life.
//! - **Where it lives.** The mode, a scheduled change of it, and the proposals not yet scheduled
//!   are one record ([`AuthorityRecord`]), one leaf of the service tree, absent until the first
//!   proposal. The writer's hot row holds the digest of the mode in force ([`Mode::digest`]) and
//!   any scheduled change (`authority`, `authority_next`), and every checkpoint binds the first
//!   (`T-037-03`).
//! - **The genesis mode.** A book whose record is absent is under the mode its genesis fixed:
//!   the operator's organisation identity alone, with any of its credentials (ruling
//!   `a0023-q2`), whose digest is [`GENESIS`], zeros. So a book that never changes its mode keeps
//!   every root, log hash and commitment it had before modes existed. A chainspec that names
//!   another initial mode is `F-036`'s to add.
//! - **Deterministic.** Every decision here is a function of the record, the mode in force, the
//!   credentials the registry holds and the input number, so every replica, and every code version
//!   that decides the same input, decides it the same (`REQ-RT-3`).
//!
//! **The seam an upgrade plugs into** (`T-037-04`, `T-037-10`). Nothing in this code version
//! schedules an upgrade or a migration. A proposal of one that meets the mode's conditions is
//! [`Decision::AwaitingScheduler`]: it stays pending and visible in the record, never consumed,
//! until the step those tasks add schedules it. An `Upgrade { code, from }` under a mode that one
//! member satisfies alone is [`alone`] and [`check_from`]. Nothing here installs anything.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use cipher_clerk::crypto::blake2b_256;

use crate::export::canon::{self, Canonical, Dec, DecodeError, Enc};
use crate::id::{IdentityId, OperationId};
use crate::principal::Verifier;
use crate::refusal::Refusal;

/// The domain of a mode's digest.
pub const MODE_DOMAIN: &[u8] = b"aledgent/upgrade-authority/v1";

/// The domain of a member's verifier digest.
pub const VERIFIER_DOMAIN: &[u8] = b"aledgent/upgrade-authority/verifier/v1";

/// The digest that stands for the genesis mode: the operator's organisation identity alone, with
/// any of its credentials (module documentation). No verifier digest is zeros.
pub const GENESIS: [u8; 32] = [0; 32];

/// The most principals one mode may name, all sets together: the record is one leaf, and the
/// writer's hot state stays bounded (`DEC-4`).
pub const MAX_PRINCIPALS: usize = 32;

/// The longest delay a mode may carry, in inputs: 2¹⁶. A longer one could hold every upgrade, and
/// every change of mode, for longer than the book is likely to live, so it cannot be satisfied
/// (`REQ-RT-18`). The core writer takes at least one input a minute (the ticker's heartbeat), so a
/// mode's delay elapses in at most about 45 days; real traffic only shortens that.
pub const MAX_DELAY: u64 = 1 << 16;

/// The furthest ahead a change may apply, in inputs from the input that authorises it: 2¹⁷,
/// twice [`MAX_DELAY`], so the longest delay still leaves room. A change of mode scheduled further
/// ahead would never take force within the book's life, while the outgoing mode authorises
/// nothing more, and no input withdraws a scheduled change: the book's code could never change
/// again (amendment 0034, clarification of 2026-10-20). At the ticker's floor of one input a
/// minute, the furthest change applies within about 91 days.
pub const MAX_LEAD: u64 = 2 * MAX_DELAY;

/// The most proposals pending at once: one per proposer and kind ([`Kind`]), and at most this many
/// in all, so the record stays one bounded leaf. **The limit, stated plainly:** a proposer holds
/// at most three slots (one per kind), so six or more proposers between them can fill the queue,
/// and every other proposal is then refused (`ERR-InvalidTransition`) until one is replaced by its
/// proposer, or a change of mode clears them all. A full record of sixteen changes of mode, each
/// naming thirty-two members and approved by all of them, is about 37 KB (tested); how the core's
/// guest heap takes a witness that size under the platform is not tested here.
pub const MAX_PENDING: usize = 16;

/// The digest a member names its credential by: `blake2b-256(VERIFIER_DOMAIN ‖ scheme ‖
/// material)`. Public material only (`REQ-AU-8`).
pub fn verifier_digest(v: &Verifier) -> [u8; 32] {
    blake2b_256(VERIFIER_DOMAIN, &[&[v.scheme().tag()], v.as_bytes()])
}

/// A principal bound to one of its credentials, by the digest of that credential's verifier
/// (amendment 0034).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Member {
    /// The principal.
    pub principal: IdentityId,
    /// Its credential's [`verifier_digest`].
    pub verifier: [u8; 32],
}

/// Who may authorise (module documentation). A closed list: a kind the writer's code does not know
/// needs an upgrade first (`REQ-RT-18`).
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Rule {
    /// One member holds the authority alone.
    Single {
        /// The member.
        member: Member,
    },
    /// One member proposes and a different one approves, each holding its own authority
    /// (`REQ-OS-3`).
    ProposeApprove {
        /// Who may propose.
        proposers: BTreeSet<Member>,
        /// Who may approve.
        approvers: BTreeSet<Member>,
    },
    /// `k` distinct members out of the set; the proposer is the first of them.
    Threshold {
        /// The members.
        members: BTreeSet<Member>,
        /// How many distinct members authorise.
        k: u32,
    },
}

/// An upgrade-authority mode: its rule and its delay.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Mode {
    /// Who may authorise.
    pub rule: Rule,
    /// The least number of inputs strictly between the input that authorises a change and the
    /// input it applies from; 0 for none ([`check_from`]); at most [`MAX_DELAY`].
    pub delay: u64,
}

impl Mode {
    /// One member, with no delay.
    pub fn single(member: Member) -> Mode {
        Mode {
            rule: Rule::Single { member },
            delay: 0,
        }
    }

    /// Its digest, `blake2b-256(MODE_DOMAIN ‖ canonical encoding)`: the hot row's `authority`
    /// when it is in force. Sets are sorted and hold each member once, so one mode has one
    /// digest however it was written.
    pub fn digest(&self) -> [u8; 32] {
        blake2b_256(MODE_DOMAIN, &[&canon::to_bytes(self)])
    }

    /// Every member it names.
    pub fn members(&self) -> BTreeSet<Member> {
        match &self.rule {
            Rule::Single { member } => [*member].into(),
            Rule::ProposeApprove {
                proposers,
                approvers,
            } => proposers.union(approvers).copied().collect(),
            Rule::Threshold { members, .. } => members.clone(),
        }
    }

    /// Whether it can be satisfied (`REQ-RT-18`, `AC-K7.8`), `ERR-MalformedCommand` otherwise:
    /// every set non-empty; each principal named with one credential; at most [`MAX_PRINCIPALS`]
    /// principals; a proposer with an approver who is another principal; `1 ≤ k ≤ n`; a delay of
    /// at most [`MAX_DELAY`]. And every member one that `holds` the authority, with the credential
    /// it is named with (`ERR-NotAuthorised` otherwise).
    pub fn satisfiable(&self, holds: impl Fn(&Member) -> bool) -> Result<(), Refusal> {
        let members = self.members();
        let mut by_principal = BTreeMap::new();
        for m in &members {
            if *by_principal.entry(m.principal).or_insert(m.verifier) != m.verifier {
                return Err(Refusal::MalformedCommand);
            }
        }
        let shape = match &self.rule {
            Rule::Single { .. } => true,
            Rule::ProposeApprove {
                proposers,
                approvers,
            } => proposers
                .iter()
                .any(|p| approvers.iter().any(|a| a.principal != p.principal)),
            Rule::Threshold { members, k } => *k >= 1 && (*k as usize) <= members.len(),
        };
        if !shape || by_principal.len() > MAX_PRINCIPALS || self.delay > MAX_DELAY {
            return Err(Refusal::MalformedCommand);
        }
        if !members.iter().all(holds) {
            return Err(Refusal::NotAuthorised);
        }
        Ok(())
    }

    /// Whether `m` may propose under it.
    pub fn may_propose(&self, m: &Member) -> bool {
        match &self.rule {
            Rule::Single { member } => member == m,
            Rule::ProposeApprove { proposers, .. } => proposers.contains(m),
            Rule::Threshold { members, .. } => members.contains(m),
        }
    }

    /// Whether `m` may approve anything under it. Checked before any proposal is looked up, so a
    /// principal outside the mode learns nothing of what is pending.
    pub fn may_approve(&self, m: &Member) -> bool {
        match &self.rule {
            Rule::Single { member } => member == m,
            Rule::ProposeApprove { approvers, .. } => approvers.contains(m),
            Rule::Threshold { members, .. } => members.contains(m),
        }
    }

    /// Count `m`'s approval of `proposal`, `m` being one who [`Mode::may_approve`]:
    /// `ERR-ApprovalRequired` for the proposer's own approval under `ProposeApprove`, or a second
    /// approval by the same principal (two checks by one principal are not two);
    /// `ERR-InvalidTransition` under one member, where nothing waits to be approved.
    pub fn approve(&self, proposal: &mut Proposal, m: &Member) -> Result<(), Refusal> {
        match &self.rule {
            Rule::Single { .. } => return Err(Refusal::InvalidTransition),
            Rule::ProposeApprove { .. } if proposal.proposer == m.principal => {
                return Err(Refusal::ApprovalRequired);
            }
            _ => {}
        }
        if !proposal.approvals.insert(m.principal) {
            return Err(Refusal::ApprovalRequired);
        }
        Ok(())
    }
}

/// `m` satisfies `mode` alone: the mode a single `Upgrade { code, from }` is authorised under
/// (`DEC-26` (b′), `T-037-04`).
pub fn alone(mode: &Mode, m: &Member) -> bool {
    match &mode.rule {
        Rule::Single { member } => member == m,
        Rule::Threshold { members, k } => *k == 1 && members.contains(m),
        Rule::ProposeApprove { .. } => false,
    }
}

/// A change authorised at input `at` may apply from input `from` (`ERR-InvalidTransition`
/// otherwise): forward only (`from > at`), with at least the mode's delay of inputs strictly
/// between them (`from − at − 1 ≥ delay`), and at most [`MAX_LEAD`] inputs ahead
/// (`from − at ≤ MAX_LEAD`).
pub fn check_from(mode: &Mode, at: u64, from: u64) -> Result<(), Refusal> {
    match from.checked_sub(at) {
        Some(gap) if gap > mode.delay && gap <= MAX_LEAD => Ok(()),
        _ => Err(Refusal::InvalidTransition),
    }
}

/// The kind of a change: an approval names it, and must match the proposal's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Kind {
    /// An upgrade of the code.
    Upgrade,
    /// A migration of construction.
    Migration,
    /// A change of mode.
    SetAuthority,
}

/// What a proposal would do once authorised.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Action {
    /// Run the code version `code` from input `from` (`REQ-RT-16`; scheduled by `T-037-04`).
    Upgrade {
        /// The code version *H*.
        code: [u8; 32],
        /// The input it applies from.
        from: u64,
    },
    /// Migrate the state commitment to construction `to` from input `from` (`REQ-PR-11`;
    /// `T-037-10`).
    Migration {
        /// The construction.
        to: u8,
        /// The input it applies from.
        from: u64,
    },
    /// Change the mode from input `from` (`REQ-RT-18`).
    SetAuthority {
        /// The new mode.
        mode: Mode,
        /// The input it applies from.
        from: u64,
    },
}

impl Action {
    /// The input it applies from.
    pub fn from(&self) -> u64 {
        match self {
            Action::Upgrade { from, .. }
            | Action::Migration { from, .. }
            | Action::SetAuthority { from, .. } => *from,
        }
    }

    /// Its kind.
    pub fn kind(&self) -> Kind {
        match self {
            Action::Upgrade { .. } => Kind::Upgrade,
            Action::Migration { .. } => Kind::Migration,
            Action::SetAuthority { .. } => Kind::SetAuthority,
        }
    }
}

/// A change proposed and not yet scheduled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Proposal {
    /// Its id: the operation reference of the input that proposed it.
    pub id: OperationId,
    /// What it would do.
    pub action: Action,
    /// The digest of the mode it was proposed under: it is approved only while that mode is in
    /// force, so no proposal survives a change of mode (`REQ-RT-18`).
    pub under: [u8; 32],
    /// Who proposed it. A principal has at most one pending proposal of each kind; a new one
    /// replaces it, and replaces no one else's.
    pub proposer: IdentityId,
    /// The distinct principals whose approval counts: the approver under `ProposeApprove`, every
    /// member so far (the proposer first) under `Threshold`.
    pub approvals: BTreeSet<IdentityId>,
    /// The input that proposed it.
    pub at: u64,
}

/// The mode in force at an input, with its digest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InForce {
    /// The mode.
    pub mode: Mode,
    /// Its digest: [`GENESIS`] for the genesis mode.
    pub digest: [u8; 32],
}

impl InForce {
    /// The member a caller is under this mode: `principal` with the credential whose verifier
    /// digest is `verifier`. The genesis mode binds no credential: any of the operator's will do.
    pub fn member(&self, principal: IdentityId, verifier: [u8; 32]) -> Member {
        Member {
            principal,
            verifier: if self.digest == GENESIS {
                GENESIS
            } else {
                verifier
            },
        }
    }
}

/// One upgrade-authority input (`DEC-26` (b′)): an operator intervention on `CTR-OPS`, audited
/// with its justification, and decided by the writer under the mode in force.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Input {
    /// `SetAuthority { mode, from }`: change the mode from input `from` (`REQ-RT-18`).
    SetAuthority {
        /// The new mode.
        mode: Mode,
        /// The input it applies from.
        from: u64,
    },
    /// `ProposeUpgrade { code, from }` (`REQ-RT-16`).
    ProposeUpgrade {
        /// The code version *H*.
        code: [u8; 32],
        /// The input it applies from.
        from: u64,
    },
    /// `ApproveUpgrade { proposal, kind }`: approve a proposed upgrade, migration or change of
    /// mode. The kind must be the proposal's, so an approver never approves another kind of
    /// change than the one it means to.
    ApproveUpgrade {
        /// The proposal: the operation reference of the input that proposed it.
        proposal: OperationId,
        /// Its kind.
        kind: Kind,
    },
    /// `ProposeMigration { to, from }` (`REQ-PR-11`).
    ProposeMigration {
        /// The construction.
        to: u8,
        /// The input it applies from.
        from: u64,
    },
}

impl Input {
    /// The `Ledger` method this input is.
    pub fn method(&self) -> &'static str {
        match self {
            Input::SetAuthority { .. } => "set_authority",
            Input::ProposeUpgrade { .. } => "propose_upgrade",
            Input::ApproveUpgrade { .. } => "approve_upgrade",
            Input::ProposeMigration { .. } => "propose_migration",
        }
    }

    /// The command its audit record names.
    pub fn command(&self) -> crate::state::Command {
        use crate::state::Command;
        match self {
            Input::SetAuthority { .. } => Command::SetAuthority,
            Input::ProposeUpgrade { .. } => Command::ProposeUpgrade,
            Input::ApproveUpgrade { .. } => Command::ApproveUpgrade,
            Input::ProposeMigration { .. } => Command::ProposeMigration,
        }
    }

    /// The change it proposes, if it proposes one.
    pub fn action(&self) -> Option<Action> {
        match self {
            Input::SetAuthority { mode, from } => Some(Action::SetAuthority {
                mode: mode.clone(),
                from: *from,
            }),
            Input::ProposeUpgrade { code, from } => Some(Action::Upgrade {
                code: *code,
                from: *from,
            }),
            Input::ProposeMigration { to, from } => Some(Action::Migration {
                to: *to,
                from: *from,
            }),
            Input::ApproveUpgrade { .. } => None,
        }
    }
}

/// What a proposal or an approval came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Not authorised yet: it waits for more approvals.
    Pending,
    /// A change of mode, authorised: scheduled at once, through `authority_next`.
    Authorised(Action),
    /// An upgrade or a migration whose proposal meets the mode's conditions at this input, and
    /// which **is not scheduled**: nothing in this code version schedules one (`T-037-04`,
    /// `T-037-10`). The proposal stays pending and visible in the record, and a change of mode
    /// cancels it like any other pending proposal.
    AwaitingScheduler(Action),
    /// The same command was accepted before under this operation reference (`REQ-OP-10`): its
    /// outcome is in its record and its audit trail.
    Repeated,
}

/// The seam (module documentation): whether `proposal` has met the mode in force's conditions at
/// input `at`. `ERR-InvalidTransition` if it was proposed under another mode, or if its change,
/// authorised now, would apply too soon ([`check_from`]).
pub fn authorise(in_force: &InForce, proposal: &Proposal, at: u64) -> Result<Decision, Refusal> {
    if proposal.under != in_force.digest {
        return Err(Refusal::InvalidTransition);
    }
    let met = match &in_force.mode.rule {
        Rule::Single { member } => proposal.proposer == member.principal,
        Rule::ProposeApprove { .. } => !proposal.approvals.is_empty(),
        Rule::Threshold { k, .. } => proposal.approvals.len() >= *k as usize,
    };
    if !met {
        return Ok(Decision::Pending);
    }
    check_from(&in_force.mode, at, proposal.action.from())?;
    Ok(match proposal.action.kind() {
        Kind::SetAuthority => Decision::Authorised(proposal.action.clone()),
        Kind::Upgrade | Kind::Migration => Decision::AwaitingScheduler(proposal.action.clone()),
    })
}

/// The writer's upgrade-authority record: one leaf of the service tree (module documentation).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthorityRecord {
    /// The mode in force since the last change; `None` for the genesis mode.
    pub mode: Option<Mode>,
    /// A change of mode authorised and not yet in force, and the input it applies from.
    pub next: Option<(Mode, u64)>,
    /// The proposals not yet scheduled: at most one per proposer and kind, at most
    /// [`MAX_PENDING`].
    pub pending: Vec<Proposal>,
}

impl AuthorityRecord {
    /// The mode in force at input `n`, the genesis one (`operator` alone) if no other was set.
    pub fn in_force(&self, n: u64, operator: IdentityId) -> InForce {
        match (&self.next, &self.mode) {
            (Some((m, from)), _) if n >= *from => InForce {
                mode: m.clone(),
                digest: m.digest(),
            },
            (_, Some(m)) => InForce {
                mode: m.clone(),
                digest: m.digest(),
            },
            (_, None) => InForce {
                mode: Mode::single(Member {
                    principal: operator,
                    verifier: GENESIS,
                }),
                digest: GENESIS,
            },
        }
    }

    /// The record as it stands at input `n`: a scheduled change that is in force is the mode,
    /// and a proposal made under any other mode is gone (`REQ-RT-18`).
    pub fn settle(&mut self, n: u64, operator: IdentityId) {
        if let Some((m, from)) = self.next.take() {
            if n >= from {
                self.mode = Some(m);
            } else {
                self.next = Some((m, from));
            }
        }
        let digest = self.in_force(n, operator).digest;
        self.pending.retain(|p| p.under == digest);
    }
}

/// The writer's upgrade-authority position, which the writer keeps beside its code: not service
/// state (`crate::Ledger::set_code`). Its hot row holds `authority` and `authority_next`; this is
/// the rest of what one input needs: what it has decided so far. The input's number is the
/// ledger's own (`crate::Ledger::set_input`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Position {
    /// A scheduled change of mode: the new mode's digest and the input it applies from.
    pub next: Option<([u8; 32], u64)>,
    /// Whether this input has changed the mode.
    pub changed_mode: bool,
    /// Whether an upgrade or a migration met the mode's conditions in this input: a change of
    /// mode and an upgrade never share an input (`DEC-26` (b′)).
    pub upgraded: bool,
}

impl Position {
    /// Begin input `n` under the mode digest `authority`: a scheduled change of mode whose input
    /// has come is the mode in force from it. The one place that makes a change of mode take
    /// force, whoever decides the input (the Agent, the host's replica, a replay, the in-process
    /// core).
    pub fn begin(&mut self, authority: &mut [u8; 32], n: u64) {
        self.changed_mode = false;
        self.upgraded = false;
        if let Some((d, from)) = self.next
            && n >= from
        {
            *authority = d;
            self.next = None;
        }
    }
}

// ------------------------------------------------------------------ the canonical encoding

impl Canonical for Member {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.principal).put(&self.verifier);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Member {
            principal: d.get()?,
            verifier: d.get()?,
        })
    }
}

impl Canonical for Rule {
    fn encode(&self, e: &mut Enc) {
        match self {
            Rule::Single { member } => {
                e.tag(1).put(member);
            }
            Rule::ProposeApprove {
                proposers,
                approvers,
            } => {
                e.tag(2).put(proposers).put(approvers);
            }
            Rule::Threshold { members, k } => {
                e.tag(3).put(members).put(&u64::from(*k));
            }
        }
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        match d.tag()? {
            1 => Ok(Rule::Single { member: d.get()? }),
            2 => Ok(Rule::ProposeApprove {
                proposers: d.get()?,
                approvers: d.get()?,
            }),
            3 => {
                let members = d.get()?;
                let k: u64 = d.get()?;
                let k = u32::try_from(k).map_err(|_| DecodeError("a threshold over u32"))?;
                Ok(Rule::Threshold { members, k })
            }
            _ => Err(DecodeError("an upgrade-authority rule")),
        }
    }
}

impl Canonical for Mode {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.rule).put(&self.delay);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Mode {
            rule: d.get()?,
            delay: d.get()?,
        })
    }
}

impl Canonical for Kind {
    fn encode(&self, e: &mut Enc) {
        e.tag(match self {
            Kind::Upgrade => 1,
            Kind::Migration => 2,
            Kind::SetAuthority => 3,
        });
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        match d.tag()? {
            1 => Ok(Kind::Upgrade),
            2 => Ok(Kind::Migration),
            3 => Ok(Kind::SetAuthority),
            _ => Err(DecodeError("an upgrade-authority kind")),
        }
    }
}

impl Canonical for Action {
    fn encode(&self, e: &mut Enc) {
        match self {
            Action::Upgrade { code, from } => {
                e.tag(1).put(code).put(from);
            }
            Action::Migration { to, from } => {
                e.tag(2).put(to).put(from);
            }
            Action::SetAuthority { mode, from } => {
                e.tag(3).put(mode).put(from);
            }
        }
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        match d.tag()? {
            1 => Ok(Action::Upgrade {
                code: d.get()?,
                from: d.get()?,
            }),
            2 => Ok(Action::Migration {
                to: d.get()?,
                from: d.get()?,
            }),
            3 => Ok(Action::SetAuthority {
                mode: d.get()?,
                from: d.get()?,
            }),
            _ => Err(DecodeError("an upgrade-authority action")),
        }
    }
}

impl Canonical for Proposal {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.id)
            .put(&self.action)
            .put(&self.under)
            .put(&self.proposer)
            .put(&self.approvals)
            .put(&self.at);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Proposal {
            id: d.get()?,
            action: d.get()?,
            under: d.get()?,
            proposer: d.get()?,
            approvals: d.get()?,
            at: d.get()?,
        })
    }
}

impl Canonical for AuthorityRecord {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.mode).put(&self.next).put(&self.pending);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(AuthorityRecord {
            mode: d.get()?,
            next: d.get()?,
            pending: d.get()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn op(n: u8) -> OperationId {
        OperationId::from_ref(ledger_kernel::posting::OperationRef([n; 16]))
    }

    fn id(n: u8) -> IdentityId {
        IdentityId::mint(&op(n), 0)
    }

    /// Principal `n`, named with a credential of its own.
    fn m(n: u8) -> Member {
        Member {
            principal: id(n),
            verifier: [n; 32],
        }
    }

    fn set(ns: &[u8]) -> BTreeSet<Member> {
        ns.iter().map(|n| m(*n)).collect()
    }

    fn proposal(under: [u8; 32], proposer: u8, from: u64) -> Proposal {
        Proposal {
            id: op(0xAA),
            action: Action::Upgrade {
                code: [7; 32],
                from,
            },
            under,
            proposer: id(proposer),
            approvals: BTreeSet::new(),
            at: 1,
        }
    }

    #[test]
    fn t037_05_one_mode_has_one_digest_and_the_genesis_mode_is_zeros() {
        let a = Mode {
            rule: Rule::ProposeApprove {
                proposers: set(&[1, 2]),
                approvers: set(&[3]),
            },
            delay: 0,
        };
        let b = Mode {
            rule: Rule::ProposeApprove {
                proposers: [m(2), m(1), m(2)].into_iter().collect(),
                approvers: set(&[3]),
            },
            delay: 0,
        };
        assert_eq!(a.digest(), b.digest());
        assert_ne!(
            a.digest(),
            Mode {
                delay: 1,
                ..a.clone()
            }
            .digest()
        );
        // Another credential of the same principal is another mode.
        let other = Mode::single(Member {
            verifier: [9; 32],
            ..m(1)
        });
        assert_ne!(Mode::single(m(1)).digest(), other.digest());
        assert_ne!(Mode::single(m(1)).digest(), GENESIS);
        let r = AuthorityRecord::default();
        let f = r.in_force(5, id(9));
        assert_eq!(f.digest, GENESIS);
        // The genesis mode binds no credential.
        assert!(f.mode.may_propose(&f.member(id(9), [4; 32])));
        assert!(!f.mode.may_propose(&f.member(id(8), [4; 32])));
        let back: AuthorityRecord = canon::from_bytes(&canon::to_bytes(&AuthorityRecord {
            mode: Some(a.clone()),
            next: Some((b, 4)),
            pending: vec![proposal(a.digest(), 1, 9)],
        }))
        .unwrap();
        assert_eq!(back.mode, Some(a));
    }

    #[test]
    fn t037_05_a_mode_must_be_satisfiable() {
        let all = |_: &Member| true;
        let mode = |rule| Mode { rule, delay: 0 };
        // Empty sets, a proposer with no different approver, k of 0 or above n, too many
        // principals, one principal under two credentials.
        for bad in [
            mode(Rule::ProposeApprove {
                proposers: set(&[]),
                approvers: set(&[1]),
            }),
            mode(Rule::ProposeApprove {
                proposers: set(&[1]),
                approvers: set(&[1]),
            }),
            mode(Rule::Threshold {
                members: set(&[1, 2]),
                k: 0,
            }),
            mode(Rule::Threshold {
                members: set(&[1, 2]),
                k: 3,
            }),
            mode(Rule::Threshold {
                members: (0..=MAX_PRINCIPALS as u8).map(m).collect(),
                k: 1,
            }),
            mode(Rule::Threshold {
                members: [
                    m(1),
                    Member {
                        verifier: [8; 32],
                        ..m(1)
                    },
                    m(2),
                ]
                .into(),
                k: 2,
            }),
        ] {
            assert_eq!(
                bad.satisfiable(all),
                Err(Refusal::MalformedCommand),
                "{bad:?}"
            );
        }
        // Exactly the cap is fine.
        let cap = mode(Rule::Threshold {
            members: (0..MAX_PRINCIPALS as u8).map(m).collect(),
            k: MAX_PRINCIPALS as u32,
        });
        assert_eq!(cap.satisfiable(all), Ok(()));
        // One principal both proposes and approves only with another approver beside it.
        let ok = mode(Rule::ProposeApprove {
            proposers: set(&[1]),
            approvers: set(&[1, 2]),
        });
        assert_eq!(ok.satisfiable(all), Ok(()));
        // A member that does not hold the authority.
        assert_eq!(ok.satisfiable(|x| *x != m(2)), Err(Refusal::NotAuthorised));
    }

    #[test]
    fn t037_05_the_delay_is_bounded() {
        let at = |delay| Mode {
            rule: Rule::Single { member: m(1) },
            delay,
        };
        assert_eq!(at(MAX_DELAY).satisfiable(|_| true), Ok(()));
        assert_eq!(
            at(MAX_DELAY + 1).satisfiable(|_| true),
            Err(Refusal::MalformedCommand)
        );
        assert_eq!(
            at(u64::MAX).satisfiable(|_| true),
            Err(Refusal::MalformedCommand)
        );
        // The longest delay can elapse.
        assert_eq!(check_from(&at(MAX_DELAY), 10, 10 + MAX_DELAY + 1), Ok(()));
    }

    /// Review of #45 at `4cc6ae9`, F1: a change applies at most `MAX_LEAD` inputs ahead, so a
    /// change of mode scheduled too far ahead, which would freeze the outgoing mode for good, is
    /// refused. The bound leaves room for the longest delay.
    #[test]
    fn t037_05_the_lead_is_bounded() {
        let none = Mode::single(m(1));
        assert_eq!(check_from(&none, 10, 10 + MAX_LEAD), Ok(()));
        assert_eq!(
            check_from(&none, 10, 10 + MAX_LEAD + 1),
            Err(Refusal::InvalidTransition)
        );
        assert_eq!(
            check_from(&none, 10, u64::MAX),
            Err(Refusal::InvalidTransition)
        );
        let longest = Mode {
            delay: MAX_DELAY,
            ..none
        };
        assert_eq!(check_from(&longest, 10, 10 + MAX_LEAD), Ok(()));
        const { assert!(MAX_LEAD > MAX_DELAY) };
    }

    #[test]
    fn t037_05_threshold_of_one_and_of_n() {
        let members = set(&[1, 2, 3]);
        for k in [1u32, 3] {
            let mode = Mode {
                rule: Rule::Threshold {
                    members: members.clone(),
                    k,
                },
                delay: 0,
            };
            let f = InForce {
                digest: mode.digest(),
                mode: mode.clone(),
            };
            let mut p = proposal(f.digest, 1, 10);
            p.approvals.insert(id(1));
            let mut d = authorise(&f, &p, 2).unwrap();
            for n in 2..=3u8 {
                if d != Decision::Pending {
                    break;
                }
                mode.approve(&mut p, &m(n)).unwrap();
                // A second approval by the same member is not counted.
                assert_eq!(mode.approve(&mut p, &m(n)), Err(Refusal::ApprovalRequired));
                d = authorise(&f, &p, 2).unwrap();
            }
            assert!(matches!(d, Decision::AwaitingScheduler(_)));
            assert_eq!(p.approvals.len(), k as usize);
            assert_eq!(alone(&mode, &m(1)), k == 1);
        }
    }

    #[test]
    fn t037_05_the_delay_is_counted_in_inputs() {
        let mode = Mode {
            rule: Rule::Single { member: m(1) },
            delay: 3,
        };
        // Authorised at input 10: inputs 11, 12, 13 lie between, so 14 is the first it applies from.
        assert_eq!(check_from(&mode, 10, 13), Err(Refusal::InvalidTransition));
        assert_eq!(check_from(&mode, 10, 14), Ok(()));
        let none = Mode::single(m(1));
        assert_eq!(check_from(&none, 10, 10), Err(Refusal::InvalidTransition));
        assert_eq!(check_from(&none, 10, 11), Ok(()));
        assert_eq!(check_from(&none, 10, 3), Err(Refusal::InvalidTransition));
    }

    #[test]
    fn t037_05_a_change_of_mode_takes_force_at_its_input_and_strands_older_proposals() {
        let old = Mode::single(m(1));
        let new = Mode::single(m(2));
        let mut r = AuthorityRecord {
            mode: Some(old.clone()),
            next: Some((new.clone(), 8)),
            pending: vec![proposal(old.digest(), 1, 20)],
        };
        assert_eq!(r.in_force(7, id(9)).digest, old.digest());
        assert_eq!(r.in_force(8, id(9)).digest, new.digest());
        r.settle(7, id(9));
        assert_eq!(r.pending.len(), 1);
        r.settle(8, id(9));
        assert_eq!((r.mode.clone(), r.next.clone()), (Some(new.clone()), None));
        assert!(r.pending.is_empty());
        // And the writer's position swaps its digest at the same input.
        let mut a = old.digest();
        let mut p = Position {
            next: Some((new.digest(), 8)),
            ..Position::default()
        };
        p.begin(&mut a, 7);
        assert_eq!(a, old.digest());
        p.begin(&mut a, 8);
        assert_eq!((a, p.next), (new.digest(), None));
    }
}
