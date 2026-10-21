// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The wire form of the query surface (`CTR-QRY`, SPEC §8.2): `F-011`'s requests, replies and
//! cursor (`F-011` NOTES, "F-009: the wire form").
//!
//! Queries are not core inputs: they never enter the received-log and change nothing
//! (`REQ-QR-3`). Ingress authenticates them, names the principal and the request id in its access
//! log (`REQ-PV-5`), and routes them to `F-011`'s entry points, which decide what the principal may
//! see (`INV-16`) and refuse an unbounded query (`ERR-QueryUnbounded`, `REQ-CS-5`).
//!
//! Balances are signed decimal-integer strings of minor units: a Treasury account may be below
//! zero. A cursor is opaque hex; a caller passes it back unchanged (`REQ-QR-5`).

use ledger_core::Refusal;
use ledger_core::account::Time;
use ledger_core::asset::{Asset, AssetBinding, AssetId, AssetKind, AssetStatus, Minor};
use ledger_core::authority::AuthorityUsed;
use ledger_core::id::{AccountId, IdentityId, OperationId};
use ledger_core::query::journal::{BalanceKind, Direction, Sums};
use ledger_core::query::movement::{Movement, MovementStatus};
use ledger_core::query::{
    Balances, Cursor, MovementFilter, MovementsRequest, OperationView, Page, Statement,
    StatementRequest, Subject, TimeRange, TrialBalance,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::wire::{OperationReply, ShownBody};

/// A count in a query string: digits only, and saturating, so that a count past what a `u64`
/// holds is still a count, and one past the bound (`ERR-QueryUnbounded`), not a shape fault.
/// Anything that is not made of digits does not parse (`ERR-MalformedCommand`).
fn count<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    let Some(s) = Option::<String>::deserialize(d)? else {
        return Ok(None);
    };
    if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
        return Err(serde::de::Error::custom("not a count"));
    }
    Ok(Some(s.parse::<u64>().unwrap_or(u64::MAX)))
}

/// Reads the balances of one subject, from one view. Name exactly one subject: `account` for one
/// account, or `identity` for every account of that identity the caller reaches. Naming both, or
/// neither, is refused as `ERR-QueryUnbounded`. `asset` narrows the answer to one asset; without it, a row
/// comes back for every asset the account has held. A parameter this route does not know, or an
/// `account`, `identity` or `asset` that is not a valid identifier or symbol, is refused as `ERR-MalformedCommand`
/// (amendment 0032, `REQ-ER-3`), never ignored.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BalancesParams {
    /// The subject: one account, as a `did:aledgent:acct:` identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// The subject: one identity, as a `did:aledgent:id:` identifier. Every account of it the
    /// caller reaches is read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// One asset, by its symbol; every asset held if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
}

/// Lists the entries made against a subject's accounts, oldest first, one page at a time. Name
/// exactly one subject: `account` or `identity`. Naming both, or neither, is refused as
/// `ERR-QueryUnbounded`. `limit` is required, from 1 to 1000; a
/// request without it, or past it, is refused the same way. `from` and `to` come together, in
/// milliseconds since the Unix epoch: `from` is inclusive and `to` is exclusive, and `from` may
/// not be after `to`. A parameter this route does not know, a value that does not parse
/// (`limit=ten`), a `direction` that is neither `debit` nor `credit`, an account, identity or
/// asset that is not a valid identifier or symbol, and a `cursor` that is not hex are refused as
/// `ERR-MalformedCommand` (amendment 0032, `REQ-ER-3`), never ignored. A page may hold fewer movements than `limit` and still carry `next`: keep
/// passing `next` back as `cursor` until a page comes without one.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MovementsParams {
    /// The subject: one account, as a `did:aledgent:acct:` identifier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// The subject: one identity, as a `did:aledgent:id:` identifier. Every account of it the
    /// caller reaches is read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// Only movements on this account, which must be one of the subject's (useful when the
    /// subject is an identity).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_account: Option<String>,
    /// Only this asset, by its symbol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    /// Only `debit` or `credit`; any other value is refused as `ERR-MalformedCommand`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    /// Only against this counterparty: the ledger account on the other side of the movement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counterparty: Option<String>,
    /// From this time, in milliseconds since the Unix epoch. Inclusive: a movement at exactly
    /// this time is returned. Send it together with `to`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<Time>,
    /// Up to this time, in milliseconds since the Unix epoch. Exclusive: a movement at exactly
    /// this time is not returned. Send it together with `from`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<Time>,
    /// The page size, from 1 to 1000. Required: an unbounded page is refused. Any value made of
    /// digits is a count, and one above 1000 is outside the bound (`ERR-QueryUnbounded`),
    /// however many digits it has; a value that is not made of digits is a shape fault
    /// (`ERR-MalformedCommand`).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "count"
    )]
    pub limit: Option<u64>,
    /// The previous page's `next`, passed back unchanged. It belongs to the request that made
    /// it: a cursor sent with other filters is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
}

