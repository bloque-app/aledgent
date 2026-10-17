// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The disclosure envelope (`F-017` §Disclosure issue step 5, `REQ-DS-2`) and its one canonical
//! encoding.
//!
//! A [`Disclosure`] names its subject, its recipient, its assertion, the checkpoint it is anchored
//! against, and the deployment's assurance level; it carries the kernel's claims in CipherClerk's
//! wire form, each bound to the checkpoint's kernel root; and a signature by the deployment's
//! clerk key over every other field.
//!
//! ## Encoding
//!
//! `"aledgent-disclosure"`, then the fields in declaration order: integers big-endian and fixed
//! width, byte strings and text as a `u32` length and the bytes, enumerations as a one-byte tag,
//! options as a presence byte. The signature covers `SIGNATURE_DOMAIN ‖ body`, where the body is
//! the encoding of every field but the signature. Decoding accepts that one form only: an unknown
//! tag, text that is not UTF-8, or trailing bytes is a format error, so a decoded envelope
//! re-encodes to the same bytes.
//!
//! ## Versions
//!
//! Version 2 (`F-027` T-027-04) names the venue the anchor lives in, right after the instance: a
//! tag, then the pallet's instance (`1`) or the contract's account (`2`). Version 1 has no venue
//! field; its anchor is in the settlement pallet under the instance. A version-1 envelope is still
//! read, and re-encodes to the bytes it was signed over, so a disclosure issued before the
//! deployment changed venue still verifies.
//!
//! Version 3 (`REQ-PR-10`, `DEC-25`) names the construction of the checkpoint's state commitment, one
//! byte right after the checkpoint's commitment. Versions 1 and 2 have no such field: their
//! checkpoints are construction 1 (CipherClerk's full-depth trees).
//!
//! Version 4 (`T-037-03`, `REQ-RT-15`) names the checkpoint's code binding right after the
//! construction: a presence byte, then the book's genesis, the code version and the
//! upgrade-authority mode's digest ([`crate::CodeBinding`]), which a `v3` commitment binds
//! ([`crate::commitment_v3`]). A checkpoint without one is a `v2` checkpoint, whose commitment is the
//! `v2` form. Versions 1 to 3 have no such field, and their checkpoints are all `v2` (or `v1`).

use alloc::string::String;
use alloc::vec::Vec;

use crate::claim::SignedClaim;
use crate::source::Venue;

/// The bytes every disclosure starts with.
pub const MAGIC: &[u8] = b"aledgent-disclosure";

/// The envelope version this crate writes. It also reads [`VERSION_1`] to [`VERSION_3`].
pub const VERSION: u8 = 4;

/// The third envelope version: a construction, no code binding (a `v2` checkpoint).
pub const VERSION_3: u8 = 3;

/// The second envelope version: a venue, no construction (construction 1).
pub const VERSION_2: u8 = 2;

/// The first envelope version: no venue; the anchor is in the settlement pallet.
pub const VERSION_1: u8 = 1;

/// The domain of the envelope's signature.
pub const SIGNATURE_DOMAIN: &[u8] = b"aledgent/disclosure/v1";

/// The balance kind an assertion is about (SPEC §5.4). Only `available` maps onto one kernel
/// layer (`DEC-2`); aledgent refuses the others as unsupported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum BalanceKind {
    /// The only number that decides a spend.
    Available,
    /// Held for open operations.
    Reserved,
    /// The sum of the three kinds.
    Total,
}

impl BalanceKind {
    const fn code(self) -> u8 {
        match self {
            BalanceKind::Available => 1,
            BalanceKind::Reserved => 2,
            BalanceKind::Total => 3,
        }
    }

    /// Its name.
    pub const fn name(self) -> &'static str {
        match self {
            BalanceKind::Available => "available",
            BalanceKind::Reserved => "reserved",
            BalanceKind::Total => "total",
        }
    }
}

/// Which way a flow goes, relative to the subject.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum FlowDirection {
    /// Value entering the subject.
    In,
    /// Value leaving the subject.
    Out,
}

