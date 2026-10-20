// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! [`Canonical`] for the records the service store holds (`F-014` T-014-01).
//!
//! Each record is written field by field in declaration order. A record whose identifier is a
//! minting is written as its minting and rebuilt through its own constructor, so the store's
//! `INV-17` holds for an imported record exactly as for a new one; an enum is written behind a
//! tag this module owns, in a `match` that names every variant, so a variant another feature adds
//! fails to compile here until it is given a tag. Tags are never reused.

use alloc::collections::BTreeSet;
use alloc::vec::Vec;

use ledger_kernel::account::{AssetTag, BalanceRule, KernelAccountSpec, Role};
use ledger_kernel::posting::PostingKind;

use super::canon::{Canonical, Dec, DecodeError, Enc};
use crate::account::{AccountKind, AccountStatus, Custody, LedgerAccount};
use crate::anchor::{AnchorIncident, Checkpoint, CheckpointStatus, Comparison, PositionRow};
use crate::asset::{AssetId, AssetKind, AssetLocation, AssetStatus, Minor, NonZeroMinor};
use crate::authority::{
    AuthorityKind, AuthorityUsed, Bounds, Grant, GrantKind, GrantTerms, OperationKind, Subject,
    Window, WindowKey,
};
use crate::custody::machine::ChangeState;
use crate::custody::store::{
    ChangeKind, ChangeTarget, Closed, Notice, NoticeEvent, NoticeId, PendingChange, PendingId,
};
use crate::id::{AccountId, CredentialId, GrantId, IdentityId, Mint, OperationId};
use crate::identity::{Identity, IdentityKind, IdentityStatus};
use crate::lifecycle::Resolution;
use crate::lifecycle::keys::{
    ClientKey, Evidence, NotificationId, RequestDigest, Surface, TransitionKey,
};
use crate::lifecycle::next::{Lifecycle, Shown, StateClass};
use crate::lifecycle::store::{Notification, RequestEntry, TransitionRecord};
use crate::lifecycle::{KindClass, Origin, State as OpState, Trigger};
use crate::operator::{
    EvidenceSubmission, Finding, Incident as OpsIncident, IncidentKind, OperatorOpKind,
    PendingReversal, ProposedResolution, ResolutionCase,
};
use crate::principal::{Credential, CredentialMarker, CredentialStatus, Scheme, Verifier};
use crate::query::journal::{BalanceKind, Direction, Line, PostingEntry};
use crate::refusal::Refusal;
use crate::reservation::{Closing, Position, Reservation, Status};
use crate::set::NonEmptySet;
use crate::settlement::store::{Ingested, Instructed, Standing};
use crate::settlement::{
    AnchorFact, Coordinate, Destination, EventKind, FinalisedHead, Gap, GapReason,
    Incident as SetIncident, Movement, NonceFact, Observation, ObservedFinality, Payload,
    SettlementConfig, Signing, SigningRecord, SourceId, SupportDeclaration, Venue,
};
use crate::state::{Command, OperationRecord, Roles};

/// [`Canonical`] for a fieldless enum, behind the tags given. Every variant must be named.
macro_rules! tagged {
    ($t:ty { $($v:ident => $n:expr),+ $(,)? }) => {
        impl Canonical for $t {
            fn encode(&self, e: &mut Enc) {
                e.tag(match self { $(<$t>::$v => $n),+ });
            }
            fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
                match d.tag()? {
                    $($n => Ok(<$t>::$v),)+
                    _ => Err(DecodeError(stringify!($t))),
                }
            }
        }
    };
}

// ------------------------------------------------------------------ identifiers and amounts

macro_rules! id {
    ($($t:ty),+) => {$(
        impl Canonical for $t {
            fn encode(&self, e: &mut Enc) {
                e.raw(self.as_bytes());
            }
            fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
                Ok(<$t>::from_bytes(d.array()?))
            }
        }
    )+};
}

id!(IdentityId, AccountId, GrantId, CredentialId, OperationId);

impl Canonical for Mint {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.op).put(&self.ordinal);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Mint::new(d.get()?, d.get()?))
    }
}

impl Canonical for AssetId {
    fn encode(&self, e: &mut Enc) {
        e.bytes(self.as_str().as_bytes());
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        let raw = d.bytes()?;
        let s = core::str::from_utf8(raw).map_err(|_| DecodeError("asset id"))?;
        let id = AssetId::parse(s).map_err(|_| DecodeError("asset id"))?;
        // Only the normalised spelling is canonical (`REQ-AS-3`).
        if id.as_str() != s {
            return Err(DecodeError("asset id not normalised"));
        }
        Ok(id)
    }
}

impl Canonical for AssetLocation {
    fn encode(&self, e: &mut Enc) {
        e.bytes(self.as_str().as_bytes());
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        let raw = d.bytes()?;
        let s = core::str::from_utf8(raw).map_err(|_| DecodeError("asset location"))?;
        let l = AssetLocation::parse(s).map_err(|_| DecodeError("asset location"))?;
        // Only the normalised spelling is canonical (`REQ-AS-3`).
        if l.as_str() != s {
            return Err(DecodeError("asset location not normalised"));
        }
        Ok(l)
    }
}