/// Reads one account's statement in one asset for a period, with its opening and closing
/// balances. `account`, `asset`, `from` and `to` are all required; without any of them the request
/// is refused as `ERR-QueryUnbounded`, and so is `from` after `to`, an `as_of` the book has not
/// reached, and a period of more than 1000 movements (ask for a shorter one, or page through
/// `movements`). A parameter this route does not know, a value that does not parse (`from=abc`)
/// and an `account` or `asset` that is not a valid identifier or symbol are refused as `ERR-MalformedCommand`
/// (amendment 0032, `REQ-ER-3`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatementParams {
    /// The account, as a `did:aledgent:acct:` identifier. Required: a missing one is
    /// `ERR-QueryUnbounded` (`REQ-CS-5`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    /// The asset, by its symbol. Required, as `account` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    /// The period's start, in milliseconds since the Unix epoch. Inclusive. Required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<Time>,
    /// The period's end, in milliseconds since the Unix epoch. Exclusive. Required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<Time>,
    /// Read at this view, for a statement that must not move: the `view` of an earlier balances
    /// or movements read. A view the book has not reached is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub as_of: Option<u64>,
}

/// The query string of `trial_balance` and `can_spend`. `trial_balance` reads only `asset`, which
/// narrows the check to one asset; it accepts `amount` and ignores it. `can_spend` needs both. Any
/// other parameter, an `asset` that is not a valid symbol and an `amount` that is not a count of
/// minor units are refused as `ERR-MalformedCommand` (amendment 0032, `REQ-ER-3`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AssetParams {
    /// The asset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset: Option<String>,
    /// For `can_spend`: the amount, in minor units.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub amount: Option<String>,
}

/// One balance row: each balance kind labelled, and their total (`REQ-BA-2`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BalanceRowBody {
    /// The account.
    pub account: String,
    /// The asset.
    pub asset: String,
    /// Available, minor units.
    pub available: String,
    /// Reserved, minor units.
    pub reserved: String,
    /// Encumbered, minor units (zero in V0).
    pub encumbered: String,
    /// Their sum.
    pub total: String,
}

/// `balances` (SPEC §8.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BalancesBody {
    /// The view it was read at: the posting sequence number (`REQ-QR-2`).
    pub view: u64,
    /// The rows.
    pub rows: Vec<BalanceRowBody>,
}

/// An authority a posting relied on, and who used it (`REQ-AU-7`, `INV-20`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AuthorityBody {
    /// `owner`, `delegated`, `operator`, `issuance` or `recovery`.
    pub kind: String,
    /// The grant, where one was used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<String>,
    /// The principal that used it.
    pub principal: String,
    /// The credential it presented.
    pub credential: String,
    /// Whom it acted for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_behalf_of: Option<String>,
}