impl FlowDirection {
    const fn code(self) -> u8 {
        match self {
            FlowDirection::In => 1,
            FlowDirection::Out => 2,
        }
    }

    /// Its name.
    pub const fn name(self) -> &'static str {
        match self {
            FlowDirection::In => "in",
            FlowDirection::Out => "out",
        }
    }
}

/// A bound on an aggregate, in minor units.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Bound {
    /// At least this much.
    AtLeast(u64),
    /// At most this much.
    AtMost(u64),
}

impl Bound {
    /// The bound's amount.
    pub const fn amount(self) -> u64 {
        match self {
            Bound::AtLeast(a) | Bound::AtMost(a) => a,
        }
    }
}

/// What a disclosure asserts about its subject (`REQ-DS-1`): one of the four shapes, in the book's
/// terms. Assets are named by their registry id; amounts are exact minor units (`INV-7`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Assertion {
    /// The subject account holds at least `amount` of `asset`, in balance kind `kind`.
    HoldsAtLeast {
        /// The asset.
        asset: String,
        /// The amount, in minor units.
        amount: u64,
        /// The balance kind.
        kind: BalanceKind,
    },
    /// The named operation occurred, on the subject account.
    OperationOccurred {
        /// The operation reference.
        operation: String,
    },
    /// The named operation moved exactly `amount` of `asset`.
    OperationMoved {
        /// The operation reference.
        operation: String,
        /// The asset.
        asset: String,
        /// The amount, in minor units.
        amount: u64,
    },
    /// The subject's flows of `asset` in `direction` over the entry sequences
    /// `[from_sequence, to_sequence]` meet `bound`.
    FlowWithin {
        /// The asset.
        asset: String,
        /// In or out.
        direction: FlowDirection,
        /// The first entry sequence of the window.
        from_sequence: u64,
        /// The last entry sequence of the window.
        to_sequence: u64,
        /// The bound.
        bound: Bound,
    },
}

/// Who a disclosure is for. Never personal data (`REQ-PV-3`): a principal of the book, by its
/// identifier, or an opaque 32-byte reference the holder chose for a party outside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Recipient {
    /// An identity of the book, `did:`-shaped.
    Identity(String),
    /// An opaque reference.
    Reference([u8; 32]),
}

/// The checkpoint a disclosure rests on: every field the anchor's commitment binds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CheckpointRef {
    /// The anchor's number on the settlement layer.
    pub number: u64,
    /// The sequence of the last entry the checkpoint covers.
    pub entry_sequence: u64,
    /// The kernel's composite root at that sequence.
    pub kernel_root: [u8; 32],
    /// The commitment the anchor carries.
    pub commitment: [u8; 32],
    /// The construction of the state commitment (`REQ-PR-10`): [`crate::CONSTRUCTION_V1`] or
    /// [`crate::CONSTRUCTION_V2`]. Bound by the commitment.
    pub construction: u8,
    /// What a `v3` checkpoint binds beside: the book's genesis, the code version and the mode
    /// (`T-037-03`). `None` for a `v2` checkpoint, and in envelopes before version 4.
    pub binding: Option<crate::CodeBinding>,
}

impl CheckpointRef {
    /// The commitment its fields produce, under the deployment's `instance`, in the form it names
    /// ([`crate::checkpoint_commitment`]).
    pub fn recomputed(&self, instance: u32) -> [u8; 32] {
        crate::checkpoint_commitment(
            self.construction,
            self.binding.as_ref(),
            instance,
            self.number,
            self.entry_sequence,
            &self.kernel_root,
        )
    }
}

/// What kind of proof a disclosure is (`REQ-PR-6`, `REQ-AR-6`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssuranceKind {
    /// A signed assertion by aledgent: evidence of its claim, not of its truth.
    SignedAssertion,
}

/// The deployment's assurance level, as a disclosure carries it (`REQ-AR-5`, `AC-I3.3`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assurance {
    /// The digest of the deployment's machine-readable assurance declaration.
    pub declaration_digest: [u8; 32],
    /// A summary of it, which says what kind of proof this is.
    pub summary: String,
    /// The kind of proof.
    pub kind: AssuranceKind,
}

