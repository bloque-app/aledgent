// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Anchoring and disclosure through ingress (`F-017`; `CTR-DSC`, SPEC §7.E; `CTR-OPS`).
//!
//! - **`issue_disclosure`** on `CTR-DSC` (`POST /v1/disclosures`): a holder, or an authority it
//!   granted, asks for a disclosure; with `legal_obligation` set it arrives on the operator
//!   surface instead, as the operator's under a legal obligation (`REQ-DS-5`). The reply carries
//!   the disclosure's bytes, hex. The core signs with the clerk key it holds; ingress never sees
//!   the key.
//! - **`verify`** (`GET /v1/disclosures/verify?disclosure=<hex>`): a query with no side effect,
//!   the offline verifier's checks against the core's own final checkpoints and key.
//! - **`anchor_checkpoint`** and **`compare_settlement`** on `CTR-OPS` (`POST /v1/ops/anchors`,
//!   `POST /v1/ops/settlement/comparisons`), through the operator frame; and the operator's read
//!   of anchoring (`GET /v1/ops/anchors`).
//! - **The adapter's** anchor read (`GET /v1/settlement/anchor`) and inputs live with its other
//!   routes, in [`crate::settlement`].

use ledger_core::Refusal;
use ledger_core::anchor::clerk::ClerkSigner;
use ledger_core::anchor::disclosure::{
    Assertion, BalanceKind, Bound, DisclosureRequest, FlowDirection, Recipient,
};
use ledger_core::anchor::{AnchorIncident, AnchorStore, Checkpoint};
use ledger_core::asset::{AssetId, Minor};
use ledger_core::id::{AccountId, IdentityId, OperationId};
use ledger_core::ledger::{Ctx, Ledger};
use ledger_core::principal::Caller;
use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::boundary::{Outcome, QueryReply};
use crate::settlement::{ComparisonBody, comparison_body};
use crate::wire::{CommandBody, CommandResult, CoordinateBody};

// ------------------------------------------------------------------------------------ wire

/// What a disclosure asserts (`REQ-DS-1`). Any other shape is `ERR-DisclosureUnsupported`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "shape", rename_all = "snake_case", deny_unknown_fields)]
pub enum AssertionBody {
    /// The subject holds at least `amount` of `asset`, in balance kind `kind` (only `available`
    /// is supported).
    HoldsAtLeast {
        /// The asset.
        asset: String,
        /// Minor units, as a decimal-integer string.
        amount: String,
        /// `available`, `reserved` or `total`.
        kind: String,
    },
    /// The named operation occurred on the subject.
    OperationOccurred {
        /// The operation reference.
        operation_reference: String,
    },
    /// The named operation moved exactly `amount` of `asset`.
    OperationMoved {
        /// The operation reference.
        operation_reference: String,
        /// The asset.
        asset: String,
        /// Minor units, as a decimal-integer string.
        amount: String,
    },
    /// The subject's flows over entries `[from_sequence, to_sequence]` meet a bound.
    FlowWithin {
        /// The asset.
        asset: String,
        /// `in` or `out`.
        direction: String,
        /// The first entry of the window.
        from_sequence: u64,
        /// The last entry of the window.
        to_sequence: u64,
        /// `at_least` or `at_most`.
        bound: String,
        /// The bound, minor units, as a decimal-integer string.
        amount: String,
    },
    /// Any shape aledgent does not make.
    #[serde(other)]
    Other,
}

/// `issue_disclosure` (SPEC §7.E, `REQ-DS-2`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DisclosureBody {
    /// The subject: a ledger account.
    pub subject: String,
    /// The recipient: an identity of the book, or an opaque 32-byte reference, hex. One of the
    /// two. Never personal data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient_identity: Option<String>,
    /// See `recipient_identity`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipient_reference: Option<String>,
    /// The assertion.
    pub assertion: AssertionBody,
    /// The checkpoint to rest on; the latest final one if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<u64>,
    /// The operator's, under a legal obligation: the justification (`REQ-DS-5`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legal_obligation: Option<String>,
}

/// An operator command of anchoring's: its justification.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnchoringJustifiedBody {
    /// Why.
    pub justification: String,
}

/// `verify(disclosure)`'s query parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerifyParams {
    /// The disclosure's bytes, hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disclosure: Option<String>,
}

