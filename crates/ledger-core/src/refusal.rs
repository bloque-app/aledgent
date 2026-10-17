// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The closed list of refusals (SPEC §10, `REQ-ER-1`, `REQ-CS-4`).
//!
//! Every command in `ledger-core` returns `Result<_, Refusal>`, so a refusal
//! can never travel inside a success body. Each variant is one `ERR-*` of
//! SPEC §10, with its identifier ([`Refusal::code`]) and its published
//! condition ([`Refusal::description`]) verbatim.
//!
//! The conformance suite (`F-015`) checks this enum against its catalogue,
//! which is generated from SPEC §10, in both directions: a variant with no
//! catalogue entry, a catalogue entry with no variant, or a description that
//! differs from the published one fails the build.
//!
//! ## Serialised shape
//!
//! With the `serde` feature a refusal serialises as an object carrying its
//! identifier and its published description:
//!
//! ```json
//! { "error": "ERR-ZeroAmount",
//!   "description": "A zero amount was submitted where a movement was intended" }
//! ```
//!
//! Deserialisation is as strict as the list is closed: both fields are
//! required, an unknown identifier or description is refused, the two must
//! name the same refusal, and no other field is accepted. The wire shape of
//! the command surface is `F-009`'s; this is the library's half of the
//! refusal-shape test (`REQ-CS-4`).