/// A disclosure (`REQ-DS-2`). See the module documentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disclosure {
    /// The envelope version.
    pub version: u8,
    /// The deployment's instance on the settlement layer, which the anchor's commitment binds.
    pub instance: u32,
    /// The venue the anchor lives in (`F-027` T-027-04): where the verifier reads it. A version-1
    /// envelope carries none; its venue is the settlement pallet, under [`Disclosure::instance`].
    pub venue: Venue,
    /// The subject: a ledger account, `did:`-shaped.
    pub subject: String,
    /// The recipient.
    pub recipient: Recipient,
    /// The assertion.
    pub assertion: Assertion,
    /// The checkpoint it rests on.
    pub checkpoint: CheckpointRef,
    /// The assurance level.
    pub assurance: Assurance,
    /// When aledgent issued it, in the core's time.
    pub issued_at: u64,
    /// The kernel's claims, each in CipherClerk's wire form with `state_root = kernel_root`.
    pub claims: Vec<SignedClaim>,
    /// The clerk's signature over [`Disclosure::signed_bytes`].
    pub signature: [u8; 64],
}

/// Why bytes are not a disclosure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FormatError(pub &'static str);

impl core::fmt::Display for FormatError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "not a disclosure: {}", self.0)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for FormatError {}

struct W(Vec<u8>);

impl W {
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    fn raw(&mut self, b: &[u8]) -> &mut Self {
        self.0.extend_from_slice(b);
        self
    }
    fn bytes(&mut self, b: &[u8]) -> &mut Self {
        let n = u32::try_from(b.len()).unwrap_or(u32::MAX);
        self.u32(n).raw(b)
    }
    fn text(&mut self, s: &str) -> &mut Self {
        self.bytes(s.as_bytes())
    }
}

struct R<'a>(&'a [u8]);

impl R<'_> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], FormatError> {
        if self.0.len() < N {
            return Err(FormatError("truncated"));
        }
        let mut out = [0u8; N];
        out.copy_from_slice(&self.0[..N]);
        self.0 = &self.0[N..];
        Ok(out)
    }
    fn u8(&mut self) -> Result<u8, FormatError> {
        Ok(self.take::<1>()?[0])
    }
    fn u32(&mut self) -> Result<u32, FormatError> {
        Ok(u32::from_be_bytes(self.take()?))
    }
    fn u64(&mut self) -> Result<u64, FormatError> {
        Ok(u64::from_be_bytes(self.take()?))
    }
    fn bytes(&mut self) -> Result<Vec<u8>, FormatError> {
        let n = usize::try_from(self.u32()?).map_err(|_| FormatError("length"))?;
        if self.0.len() < n {
            return Err(FormatError("truncated"));
        }
        let (b, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(b.to_vec())
    }
    fn text(&mut self) -> Result<String, FormatError> {
        String::from_utf8(self.bytes()?).map_err(|_| FormatError("text is not UTF-8"))
    }
}

impl Assertion {
    fn encode(&self, w: &mut W) {
        match self {
            Assertion::HoldsAtLeast {
                asset,
                amount,
                kind,
            } => {
                w.u8(1).text(asset).u64(*amount).u8(kind.code());
            }
            Assertion::OperationOccurred { operation } => {
                w.u8(2).text(operation);
            }
            Assertion::OperationMoved {
                operation,
                asset,
                amount,
            } => {
                w.u8(3).text(operation).text(asset).u64(*amount);
            }
            Assertion::FlowWithin {
                asset,
                direction,
                from_sequence,
                to_sequence,
                bound,
            } => {
                w.u8(4)
                    .text(asset)
                    .u8(direction.code())
                    .u64(*from_sequence)
                    .u64(*to_sequence);
                match bound {
                    Bound::AtLeast(a) => w.u8(1).u64(*a),
                    Bound::AtMost(a) => w.u8(2).u64(*a),
                };
            }
        }
    }