/// An anchoring command, as the core receives it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "anchoring", rename_all = "snake_case")]
pub enum AnchoringCommandBody {
    /// `issue_disclosure`.
    IssueDisclosure(DisclosureBody),
    /// `anchor_checkpoint`.
    AnchorCheckpoint(AnchoringJustifiedBody),
    /// `compare_settlement`.
    CompareSettlement(AnchoringJustifiedBody),
}

/// A checkpoint on the wire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckpointBody {
    /// Its number: the settlement layer's anchor number.
    pub number: u64,
    /// The sequence of the last entry it covers.
    pub entry_sequence: u64,
    /// The kernel root, hex.
    pub kernel_root: String,
    /// The commitment, hex.
    pub commitment: String,
    /// When it was created.
    pub created_at: u64,
    /// `pending`, `submitted` or `final`.
    pub status: String,
    /// Its `Anchored` record, once final.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coordinate: Option<CoordinateBody>,
}

/// What an anchoring command produced.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnchoringResultBody {
    /// The disclosure, hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disclosure: Option<String>,
    /// The checkpoint `anchor_checkpoint` answered with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<CheckpointBody>,
    /// The comparison `compare_settlement` made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comparison: Option<ComparisonBody>,
}

/// What `verify(disclosure)` found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct VerifiedBody {
    /// Whether every check passed.
    pub passed: bool,
    /// The report, as the offline verifier prints it.
    pub report: String,
}

/// An anchoring incident on the wire.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnchorIncidentBody {
    /// `commitment_mismatch`, `unknown_anchor` or `position_difference`.
    pub kind: String,
    /// What it concerns.
    pub detail: String,
}

/// Anchoring as the operator reads it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnchoringBody {
    /// Every checkpoint.
    pub checkpoints: Vec<CheckpointBody>,
    /// Every recorded comparison.
    pub comparisons: Vec<ComparisonBody>,
    /// The latest comparison, recorded or not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_comparison: Option<ComparisonBody>,
    /// Every incident.
    pub incidents: Vec<AnchorIncidentBody>,
}

// ------------------------------------------------------------------------------------ routes

/// An anchoring route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnchoringRoute {
    /// `POST /v1/disclosures`.
    IssueDisclosure,
    /// `POST /v1/ops/anchors`.
    AnchorCheckpoint,
    /// `POST /v1/ops/settlement/comparisons`.
    CompareSettlement,
}

fn json<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, Refusal> {
    let bytes: &[u8] = if bytes.is_empty() { b"{}" } else { bytes };
    serde_json::from_slice(bytes).map_err(|_| Refusal::MalformedCommand)
}

impl AnchoringRoute {
    /// The request body's schema name in the OpenAPI document.
    pub const fn schema(self) -> &'static str {
        match self {
            AnchoringRoute::IssueDisclosure => "DisclosureBody",
            AnchoringRoute::AnchorCheckpoint | AnchoringRoute::CompareSettlement => {
                "AnchoringJustifiedBody"
            }
        }
    }

    /// The command a request on this route carries.
    pub fn build(self, bytes: &[u8]) -> Result<CommandBody, Refusal> {
        Ok(CommandBody::Anchoring(match self {
            AnchoringRoute::IssueDisclosure => AnchoringCommandBody::IssueDisclosure(json(bytes)?),
            AnchoringRoute::AnchorCheckpoint => {
                AnchoringCommandBody::AnchorCheckpoint(json(bytes)?)
            }
            AnchoringRoute::CompareSettlement => {
                AnchoringCommandBody::CompareSettlement(json(bytes)?)
            }
        }))
    }
}

// ------------------------------------------------------------------------------------ parsing

/// An anchoring command, validated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnchoringCommand {
    /// `issue_disclosure`.
    IssueDisclosure(DisclosureRequest),
    /// `anchor_checkpoint`, with its justification.
    AnchorCheckpoint(Vec<u8>),
    /// `compare_settlement`, with its justification.
    CompareSettlement(Vec<u8>),
}

fn minor(s: &str) -> Result<Minor, Refusal> {
    if s.is_empty() || !s.bytes().all(|c| c.is_ascii_digit()) {
        return Err(Refusal::PrecisionExceeded);
    }
    s.parse::<u64>()
        .map(Minor::new)
        .map_err(|_| Refusal::PrecisionExceeded)
}

fn operation(s: &str) -> Result<OperationId, Refusal> {
    OperationId::parse(s).map_err(|_| Refusal::UnknownOperation)
}