/// Every refusal aledgent can return: the errors of SPEC §10, and nothing else.
///
/// The list is closed (`REQ-ER-1`). The enum is deliberately not
/// `#[non_exhaustive]`: adding or removing a variant must break every
/// exhaustive match and the conformance suite's two-way check against its
/// catalogue, so the list cannot change without SPEC §10 changing with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Refusal {
    /// `ERR-NotAuthorised`: The initiating principal holds no authority that covers this operation, account, asset or bound.
    NotAuthorised,
    /// `ERR-AuthorityRevoked`: The authority relied on has been revoked.
    AuthorityRevoked,
    /// `ERR-CredentialInvalid`: The credential presented is invalid, expired, or of the wrong kind.
    CredentialInvalid,
    /// `ERR-RecoveryNotElapsed`: A recovery or succession was completed before its delay elapsed (`REQ-AU-16`).
    RecoveryNotElapsed,
    /// `ERR-UnboundedGrant`: An authority grant was submitted without a bound (`REQ-AU-2`).
    UnboundedGrant,
    /// `ERR-BoundExceeded`: The operation would exceed a bound on the authority used.
    BoundExceeded,
    /// `ERR-UnknownAccount`: No such ledger account exists, or this principal cannot reach it.
    UnknownAccount,
    /// `ERR-AccountFrozen`: The account is frozen.
    AccountFrozen,
    /// `ERR-AccountClosed`: The account is closed.
    AccountClosed,
    /// `ERR-AccountNotEmpty`: Closure attempted with a non-zero balance (`REQ-LA-4`).
    AccountNotEmpty,
    /// `ERR-IdentityRestricted`: The owning identity is not allowed to transact.
    IdentityRestricted,
    /// `ERR-UnknownAsset`: The asset is not in the registry (`REQ-AS-1`).
    UnknownAsset,
    /// `ERR-AssetDeprecated`: The asset is registered but no longer accepts new operations.
    AssetDeprecated,
    /// `ERR-PrecisionExceeded`: The amount has more precision than the asset (`REQ-AS-4`).
    PrecisionExceeded,
    /// `ERR-ZeroAmount`: A zero amount was submitted where a movement was intended.
    ZeroAmount,
    /// `ERR-InsufficientAvailable`: The available balance does not cover the operation (`INV-3`).
    InsufficientAvailable,
    /// `ERR-AmountMismatch`: A capture, or an observed external execution, differs from the reservation it belongs to (`INV-14`).
    AmountMismatch,
    /// `ERR-ReleaseUnsafe`: A release was attempted while the external leg could still execute (`REQ-OP-9`).
    ReleaseUnsafe,
    /// `ERR-OperationTerminal`: The operation has already reached a terminal state (`INV-21`).
    OperationTerminal,
    /// `ERR-InvalidTransition`: The requested state transition is not permitted (§5.6).
    InvalidTransition,
    /// `ERR-UnknownOperation`: No operation with this reference.
    UnknownOperation,
    /// `ERR-DeadlineRequired`: A non-terminal operation was submitted without a deadline (`INV-15`).
    DeadlineRequired,
    /// `ERR-ClientKeyReused`: The same client key was submitted with a different request (`REQ-OP-10`).
    ClientKeyReused,
    /// `ERR-BatchTooLarge`: A multi-leg submission is larger than the declared maximum.
    BatchTooLarge,
    /// `ERR-UnsupportedLeg`: The settlement adapter does not support this operation or asset (`REQ-SA-1`).
    UnsupportedLeg,
    /// `ERR-SettlementLayerUnavailable`: The settlement adapter cannot be reached, or refuses to serve (`REQ-SA-2`).
    SettlementLayerUnavailable,
    /// `ERR-DecisionUnavailable`: The command cannot be decided now. Only a command answered with its decision gets it (`REQ-CS-8`). The command is kept; retrying with the same client key is safe and returns its decision (`REQ-CS-7`).
    DecisionUnavailable,
    /// `ERR-ObservationDuplicate`: An observation with this coordinate has already been applied (`INV-12`).
    ObservationDuplicate,
    /// `ERR-ObservationUnassociated`: An observation is valid but relates to no operation aledgent holds.
    ObservationUnassociated,
    /// `ERR-EvidenceInsufficient`: The evidence supplied does not establish the outcome claimed (`INV-11`).
    EvidenceInsufficient,
    /// `ERR-JustificationRequired`: An operator action was submitted without a justification.
    JustificationRequired,
    /// `ERR-ApprovalRequired`: A reversal, or a release without proof (`REQ-RC-5` step 3), was submitted without a second, distinct principal approving it (`REQ-OS-3`).
    ApprovalRequired,
    /// `ERR-DisclosureUnsupported`: aledgent cannot make the assertion requested.
    DisclosureUnsupported,
    /// `ERR-DisclosureFalse`: aledgent can determine locally that the assertion is false (`REQ-DS-4`).
    DisclosureFalse,
    /// `ERR-NotDisclosable`: The subject has not authorised this disclosure, and no legal obligation applies.
    NotDisclosable,
    /// `ERR-AnchorNotFinal`: An anchor was cited whose publication has not reached finality (`REQ-PR-8`).
    AnchorNotFinal,
    /// `ERR-QueryUnbounded`: A query was submitted without bounds that aledgent can serve (`REQ-CS-5`).
    QueryUnbounded,
    /// `ERR-MalformedCommand`: The request does not have a shape aledgent accepts: a field unknown or of the wrong type, a field a command requires and lacks, a value that does not parse, or a body over its bound (`REQ-ER-3`).
    MalformedCommand,
    /// `ERR-InitialStateNotSealed`: The initial state committed at the book's genesis is not yet in place and checked, and the book takes no command until it is sealed (`REQ-RC-6`).
    InitialStateNotSealed,
    /// `ERR-IdentifierArchived`: The identifier is one a prior system issued that the book's initial state deliberately left out: it names no ledger account and never will (`REQ-RC-7`).
    IdentifierArchived,
    /// `ERR-Internal`: An unexpected fault inside aledgent. No detail is returned to the caller, and an incident is raised (`REQ-ER-2`).
    Internal,
}