    fn decode(r: &mut R<'_>) -> Result<Assertion, FormatError> {
        Ok(match r.u8()? {
            1 => Assertion::HoldsAtLeast {
                asset: r.text()?,
                amount: r.u64()?,
                kind: match r.u8()? {
                    1 => BalanceKind::Available,
                    2 => BalanceKind::Reserved,
                    3 => BalanceKind::Total,
                    _ => return Err(FormatError("balance kind")),
                },
            },
            2 => Assertion::OperationOccurred {
                operation: r.text()?,
            },
            3 => Assertion::OperationMoved {
                operation: r.text()?,
                asset: r.text()?,
                amount: r.u64()?,
            },
            4 => Assertion::FlowWithin {
                asset: r.text()?,
                direction: match r.u8()? {
                    1 => FlowDirection::In,
                    2 => FlowDirection::Out,
                    _ => return Err(FormatError("direction")),
                },
                from_sequence: r.u64()?,
                to_sequence: r.u64()?,
                bound: match r.u8()? {
                    1 => Bound::AtLeast(r.u64()?),
                    2 => Bound::AtMost(r.u64()?),
                    _ => return Err(FormatError("bound")),
                },
            },
            _ => return Err(FormatError("assertion")),
        })
    }
}

impl Disclosure {
    fn encode_body(&self, w: &mut W) {
        w.raw(MAGIC).u8(self.version).u32(self.instance);
        if self.version != VERSION_1 {
            match self.venue {
                Venue::Pallet { instance } => w.u8(1).u32(instance),
                Venue::Contract { account } => w.u8(2).raw(&account),
            };
        }
        w.text(&self.subject);
        match &self.recipient {
            Recipient::Identity(id) => {
                w.u8(1).text(id);
            }
            Recipient::Reference(r) => {
                w.u8(2).raw(r);
            }
        }
        self.assertion.encode(w);
        let c = &self.checkpoint;
        w.u64(c.number)
            .u64(c.entry_sequence)
            .raw(&c.kernel_root)
            .raw(&c.commitment);
        if self.version >= VERSION_3 {
            w.u8(c.construction);
        }
        if self.version >= VERSION {
            match &c.binding {
                None => {
                    w.u8(0);
                }
                Some(b) => {
                    w.u8(1).raw(&b.genesis).raw(&b.code).raw(&b.authority);
                }
            }
        }
        let a = &self.assurance;
        w.raw(&a.declaration_digest).text(&a.summary);
        match a.kind {
            AssuranceKind::SignedAssertion => w.u8(1),
        };
        w.u64(self.issued_at);
        w.u32(u32::try_from(self.claims.len()).unwrap_or(u32::MAX));
        for c in &self.claims {
            w.bytes(&c.to_wire());
        }
    }

    /// The bytes the clerk signs: `SIGNATURE_DOMAIN ‖ body`.
    pub fn signed_bytes(&self) -> Vec<u8> {
        let mut w = W(Vec::new());
        w.raw(SIGNATURE_DOMAIN);
        self.encode_body(&mut w);
        w.0
    }