tagged!(AssetKind { Fiat => 1, Stable => 2, Native => 3, Other => 4 });
tagged!(AssetStatus { Active => 1, Deprecated => 2 });

impl Canonical for Minor {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.get());
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Minor::new(d.get()?))
    }
}

impl Canonical for NonZeroMinor {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.minor());
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        NonZeroMinor::new(d.get()?).map_err(|_| DecodeError("non-zero amount"))
    }
}

impl<T: Canonical + Ord> Canonical for NonEmptySet<T> {
    fn encode(&self, e: &mut Enc) {
        e.put(self.as_set());
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        NonEmptySet::new(d.get::<BTreeSet<T>>()?).ok_or(DecodeError("empty set"))
    }
}

// ------------------------------------------------------------------ the kernel's own types

impl Canonical for AssetTag {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.0);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(AssetTag(d.get()?))
    }
}

tagged!(Role { LedgerAccount => 1, Issuance => 2, Holding => 3 });
tagged!(BalanceRule { NeverNegative => 1, Unchecked => 2 });

impl Canonical for PostingKind {
    fn encode(&self, e: &mut Enc) {
        e.tag(self.code());
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        PostingKind::from_code(d.tag()?).ok_or(DecodeError("posting kind"))
    }
}

impl Canonical for KernelAccountSpec {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.role)
            .put(&self.subject)
            .put(&self.asset)
            .put(&self.rule)
            .put(&self.home.map(|p| p.0));
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        let spec = KernelAccountSpec::new(d.get()?, d.get()?, d.get()?, d.get()?);
        let home: Option<u16> = d.get()?;
        let placed = match home {
            Some(p) => spec.in_partition(ledger_kernel::partition::Partition(p)),
            None => spec,
        };
        // A placement in the subject's own partition is written as none: refuse the other
        // spelling, so one spec has one encoding.
        if placed.home.map(|p| p.0) != home {
            return Err(DecodeError("kernel account placed in its own partition"));
        }
        Ok(placed)
    }
}

// ------------------------------------------------------------------ identities and accounts

tagged!(IdentityKind { Person => 1, Organisation => 2, Agent => 3 });
tagged!(IdentityStatus { Active => 1, Restricted => 2, Closed => 3 });

impl Canonical for Identity {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.minted())
            .put(&self.kind())
            .put(&self.acts_for())
            .put(&self.status());
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        let minted: Mint = d.get()?;
        let kind = d.get()?;
        let acts_for = d.get()?;
        let status = d.get()?;
        Ok(Identity::new(minted, kind, acts_for)
            .map_err(|_| DecodeError("identity"))?
            .with_status(status))
    }
}

tagged!(AccountKind { Customer => 1, Treasury => 2, Issuance => 3, Suspense => 4 });
tagged!(Custody { Delegated => 1, Owned => 2 });
tagged!(AccountStatus { Active => 1, Frozen => 2, Closing => 3, Closed => 4 });

impl Canonical for LedgerAccount {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.minted())
            .put(&self.owner())
            .put(&self.kind())
            .put(&self.custody())
            .put(&self.status())
            .put(&self.opened_at())
            .put(self.treasury_limits());
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        let minted = d.get()?;
        let owner = d.get()?;
        let kind = d.get()?;
        let custody = d.get()?;
        let status = d.get()?;
        let opened_at = d.get()?;
        let limits = d.get()?;
        Ok(
            LedgerAccount::new(minted, owner, kind, custody, opened_at, limits)
                .map_err(|_| DecodeError("ledger account"))?
                .with_status(status),
        )
    }
}

// ------------------------------------------------------------------ credentials and authority

tagged!(CredentialMarker { Ordinary => 1, RecoveryHeld => 2, Imported => 3 });
tagged!(CredentialStatus { Active => 1, Removed => 2 });

/// Public material only (`REQ-AU-8`): a verifier holds no secret, and its bytes are what the
/// boundary checks proofs against. Its scheme's tag first (`DEC-19`, format 7); a tag this build
/// does not know, or material not well formed for its scheme, does not decode.
impl Canonical for Verifier {
    fn encode(&self, e: &mut Enc) {
        e.tag(self.scheme().tag()).bytes(self.as_bytes());
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        let scheme = Scheme::from_tag(d.tag()?).ok_or(DecodeError("credential scheme"))?;
        let v = Verifier::of(scheme, d.bytes()?.to_vec());
        if !v.well_formed() {
            return Err(DecodeError("verifier not well formed for its scheme"));
        }
        Ok(v)
    }
}

impl Canonical for Credential {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.minted())
            .put(&self.identity())
            .put(&self.marker())
            .put(&self.scope().cloned())
            .put(&self.status())
            .put(self.verifier());
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        let minted = d.get()?;
        let identity = d.get()?;
        let marker = d.get()?;
        let scope = d.get()?;
        let status = d.get()?;
        let verifier = d.get()?;
        Ok(Credential::new(minted, identity, marker, scope, verifier).with_status(status))
    }
}