impl Refusal {
    /// Every variant, in the order of SPEC §10.
    pub const ALL: [Refusal; 41] = [
        Refusal::NotAuthorised,
        Refusal::AuthorityRevoked,
        Refusal::CredentialInvalid,
        Refusal::RecoveryNotElapsed,
        Refusal::UnboundedGrant,
        Refusal::BoundExceeded,
        Refusal::UnknownAccount,
        Refusal::AccountFrozen,
        Refusal::AccountClosed,
        Refusal::AccountNotEmpty,
        Refusal::IdentityRestricted,
        Refusal::UnknownAsset,
        Refusal::AssetDeprecated,
        Refusal::PrecisionExceeded,
        Refusal::ZeroAmount,
        Refusal::InsufficientAvailable,
        Refusal::AmountMismatch,
        Refusal::ReleaseUnsafe,
        Refusal::OperationTerminal,
        Refusal::InvalidTransition,
        Refusal::UnknownOperation,
        Refusal::DeadlineRequired,
        Refusal::ClientKeyReused,
        Refusal::BatchTooLarge,
        Refusal::UnsupportedLeg,
        Refusal::SettlementLayerUnavailable,
        Refusal::DecisionUnavailable,
        Refusal::ObservationDuplicate,
        Refusal::ObservationUnassociated,
        Refusal::EvidenceInsufficient,
        Refusal::JustificationRequired,
        Refusal::ApprovalRequired,
        Refusal::DisclosureUnsupported,
        Refusal::DisclosureFalse,
        Refusal::NotDisclosable,
        Refusal::AnchorNotFinal,
        Refusal::QueryUnbounded,
        Refusal::MalformedCommand,
        Refusal::InitialStateNotSealed,
        Refusal::IdentifierArchived,
        Refusal::Internal,
    ];