fn hex32(s: &str) -> Result<[u8; 32], Refusal> {
    hex::decode(s)
        .ok()
        .and_then(|v| <[u8; 32]>::try_from(v).ok())
        .ok_or(Refusal::MalformedCommand)
}

fn assertion(b: &AssertionBody) -> Result<Assertion, Refusal> {
    Ok(match b {
        AssertionBody::HoldsAtLeast {
            asset,
            amount,
            kind,
        } => Assertion::HoldsAtLeast {
            asset: AssetId::parse(asset)?,
            amount: minor(amount)?,
            kind: match kind.as_str() {
                "available" => BalanceKind::Available,
                "reserved" => BalanceKind::Reserved,
                "total" => BalanceKind::Total,
                _ => return Err(Refusal::DisclosureUnsupported),
            },
        },
        AssertionBody::OperationOccurred {
            operation_reference,
        } => Assertion::OperationOccurred {
            operation: operation(operation_reference)?,
        },
        AssertionBody::OperationMoved {
            operation_reference,
            asset,
            amount,
        } => Assertion::OperationMoved {
            operation: operation(operation_reference)?,
            asset: AssetId::parse(asset)?,
            amount: minor(amount)?,
        },
        AssertionBody::FlowWithin {
            asset,
            direction,
            from_sequence,
            to_sequence,
            bound,
            amount,
        } => Assertion::FlowWithin {
            asset: AssetId::parse(asset)?,
            direction: match direction.as_str() {
                "in" => FlowDirection::In,
                "out" => FlowDirection::Out,
                _ => return Err(Refusal::DisclosureUnsupported),
            },
            from_sequence: *from_sequence,
            to_sequence: *to_sequence,
            bound: match bound.as_str() {
                "at_least" => Bound::AtLeast(minor(amount)?.get()),
                "at_most" => Bound::AtMost(minor(amount)?.get()),
                _ => return Err(Refusal::DisclosureUnsupported),
            },
        },
        AssertionBody::Other => return Err(Refusal::DisclosureUnsupported),
    })
}

fn assertion_body(a: &Assertion) -> AssertionBody {
    match *a {
        Assertion::HoldsAtLeast {
            asset,
            amount,
            kind,
        } => AssertionBody::HoldsAtLeast {
            asset: asset.as_str().to_owned(),
            amount: amount.get().to_string(),
            kind: kind.name().to_owned(),
        },
        Assertion::OperationOccurred { operation } => AssertionBody::OperationOccurred {
            operation_reference: operation.to_did(),
        },
        Assertion::OperationMoved {
            operation,
            asset,
            amount,
        } => AssertionBody::OperationMoved {
            operation_reference: operation.to_did(),
            asset: asset.as_str().to_owned(),
            amount: amount.get().to_string(),
        },
        Assertion::FlowWithin {
            asset,
            direction,
            from_sequence,
            to_sequence,
            bound,
        } => AssertionBody::FlowWithin {
            asset: asset.as_str().to_owned(),
            direction: direction.name().to_owned(),
            from_sequence,
            to_sequence,
            bound: match bound {
                Bound::AtLeast(_) => "at_least",
                Bound::AtMost(_) => "at_most",
            }
            .to_owned(),
            amount: bound.amount().to_string(),
        },
    }
}

impl AnchoringCommand {
    /// Validate the anchoring commands of `body`; `None` if it is another command.
    pub fn parse(body: &CommandBody) -> Option<Result<AnchoringCommand, Refusal>> {
        let CommandBody::Anchoring(b) = body else {
            return None;
        };
        Some(match b {
            AnchoringCommandBody::IssueDisclosure(d) => (|| {
                let recipient = match (&d.recipient_identity, &d.recipient_reference) {
                    (Some(i), None) => Recipient::Identity(
                        IdentityId::parse(i).map_err(|_| Refusal::MalformedCommand)?,
                    ),
                    (None, Some(r)) => Recipient::Reference(hex32(r)?),
                    _ => return Err(Refusal::MalformedCommand),
                };
                Ok(AnchoringCommand::IssueDisclosure(DisclosureRequest {
                    subject: AccountId::parse(&d.subject).map_err(|_| Refusal::UnknownAccount)?,
                    recipient,
                    assertion: assertion(&d.assertion)?,
                    checkpoint: d.checkpoint,
                    legal_obligation: d.legal_obligation.as_ref().map(|j| j.as_bytes().to_vec()),
                }))
            })(),
            AnchoringCommandBody::AnchorCheckpoint(j) => Ok(AnchoringCommand::AnchorCheckpoint(
                j.justification.as_bytes().to_vec(),
            )),
            AnchoringCommandBody::CompareSettlement(j) => Ok(AnchoringCommand::CompareSettlement(
                j.justification.as_bytes().to_vec(),
            )),
        })
    }