/// One movement: one entry against one account.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct MovementBody {
    /// The posting's sequence number.
    pub seq: u64,
    /// The posting's time.
    pub at: Time,
    /// The posting's kind.
    pub kind: String,
    /// `held`, `completed`, `returned`, `reversed` or `opening`.
    pub status: String,
    /// The operation.
    pub operation: String,
    /// The account.
    pub account: String,
    /// `available`, `reserved` or `encumbered`.
    pub balance: String,
    /// The asset.
    pub asset: String,
    /// `debit` or `credit`.
    pub direction: String,
    /// Minor units.
    pub amount: String,
    /// The ledger account on the other side, where there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counterparty: Option<String>,
    /// The authorities that caused it.
    pub authorities: Vec<AuthorityBody>,
    /// The pass-through data its operation was submitted with (`REQ-PO-5`), exactly as sent.
    /// Shown only to principals of the submitting principal's integrating party: another party
    /// reading the same movement never sees it (`AC-M8.4`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<crate::wire::PassThrough>,
}

/// A page of movements (`REQ-QR-5`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PageBody {
    /// The view it was read at.
    pub view: u64,
    /// The movements.
    pub movements: Vec<MovementBody>,
    /// The cursor of the next page, hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// Balances by kind, minor units.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SumsBody {
    /// Available.
    pub available: String,
    /// Reserved.
    pub reserved: String,
    /// Encumbered.
    pub encumbered: String,
}

/// `statement` (SPEC §8.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StatementBody {
    /// The account.
    pub account: String,
    /// The asset.
    pub asset: String,
    /// The period's start.
    pub from: Time,
    /// The period's end.
    pub to: Time,
    /// The first posting of the period.
    pub from_seq: u64,
    /// The last.
    pub to_seq: u64,
    /// Whether the period is closed: no posting can still enter it.
    pub closed: bool,
    /// The opening balances.
    pub opening: SumsBody,
    /// The movements.
    pub movements: Vec<MovementBody>,
    /// The closing balances.
    pub closing: SumsBody,
}

/// One asset of the registry (`REQ-AS-1`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AssetBody {
    /// Its symbol.
    pub id: String,
    /// Its location on the settlement layer, once a binding was asked for (`REQ-AS-8`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// `book_only`, `binding` or `bound` (amendment 0011, `REQ-AS-7`, `REQ-AS-8`).
    pub binding: String,
    /// Its one precision (`INV-24`).
    pub precision: u8,
    /// `fiat`, `stable`, `native` or `other`.
    pub kind: String,
    /// `active` or `deprecated`.
    pub status: String,
}

/// One unbalanced (currency, layer) of the commitment check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct UnbalancedBody {
    /// The kernel currency tag.
    pub currency: u32,
    /// The balance layer.
    pub layer: u64,
}

/// One asset's trial balance on openings.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TrialRowBody {
    /// The asset.
    pub asset: String,
    /// The signed sum of openings, minor units: zero when it balances.
    pub openings: String,
}

/// `trial_balance` (SPEC §8.2, `INV-5`): the operator's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TrialBalanceBody {
    /// The view it was read at.
    pub view: u64,
    /// How many kernel accounts the commitment check summed.
    pub accounts: u64,
    /// Every (currency, layer) that does not sum to zero. Empty when the book balances.
    pub unbalanced: Vec<UnbalancedBody>,
    /// The check on openings, per asset.
    pub rows: Vec<TrialRowBody>,
}

/// Incidents of one kind on the operator channel (`F-013`), for `F-018`'s signals.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct IncidentCountBody {
    /// The kind.
    pub kind: String,
    /// How many were raised.
    pub count: u64,
    /// When the latest was raised, in milliseconds.
    pub latest: u64,
}

/// The audit trail's shipping to the audit store (`T-013-13`, `REQ-AT-2`, `NFR-6`), as the core
/// reports it: counts, ages and flags, never a record.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AuditShipBody {
    /// Records in the core's audit outbox, not yet confirmed by the store.
    pub unshipped: u64,
    /// The age of the oldest of them, by its time input; absent when none waits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_unshipped_age: Option<u64>,
    /// Records the store holds.
    pub shipped: u64,
    /// Whether the last round reached the store.
    pub store_up: bool,
    /// Whether the store refused a record under its rules, or holds another record than the
    /// core's at its head: shipping has stopped.
    pub refused: bool,
    /// Whether the core refuses new inputs because its outbox is at its bound.
    pub backpressure: bool,
    /// That bound.
    pub max_unshipped: u64,
}