tagged!(OperationKind {
    Transfer => 1,
    Reserve => 2,
    Capture => 3,
    Release => 4,
    Convert => 5,
    Encumber => 6,
    Disencumber => 7,
    Issue => 8,
    Redeem => 9,
    CloseAccount => 10,
    Disclose => 11,
    Reversal => 12,
    OpenAccount => 13,
});
tagged!(GrantKind { Delegated => 1, Operator => 2, Issuance => 3 });
tagged!(AuthorityKind { Owner => 1, Delegated => 2, Operator => 3, Issuance => 4, Recovery => 5 });

impl Canonical for Subject {
    fn encode(&self, e: &mut Enc) {
        match self {
            Subject::Account(a) => {
                e.tag(1).put(a);
            }
            Subject::DelegatedAccounts => {
                e.tag(2);
            }
            Subject::Asset(a) => {
                e.tag(3).put(a);
            }
            Subject::Identity(i) => {
                e.tag(4).put(i);
            }
        }
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        match d.tag()? {
            1 => Ok(Subject::Account(d.get()?)),
            2 => Ok(Subject::DelegatedAccounts),
            3 => Ok(Subject::Asset(d.get()?)),
            4 => Ok(Subject::Identity(d.get()?)),
            _ => Err(DecodeError("subject")),
        }
    }
}

impl Canonical for Window {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.length).put(&self.limit);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Window {
            length: d.get()?,
            limit: d.get()?,
        })
    }
}

impl Canonical for Bounds {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.per_operation())
            .put(&self.per_window())
            .put(&self.assets().cloned())
            .put(&self.counterparties().cloned());
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Bounds::new(d.get()?, d.get()?, d.get()?, d.get()?).map_err(|_| DecodeError("bounds"))
    }
}

impl Canonical for Grant {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.minted())
            .put(&self.grantor())
            .put(&self.kind())
            .put(&self.principal())
            .put(&self.subject())
            .put(self.ops())
            .put(self.bounds())
            .put(&self.parent())
            .put(&self.granted_at())
            .put(&self.expires())
            .put(&self.revoked());
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        let minted = d.get()?;
        let grantor = d.get()?;
        let terms = GrantTerms {
            kind: d.get()?,
            principal: d.get()?,
            subject: d.get()?,
            ops: d.get()?,
            bounds: d.get()?,
            parent: d.get()?,
            expires: None,
        };
        let granted_at = d.get()?;
        let expires = d.get()?;
        let revoked = d.get()?;
        let terms = GrantTerms { expires, ..terms };
        Ok(Grant::new(minted, grantor, terms, granted_at).with_revoked(revoked))
    }
}

impl Canonical for WindowKey {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.grant).put(&self.window).put(&self.asset);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(WindowKey {
            grant: d.get()?,
            window: d.get()?,
            asset: d.get()?,
        })
    }
}

impl Canonical for AuthorityUsed {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.kind)
            .put(&self.grant)
            .put(&self.principal)
            .put(&self.credential)
            .put(&self.on_behalf_of);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(AuthorityUsed {
            kind: d.get()?,
            grant: d.get()?,
            principal: d.get()?,
            credential: d.get()?,
            on_behalf_of: d.get()?,
        })
    }
}

// ------------------------------------------------------------------ attribution records

tagged!(Command {
    Genesis => 1,
    OpenIdentity => 2,
    SetIdentityStatus => 3,
    OpenAccount => 4,
    CloseAccount => 5,
    GrantAuthority => 6,
    RevokeAuthority => 7,
    Issue => 8,
    Redeem => 9,
    Reserve => 10,
    Capture => 11,
    Release => 12,
    GrantOperator => 13,
    RevokeOperator => 14,
    ResolveOperation => 15,
    Resume => 16,
    ProposeReversal => 17,
    ApproveReversal => 18,
    FreezeAccount => 19,
    UnfreezeAccount => 20,
    RegisterAsset => 21,
    DeprecateAsset => 22,
    GrantIssuance => 23,
    RevokeIssuance => 24,
    DeclareAdapterSupport => 25,
    Transfer => 26,
    Withdraw => 27,
    SubmitOutbound => 28,
    ConfigureSettlement => 29,
    RegisterCredential => 30,
    RemoveCredential => 31,
    DelegateCustody => 32,
    ReclaimCustody => 33,
    InitiateRecovery => 34,
    CancelRecovery => 35,
    CompleteRecovery => 36,
    NameSuccessor => 37,
    ClaimSuccession => 38,
    ImportBook => 39,
    LoadOpeningBalances => 40,
    AnchorCheckpoint => 41,
    CompareSettlement => 42,
    IssueDisclosure => 43,
    SubmitEvidence => 44,
    ImportCredentials => 45,
    RegisterIntegratingParty => 46,
    TiePrincipal => 47,
    BindAsset => 48,
    ApplyChainSpec => 49,
    RegisterAlias => 50,
    RescheduleUpgrade => 51,
    CancelUpgrade => 52,
});