    /// The canonical encoding: the body, then the signature.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = W(Vec::new());
        self.encode_body(&mut w);
        w.raw(&self.signature);
        w.0
    }

    /// Decode the one canonical encoding.
    pub fn from_bytes(bytes: &[u8]) -> Result<Disclosure, FormatError> {
        let mut r = R(bytes);
        if r.0.len() < MAGIC.len() || &r.0[..MAGIC.len()] != MAGIC {
            return Err(FormatError("magic"));
        }
        r.0 = &r.0[MAGIC.len()..];
        let version = r.u8()?;
        if !(VERSION_1..=VERSION).contains(&version) {
            return Err(FormatError("unknown version"));
        }
        let instance = r.u32()?;
        let venue = if version == VERSION_1 {
            Venue::Pallet { instance }
        } else {
            match r.u8()? {
                1 => Venue::Pallet { instance: r.u32()? },
                2 => Venue::Contract { account: r.take()? },
                _ => return Err(FormatError("venue")),
            }
        };
        let subject = r.text()?;
        let recipient = match r.u8()? {
            1 => Recipient::Identity(r.text()?),
            2 => Recipient::Reference(r.take()?),
            _ => return Err(FormatError("recipient")),
        };
        let assertion = Assertion::decode(&mut r)?;
        let mut checkpoint = CheckpointRef {
            number: r.u64()?,
            entry_sequence: r.u64()?,
            kernel_root: r.take()?,
            commitment: r.take()?,
            construction: crate::CONSTRUCTION_V1,
            binding: None,
        };
        if version >= VERSION_3 {
            checkpoint.construction = r.u8()?;
        }
        if version >= VERSION {
            checkpoint.binding = match r.u8()? {
                0 => None,
                1 => Some(crate::CodeBinding {
                    genesis: r.take()?,
                    code: r.take()?,
                    authority: r.take()?,
                }),
                _ => return Err(FormatError("code binding")),
            };
        }
        let declaration_digest = r.take()?;
        let summary = r.text()?;
        let kind = match r.u8()? {
            1 => AssuranceKind::SignedAssertion,
            _ => return Err(FormatError("assurance kind")),
        };
        let issued_at = r.u64()?;
        let n = r.u32()?;
        let mut claims = Vec::new();
        for _ in 0..n {
            let wire = r.bytes()?;
            claims.push(SignedClaim::from_wire(&wire).ok_or(FormatError("claim"))?);
        }
        let signature = r.take::<64>()?;
        if !r.0.is_empty() {
            return Err(FormatError("trailing bytes"));
        }
        Ok(Disclosure {
            version,
            instance,
            venue,
            subject,
            recipient,
            assertion,
            checkpoint,
            assurance: Assurance {
                declaration_digest,
                summary,
                kind,
            },
            issued_at,
            claims,
            signature,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::claim::Claim;
    use alloc::string::ToString;
    use alloc::vec;

    pub(crate) fn sample() -> Disclosure {
        Disclosure {
            version: VERSION,
            instance: 3,
            venue: Venue::Contract { account: [3; 32] },
            subject: "did:aledgent:account:abc".to_string(),
            recipient: Recipient::Reference([8; 32]),
            assertion: Assertion::FlowWithin {
                asset: "USD".to_string(),
                direction: FlowDirection::Out,
                from_sequence: 1,
                to_sequence: 9,
                bound: Bound::AtMost(500),
            },
            checkpoint: CheckpointRef {
                number: 2,
                entry_sequence: 9,
                kernel_root: [1; 32],
                commitment: [2; 32],
                construction: crate::CONSTRUCTION_V2,
                binding: Some(crate::CodeBinding {
                    genesis: [0x0A; 32],
                    code: [0x0B; 32],
                    authority: [0x0C; 32],
                }),
            },
            assurance: Assurance {
                declaration_digest: [4; 32],
                summary: "s".to_string(),
                kind: AssuranceKind::SignedAssertion,
            },
            issued_at: 77,
            claims: vec![SignedClaim {
                claim: Claim::TransferExisted {
                    transfer_id: [5; 16],
                    state_root: [1; 32],
                },
                signature: [6; 64],
            }],
            signature: [7; 64],
        }
    }

    #[test]
    fn an_envelope_round_trips_through_its_one_encoding() {
        let d = sample();
        let b = d.to_bytes();
        assert_eq!(Disclosure::from_bytes(&b), Ok(d.clone()));
        let mut long = b.clone();
        long.push(0);
        assert_eq!(
            Disclosure::from_bytes(&long),
            Err(FormatError("trailing bytes"))
        );
        assert!(Disclosure::from_bytes(&b[..b.len() - 1]).is_err());
        let mut v5 = b.clone();
        v5[MAGIC.len()] = 5;
        assert_eq!(
            Disclosure::from_bytes(&v5),
            Err(FormatError("unknown version"))
        );
        assert!(Disclosure::from_bytes(b"not a disclosure").is_err());
        // The signed bytes are the body under the domain, without the signature.
        let s = d.signed_bytes();
        assert!(s.starts_with(SIGNATURE_DOMAIN));
        assert_eq!(&s[SIGNATURE_DOMAIN.len()..], &b[..b.len() - 64]);
    }

    #[test]
    fn t_027_04_the_envelope_names_the_venue_and_still_reads_version_1() {
        // Version 2 names the venue right after the instance.
        let d = sample();
        let b = d.to_bytes();
        let at = MAGIC.len() + 1 + 4;
        assert_eq!(b[at], 2, "the contract's tag");
        assert_eq!(&b[at + 1..at + 33], &[3u8; 32]);
        let mut unknown = b.clone();
        unknown[at] = 9;
        assert_eq!(Disclosure::from_bytes(&unknown), Err(FormatError("venue")));
        let pallet = Disclosure {
            venue: Venue::Pallet { instance: 3 },
            ..sample()
        };
        assert_eq!(Disclosure::from_bytes(&pallet.to_bytes()), Ok(pallet));
        // Version 1 has no venue field: it reads as the pallet under its instance, and re-encodes
        // to the same bytes, so what it was signed over is unchanged.
        let mut v1 = Disclosure {
            version: VERSION_1,
            venue: Venue::Pallet { instance: 3 },
            ..sample()
        };
        v1.checkpoint.construction = crate::CONSTRUCTION_V1;
        v1.checkpoint.binding = None;
        let bytes = v1.to_bytes();
        assert_eq!(bytes.len(), b.len() - 34 - 97);
        let back = Disclosure::from_bytes(&bytes).unwrap();
        assert_eq!(back, v1);
        assert_eq!(back.to_bytes(), bytes);
    }

    /// `AC-J1.5` (`REQ-PR-10`): version 3 names the checkpoint's construction, one byte after its
    /// commitment; versions 1 and 2 read as construction 1 and re-encode to their own bytes.
    #[test]
    fn ac_j1_5_the_envelope_names_the_construction_and_still_reads_version_2() {
        let d = sample();
        let b = d.to_bytes();
        assert_eq!(
            Disclosure::from_bytes(&b).unwrap().checkpoint.construction,
            2
        );
        let mut v2 = Disclosure {
            version: VERSION_2,
            ..sample()
        };
        v2.checkpoint.construction = crate::CONSTRUCTION_V1;
        v2.checkpoint.binding = None;
        let bytes = v2.to_bytes();
        assert_eq!(bytes.len(), b.len() - 1 - 97);
        let back = Disclosure::from_bytes(&bytes).unwrap();
        assert_eq!(back, v2);
        assert_eq!(back.to_bytes(), bytes);
    }

    /// `T-037-03`: version 4 names the checkpoint's code binding after its construction, a `v2`
    /// checkpoint names none, and version 3 still reads, as a `v2` checkpoint, to its own bytes.
    #[test]
    fn t037_03_the_envelope_names_the_code_binding_and_still_reads_version_3() {
        let d = sample();
        let b = d.to_bytes();
        assert_eq!(Disclosure::from_bytes(&b), Ok(d.clone()));
        let none = Disclosure {
            checkpoint: CheckpointRef {
                binding: None,
                ..d.checkpoint
            },
            ..sample()
        };
        let nb = none.to_bytes();
        assert_eq!(nb.len(), b.len() - 96);
        assert_eq!(Disclosure::from_bytes(&nb), Ok(none.clone()));
        // The presence byte is where the two encodings first differ; neither 0 nor 1 is refused.
        let at = b.iter().zip(&nb).position(|(x, y)| x != y).unwrap();
        assert_eq!((b[at], nb[at]), (1, 0));
        let mut bad = nb.clone();
        bad[at] = 2;
        assert_eq!(
            Disclosure::from_bytes(&bad),
            Err(FormatError("code binding"))
        );
        let v3 = Disclosure {
            version: VERSION_3,
            ..none
        };
        let v3b = v3.to_bytes();
        assert_eq!(v3b.len(), nb.len() - 1);
        let back = Disclosure::from_bytes(&v3b).unwrap();
        assert_eq!(back, v3);
        assert_eq!(back.to_bytes(), v3b);
    }
}