/// The operational signals (`F-018`, `NFR-11`): counts, ages and times, never a ledger fact.
/// The operator's, with the `read_signals` operator kind. Ages are in milliseconds at the time
/// of the read.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SignalsBody {
    /// When the signals were read.
    pub now: u64,
    /// Non-terminal operations.
    pub open: u64,
    /// The age of the oldest non-terminal operation (`REQ-OP-8`); absent when none is open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_open_age: Option<u64>,
    /// Operations in `Received` in the core.
    pub received: u64,
    /// The review queue's length (`REQ-OP-6`).
    pub review: u64,
    /// Of which in `Needs-Review`.
    pub needs_review: u64,
    /// Of which past their service level.
    pub review_overdue: u64,
    /// How far past its service level the most overdue entry is.
    pub review_most_overdue: u64,
    /// The review queue's service level.
    pub review_service_level: u64,
    /// Undelivered notifications (`REQ-EV-6`).
    pub undelivered: u64,
    /// The age of the oldest undelivered notification; absent when none is owed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_undelivered_age: Option<u64>,
    /// The age of the last tick the core applied (`DEC-6`, `RISK-7`); absent before the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tick_age: Option<u64>,
    /// The age of the oldest row ingress holds undecided — logged, not yet decided by the core
    /// (`REQ-CS-7`); absent when every row is decided. Ingress's own, added by the surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_undecided_age: Option<u64>,
    /// Incidents on the operator channel, per kind.
    pub incidents: Vec<IncidentCountBody>,
    /// Anchor lag (`F-017`): time since the newest `Final` checkpoint was created; before the
    /// first is final, the age of the oldest checkpoint; absent while there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_lag: Option<u64>,
    /// How long the checkpoint in flight has waited for finality, if one is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_in_flight_age: Option<u64>,
    /// Anchoring's incidents, per kind (`commitment_mismatch`, `unknown_anchor`,
    /// `position_difference`); `latest` is not known for them and is zero.
    #[serde(default)]
    pub anchor_incidents: Vec<IncidentCountBody>,
    /// Assets whose latest settlement comparison differs (`REQ-PR-5`).
    #[serde(default)]
    pub settlement_differing: u64,
    /// The age of the latest settlement comparison.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub settlement_comparison_age: Option<u64>,
    /// Completion of commands at p50 and p99, in milliseconds, over ingress's latest samples
    /// (`NFR-15`, amendment 0007); absent before the first. Ingress's own, added by the surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_p50: Option<u64>,
    /// See `completion_p50`. `F-018` pages when it passes the 30 s ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion_p99: Option<u64>,
    /// Whether the core has stopped deciding: halted on an input the Agent cannot apply (finding
    /// F18 of `F-032`), or stopped on an incident. Reads go on; every input waits `Received`.
    #[serde(default)]
    pub core_halted: bool,
    /// The audit trail's shipping (`T-013-13`); absent when the core ships to no audit store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit: Option<AuditShipBody>,
    /// The core Agent's runtime: Authority decisions (receipts) issued whose invocation is not
    /// acknowledged yet. The runtime refuses more past 256. Absent for the in-process core.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vos_unacknowledged: Option<u64>,
    /// How long the core's writer has been fenced, in milliseconds (`T-037-15`, `REQ-RT-20`,
    /// `NFR-11`): since the fence first refused an input that has not committed since, because the
    /// code installed is not the code the writer's state names. Absent while it is not fenced, and
    /// from a core with no fence (the in-process core).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer_fenced_age: Option<u64>,
    /// Answers the core host's code fetcher discarded because the bytes were not what their digest
    /// and length name (`T-037-06`, `aledgent_code_source_mismatch_total`), per place it asked: the
    /// cache and each configured source, each listed even at zero. Empty when the node has no
    /// source of code, and from a core with none (the in-process core).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub code_source_mismatches: Vec<CodeSourceMismatchBody>,
    /// The book's size (`T-018-11`): what `T-036-17`'s disk-backed store is required by. Absent
    /// from a core that does not count it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub book: Option<BookSizeBody>,
    /// The code the core's writer runs (`T-037-23`, `REQ-RT-16`, `REQ-RT-20`, `NFR-11`): the
    /// version in force, its generation, and the upgrade scheduled, as of the last input that
    /// committed. Absent from a core that does not report it (the in-process core, an older core
    /// host): a reader that does not know the field ignores it, and a body without it reads as
    /// "not reported".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub writer_code: Option<WriterCodeBody>,
}