impl Canonical for OperationRecord {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.op)
            .put(&self.command)
            .put(&self.authorities)
            .put(&self.accounts)
            .put(&self.postings)
            .put(&self.at);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(OperationRecord {
            op: d.get()?,
            command: d.get()?,
            authorities: d.get()?,
            accounts: d.get()?,
            postings: d.get()?,
            at: d.get()?,
        })
    }
}

impl Canonical for Roles {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.operator).put(&self.issuance);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Roles {
            operator: d.get()?,
            issuance: d.get()?,
        })
    }
}

// ------------------------------------------------------------------ reservations

impl Canonical for Position {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.account).put(&self.spec);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Position {
            account: d.get()?,
            spec: d.get()?,
        })
    }
}

impl Canonical for Reservation {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.op)
            .put(&self.payer)
            .put(&self.asset)
            .put(&self.holding)
            .put(&self.amount);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Reservation {
            op: d.get()?,
            payer: d.get()?,
            asset: d.get()?,
            holding: d.get()?,
            amount: d.get()?,
        })
    }
}

impl Canonical for Closing {
    fn encode(&self, e: &mut Enc) {
        match self {
            Closing::Captured {
                destination,
                cleared,
            } => {
                e.tag(1).put(destination).put(cleared);
            }
            Closing::Released => {
                e.tag(2);
            }
            Closing::SettledOutbound => {
                e.tag(3);
            }
        }
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        match d.tag()? {
            1 => Ok(Closing::Captured {
                destination: d.get()?,
                cleared: d.get()?,
            }),
            2 => Ok(Closing::Released),
            3 => Ok(Closing::SettledOutbound),
            _ => Err(DecodeError("closing")),
        }
    }
}

impl Canonical for Status {
    fn encode(&self, e: &mut Enc) {
        match self {
            Status::Open(r) => {
                e.tag(1).put(r);
            }
            Status::Closed(r, c) => {
                e.tag(2).put(r).put(c);
            }
        }
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        match d.tag()? {
            1 => Ok(Status::Open(d.get()?)),
            2 => Ok(Status::Closed(d.get()?, d.get()?)),
            _ => Err(DecodeError("reservation status")),
        }
    }
}

// ------------------------------------------------------------------ the operation lifecycle

/// The lifecycle's enums carry stable codes of their own; they are written as those.
macro_rules! coded {
    ($($t:ty),+) => {$(
        impl Canonical for $t {
            fn encode(&self, e: &mut Enc) {
                e.tag(self.code());
            }
            fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
                let c = d.tag()?;
                <$t>::ALL
                    .into_iter()
                    .find(|v| v.code() == c)
                    .ok_or(DecodeError(stringify!($t)))
            }
        }
    )+};
}

coded!(OpState, Trigger, Origin, KindClass, OperatorOpKind);

impl Canonical for Lifecycle {
    fn encode(&self, e: &mut Enc) {
        match (self.deadline(), self.closed_at()) {
            (Some(deadline), _) => {
                e.tag(1).put(&self.state()).put(&deadline);
            }
            (None, Some(at)) => {
                e.tag(2).put(&self.state()).put(&at);
            }
            // Every lifecycle is open with a deadline or closed at a time.
            (None, None) => {
                e.tag(0);
            }
        }
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        match d.tag()? {
            1 => Lifecycle::open(d.get()?, Some(d.get()?)).map_err(|_| DecodeError("lifecycle")),
            2 => Lifecycle::closed(d.get()?, d.get()?).map_err(|_| DecodeError("lifecycle")),
            _ => Err(DecodeError("lifecycle")),
        }
    }
}

tagged!(StateClass { Terminal => 1, OpenNonRefunding => 2, AwaitingHuman => 3 });

impl Canonical for Shown {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.state).put(&self.class);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        let state: OpState = d.get()?;
        let class: StateClass = d.get()?;
        let shown = Shown::of(state);
        if shown.class != class {
            return Err(DecodeError("shown class"));
        }
        Ok(shown)
    }
}

macro_rules! digest32 {
    ($($t:ident),+) => {$(
        impl Canonical for $t {
            fn encode(&self, e: &mut Enc) {
                e.raw(&self.0);
            }
            fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
                Ok($t(d.array()?))
            }
        }
    )+};
}

digest32!(TransitionKey, Evidence, NotificationId, RequestDigest);

tagged!(Surface { Command => 1, Operator => 2 });

impl Canonical for ClientKey {
    fn encode(&self, e: &mut Enc) {
        e.bytes(self.as_bytes());
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(ClientKey::new(d.bytes()?.to_vec()))
    }
}

impl Canonical for Refusal {
    fn encode(&self, e: &mut Enc) {
        e.bytes(self.code().as_bytes());
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        let s = core::str::from_utf8(d.bytes()?).map_err(|_| DecodeError("refusal"))?;
        Refusal::from_code(s).ok_or(DecodeError("refusal"))
    }
}

