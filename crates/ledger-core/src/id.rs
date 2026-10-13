// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Identifiers aledgent mints (`F-003` §1, `REQ-ID-1`, `REQ-ID-2`, `REQ-ID-5`, `INV-17`).
//!
//! Every identifier has the surface form
//!
//! ```text
//! did:aledgent:<kind>:<body>
//! ```
//!
//! - `<kind>` comes from a closed registry: [`IdKind`] — `id`, `acct`, `auth`, `cred`, `op`.
//! - `<body>` is 26 characters of Crockford base32 over 16 bytes, canonical upper case, first
//!   character `0`–`7` (the two spare bits are zero).
//!
//! Inside the library each kind is its own newtype ([`IdentityId`], [`AccountId`], [`GrantId`],
//! [`CredentialId`], [`OperationId`]). There is no conversion between kinds.
//!
//! **Minting.** A body is the first 16 bytes of
//! `blake2b-256("aledgent/id/v1" ‖ kind ‖ minting operation reference ‖ ordinal)`. It is
//! deterministic on replay (`REQ-RT-3`), unique because operation references are (`F-007`), and it
//! takes no caller text, so it cannot carry personal data or anything that changes during the
//! subject's life (`REQ-ID-2`, `INV-17`). The service store refuses an identifier that is not the
//! minting of the operation recorded beside it ([`crate::state`]).
//!
//! **Parsing** checks, in this order, the prefix, the part count, the kind, that the kind is the
//! one expected, the body's length, its alphabet and its canonical form, and only then reads the
//! body (`REQ-ID-1`).
//!
//! **External names.** The opaque account reference by which value is addressed on the settlement
//! layer (`REQ-SL-7`) is derived from an [`AccountId`], never from an [`IdentityId`] (`REQ-ID-5`):
//!
//! ```
//! use ledger_core::id::{account_reference, AccountId, OperationId};
//! use ledger_kernel::posting::OperationRef;
//! let op = OperationId::from_ref(OperationRef([1; 16]));
//! let _ = account_reference(&AccountId::mint(&op, 0));
//! ```
//!
//! ```compile_fail
//! // REQ-ID-5: an identity's identifier is not an input to any external name.
//! use ledger_core::id::{account_reference, IdentityId, OperationId};
//! use ledger_kernel::posting::OperationRef;
//! let op = OperationId::from_ref(OperationRef([1; 16]));
//! let _ = account_reference(&IdentityId::mint(&op, 0));
//! ```
//!
//! ```compile_fail
//! // No conversion between kinds.
//! use ledger_core::id::{AccountId, IdentityId, OperationId};
//! use ledger_kernel::posting::OperationRef;
//! let op = OperationId::from_ref(OperationRef([1; 16]));
//! let _: AccountId = IdentityId::mint(&op, 0);
//! ```
//!
//! ```compile_fail
//! // An identifier cannot be built from caller-chosen bytes outside the crate.
//! use ledger_core::id::IdentityId;
//! let _ = IdentityId::from_bytes([0x41; 16]);
//! ```

use alloc::string::String;
use core::fmt;

use cipher_clerk::crypto::blake2b_256;
use ledger_kernel::posting::OperationRef;

/// Domain of the identifier derivation. Changing it changes every identifier.
const ID_DOMAIN: &[u8] = b"aledgent/id/v1";
/// Domain of the settlement-layer account reference (`REQ-SL-7`).
const ACCOUNT_REF_DOMAIN: &[u8] = b"aledgent/account-ref/v1";

/// The method prefix every identifier starts with.
pub const DID_PREFIX: &str = "did:aledgent:";
/// The number of `:`-separated parts of an identifier.
pub const PART_COUNT: usize = 4;
/// The length of an identifier's body.
pub const BODY_LEN: usize = 26;
/// Crockford's base32 alphabet, canonical upper case.
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The closed registry of identifier kinds (`REQ-ID-1`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IdKind {
    /// An identity (`id`).
    Identity,
    /// A ledger account (`acct`).
    Account,
    /// An authority grant (`auth`).
    Authority,
    /// A credential (`cred`).
    Credential,
    /// An operation (`op`).
    Operation,
}

impl IdKind {
    /// Every kind.
    pub const ALL: [IdKind; 5] = [
        IdKind::Identity,
        IdKind::Account,
        IdKind::Authority,
        IdKind::Credential,
        IdKind::Operation,
    ];

    /// The kind's part in the surface form.
    pub const fn tag(self) -> &'static str {
        match self {
            IdKind::Identity => "id",
            IdKind::Account => "acct",
            IdKind::Authority => "auth",
            IdKind::Credential => "cred",
            IdKind::Operation => "op",
        }
    }

    /// The stable code that enters the minting derivation.
    pub const fn code(self) -> u8 {
        match self {
            IdKind::Identity => 1,
            IdKind::Account => 2,
            IdKind::Authority => 3,
            IdKind::Credential => 4,
            IdKind::Operation => 5,
        }
    }

    /// The kind whose tag this is.
    pub fn from_tag(tag: &str) -> Option<IdKind> {
        IdKind::ALL.into_iter().find(|k| k.tag() == tag)
    }
}