/// The code the core's writer runs, as the signals carry it (`T-037-23`): what to look at to know
/// which program is deciding, before and after a switch.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WriterCodeBody {
    /// The code version in force: the digest of its manifest, 64 hex characters in lower case. All
    /// zeros for a book founded without a named code version (tests).
    pub code: String,
    /// The code generation in force: 0 at genesis, one more after each upgrade.
    pub generation: u64,
    /// The upgrade scheduled, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upgrade: Option<ScheduledUpgradeBody>,
}

/// An upgrade the writer has scheduled (`T-037-23`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ScheduledUpgradeBody {
    /// The version it installs, as [`WriterCodeBody::code`].
    pub code: String,
    /// The input number it applies from: that input and those after it run under the new code.
    pub from: u64,
}

/// The answers one place of the code fetcher discarded (`T-037-06`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CodeSourceMismatchBody {
    /// The place: `cache`, or a source's kind and position in the configured list, as `fs:0`.
    /// Never a path, a URL or a credential.
    pub source: String,
    /// Answers discarded, ever, since the host started.
    pub count: u64,
}

/// The book's size, as the signals carry it: counts only, each kept by the collection that holds
/// what it counts, so the read visits no record (`T-018-11`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BookSizeBody {
    /// The book's records, in a chainspec's unit (`F-036`): one for each identity, ledger
    /// account, grant, alias and operation the book holds, and one for each of its seed's opening
    /// rows and credential imports. At genesis, the chainspec's records and the few rows genesis
    /// itself writes; later, one more for each operation, identity, account, grant or alias.
    pub records: u64,
    /// The leaves of the core's service store: what the core host's replica holds in memory.
    pub leaves: u64,
    /// The records of the chainspec the book was founded on; absent for a book founded without
    /// one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genesis_records: Option<u64>,
}

/// `can_spend` (SPEC §8.2): yes, or a refusal naming why not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CanSpendBody {
    /// Always `true`: a "no" is a refusal with its reason.
    pub ok: bool,
}