impl Canonical for TransitionRecord {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.key)
            .put(&self.from)
            .put(&self.to)
            .put(&self.trigger)
            .put(&self.evidence)
            .put(&self.posting)
            .put(&self.postings)
            .put(&self.authorities)
            .put(&self.at);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(TransitionRecord {
            key: d.get()?,
            from: d.get()?,
            to: d.get()?,
            trigger: d.get()?,
            evidence: d.get()?,
            posting: d.get()?,
            postings: d.get()?,
            authorities: d.get()?,
            at: d.get()?,
        })
    }
}

impl Canonical for Notification {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.id)
            .put(&self.op)
            .put(&self.shown)
            .put(&self.outcome)
            .put(&self.refusal)
            .put(&self.anchor);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Notification {
            id: d.get()?,
            op: d.get()?,
            shown: d.get()?,
            outcome: d.get()?,
            refusal: d.get()?,
            anchor: d.get()?,
        })
    }
}

impl Canonical for RequestEntry {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.op)
            .put(&self.initiator)
            .put(&self.surface)
            .put(&self.digest)
            .put(&self.reply);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(RequestEntry {
            op: d.get()?,
            initiator: d.get()?,
            surface: d.get()?,
            digest: d.get()?,
            reply: d.get()?,
        })
    }
}

// ------------------------------------------------------------------ the settlement store

impl Canonical for SourceId {
    fn encode(&self, e: &mut Enc) {
        e.raw(&self.0);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(SourceId(d.array()?))
    }
}

impl Canonical for Coordinate {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.instance).put(&self.seq).put(&self.digest);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Coordinate {
            instance: d.get()?,
            seq: d.get()?,
            digest: d.get()?,
        })
    }
}

tagged!(EventKind { Deposited => 1, Withdrawn => 2, WithdrawRefused => 3, Anchored => 4, AnchorRefused => 5, Bound => 6, BindRefused => 7 });
tagged!(ObservedFinality { Provisional => 1, Final => 2 });

impl Canonical for Payload {
    fn encode(&self, e: &mut Enc) {
        e.bytes(&self.0);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Payload(d.bytes()?.to_vec()))
    }
}

impl Canonical for Movement {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.asset).put(&self.amount);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Movement {
            asset: d.get()?,
            amount: d.get()?,
        })
    }
}

impl Canonical for Observation {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.source)
            .put(&self.coordinate)
            .put(&self.at)
            .put(&self.kind)
            .put(&self.payload)
            .put(&self.observed_finality)
            .put(&self.operation_ref)
            .put(&self.account_ref)
            .put(&self.movement)
            .put(&self.anchor);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Observation {
            source: d.get()?,
            coordinate: d.get()?,
            at: d.get()?,
            kind: d.get()?,
            payload: d.get()?,
            observed_finality: d.get()?,
            operation_ref: d.get()?,
            account_ref: d.get()?,
            movement: d.get()?,
            anchor: d.get()?,
        })
    }
}

impl Canonical for AnchorFact {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.number).put(&self.commitment).put(&self.venue);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(AnchorFact {
            number: d.get()?,
            commitment: d.get()?,
            venue: d.get()?,
        })
    }
}

impl Canonical for Venue {
    fn encode(&self, e: &mut Enc) {
        match self {
            Venue::Pallet { instance } => {
                e.tag(1).put(instance);
            }
            Venue::Contract { account } => {
                e.tag(2).put(account);
            }
        }
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        match d.tag()? {
            1 => Ok(Venue::Pallet { instance: d.get()? }),
            2 => Ok(Venue::Contract { account: d.get()? }),
            _ => Err(DecodeError("venue")),
        }
    }
}

// ------------------------------------------------------------------ the anchor store (F-017)

tagged!(CheckpointStatus { Pending => 1, Submitted => 2, Final => 3 });

impl Canonical for Checkpoint {
    fn encode(&self, e: &mut Enc) {
        let Checkpoint {
            number,
            instance,
            entry_sequence,
            kernel_root,
            construction,
            binding,
            commitment,
            created_at,
            status,
            coordinate,
            venue,
            height,
        } = self;
        e.put(number)
            .put(instance)
            .put(entry_sequence)
            .put(kernel_root)
            .put(construction)
            .put(binding)
            .put(commitment)
            .put(created_at)
            .put(status)
            .put(coordinate)
            .put(venue)
            .put(height);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Checkpoint {
            number: d.get()?,
            instance: d.get()?,
            entry_sequence: d.get()?,
            kernel_root: d.get()?,
            construction: d.get()?,
            binding: d.get()?,
            commitment: d.get()?,
            created_at: d.get()?,
            status: d.get()?,
            coordinate: d.get()?,
            venue: d.get()?,
            height: d.get()?,
        })
    }
}

impl Canonical for crate::anchor::CodeBinding {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.genesis).put(&self.code).put(&self.authority);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(crate::anchor::CodeBinding {
            genesis: d.get()?,
            code: d.get()?,
            authority: d.get()?,
        })
    }
}