    /// The command in its canonical wire form.
    pub fn to_body(&self) -> CommandBody {
        let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        CommandBody::Anchoring(match self {
            AnchoringCommand::IssueDisclosure(r) => {
                AnchoringCommandBody::IssueDisclosure(DisclosureBody {
                    subject: r.subject.to_did(),
                    recipient_identity: match r.recipient {
                        Recipient::Identity(i) => Some(i.to_did()),
                        Recipient::Reference(_) => None,
                    },
                    recipient_reference: match r.recipient {
                        Recipient::Identity(_) => None,
                        Recipient::Reference(b) => Some(hex::encode(b)),
                    },
                    assertion: assertion_body(&r.assertion),
                    checkpoint: r.checkpoint,
                    legal_obligation: r.legal_obligation.as_deref().map(text),
                })
            }
            AnchoringCommand::AnchorCheckpoint(j) => {
                AnchoringCommandBody::AnchorCheckpoint(AnchoringJustifiedBody {
                    justification: text(j),
                })
            }
            AnchoringCommand::CompareSettlement(j) => {
                AnchoringCommandBody::CompareSettlement(AnchoringJustifiedBody {
                    justification: text(j),
                })
            }
        })
    }

    /// Whether it arrives on the operator surface: the operator's commands, and a disclosure
    /// under a legal obligation.
    pub fn is_operator(&self) -> bool {
        match self {
            AnchoringCommand::IssueDisclosure(r) => r.legal_obligation.is_some(),
            AnchoringCommand::AnchorCheckpoint(_) | AnchoringCommand::CompareSettlement(_) => true,
        }
    }
}

// ------------------------------------------------------------------------------------ core side

/// A checkpoint on the wire.
pub fn checkpoint_body(c: &Checkpoint) -> CheckpointBody {
    CheckpointBody {
        number: c.number,
        entry_sequence: c.entry_sequence,
        kernel_root: hex::encode(c.kernel_root),
        commitment: hex::encode(c.commitment),
        created_at: c.created_at,
        status: c.status.name().to_owned(),
        coordinate: c
            .coordinate
            .as_ref()
            .map(crate::settlement::coordinate_body),
    }
}

fn incident_body(i: &AnchorIncident) -> AnchorIncidentBody {
    match i {
        AnchorIncident::CommitmentMismatch {
            number,
            expected,
            observed,
            coordinate,
        } => AnchorIncidentBody {
            kind: "commitment_mismatch".into(),
            detail: format!(
                "anchor {number}: expected {}, the layer holds {} at {}:{}",
                hex::encode(expected),
                hex::encode(observed),
                coordinate.instance,
                coordinate.seq
            ),
        },
        AnchorIncident::UnknownAnchor {
            number,
            commitment,
            coordinate,
        } => AnchorIncidentBody {
            kind: "unknown_anchor".into(),
            detail: format!(
                "anchor {number} ({}) at {}:{} has no checkpoint",
                hex::encode(commitment),
                coordinate.instance,
                coordinate.seq
            ),
        },
        AnchorIncident::PositionDifference {
            cut,
            asset,
            book,
            layer,
        } => AnchorIncidentBody {
            kind: "position_difference".into(),
            detail: format!(
                "cut {cut}, {}: the book records {book}, the layer holds {}",
                asset.as_str(),
                layer.get()
            ),
        },
    }
}

/// The anchor store on the wire.
pub fn anchoring_body(s: &AnchorStore) -> AnchoringBody {
    AnchoringBody {
        checkpoints: s.checkpoints().map(checkpoint_body).collect(),
        comparisons: s.comparisons().iter().map(comparison_body).collect(),
        latest_comparison: s.latest_comparison().map(comparison_body),
        incidents: s.incidents().iter().map(incident_body).collect(),
    }
}

fn decided(op: &OperationId, result: AnchoringResultBody) -> Outcome {
    Outcome::Decided {
        operation_reference: op.to_did(),
        state: None,
        result: CommandResult {
            anchoring: Some(Box::new(result)),
            ..CommandResult::default()
        },
    }
}