fn snake(debug: &str) -> String {
    let mut out = String::new();
    for (i, c) in debug.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn authority(a: &AuthorityUsed) -> AuthorityBody {
    AuthorityBody {
        kind: snake(&format!("{:?}", a.kind)),
        grant: a.grant.map(|g| g.to_did()),
        principal: a.principal.to_did(),
        credential: a.credential.to_did(),
        on_behalf_of: a.on_behalf_of.map(|i| i.to_did()),
    }
}

/// A movement on the wire.
pub fn movement(m: &Movement) -> MovementBody {
    MovementBody {
        seq: m.seq,
        at: m.at,
        kind: snake(&format!("{:?}", m.kind)),
        status: match m.status {
            MovementStatus::Held => "held",
            MovementStatus::Completed => "completed",
            MovementStatus::Returned => "returned",
            MovementStatus::Reversed => "reversed",
            MovementStatus::Opening => "opening",
        }
        .to_owned(),
        operation: m.operation.to_did(),
        account: m.account.to_did(),
        balance: match m.balance {
            BalanceKind::Available => "available",
            BalanceKind::Reserved => "reserved",
            BalanceKind::Encumbered => "encumbered",
        }
        .to_owned(),
        asset: m.asset.as_str().to_owned(),
        direction: match m.direction {
            Direction::Debit => "debit",
            Direction::Credit => "credit",
        }
        .to_owned(),
        amount: m.amount.get().to_string(),
        counterparty: m.counterparty.map(|c| c.to_did()),
        authorities: m.authorities.iter().map(authority).collect(),
        // Ingress adds it, for the reader it may be shown to (`REQ-PO-5`): the core never
        // holds it.
        reference: None,
    }
}

fn sums(s: &Sums) -> SumsBody {
    SumsBody {
        available: s.available.to_string(),
        reserved: s.reserved.to_string(),
        encumbered: s.encumbered.to_string(),
    }
}

/// Balances on the wire.
pub fn balances(b: &Balances) -> BalancesBody {
    BalancesBody {
        view: b.view,
        rows: b
            .rows
            .iter()
            .map(|r| BalanceRowBody {
                account: r.account.to_did(),
                asset: r.asset.as_str().to_owned(),
                available: r.available.to_string(),
                reserved: r.reserved.to_string(),
                encumbered: r.encumbered.to_string(),
                total: r.total.to_string(),
            })
            .collect(),
    }
}

/// A page on the wire.
pub fn page(p: &Page) -> PageBody {
    PageBody {
        view: p.view,
        movements: p.movements.iter().map(movement).collect(),
        next: p.next.as_ref().map(|c| hex::encode(c.as_bytes())),
    }
}

/// A statement on the wire.
pub fn statement(s: &Statement) -> StatementBody {
    StatementBody {
        account: s.account.to_did(),
        asset: s.asset.as_str().to_owned(),
        from: s.period.from,
        to: s.period.to,
        from_seq: s.from_seq,
        to_seq: s.to_seq,
        closed: s.closed,
        opening: sums(&s.opening),
        movements: s.movements.iter().map(movement).collect(),
        closing: sums(&s.closing),
    }
}

/// An asset on the wire.
pub fn asset(a: &Asset) -> AssetBody {
    AssetBody {
        id: a.id().as_str().to_owned(),
        location: a.binding().location().map(|l| l.as_str().to_owned()),
        binding: match a.binding() {
            AssetBinding::BookOnly => "book_only",
            AssetBinding::Binding(_) => "binding",
            AssetBinding::Bound(_) => "bound",
        }
        .to_owned(),
        precision: a.precision(),
        kind: match a.kind() {
            AssetKind::Fiat => "fiat",
            AssetKind::Stable => "stable",
            AssetKind::Native => "native",
            AssetKind::Other => "other",
        }
        .to_owned(),
        status: match a.status() {
            AssetStatus::Active => "active",
            AssetStatus::Deprecated => "deprecated",
        }
        .to_owned(),
    }
}

/// A trial balance on the wire.
pub fn trial(t: &TrialBalance) -> TrialBalanceBody {
    TrialBalanceBody {
        view: t.view,
        accounts: t.commitments.accounts as u64,
        unbalanced: t
            .commitments
            .unbalanced
            .iter()
            .map(|(currency, layer)| UnbalancedBody {
                currency: *currency,
                layer: *layer as u64,
            })
            .collect(),
        rows: t
            .rows
            .iter()
            .map(|r| TrialRowBody {
                asset: r.asset.as_str().to_owned(),
                openings: r.openings.to_string(),
            })
            .collect(),
    }
}

/// An operation on the wire.
pub fn operation(v: &OperationView) -> OperationReply {
    let shown = v.status.map(ShownBody::from);
    OperationReply {
        operation_reference: v.reference.to_did(),
        command: v.command.map(|c| snake(&format!("{c:?}"))),
        kind: v.kind.map(|k| snake(&format!("{k:?}"))),
        state: shown.as_ref().map(|s| s.state.clone()),
        class: shown.as_ref().map(|s| s.class),
        terminal: shown.as_ref().is_none_or(|s| s.terminal),
        deadline: v.deadline,
        closed_at: v.closed_at,
        outcome: v.outcome.map(|s| s.name().to_owned()),
        error: v.refusal,
        evidence: v.evidence.iter().map(|e| hex::encode(e.0)).collect(),
        postings: v.postings.clone(),
        // Ingress adds it (`REQ-PO-5`): the core never holds it.
        reference: None,
    }
}

/// An account identifier a read names, in a parameter or in the path. One that does not parse is
/// a shape fault, `ERR-MalformedCommand`; a well-formed one the caller does not reach is
/// `ERR-UnknownAccount`, which the core decides (amendment 0032, `REQ-ER-3`).
pub fn account(s: &str) -> Result<AccountId, Refusal> {
    AccountId::parse(s).map_err(|_| Refusal::MalformedCommand)
}

/// An identity identifier a read names: as [`account`].
pub fn identity(s: &str) -> Result<IdentityId, Refusal> {
    IdentityId::parse(s).map_err(|_| Refusal::MalformedCommand)
}

/// The subject of a query: exactly one of an account and an identity, else unbounded.
pub fn subject(account_: Option<&str>, identity_: Option<&str>) -> Result<Subject, Refusal> {
    match (account_, identity_) {
        (Some(a), None) => Ok(Subject::Account(account(a)?)),
        (None, Some(i)) => Ok(Subject::Identity(identity(i)?)),
        _ => Err(Refusal::QueryUnbounded),
    }
}

/// An optional asset, by its symbol. A symbol that cannot be one (not 2 to 12 ASCII letters and
/// digits) is a shape fault, `ERR-MalformedCommand`; the grammar is [`AssetId::parse`]'s, which is
/// left as it is for commands. A well-formed symbol that is not in the registry is
/// `ERR-UnknownAsset`, which the core decides (amendment 0032, `REQ-ER-3`).
pub fn asset_id(s: Option<&str>) -> Result<Option<AssetId>, Refusal> {
    s.map(|s| AssetId::parse(s).map_err(|_| Refusal::MalformedCommand))
        .transpose()
}

/// An amount in minor units, as a query parameter: digits only. A value that is not a count of
/// minor units does not parse, `ERR-MalformedCommand` (amendment 0032, `REQ-ER-3`).
pub fn minor(s: &str) -> Result<Minor, Refusal> {
    if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
        return Err(Refusal::MalformedCommand);
    }
    s.parse::<u64>()
        .map(Minor::new)
        .map_err(|_| Refusal::MalformedCommand)
}