impl Canonical for AnchorIncident {
    fn encode(&self, e: &mut Enc) {
        match self {
            AnchorIncident::CommitmentMismatch {
                number,
                expected,
                observed,
                coordinate,
            } => {
                e.tag(1)
                    .put(number)
                    .put(expected)
                    .put(observed)
                    .put(coordinate);
            }
            AnchorIncident::UnknownAnchor {
                number,
                commitment,
                coordinate,
            } => {
                e.tag(2).put(number).put(commitment).put(coordinate);
            }
            AnchorIncident::PositionDifference {
                cut,
                asset,
                book,
                layer,
            } => {
                e.tag(3).put(cut).put(asset).put(book).put(layer);
            }
        }
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(match d.tag()? {
            1 => AnchorIncident::CommitmentMismatch {
                number: d.get()?,
                expected: d.get()?,
                observed: d.get()?,
                coordinate: d.get()?,
            },
            2 => AnchorIncident::UnknownAnchor {
                number: d.get()?,
                commitment: d.get()?,
                coordinate: d.get()?,
            },
            3 => AnchorIncident::PositionDifference {
                cut: d.get()?,
                asset: d.get()?,
                book: d.get()?,
                layer: d.get()?,
            },
            _ => return Err(DecodeError("AnchorIncident")),
        })
    }
}

impl Canonical for PositionRow {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.asset).put(&self.book).put(&self.layer);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(PositionRow {
            asset: d.get()?,
            book: d.get()?,
            layer: d.get()?,
        })
    }
}

impl Canonical for Comparison {
    fn encode(&self, e: &mut Enc) {
        let Comparison {
            cut,
            height,
            at,
            anchors,
            requested,
            rows,
        } = self;
        e.put(cut)
            .put(height)
            .put(at)
            .put(anchors)
            .put(requested)
            .put(rows);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Comparison {
            cut: d.get()?,
            height: d.get()?,
            at: d.get()?,
            anchors: d.get()?,
            requested: d.get()?,
            rows: d.get()?,
        })
    }
}

impl Canonical for FinalisedHead {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.height).put(&self.next_seq);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(FinalisedHead {
            height: d.get()?,
            next_seq: d.get()?,
        })
    }
}

impl Canonical for SupportDeclaration {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.legs);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(SupportDeclaration { legs: d.get()? })
    }
}

impl Canonical for Destination {
    fn encode(&self, e: &mut Enc) {
        match self {
            Destination::Account(a) => {
                e.tag(1).put(a);
            }
            Destination::Instance {
                instance,
                account_ref,
            } => {
                e.tag(2).put(instance).put(account_ref);
            }
        }
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        match d.tag()? {
            1 => Ok(Destination::Account(d.get()?)),
            2 => Ok(Destination::Instance {
                instance: d.get()?,
                account_ref: d.get()?,
            }),
            _ => Err(DecodeError("destination")),
        }
    }
}

impl Canonical for SettlementConfig {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.authoritative)
            .put(&self.instance)
            .put(&self.suspense)
            .put(&self.inbound_deadline)
            .put(&self.expiry_window)
            .put(&self.block_time)
            .put(&self.finality_margin);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(SettlementConfig {
            authoritative: d.get()?,
            instance: d.get()?,
            suspense: d.get()?,
            inbound_deadline: d.get()?,
            expiry_window: d.get()?,
            block_time: d.get()?,
            finality_margin: d.get()?,
        })
    }
}

tagged!(GapReason { Pruned => 1, Unreadable => 2 });

impl Canonical for Gap {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.from).put(&self.to).put(&self.reason);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Gap {
            from: d.get()?,
            to: d.get()?,
            reason: d.get()?,
        })
    }
}

impl Canonical for SetIncident {
    fn encode(&self, e: &mut Enc) {
        match self {
            SetIncident::AmountMismatch {
                op,
                reserved,
                executed,
            } => {
                e.tag(1).put(op).put(reserved).put(executed);
            }
            SetIncident::Unassociated { coordinate } => {
                e.tag(2).put(coordinate);
            }
            SetIncident::Unusable { coordinate } => {
                e.tag(3).put(coordinate);
            }
            SetIncident::LateEvidence { op, coordinate } => {
                e.tag(4).put(op).put(coordinate);
            }
            SetIncident::FinalFailure { op, coordinate } => {
                e.tag(5).put(op).put(coordinate);
            }
            SetIncident::SettlementRefused { op, refusal } => {
                e.tag(6).put(op).put(refusal);
            }
            SetIncident::Gap(g) => {
                e.tag(7).put(g);
            }
            SetIncident::Missing { seq, digest } => {
                e.tag(8).put(seq).put(digest);
            }
            SetIncident::Unknown { coordinate } => {
                e.tag(9).put(coordinate);
            }
            SetIncident::IncludedWithoutRecord { op, key, nonce } => {
                e.tag(10).put(op).put(key).put(nonce);
            }
            SetIncident::BindRefused { asset, coordinate } => {
                e.tag(11).put(asset).put(coordinate);
            }
        }
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(match d.tag()? {
            1 => SetIncident::AmountMismatch {
                op: d.get()?,
                reserved: d.get()?,
                executed: d.get()?,
            },
            2 => SetIncident::Unassociated {
                coordinate: d.get()?,
            },
            3 => SetIncident::Unusable {
                coordinate: d.get()?,
            },
            4 => SetIncident::LateEvidence {
                op: d.get()?,
                coordinate: d.get()?,
            },
            5 => SetIncident::FinalFailure {
                op: d.get()?,
                coordinate: d.get()?,
            },
            6 => SetIncident::SettlementRefused {
                op: d.get()?,
                refusal: d.get()?,
            },
            7 => SetIncident::Gap(d.get()?),
            8 => SetIncident::Missing {
                seq: d.get()?,
                digest: d.get()?,
            },
            9 => SetIncident::Unknown {
                coordinate: d.get()?,
            },
            10 => SetIncident::IncludedWithoutRecord {
                op: d.get()?,
                key: d.get()?,
                nonce: d.get()?,
            },
            11 => SetIncident::BindRefused {
                asset: d.get()?,
                coordinate: d.get()?,
            },
            _ => return Err(DecodeError("settlement incident")),
        })
    }
}