    /// The SPEC §10 identifier, e.g. `"ERR-ZeroAmount"`.
    pub const fn code(self) -> &'static str {
        match self {
            Refusal::NotAuthorised => "ERR-NotAuthorised",
            Refusal::AuthorityRevoked => "ERR-AuthorityRevoked",
            Refusal::CredentialInvalid => "ERR-CredentialInvalid",
            Refusal::RecoveryNotElapsed => "ERR-RecoveryNotElapsed",
            Refusal::UnboundedGrant => "ERR-UnboundedGrant",
            Refusal::BoundExceeded => "ERR-BoundExceeded",
            Refusal::UnknownAccount => "ERR-UnknownAccount",
            Refusal::AccountFrozen => "ERR-AccountFrozen",
            Refusal::AccountClosed => "ERR-AccountClosed",
            Refusal::AccountNotEmpty => "ERR-AccountNotEmpty",
            Refusal::IdentityRestricted => "ERR-IdentityRestricted",
            Refusal::UnknownAsset => "ERR-UnknownAsset",
            Refusal::AssetDeprecated => "ERR-AssetDeprecated",
            Refusal::PrecisionExceeded => "ERR-PrecisionExceeded",
            Refusal::ZeroAmount => "ERR-ZeroAmount",
            Refusal::InsufficientAvailable => "ERR-InsufficientAvailable",
            Refusal::AmountMismatch => "ERR-AmountMismatch",
            Refusal::ReleaseUnsafe => "ERR-ReleaseUnsafe",
            Refusal::OperationTerminal => "ERR-OperationTerminal",
            Refusal::InvalidTransition => "ERR-InvalidTransition",
            Refusal::UnknownOperation => "ERR-UnknownOperation",
            Refusal::DeadlineRequired => "ERR-DeadlineRequired",
            Refusal::ClientKeyReused => "ERR-ClientKeyReused",
            Refusal::BatchTooLarge => "ERR-BatchTooLarge",
            Refusal::UnsupportedLeg => "ERR-UnsupportedLeg",
            Refusal::SettlementLayerUnavailable => "ERR-SettlementLayerUnavailable",
            Refusal::DecisionUnavailable => "ERR-DecisionUnavailable",
            Refusal::ObservationDuplicate => "ERR-ObservationDuplicate",
            Refusal::ObservationUnassociated => "ERR-ObservationUnassociated",
            Refusal::EvidenceInsufficient => "ERR-EvidenceInsufficient",
            Refusal::JustificationRequired => "ERR-JustificationRequired",
            Refusal::ApprovalRequired => "ERR-ApprovalRequired",
            Refusal::DisclosureUnsupported => "ERR-DisclosureUnsupported",
            Refusal::DisclosureFalse => "ERR-DisclosureFalse",
            Refusal::NotDisclosable => "ERR-NotDisclosable",
            Refusal::AnchorNotFinal => "ERR-AnchorNotFinal",
            Refusal::QueryUnbounded => "ERR-QueryUnbounded",
            Refusal::MalformedCommand => "ERR-MalformedCommand",
            Refusal::InitialStateNotSealed => "ERR-InitialStateNotSealed",
            Refusal::IdentifierArchived => "ERR-IdentifierArchived",
            Refusal::Internal => "ERR-Internal",
        }
    }

    /// The published condition, verbatim from SPEC §10.
    pub const fn description(self) -> &'static str {
        match self {
            Refusal::NotAuthorised => {
                "The initiating principal holds no authority that covers this operation, account, asset or bound"
            }
            Refusal::AuthorityRevoked => "The authority relied on has been revoked",
            Refusal::CredentialInvalid => {
                "The credential presented is invalid, expired, or of the wrong kind"
            }
            Refusal::RecoveryNotElapsed => {
                "A recovery or succession was completed before its delay elapsed (`REQ-AU-16`)"
            }
            Refusal::UnboundedGrant => {
                "An authority grant was submitted without a bound (`REQ-AU-2`)"
            }
            Refusal::BoundExceeded => "The operation would exceed a bound on the authority used",
            Refusal::UnknownAccount => {
                "No such ledger account exists, or this principal cannot reach it"
            }
            Refusal::AccountFrozen => "The account is frozen",
            Refusal::AccountClosed => "The account is closed",
            Refusal::AccountNotEmpty => "Closure attempted with a non-zero balance (`REQ-LA-4`)",
            Refusal::IdentityRestricted => "The owning identity is not allowed to transact",
            Refusal::UnknownAsset => "The asset is not in the registry (`REQ-AS-1`)",
            Refusal::AssetDeprecated => {
                "The asset is registered but no longer accepts new operations"
            }
            Refusal::PrecisionExceeded => {
                "The amount has more precision than the asset (`REQ-AS-4`)"
            }
            Refusal::ZeroAmount => "A zero amount was submitted where a movement was intended",
            Refusal::InsufficientAvailable => {
                "The available balance does not cover the operation (`INV-3`)"
            }
            Refusal::AmountMismatch => {
                "A capture, or an observed external execution, differs from the reservation it belongs to (`INV-14`)"
            }
            Refusal::ReleaseUnsafe => {
                "A release was attempted while the external leg could still execute (`REQ-OP-9`)"
            }
            Refusal::OperationTerminal => {
                "The operation has already reached a terminal state (`INV-21`)"
            }
            Refusal::InvalidTransition => "The requested state transition is not permitted (§5.6)",
            Refusal::UnknownOperation => "No operation with this reference",
            Refusal::DeadlineRequired => {
                "A non-terminal operation was submitted without a deadline (`INV-15`)"
            }
            Refusal::ClientKeyReused => {
                "The same client key was submitted with a different request (`REQ-OP-10`)"
            }
            Refusal::BatchTooLarge => "A multi-leg submission is larger than the declared maximum",
            Refusal::UnsupportedLeg => {
                "The settlement adapter does not support this operation or asset (`REQ-SA-1`)"
            }
            Refusal::SettlementLayerUnavailable => {
                "The settlement adapter cannot be reached, or refuses to serve (`REQ-SA-2`)"
            }
            Refusal::DecisionUnavailable => {
                "The command cannot be decided now. Only a command answered with its decision gets it (`REQ-CS-8`). The command is kept; retrying with the same client key is safe and returns its decision (`REQ-CS-7`)"
            }
            Refusal::ObservationDuplicate => {
                "An observation with this coordinate has already been applied (`INV-12`)"
            }
            Refusal::ObservationUnassociated => {
                "An observation is valid but relates to no operation aledgent holds"
            }
            Refusal::EvidenceInsufficient => {
                "The evidence supplied does not establish the outcome claimed (`INV-11`)"
            }
            Refusal::JustificationRequired => {
                "An operator action was submitted without a justification"
            }
            Refusal::ApprovalRequired => {
                "A reversal, or a release without proof (`REQ-RC-5` step 3), was submitted without a second, distinct principal approving it (`REQ-OS-3`)"
            }
            Refusal::DisclosureUnsupported => "aledgent cannot make the assertion requested",
            Refusal::DisclosureFalse => {
                "aledgent can determine locally that the assertion is false (`REQ-DS-4`)"
            }
            Refusal::NotDisclosable => {
                "The subject has not authorised this disclosure, and no legal obligation applies"
            }
            Refusal::AnchorNotFinal => {
                "An anchor was cited whose publication has not reached finality (`REQ-PR-8`)"
            }
            Refusal::QueryUnbounded => {
                "A query was submitted without bounds that aledgent can serve (`REQ-CS-5`)"
            }
            Refusal::MalformedCommand => {
                "The request does not have a shape aledgent accepts: a field unknown or of the wrong type, a field a command requires and lacks, a value that does not parse, or a body over its bound (`REQ-ER-3`)"
            }
            Refusal::InitialStateNotSealed => {
                "The initial state committed at the book's genesis is not yet in place and checked, and the book takes no command until it is sealed (`REQ-RC-6`)"
            }
            Refusal::IdentifierArchived => {
                "The identifier is one a prior system issued that the book's initial state deliberately left out: it names no ledger account and never will (`REQ-RC-7`)"
            }
            Refusal::Internal => {
                "An unexpected fault inside aledgent. No detail is returned to the caller, and an incident is raised (`REQ-ER-2`)"
            }
        }
    }
}