/// `movements`' request.
pub fn movements_request(p: &MovementsParams) -> Result<MovementsRequest, Refusal> {
    let time = match (p.from, p.to) {
        (None, None) => None,
        (Some(from), Some(to)) => Some(TimeRange { from, to }),
        _ => return Err(Refusal::QueryUnbounded),
    };
    Ok(MovementsRequest {
        subject: subject(p.account.as_deref(), p.identity.as_deref())?,
        filter: MovementFilter {
            account: p.on_account.as_deref().map(account).transpose()?,
            asset: asset_id(p.asset.as_deref())?,
            direction: match p.direction.as_deref() {
                None => None,
                Some("debit") => Some(Direction::Debit),
                Some("credit") => Some(Direction::Credit),
                Some(_) => return Err(Refusal::MalformedCommand),
            },
            counterparty: p.counterparty.as_deref().map(account).transpose()?,
            time,
        },
        // A count past `u32` is past 1000 all the same: the core refuses it as unbounded.
        limit: p.limit.map(|l| u32::try_from(l).unwrap_or(u32::MAX)),
        cursor: match &p.cursor {
            None => None,
            Some(c) => Some(Cursor::from_bytes(
                hex::decode(c).map_err(|_| Refusal::MalformedCommand)?,
            )),
        },
    })
}