tagged!(Standing { Provisional => 1, Final => 2, Orphaned => 3 });

impl Canonical for Ingested {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.observation).put(&self.op).put(&self.standing);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Ingested {
            observation: d.get()?,
            op: d.get()?,
            standing: d.get()?,
        })
    }
}

impl Canonical for Signing {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.key).put(&self.nonce).put(&self.last_valid);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Signing {
            key: d.get()?,
            nonce: d.get()?,
            last_valid: d.get()?,
        })
    }
}

impl Canonical for SigningRecord {
    fn encode(&self, e: &mut Enc) {
        match self {
            SigningRecord::Signed(s) => {
                e.tag(1).put(s);
            }
            SigningRecord::NeverSigned => {
                e.tag(2);
            }
        }
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        match d.tag()? {
            1 => Ok(SigningRecord::Signed(d.get()?)),
            2 => Ok(SigningRecord::NeverSigned),
            _ => Err(DecodeError("signing record")),
        }
    }
}

impl Canonical for NonceFact {
    fn encode(&self, e: &mut Enc) {
        match self {
            NonceFact::Consumed { height } => {
                e.tag(1).put(height);
            }
            NonceFact::Unconsumed { height } => {
                e.tag(2).put(height);
            }
        }
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        match d.tag()? {
            1 => Ok(NonceFact::Consumed { height: d.get()? }),
            2 => Ok(NonceFact::Unconsumed { height: d.get()? }),
            _ => Err(DecodeError("nonce fact")),
        }
    }
}

impl Canonical for Instructed {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.instruction)
            .put(&self.from_seq)
            .put(&self.acknowledged)
            .put(&self.signing)
            .put(&self.nonce);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Instructed {
            instruction: d.get()?,
            from_seq: d.get()?,
            acknowledged: d.get()?,
            signing: d.get()?,
            nonce: d.get()?,
        })
    }
}

// ------------------------------------------------------------------ operator records

impl Canonical for PendingReversal {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.proposal)
            .put(&self.op)
            .put(&self.from)
            .put(&self.reverses)
            .put(&self.proposer)
            .put(&self.proposed_at)
            .put(&self.approval);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(PendingReversal {
            proposal: d.get()?,
            op: d.get()?,
            from: d.get()?,
            reverses: d.get()?,
            proposer: d.get()?,
            proposed_at: d.get()?,
            approval: d.get()?,
        })
    }
}

tagged!(IncidentKind {
    ReversalProposed => 1,
    AuditGap => 2,
    ResolvedWithoutProof => 3,
});
tagged!(Finding { Unprovable => 1 });
tagged!(Resolution { Settle => 1, Release => 2 });

impl Canonical for EvidenceSubmission {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.id)
            .put(&self.op)
            .put(&self.block)
            .put(&self.transaction)
            .put(&self.by)
            .put(&self.at)
            .put(&self.finding);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(EvidenceSubmission {
            id: d.get()?,
            op: d.get()?,
            block: d.get()?,
            transaction: d.get()?,
            by: d.get()?,
            at: d.get()?,
            finding: d.get()?,
        })
    }
}

impl Canonical for ProposedResolution {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.target).put(&self.by).put(&self.at);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(ProposedResolution {
            target: d.get()?,
            by: d.get()?,
            at: d.get()?,
        })
    }
}

impl Canonical for ResolutionCase {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.op).put(&self.evidence).put(&self.proposed);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(ResolutionCase {
            op: d.get()?,
            evidence: d.get()?,
            proposed: d.get()?,
        })
    }
}

impl Canonical for OpsIncident {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.id).put(&self.kind).put(&self.op).put(&self.at);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(OpsIncident {
            id: d.get()?,
            kind: d.get()?,
            op: d.get()?,
            at: d.get()?,
        })
    }
}

// ------------------------------------------------------------------ the posting journal

tagged!(BalanceKind { Available => 1, Reserved => 2, Encumbered => 3 });
tagged!(Direction { Debit => 1, Credit => 2 });