/// Why a string is not an identifier of the expected kind. Each check runs before the next, and
/// the body is read only after all of them pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdError {
    /// It does not start with `did:aledgent:`.
    Prefix,
    /// It does not have exactly four `:`-separated parts.
    PartCount,
    /// The kind is not in the closed registry.
    UnknownKind,
    /// The kind is valid but not the one expected.
    WrongKind {
        /// The kind expected.
        expected: IdKind,
        /// The kind found.
        found: IdKind,
    },
    /// The body is not 26 characters long.
    Length,
    /// The body has a character outside canonical Crockford base32.
    Alphabet,
    /// The body's first character sets one of the two spare bits.
    NonCanonical,
}

/// Validate the frame of `s` (prefix, part count, kind) and return its kind and body, unread.
fn frame(s: &str) -> Result<(IdKind, &str), IdError> {
    let rest = s.strip_prefix(DID_PREFIX).ok_or(IdError::Prefix)?;
    if s.split(':').count() != PART_COUNT {
        return Err(IdError::PartCount);
    }
    let (tag, body) = rest.split_once(':').ok_or(IdError::PartCount)?;
    let kind = IdKind::from_tag(tag).ok_or(IdError::UnknownKind)?;
    Ok((kind, body))
}

/// Validate an identifier's frame and body, and return its kind. The body is checked for length,
/// alphabet and canonical form but its value is not returned.
pub fn validate(s: &str) -> Result<IdKind, IdError> {
    let (kind, body) = frame(s)?;
    check_body(body)?;
    Ok(kind)
}

fn check_body(body: &str) -> Result<(), IdError> {
    let bytes = body.as_bytes();
    if bytes.len() != BODY_LEN {
        return Err(IdError::Length);
    }
    if !bytes.iter().all(|c| ALPHABET.contains(c)) {
        return Err(IdError::Alphabet);
    }
    // 26 × 5 = 130 bits for a 128-bit value: the first character carries 3.
    if digit(bytes[0]) > 7 {
        return Err(IdError::NonCanonical);
    }
    Ok(())
}

fn digit(c: u8) -> u128 {
    ALPHABET.iter().position(|a| *a == c).unwrap_or(0) as u128
}

/// Parse an identifier of `expected` kind into its 16-byte body.
fn parse_as(s: &str, expected: IdKind) -> Result<[u8; 16], IdError> {
    let (found, body) = frame(s)?;
    if found != expected {
        return Err(IdError::WrongKind { expected, found });
    }
    check_body(body)?;
    let value = body
        .as_bytes()
        .iter()
        .fold(0u128, |acc, c| (acc << 5) | digit(*c));
    Ok(value.to_be_bytes())
}

/// Render a body as 26 characters of Crockford base32.
fn encode(body: &[u8; 16]) -> [u8; BODY_LEN] {
    let value = u128::from_be_bytes(*body);
    let mut out = [0u8; BODY_LEN];
    for (i, slot) in out.iter_mut().enumerate() {
        let shift = 5 * (BODY_LEN - 1 - i);
        *slot = ALPHABET[((value >> shift) & 31) as usize];
    }
    out
}