/// `statement`'s request.
pub fn statement_request(p: &StatementParams) -> Result<StatementRequest, Refusal> {
    Ok(StatementRequest {
        account: account(p.account.as_deref().ok_or(Refusal::QueryUnbounded)?)?,
        asset: asset_id(p.asset.as_deref())?.ok_or(Refusal::QueryUnbounded)?,
        from: p.from,
        to: p.to,
        as_of: p.as_of,
    })
}

/// An operation reference. One that does not parse is a shape fault, `ERR-MalformedCommand`; a
/// well-formed one the caller does not reach is `ERR-UnknownOperation`.
pub fn reference(s: &str) -> Result<OperationId, Refusal> {
    OperationId::parse(s).map_err(|_| Refusal::MalformedCommand)
}

/// The shape of a query as the surface decides it, before the core is asked anything
/// (amendment 0032, `REQ-ER-3`): every identifier parses, `direction` is one of its two values,
/// `cursor` is hex, `amount` is a count of minor units, and an asset is a symbol. What is *absent* or *conflicting* is
/// not a shape fault and is left to the bounds (`ERR-QueryUnbounded`, `REQ-CS-5`); so is what
/// parses but names nothing the caller reaches (`ERR-UnknownAccount`, `ERR-UnknownAsset`).
pub fn shape(q: &crate::boundary::Query) -> Result<(), Refusal> {
    use crate::boundary::Query;
    let each = |ids: &[Option<&str>]| {
        ids.iter()
            .flatten()
            .try_for_each(|s| account(s).map(|_| ()))
    };
    match q {
        Query::Balances(p) => {
            each(&[p.account.as_deref()])?;
            p.identity.as_deref().map(identity).transpose()?;
            asset_id(p.asset.as_deref())?;
        }
        Query::Movements(p) => {
            each(&[
                p.account.as_deref(),
                p.on_account.as_deref(),
                p.counterparty.as_deref(),
            ])?;
            p.identity.as_deref().map(identity).transpose()?;
            asset_id(p.asset.as_deref())?;
            if !matches!(p.direction.as_deref(), None | Some("debit" | "credit")) {
                return Err(Refusal::MalformedCommand);
            }
            if let Some(c) = &p.cursor {
                hex::decode(c).map_err(|_| Refusal::MalformedCommand)?;
            }
        }
        Query::Statement(p) => {
            each(&[p.account.as_deref()])?;
            asset_id(p.asset.as_deref())?;
        }
        Query::TrialBalance(p) => {
            asset_id(p.asset.as_deref())?;
        }
        Query::Operation { reference: r } => {
            reference(r)?;
        }
        Query::CanSpend { account: a, params } => {
            account(a)?;
            asset_id(params.asset.as_deref())?;
            params.amount.as_deref().map(minor).transpose()?;
        }
        Query::RecoveryTerms { identity: i } | Query::IdentityGrants { identity: i } => {
            identity(i)?;
        }
        Query::RecoveryMechanisms { account: a }
        | Query::Custody { account: a }
        | Query::Grants { account: a }
        | Query::Account { account: a } => {
            account(a)?;
        }
        Query::CodeAuthority { input } => {
            if !matches!(input.as_str(), "upgrade" | "way_out") {
                return Err(Refusal::MalformedCommand);
            }
        }
        Query::Assets
        | Query::Undelivered
        | Query::Instructions
        | Query::Calls
        | Query::Evidence
        | Query::Anchor
        | Query::VerifyDisclosure { .. }
        | Query::Anchoring
        | Query::Signals
        | Query::IntegratingParties
        | Query::Alias { .. } => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_snake_case() {
        assert_eq!(snake("CloseAccount"), "close_account");
        assert_eq!(snake("Reserve"), "reserve");
    }

    #[test]
    fn a_query_needs_exactly_one_subject() {
        assert_eq!(subject(None, None), Err(Refusal::QueryUnbounded));
        assert_eq!(subject(Some("x"), Some("y")), Err(Refusal::QueryUnbounded));
        let half = MovementsParams {
            from: Some(1),
            ..MovementsParams::default()
        };
        assert_eq!(movements_request(&half), Err(Refusal::QueryUnbounded));
    }
}