impl Canonical for Line {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.account)
            .put(&self.kind)
            .put(&self.asset)
            .put(&self.side)
            .put(&self.amount);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Line {
            account: d.get()?,
            kind: d.get()?,
            asset: d.get()?,
            side: d.get()?,
            amount: d.get()?,
        })
    }
}

impl Canonical for PostingEntry {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.seq)
            .put(&self.at)
            .put(&self.operation)
            .put(&self.kind)
            .put(&self.ordinal)
            .put(&self.lines);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(PostingEntry {
            seq: d.get()?,
            at: d.get()?,
            operation: d.get()?,
            kind: d.get()?,
            ordinal: d.get()?,
            lines: d.get::<Vec<Line>>()?,
        })
    }
}

// ------------------------------------------------------------------ custody records (`F-012`)

impl Canonical for PendingId {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.0);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(PendingId(d.get()?))
    }
}

tagged!(ChangeKind { Recovery => 1, Succession => 2 });
tagged!(ChangeState { Pending => 1, Cancelled => 2, Completed => 3 });

/// The verifier of a pending recovery is public material, like every verifier (`REQ-AU-8`).
impl Canonical for ChangeTarget {
    fn encode(&self, e: &mut Enc) {
        match self {
            ChangeTarget::Credential { verifier, scope } => {
                e.tag(1).put(verifier).put(scope);
            }
            ChangeTarget::Successor(i) => {
                e.tag(2).put(i);
            }
        }
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        match d.tag()? {
            1 => Ok(ChangeTarget::Credential {
                verifier: d.get()?,
                scope: d.get()?,
            }),
            2 => Ok(ChangeTarget::Successor(d.get()?)),
            _ => Err(DecodeError("change target")),
        }
    }
}

impl Canonical for Closed {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.at).put(&self.by);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Closed {
            at: d.get()?,
            by: d.get()?,
        })
    }
}

impl Canonical for PendingChange {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.id)
            .put(&self.kind)
            .put(&self.account)
            .put(&self.owner)
            .put(&self.initiated_by)
            .put(&self.initiated_at)
            .put(&self.effective_at)
            .put(&self.target)
            .put(&self.state)
            .put(&self.closed);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(PendingChange {
            id: d.get()?,
            kind: d.get()?,
            account: d.get()?,
            owner: d.get()?,
            initiated_by: d.get()?,
            initiated_at: d.get()?,
            effective_at: d.get()?,
            target: d.get()?,
            state: d.get()?,
            closed: d.get()?,
        })
    }
}

impl Canonical for NoticeId {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.0);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(NoticeId(d.get()?))
    }
}

impl Canonical for NoticeEvent {
    fn encode(&self, e: &mut Enc) {
        match self {
            NoticeEvent::CredentialRegistered(c) => {
                e.tag(1).put(c);
            }
            NoticeEvent::CredentialRemoved(c) => {
                e.tag(2).put(c);
            }
            NoticeEvent::CustodyDelegated => {
                e.tag(3);
            }
            NoticeEvent::CustodyReclaimed => {
                e.tag(4);
            }
            NoticeEvent::RecoveryInitiated {
                pending,
                effective_at,
            } => {
                e.tag(5).put(pending).put(effective_at);
            }
            NoticeEvent::SuccessionClaimed {
                pending,
                effective_at,
            } => {
                e.tag(6).put(pending).put(effective_at);
            }
            NoticeEvent::Cancelled(p) => {
                e.tag(7).put(p);
            }
            NoticeEvent::RecoveryCompleted(p) => {
                e.tag(8).put(p);
            }
            NoticeEvent::SuccessionCompleted(p) => {
                e.tag(9).put(p);
            }
            NoticeEvent::SuccessorNamed(i) => {
                e.tag(10).put(i);
            }
        }
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(match d.tag()? {
            1 => NoticeEvent::CredentialRegistered(d.get()?),
            2 => NoticeEvent::CredentialRemoved(d.get()?),
            3 => NoticeEvent::CustodyDelegated,
            4 => NoticeEvent::CustodyReclaimed,
            5 => NoticeEvent::RecoveryInitiated {
                pending: d.get()?,
                effective_at: d.get()?,
            },
            6 => NoticeEvent::SuccessionClaimed {
                pending: d.get()?,
                effective_at: d.get()?,
            },
            7 => NoticeEvent::Cancelled(d.get()?),
            8 => NoticeEvent::RecoveryCompleted(d.get()?),
            9 => NoticeEvent::SuccessionCompleted(d.get()?),
            10 => NoticeEvent::SuccessorNamed(d.get()?),
            _ => return Err(DecodeError("custody notice")),
        })
    }
}

impl Canonical for Notice {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.id)
            .put(&self.op)
            .put(&self.recipient)
            .put(&self.account)
            .put(&self.event)
            .put(&self.at);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Notice {
            id: d.get()?,
            op: d.get()?,
            recipient: d.get()?,
            account: d.get()?,
            event: d.get()?,
            at: d.get()?,
        })
    }
}