/// The body minted for `kind` by operation `op` at position `ordinal`.
fn mint_body(kind: IdKind, op: &OperationId, ordinal: u32) -> [u8; 16] {
    let digest = blake2b_256(ID_DOMAIN, &[&[kind.code()], &op.0, &ordinal.to_be_bytes()]);
    let mut body = [0u8; 16];
    body.copy_from_slice(&digest[..16]);
    body
}

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident, $kind:expr) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
        pub struct $name([u8; 16]);

        impl $name {
            /// The kind of this identifier.
            pub const KIND: IdKind = $kind;

            /// Parse the surface form. Every check of [`IdError`] runs before the body is read.
            pub fn parse(s: &str) -> Result<Self, IdError> {
                parse_as(s, Self::KIND).map(Self)
            }

            /// The 16-byte body.
            pub const fn as_bytes(&self) -> &[u8; 16] {
                &self.0
            }

            /// The body from stored bytes. Crate-private: an identifier enters the library only by
            /// minting or by parsing, and the store checks every stored one against its minting.
            #[allow(dead_code)]
            pub(crate) const fn from_bytes(body: [u8; 16]) -> Self {
                Self(body)
            }

            /// The surface form, `did:aledgent:<kind>:<body>`.
            pub fn to_did(&self) -> String {
                alloc::format!("{self}")
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let body = encode(&self.0);
                f.write_str(DID_PREFIX)?;
                f.write_str(Self::KIND.tag())?;
                f.write_str(":")?;
                // The alphabet is ASCII, so the body is valid UTF-8.
                f.write_str(core::str::from_utf8(&body).map_err(|_| fmt::Error)?)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(self, f)
            }
        }
    };
}