/// Run an anchoring command in the in-process core: its semantics are the ledger's; this only
/// dispatches. `clerk` is the clerk key the core holds, if any: without one, no disclosure can be
/// signed (`ERR-DisclosureUnsupported`).
pub fn execute<S: LeafStore, V: VaultBackend>(
    l: &mut Ledger<S, V>,
    ctx: &Ctx,
    c: AnchoringCommand,
    clerk: Option<&dyn ClerkSigner>,
) -> Result<Outcome, Refusal> {
    let op = &ctx.op;
    Ok(match c {
        AnchoringCommand::IssueDisclosure(r) => {
            let clerk = clerk.ok_or(Refusal::DisclosureUnsupported)?;
            let a = l.issue_disclosure(ctx, r, clerk)?;
            decided(
                op,
                AnchoringResultBody {
                    disclosure: Some(hex::encode(&a.value)),
                    ..AnchoringResultBody::default()
                },
            )
        }
        AnchoringCommand::AnchorCheckpoint(j) => {
            let a = l.anchor_checkpoint(ctx, &j)?;
            decided(
                op,
                AnchoringResultBody {
                    checkpoint: a.value.as_ref().map(checkpoint_body),
                    ..AnchoringResultBody::default()
                },
            )
        }
        AnchoringCommand::CompareSettlement(j) => {
            let a = l.compare_settlement(ctx, &j)?;
            decided(
                op,
                AnchoringResultBody {
                    comparison: Some(comparison_body(&a.value)),
                    ..AnchoringResultBody::default()
                },
            )
        }
    })
}

/// `verify(disclosure)` in the in-process core.
pub fn verify<S: LeafStore, V: VaultBackend>(
    l: &Ledger<S, V>,
    caller: &Caller,
    disclosure: &str,
    clerk: Option<&dyn ClerkSigner>,
) -> QueryReply {
    let answer = || -> Result<QueryReply, Refusal> {
        let key = clerk.ok_or(Refusal::DisclosureUnsupported)?.public_key();
        let bytes = hex::decode(disclosure).map_err(|_| Refusal::DisclosureUnsupported)?;
        let r = l.verify_disclosure(caller, &bytes, key)?;
        Ok(QueryReply::Verified(VerifiedBody {
            passed: r.passed(),
            report: r.render(),
        }))
    };
    answer().unwrap_or_else(|error| QueryReply::Refused { error })
}

/// The operator's read of anchoring in the in-process core.
pub fn anchoring<S: LeafStore, V: VaultBackend>(
    l: &Ledger<S, V>,
    caller: &Caller,
    now: ledger_core::account::Time,
) -> QueryReply {
    match l.anchoring_view(caller, now) {
        Ok(s) => QueryReply::Anchoring(anchoring_body(s)),
        Err(error) => QueryReply::Refused { error },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disclosure_request_round_trips_through_its_canonical_form() {
        let body = CommandBody::Anchoring(AnchoringCommandBody::IssueDisclosure(DisclosureBody {
            subject: AccountId::mint(
                &OperationId::from_ref(ledger_kernel::posting::OperationRef([4; 16])),
                0,
            )
            .to_did(),
            recipient_identity: None,
            recipient_reference: Some(hex::encode([7u8; 32])),
            assertion: AssertionBody::FlowWithin {
                asset: "USD".into(),
                direction: "out".into(),
                from_sequence: 1,
                to_sequence: 5,
                bound: "at_most".into(),
                amount: "0100".into(),
            },
            checkpoint: Some(3),
            legal_obligation: None,
        }));
        let json = serde_json::to_value(&body).unwrap();
        assert_eq!(json["command"], "anchoring");
        let back: CommandBody = serde_json::from_value(json).unwrap();
        let c = AnchoringCommand::parse(&back).unwrap().unwrap();
        assert!(!c.is_operator());
        assert_eq!(AnchoringCommand::parse(&c.to_body()).unwrap().unwrap(), c);
    }

    #[test]
    fn err_disclosure_unsupported_for_a_shape_aledgent_does_not_make() {
        let v = serde_json::json!({"shape": "solvent", "whatever": 1});
        let a: AssertionBody = serde_json::from_value(v).unwrap();
        assert_eq!(assertion(&a), Err(Refusal::DisclosureUnsupported));
    }
}