impl Refusal {
    /// The refusal with this SPEC §10 identifier. `None` for anything the
    /// closed list does not name.
    pub fn from_code(code: &str) -> Option<Refusal> {
        Refusal::ALL.into_iter().find(|r| r.code() == code)
    }

    /// The refusal with this published description. Descriptions are unique.
    pub fn from_description(description: &str) -> Option<Refusal> {
        Refusal::ALL
            .into_iter()
            .find(|r| r.description() == description)
    }
}

impl core::fmt::Display for Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}: {}", self.code(), self.description())
    }
}

impl core::error::Error for Refusal {}

#[cfg(feature = "serde")]
mod shape {
    use super::Refusal;
    use core::fmt;
    use serde::de::{self, MapAccess, Visitor};
    use serde::ser::SerializeStruct;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    const FIELDS: &[&str] = &["error", "description"];

    impl Serialize for Refusal {
        fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
            let mut s = serializer.serialize_struct("Refusal", 2)?;
            s.serialize_field("error", self.code())?;
            s.serialize_field("description", self.description())?;
            s.end()
        }
    }

    /// Which field a key names.
    enum Field {
        Error,
        Description,
    }

    impl<'de> Deserialize<'de> for Field {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct FieldVisitor;
            impl Visitor<'_> for FieldVisitor {
                type Value = Field;
                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("`error` or `description`")
                }
                fn visit_str<E: de::Error>(self, v: &str) -> Result<Field, E> {
                    match v {
                        "error" => Ok(Field::Error),
                        "description" => Ok(Field::Description),
                        other => Err(E::unknown_field(other, FIELDS)),
                    }
                }
            }
            deserializer.deserialize_identifier(FieldVisitor)
        }
    }

    /// A string that must be one of the closed list's identifiers.
    struct Code(Refusal);

    impl<'de> Deserialize<'de> for Code {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct CodeVisitor;
            impl Visitor<'_> for CodeVisitor {
                type Value = Code;
                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("an ERR-* identifier of SPEC §10")
                }
                fn visit_str<E: de::Error>(self, v: &str) -> Result<Code, E> {
                    Refusal::from_code(v)
                        .map(Code)
                        .ok_or_else(|| E::invalid_value(de::Unexpected::Str(v), &self))
                }
            }
            deserializer.deserialize_str(CodeVisitor)
        }
    }

    /// A string that must be one of the closed list's published descriptions.
    struct Description(Refusal);

    impl<'de> Deserialize<'de> for Description {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct DescriptionVisitor;
            impl Visitor<'_> for DescriptionVisitor {
                type Value = Description;
                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("a published description of SPEC §10")
                }
                fn visit_str<E: de::Error>(self, v: &str) -> Result<Description, E> {
                    Refusal::from_description(v)
                        .map(Description)
                        .ok_or_else(|| E::invalid_value(de::Unexpected::Str(v), &self))
                }
            }
            deserializer.deserialize_str(DescriptionVisitor)
        }
    }

    impl<'de> Deserialize<'de> for Refusal {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct RefusalVisitor;
            impl<'de> Visitor<'de> for RefusalVisitor {
                type Value = Refusal;
                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("a refusal: { error, description }")
                }
                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Refusal, A::Error> {
                    let mut code: Option<Refusal> = None;
                    let mut description: Option<Refusal> = None;
                    while let Some(key) = map.next_key::<Field>()? {
                        match key {
                            Field::Error => {
                                if code.is_some() {
                                    return Err(de::Error::duplicate_field("error"));
                                }
                                code = Some(map.next_value::<Code>()?.0);
                            }
                            Field::Description => {
                                if description.is_some() {
                                    return Err(de::Error::duplicate_field("description"));
                                }
                                description = Some(map.next_value::<Description>()?.0);
                            }
                        }
                    }
                    let code = code.ok_or_else(|| de::Error::missing_field("error"))?;
                    let description =
                        description.ok_or_else(|| de::Error::missing_field("description"))?;
                    if code != description {
                        return Err(de::Error::custom(
                            "`error` and `description` name different refusals",
                        ));
                    }
                    Ok(code)
                }
                // A binary format (postcard: the core Agent's envelope, `F-005`) reads a struct
                // as its fields in order.
                fn visit_seq<A: de::SeqAccess<'de>>(self, mut seq: A) -> Result<Refusal, A::Error> {
                    let code = seq
                        .next_element::<Code>()?
                        .ok_or_else(|| de::Error::invalid_length(0, &self))?
                        .0;
                    let description = seq
                        .next_element::<Description>()?
                        .ok_or_else(|| de::Error::invalid_length(1, &self))?
                        .0;
                    if code != description {
                        return Err(de::Error::custom(
                            "`error` and `description` name different refusals",
                        ));
                    }
                    Ok(code)
                }
            }
            deserializer.deserialize_struct("Refusal", FIELDS, RefusalVisitor)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn err_list_is_closed_at_forty_one() {
        assert_eq!(Refusal::ALL.len(), 41);
        let mut codes: [&str; 41] = [""; 41];
        for (i, r) in Refusal::ALL.iter().enumerate() {
            codes[i] = r.code();
        }
        codes.sort_unstable();
        for pair in codes.windows(2) {
            assert_ne!(pair[0], pair[1], "duplicate code");
        }
    }

    #[test]
    fn err_codes_and_descriptions_look_up_both_ways() {
        for r in Refusal::ALL {
            assert!(r.code().starts_with("ERR-"));
            assert_eq!(Refusal::from_code(r.code()), Some(r));
            assert_eq!(Refusal::from_description(r.description()), Some(r));
        }
        assert_eq!(Refusal::from_code("ERR-Fault"), None);
        assert_eq!(Refusal::from_code("ZeroAmount"), None);
    }

    #[cfg(feature = "serde")]
    #[test]
    fn err_serialised_shape_carries_code_and_description() {
        for r in Refusal::ALL {
            let json = serde_json::to_value(r).unwrap();
            assert_eq!(
                json,
                serde_json::json!({ "error": r.code(), "description": r.description() })
            );
            assert_eq!(serde_json::from_value::<Refusal>(json).unwrap(), r);
        }
    }

    #[cfg(feature = "serde")]
    #[test]
    fn err_deserialisation_refuses_anything_outside_the_closed_list() {
        let zero = Refusal::ZeroAmount.description();
        let cases = [
            serde_json::json!({ "error": "ERR-Internal", "description": zero }),
            serde_json::json!({ "error": "ERR-ZeroAmount", "description": "zero" }),
            serde_json::json!({ "error": "ERR-ZeroAmount" }),
            serde_json::json!({ "description": zero }),
            serde_json::json!({ "error": "ERR-BatchTooLarge", "description": zero }),
            serde_json::json!({ "error": "ERR-ZeroAmount", "description": zero, "extra": 1 }),
            serde_json::json!("ERR-ZeroAmount"),
        ];
        for c in cases {
            assert!(serde_json::from_value::<Refusal>(c.clone()).is_err(), "{c}");
        }
    }
}