macro_rules! minted_id_type {
    ($(#[$doc:meta])* $name:ident, $kind:expr) => {
        id_type!($(#[$doc])* $name, $kind);

        impl $name {
            /// Mint the identifier that operation `op` creates at position `ordinal`.
            pub fn mint(op: &OperationId, ordinal: u32) -> Self {
                Self(mint_body(Self::KIND, op, ordinal))
            }
        }
    };
}

minted_id_type!(
    /// An identity (`did:aledgent:id:…`).
    IdentityId,
    IdKind::Identity
);
minted_id_type!(
    /// A ledger account (`did:aledgent:acct:…`).
    AccountId,
    IdKind::Account
);
minted_id_type!(
    /// An authority grant (`did:aledgent:auth:…`).
    GrantId,
    IdKind::Authority
);
minted_id_type!(
    /// A credential (`did:aledgent:cred:…`).
    CredentialId,
    IdKind::Credential
);
id_type!(
    /// An operation reference (`did:aledgent:op:…`). Its body is the kernel's
    /// [`OperationRef`]. `F-007` mints it; the other kinds are minted from it.
    OperationId,
    IdKind::Operation
);

impl OperationId {
    /// The operation reference whose body is the kernel's operation reference.
    pub const fn from_ref(op: OperationRef) -> Self {
        Self(op.0)
    }

    /// The kernel's operation reference, which the postings of this operation carry.
    pub const fn to_ref(&self) -> OperationRef {
        OperationRef(self.0)
    }
}

/// How an identifier was minted: by which operation, at which position. The store keeps it beside
/// every identifier it stores and refuses one that is not this minting (`INV-17`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Mint {
    /// The minting operation.
    pub op: OperationId,
    /// The position among the identifiers that operation minted.
    pub ordinal: u32,
}

impl Mint {
    /// A minting from its parts.
    pub const fn new(op: OperationId, ordinal: u32) -> Self {
        Self { op, ordinal }
    }
}

/// The opaque account reference by which a counterparty addresses one ledger account on the
/// settlement layer (`REQ-SL-7`): `blake2b-256("aledgent/account-ref/v1" ‖ account)`.
///
/// It takes an [`AccountId`] and nothing else, so an identity can be merged, renamed or moved
/// without changing what the outside world holds (`REQ-ID-5`).
pub fn account_reference(account: &AccountId) -> [u8; 32] {
    blake2b_256(ACCOUNT_REF_DOMAIN, &[&account.0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::string::ToString;
    use proptest::prelude::*;

    fn op(n: u8) -> OperationId {
        OperationId::from_ref(OperationRef([n; 16]))
    }

    #[test]
    fn minting_is_deterministic_and_separates_kind_op_and_ordinal() {
        let a = AccountId::mint(&op(1), 0);
        assert_eq!(a, AccountId::mint(&op(1), 0));
        assert_ne!(a.as_bytes(), IdentityId::mint(&op(1), 0).as_bytes());
        assert_ne!(a, AccountId::mint(&op(2), 0));
        assert_ne!(a, AccountId::mint(&op(1), 1));
    }

    #[test]
    fn surface_form_has_the_grammar() {
        let id = IdentityId::mint(&op(3), 0).to_did();
        assert!(id.starts_with("did:aledgent:id:"));
        assert_eq!(id.len(), DID_PREFIX.len() + 3 + BODY_LEN);
        assert_eq!(validate(&id), Ok(IdKind::Identity));
        let body = id.rsplit(':').next().unwrap();
        assert!(body.bytes().all(|c| ALPHABET.contains(&c)));
        assert!(digit(body.as_bytes()[0]) <= 7);
    }

    #[test]
    fn every_check_runs_before_the_body_is_read() {
        let good = AccountId::mint(&op(9), 4).to_did();
        let body = good.rsplit(':').next().unwrap().to_string();
        let cases = [
            (format!("did:other:acct:{body}"), IdError::Prefix),
            (format!("urn:aledgent:acct:{body}"), IdError::Prefix),
            (format!("did:aledgent:acct:{body}:x"), IdError::PartCount),
            (format!("did:aledgent:acct{body}"), IdError::PartCount),
            (format!("did:aledgent:pocket:{body}"), IdError::UnknownKind),
            (
                format!("did:aledgent:id:{body}"),
                IdError::WrongKind {
                    expected: IdKind::Account,
                    found: IdKind::Identity,
                },
            ),
            (format!("did:aledgent:acct:{}", &body[1..]), IdError::Length),
            (
                format!("did:aledgent:acct:{}", body.to_lowercase()),
                IdError::Alphabet,
            ),
            (
                format!("did:aledgent:acct:U{}", &body[1..]),
                IdError::Alphabet,
            ),
            (
                format!("did:aledgent:acct:8{}", &body[1..]),
                IdError::NonCanonical,
            ),
        ];
        for (s, want) in cases {
            assert_eq!(AccountId::parse(&s), Err(want), "{s}");
        }
        assert_eq!(AccountId::parse(&good).unwrap().to_did(), good);
    }

    #[test]
    fn account_reference_depends_on_the_account_only() {
        let a = AccountId::mint(&op(1), 0);
        assert_eq!(account_reference(&a), account_reference(&a));
        assert_ne!(
            account_reference(&a),
            account_reference(&AccountId::mint(&op(1), 1))
        );
    }

    fn any_kind() -> impl Strategy<Value = IdKind> {
        prop::sample::select(IdKind::ALL.to_vec())
    }

    fn render(kind: IdKind, body: [u8; 16]) -> String {
        match kind {
            IdKind::Identity => IdentityId(body).to_did(),
            IdKind::Account => AccountId(body).to_did(),
            IdKind::Authority => GrantId(body).to_did(),
            IdKind::Credential => CredentialId(body).to_did(),
            IdKind::Operation => OperationId(body).to_did(),
        }
    }

    fn parse_body(kind: IdKind, s: &str) -> Result<[u8; 16], IdError> {
        match kind {
            IdKind::Identity => IdentityId::parse(s).map(|i| i.0),
            IdKind::Account => AccountId::parse(s).map(|i| i.0),
            IdKind::Authority => GrantId::parse(s).map(|i| i.0),
            IdKind::Credential => CredentialId::parse(s).map(|i| i.0),
            IdKind::Operation => OperationId::parse(s).map(|i| i.0),
        }
    }

    proptest! {
        // REQ-ID-1: parse(mint(x)) == x, for every kind.
        #[test]
        fn req_id_1_parse_of_render_is_identity(kind in any_kind(), body in any::<[u8; 16]>(), ord in any::<u32>()) {
            let s = render(kind, body);
            prop_assert_eq!(parse_body(kind, &s), Ok(body));
            prop_assert_eq!(validate(&s), Ok(kind));
            let minted = AccountId::mint(&OperationId(body), ord);
            prop_assert_eq!(AccountId::parse(&minted.to_did()), Ok(minted));
        }

        // REQ-ID-1: a malformed identifier of any kind is refused, whatever its body.
        #[test]
        fn req_id_1_malformed_ids_are_refused(kind in any_kind(), body in any::<[u8; 16]>(), junk in "[ -~]{0,40}", cut in 0usize..26) {
            let s = render(kind, body);
            let b = s.rsplit(':').next().unwrap();
            let tag = kind.tag();
            // A prefix other than the method's.
            let bad_prefix = format!("did:x{}", &s[4..]);
            prop_assert_eq!(parse_body(kind, &bad_prefix), Err(IdError::Prefix));
            // A truncated body.
            let short = format!("{DID_PREFIX}{tag}:{}", &b[..cut]);
            prop_assert_eq!(parse_body(kind, &short), Err(IdError::Length));
            // Anything a caller types is either refused or a well-formed identifier of the kind.
            if let Ok(parsed) = parse_body(kind, &junk) {
                prop_assert_eq!(render(kind, parsed), junk);
            }
            // Every other kind refuses it before reading the body.
            for other in IdKind::ALL {
                if other != kind {
                    prop_assert_eq!(
                        parse_body(other, &s),
                        Err(IdError::WrongKind { expected: other, found: kind })
                    );
                }
            }
        }
    }
}
