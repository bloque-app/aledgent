// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! [`verify()`]: a recipient's check of a disclosure, without aledgent (`REQ-DS-3`, `REQ-PR-4`,
//! `AC-I2.1`, `AC-I3.1`). See the crate documentation for the steps and for what a pass means.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::claim::{Claim, Relation};
use crate::envelope::{
    Assertion, AssuranceKind, BalanceKind, Bound, Disclosure, FlowDirection, FormatError, Recipient,
};
use crate::source::AnchorSource;
use crate::{SIGNED_ASSERTION_LABEL, commitment, schnorr};

/// Why nothing could be checked at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifyError {
    /// The bytes are not a disclosure of a version this verifier reads.
    Format(FormatError),
    /// No clerk key was given, and the settlement layer publishes none for the instance.
    NoKey,
}

impl core::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            VerifyError::Format(e) => e.fmt(f),
            VerifyError::NoKey => {
                f.write_str("no clerk key was given, and the instance record publishes none")
            }
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for VerifyError {}

/// Where the clerk key the signatures were checked against came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySource {
    /// Given by the recipient, and the instance record publishes none (or could not be read).
    Given,
    /// Read from the instance record on the settlement layer.
    Published,
    /// Given by the recipient, and the same key is published.
    GivenAndPublished,
    /// Given by the recipient, and the instance record publishes **another** key: fails.
    Conflict,
}

/// What the anchor read found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnchorCheck {
    /// The anchor is stored at a finalized block with the commitment the disclosure names.
    Final {
        /// The block that stored it.
        stored_at: u64,
        /// The finalized block it was read at.
        read_at: u64,
    },
    /// No anchor of that number is final at the finalized head (`ERR-AnchorNotFinal`'s
    /// condition, seen from outside).
    Missing,
    /// The anchor of that number carries another commitment.
    Mismatch {
        /// What the settlement layer holds.
        stored: [u8; 32],
    },
    /// The settlement layer could not be read.
    Unreadable(String),
}

/// Everything [`verify()`] found. [`Report::passed`] is the verdict; [`Report::render`] is what
/// the command-line verifier prints.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    /// The disclosure, decoded.
    pub disclosure: Disclosure,
    /// The key the signatures were checked against.
    pub key: [u8; 32],
    /// Where it came from.
    pub key_source: KeySource,
    /// Whether the envelope's signature holds.
    pub envelope_signed: bool,
    /// Whether every claim's signature holds.
    pub claims_signed: bool,
    /// Whether every claim is bound to the checkpoint's kernel root.
    pub claims_bound: bool,
    /// Whether the claims say what the assertion says; the reason if not.
    pub claims_consistent: Result<(), &'static str>,
    /// Whether the commitment recomputed from the checkpoint is the one the disclosure names.
    pub commitment_recomputed: bool,
    /// The anchor read.
    pub anchor: AnchorCheck,
    /// Whether the disclosure says it is a signed assertion, in the words of `REQ-PR-6`.
    pub labelled: bool,
}

impl Report {
    /// Whether every check passed.
    pub fn passed(&self) -> bool {
        self.key_source != KeySource::Conflict
            && self.envelope_signed
            && self.claims_signed
            && self.claims_bound
            && self.claims_consistent.is_ok()
            && self.commitment_recomputed
            && matches!(self.anchor, AnchorCheck::Final { .. })
            && self.labelled
    }

    /// The report as text: the assertion, the checkpoint, the assurance level and label, and
    /// every check with its result.
    pub fn render(&self) -> String {
        let d = &self.disclosure;
        let mark = |ok: bool| if ok { "ok  " } else { "FAIL" };
        let mut out = String::new();
        out.push_str(&format!(
            "verdict: {}\n",
            if self.passed() { "PASS" } else { "FAIL" }
        ));
        out.push_str(&format!("label: {SIGNED_ASSERTION_LABEL}\n"));
        out.push_str(&format!("subject: {}\n", d.subject));
        out.push_str(&format!(
            "recipient: {}\n",
            match &d.recipient {
                Recipient::Identity(i) => i.clone(),
                Recipient::Reference(r) => format!("reference {}", hex(r)),
            }
        ));
        out.push_str(&format!("assertion: {}\n", describe(&d.assertion)));
        let c = &d.checkpoint;
        out.push_str(&format!(
            "checkpoint: anchor {} of instance {}, entries up to and including {}, kernel root {} (construction {}), commitment {}\n",
            c.number,
            d.instance,
            c.entry_sequence,
            hex(&c.kernel_root),
            c.construction,
            hex(&c.commitment)
        ));
        out.push_str(&format!(
            "venue: {}\n",
            match d.venue {
                crate::source::Venue::Pallet { instance } =>
                    format!("the settlement pallet, instance {instance}"),
                crate::source::Venue::Contract { account } =>
                    format!("the settlement contract {}", hex(&account)),
            }
        ));
        out.push_str(&format!(
            "assurance: {} (declaration {})\n",
            d.assurance.summary,
            hex(&d.assurance.declaration_digest)
        ));
        out.push_str(&format!(
            "assurance kind: {}\n",
            match d.assurance.kind {
                AssuranceKind::SignedAssertion => "signed assertion",
            }
        ));
        out.push_str(&format!("issued at: {}\n", d.issued_at));
        out.push_str(&format!(
            "[{}] clerk key {} ({})\n",
            mark(self.key_source != KeySource::Conflict),
            hex(&self.key),
            match self.key_source {
                KeySource::Given => "given",
                KeySource::Published => "published on the instance record",
                KeySource::GivenAndPublished => "given, and published on the instance record",
                KeySource::Conflict => "given, but the instance record publishes another",
            }
        ));
        out.push_str(&format!(
            "[{}] envelope signature\n",
            mark(self.envelope_signed)
        ));
        out.push_str(&format!(
            "[{}] {} claim signature(s)\n",
            mark(self.claims_signed),
            d.claims.len()
        ));
        out.push_str(&format!(
            "[{}] claims bound to the checkpoint's kernel root\n",
            mark(self.claims_bound)
        ));
        out.push_str(&format!(
            "[{}] claims say what the assertion says{}\n",
            mark(self.claims_consistent.is_ok()),
            match self.claims_consistent {
                Ok(()) => String::new(),
                Err(why) => format!(": {why}"),
            }
        ));
        out.push_str(&format!(
            "[{}] commitment recomputed from the checkpoint\n",
            mark(self.commitment_recomputed)
        ));
        out.push_str(&match &self.anchor {
            AnchorCheck::Final { stored_at, read_at } => format!(
                "[ok  ] anchor stored at block {stored_at}, read at finalized block {read_at}\n"
            ),
            AnchorCheck::Missing => {
                "[FAIL] no such anchor at the finalized head: it establishes nothing\n".into()
            }
            AnchorCheck::Mismatch { stored } => format!(
                "[FAIL] the anchor carries another commitment: {}\n",
                hex(stored)
            ),
            AnchorCheck::Unreadable(why) => {
                format!("[FAIL] the settlement layer could not be read: {why}\n")
            }
        });
        out.push_str(&format!(
            "[{}] labelled as a signed assertion\n",
            mark(self.labelled)
        ));
        out.push_str(
            "note: an anchor proves aledgent held this book state and cannot later claim another; \
             it proves nothing about solvency. A signed assertion is evidence of aledgent's claim, \
             not of its truth.\n",
        );
        out
    }
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    s
}

fn describe(a: &Assertion) -> String {
    match a {
        Assertion::HoldsAtLeast {
            asset,
            amount,
            kind,
        } => format!(
            "holds at least {amount} minor units of {asset}, {}",
            kind.name()
        ),
        Assertion::OperationOccurred { operation } => format!("operation {operation} occurred"),
        Assertion::OperationMoved {
            operation,
            asset,
            amount,
        } => format!("operation {operation} moved {amount} minor units of {asset}"),
        Assertion::FlowWithin {
            asset,
            direction,
            from_sequence,
            to_sequence,
            bound,
        } => format!(
            "flows {} of {asset} over entries {from_sequence}..={to_sequence} are {} {} minor units",
            direction.name(),
            match bound {
                Bound::AtLeast(_) => "at least",
                Bound::AtMost(_) => "at most",
            },
            bound.amount()
        ),
    }
}

/// Whether the claims say exactly what the assertion says.
fn consistent(d: &Disclosure) -> Result<(), &'static str> {
    let claims: Vec<&Claim> = d.claims.iter().map(|c| &c.claim).collect();
    match &d.assertion {
        Assertion::HoldsAtLeast { amount, kind, .. } => {
            if *kind != BalanceKind::Available {
                return Err("only an available balance maps to one kernel claim");
            }
            match claims.as_slice() {
                [
                    Claim::BalanceAtLeast {
                        layer: 0,
                        threshold,
                        ..
                    },
                ] if threshold == amount => Ok(()),
                _ => Err("one settled-layer balance claim at the asserted amount"),
            }
        }
        Assertion::OperationOccurred { .. } => {
            if !claims.is_empty()
                && claims
                    .iter()
                    .all(|c| matches!(c, Claim::TransferExisted { .. }))
            {
                Ok(())
            } else {
                Err("one or more transfer-existed claims")
            }
        }
        Assertion::OperationMoved { amount, .. } => {
            let mut sum: u64 = 0;
            if claims.is_empty() {
                return Err("one or more transfer-amount claims");
            }
            for c in &claims {
                let Claim::TransferAmountEquals { value, .. } = c else {
                    return Err("one or more transfer-amount claims");
                };
                sum = sum.checked_add(*value).ok_or("the amounts overflow")?;
            }
            if sum == *amount {
                Ok(())
            } else {
                Err("the claimed amounts do not sum to the asserted amount")
            }
        }
        Assertion::FlowWithin {
            direction,
            from_sequence,
            to_sequence,
            bound,
            ..
        } => {
            let [
                Claim::AggregateInWindow {
                    filter,
                    t_lo,
                    t_hi,
                    relation,
                    ..
                },
            ] = claims.as_slice()
            else {
                return Err("one aggregate claim");
            };
            let side = match direction {
                FlowDirection::In => 2,
                FlowDirection::Out => 1,
            };
            if filter.account.is_none() || filter.direction != Some(side) {
                return Err("the aggregate is not over the subject in the asserted direction");
            }
            if t_lo != from_sequence
                || t_hi != to_sequence
                || from_sequence > to_sequence
                || *to_sequence > d.checkpoint.entry_sequence
            {
                return Err("the window is not the asserted one, or is past the checkpoint");
            }
            let same = match (bound, relation) {
                (Bound::AtLeast(a), Relation::AtLeast(c))
                | (Bound::AtMost(a), Relation::AtMost(c)) => schnorr::public_commitment(*a) == *c,
                _ => false,
            };
            if same {
                Ok(())
            } else {
                Err("the aggregate's bound is not the asserted one")
            }
        }
    }
}

/// Check a disclosure (see the crate documentation). `clerk_key` is the deployment's clerk public
/// key if the recipient has it; otherwise the key the instance record publishes is used. If both
/// exist and differ, the check fails.
pub fn verify(
    bytes: &[u8],
    clerk_key: Option<[u8; 32]>,
    source: &impl AnchorSource,
) -> Result<Report, VerifyError> {
    // 1. Format and version.
    let d = Disclosure::from_bytes(bytes).map_err(VerifyError::Format)?;
    // The key: given, published, or both and equal.
    let published = source.attestation_key(&d.venue).ok().flatten();
    let (key, key_source) = match (clerk_key, published) {
        (Some(k), Some(p)) if k == p => (k, KeySource::GivenAndPublished),
        (Some(k), Some(_)) => (k, KeySource::Conflict),
        (Some(k), None) => (k, KeySource::Given),
        (None, Some(p)) => (p, KeySource::Published),
        (None, None) => return Err(VerifyError::NoKey),
    };
    // 2. Both signatures, and the claims' binding to the checkpoint and the assertion.
    let envelope_signed = schnorr::verify(&key, &d.signed_bytes(), &d.signature);
    let claims_signed = !d.claims.is_empty() && d.claims.iter().all(|c| c.verify(&key));
    let claims_bound = d
        .claims
        .iter()
        .all(|c| *c.claim.state_root() == d.checkpoint.kernel_root);
    let claims_consistent = consistent(&d);
    // 3. The anchor, at a finalized block, in the venue it lives in (`F-027` T-027-04).
    let c = d.checkpoint;
    let anchor = match source.anchor(&d.venue, c.number) {
        Ok(Some(a)) if a.commitment == c.commitment => AnchorCheck::Final {
            stored_at: a.stored_at,
            read_at: a.read_at,
        },
        Ok(Some(a)) => AnchorCheck::Mismatch {
            stored: a.commitment,
        },
        Ok(None) => AnchorCheck::Missing,
        Err(why) => AnchorCheck::Unreadable(why),
    };
    // 4. The commitment, recomputed.
    // The construction must be one this verifier knows (`REQ-PR-10`); the commitment binds it.
    let commitment_recomputed = crate::construction_known(c.construction)
        && commitment(
            c.construction,
            d.instance,
            c.number,
            c.entry_sequence,
            &c.kernel_root,
        ) == c.commitment;
    // 5. The label.
    let labelled = d.assurance.kind == AssuranceKind::SignedAssertion
        && d.assurance.summary.contains(SIGNED_ASSERTION_LABEL);
    Ok(Report {
        disclosure: d,
        key,
        key_source,
        envelope_signed,
        claims_signed,
        claims_bound,
        claims_consistent,
        commitment_recomputed,
        anchor,
        labelled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claim::{Filter, SignedClaim};
    use crate::envelope::{Assurance, CheckpointRef, VERSION};
    use crate::source::{MemAnchors, PublicAnchor};
    use alloc::string::ToString;
    use alloc::vec;
    use cipher_clerk::crypto::SecretKey;

    fn sign(sk: &SecretKey, msg: &[u8]) -> [u8; 64] {
        let s = sk.sign(msg, &mut rand_core::OsRng);
        let mut out = [0u8; 64];
        out[..32].copy_from_slice(&s.r);
        out[32..].copy_from_slice(&s.s);
        out
    }

    fn signed(sk: &SecretKey, claim: Claim) -> SignedClaim {
        SignedClaim {
            claim,
            signature: sign(sk, &claim.public_bytes()),
        }
    }

    fn issue(sk: &SecretKey, assertion: Assertion, claims: Vec<Claim>) -> Disclosure {
        let root = [3u8; 32];
        let mut d = Disclosure {
            version: VERSION,
            instance: 1,
            venue: crate::source::Venue::Pallet { instance: 1 },
            subject: "did:aledgent:account:x".to_string(),
            recipient: Recipient::Reference([1; 32]),
            assertion,
            checkpoint: CheckpointRef {
                number: 4,
                entry_sequence: 20,
                kernel_root: root,
                commitment: commitment(crate::CONSTRUCTION_V2, 1, 4, 20, &root),
                construction: crate::CONSTRUCTION_V2,
            },
            assurance: Assurance {
                declaration_digest: [9; 32],
                summary: format!("{SIGNED_ASSERTION_LABEL} Assurance m2."),
                kind: AssuranceKind::SignedAssertion,
            },
            issued_at: 5,
            claims: claims.into_iter().map(|c| signed(sk, c)).collect(),
            signature: [0; 64],
        };
        d.signature = sign(sk, &d.signed_bytes());
        d
    }

    fn chain(d: &Disclosure) -> MemAnchors {
        let mut m = MemAnchors::new();
        m.put(
            1,
            d.checkpoint.number,
            PublicAnchor {
                commitment: d.checkpoint.commitment,
                stored_at: 10,
                read_at: 12,
            },
        );
        m
    }

    fn holds(sk: &SecretKey) -> Disclosure {
        issue(
            sk,
            Assertion::HoldsAtLeast {
                asset: "USD".into(),
                amount: 50,
                kind: BalanceKind::Available,
            },
            vec![Claim::BalanceAtLeast {
                account: [2; 16],
                layer: 0,
                threshold: 50,
                state_root: [3; 32],
            }],
        )
    }

    #[test]
    fn ac_i2_1_a_disclosure_verifies_against_the_anchor_alone() {
        let sk = SecretKey::from_bytes([5; 32]).unwrap();
        let pk = sk.public_key().0;
        let d = holds(&sk);
        let r = verify(&d.to_bytes(), Some(pk), &chain(&d)).unwrap();
        assert!(r.passed(), "{}", r.render());
        assert_eq!(r.key_source, KeySource::Given);
        let text = r.render();
        assert!(text.contains(SIGNED_ASSERTION_LABEL));
        assert!(text.contains("verdict: PASS"));
        // The key read from the instance record works the same way.
        let mut src = chain(&d);
        src.publish_key(1, pk);
        let r = verify(&d.to_bytes(), None, &src).unwrap();
        assert!(r.passed());
        assert_eq!(r.key_source, KeySource::Published);
        // Another published key is a conflict.
        let mut src = chain(&d);
        src.publish_key(1, SecretKey::from_bytes([6; 32]).unwrap().public_key().0);
        assert!(!verify(&d.to_bytes(), Some(pk), &src).unwrap().passed());
        assert_eq!(
            verify(&d.to_bytes(), None, &MemAnchors::new()),
            Err(VerifyError::NoKey)
        );
    }

    #[test]
    fn t_027_04_the_verifier_finds_an_anchor_in_either_venue() {
        use crate::source::Venue;
        let sk = SecretKey::from_bytes([5; 32]).unwrap();
        let pk = sk.public_key().0;
        let contract = Venue::Contract {
            account: [0xc0; 32],
        };
        let anchor = |d: &Disclosure| PublicAnchor {
            commitment: d.checkpoint.commitment,
            stored_at: 10,
            read_at: 12,
        };
        // Anchored in the contract: read there, with the key the contract publishes.
        let mut d = holds(&sk);
        d.venue = contract;
        d.signature = sign(&sk, &d.signed_bytes());
        let mut src = MemAnchors::new();
        src.put_in(contract, d.checkpoint.number, anchor(&d))
            .publish_key_in(contract, pk);
        let r = verify(&d.to_bytes(), None, &src).unwrap();
        assert!(r.passed(), "{}", r.render());
        assert!(r.render().contains("the settlement contract"));
        // The same anchor in the pallet does not stand for it: each is read where it lives.
        let r = verify(&d.to_bytes(), Some(pk), &chain(&d)).unwrap();
        assert_eq!(r.anchor, AnchorCheck::Missing);
        // A disclosure anchored in the pallet before the change of venue still verifies there.
        let old = holds(&sk);
        let mut both = chain(&old);
        both.put_in(contract, old.checkpoint.number, anchor(&old));
        assert!(verify(&old.to_bytes(), Some(pk), &both).unwrap().passed());
        // The venue is signed over: moving it breaks the signature.
        let mut moved = d.clone();
        moved.venue = Venue::Pallet { instance: 1 };
        let r = verify(&moved.to_bytes(), Some(pk), &chain(&moved)).unwrap();
        assert!(!r.envelope_signed && !r.passed());
    }

    #[test]
    fn t_017_06_a_tampered_disclosure_fails() {
        let sk = SecretKey::from_bytes([5; 32]).unwrap();
        let pk = sk.public_key().0;
        let d = holds(&sk);
        let src = chain(&d);
        // Any field changed after signing.
        let mut t = d.clone();
        t.assertion = Assertion::HoldsAtLeast {
            asset: "USD".into(),
            amount: 5_000,
            kind: BalanceKind::Available,
        };
        let r = verify(&t.to_bytes(), Some(pk), &src).unwrap();
        assert!(!r.envelope_signed && !r.passed());
        // Every byte flipped, one at a time: never a pass.
        let bytes = d.to_bytes();
        for i in 0..bytes.len() {
            let mut b = bytes.clone();
            b[i] ^= 0x01;
            if let Ok(r) = verify(&b, Some(pk), &src) {
                assert!(!r.passed(), "byte {i} flipped still passes");
            }
        }
        // A claim re-signed for another amount, envelope re-signed: the claim no longer says
        // what the assertion says.
        let t = issue(
            &sk,
            d.assertion.clone(),
            vec![Claim::BalanceAtLeast {
                account: [2; 16],
                layer: 0,
                threshold: 49,
                state_root: [3; 32],
            }],
        );
        assert!(
            verify(&t.to_bytes(), Some(pk), &chain(&t))
                .unwrap()
                .claims_consistent
                .is_err()
        );
        // Signed by another key.
        let other = SecretKey::from_bytes([6; 32]).unwrap();
        let t = holds(&other);
        let r = verify(&t.to_bytes(), Some(pk), &chain(&t)).unwrap();
        assert!(!r.envelope_signed && !r.claims_signed);
    }

    #[test]
    fn ac_j1_3_an_anchor_that_is_not_final_establishes_nothing() {
        let sk = SecretKey::from_bytes([5; 32]).unwrap();
        let pk = sk.public_key().0;
        let d = holds(&sk);
        let r = verify(&d.to_bytes(), Some(pk), &MemAnchors::new()).unwrap();
        assert_eq!(r.anchor, AnchorCheck::Missing);
        assert!(!r.passed());
        let mut other = MemAnchors::new();
        other.put(
            1,
            4,
            PublicAnchor {
                commitment: [0xee; 32],
                stored_at: 1,
                read_at: 2,
            },
        );
        let r = verify(&d.to_bytes(), Some(pk), &other).unwrap();
        assert!(matches!(r.anchor, AnchorCheck::Mismatch { .. }));
        let mut down = chain(&d);
        down.fail("unreachable");
        let r = verify(&d.to_bytes(), Some(pk), &down).unwrap();
        assert!(matches!(r.anchor, AnchorCheck::Unreadable(_)) && !r.passed());
        // A checkpoint whose fields do not produce its commitment.
        let mut t = d.clone();
        t.checkpoint.entry_sequence = 21;
        t.signature = sign(&sk, &t.signed_bytes());
        let r = verify(&t.to_bytes(), Some(pk), &chain(&t)).unwrap();
        assert!(!r.commitment_recomputed && !r.passed());
        // `AC-J1.5` (`REQ-PR-10`): the same checkpoint with another construction named does not
        // match the anchor, and a construction this verifier does not know is not recomputed.
        for c in [crate::CONSTRUCTION_V1, 9] {
            let mut t = d.clone();
            t.checkpoint.construction = c;
            t.signature = sign(&sk, &t.signed_bytes());
            let r = verify(&t.to_bytes(), Some(pk), &chain(&d)).unwrap();
            assert!(!r.commitment_recomputed && !r.passed(), "construction {c}");
        }
    }

    #[test]
    fn ac_i2_3_an_unlabelled_disclosure_fails() {
        let sk = SecretKey::from_bytes([5; 32]).unwrap();
        let pk = sk.public_key().0;
        let mut d = holds(&sk);
        d.assurance.summary = "Proven.".to_string();
        d.signature = sign(&sk, &d.signed_bytes());
        let r = verify(&d.to_bytes(), Some(pk), &chain(&d)).unwrap();
        assert!(!r.labelled && !r.passed());
    }

    #[test]
    fn every_shape_is_checked_for_consistency() {
        let sk = SecretKey::from_bytes([5; 32]).unwrap();
        let pk = sk.public_key().0;
        let root = [3u8; 32];
        let moved = issue(
            &sk,
            Assertion::OperationMoved {
                operation: "did:aledgent:operation:o".into(),
                asset: "USD".into(),
                amount: 30,
            },
            vec![
                Claim::TransferAmountEquals {
                    transfer_id: [1; 16],
                    value: 10,
                    amount_commit: [0; 32],
                    state_root: root,
                },
                Claim::TransferAmountEquals {
                    transfer_id: [1; 16],
                    value: 20,
                    amount_commit: [0; 32],
                    state_root: root,
                },
            ],
        );
        assert!(
            verify(&moved.to_bytes(), Some(pk), &chain(&moved))
                .unwrap()
                .passed()
        );
        let occurred = issue(
            &sk,
            Assertion::OperationOccurred {
                operation: "did:aledgent:operation:o".into(),
            },
            vec![Claim::TransferExisted {
                transfer_id: [1; 16],
                state_root: root,
            }],
        );
        assert!(
            verify(&occurred.to_bytes(), Some(pk), &chain(&occurred))
                .unwrap()
                .passed()
        );
        let flow = |bound: Bound, relation: Relation, to: u64| {
            issue(
                &sk,
                Assertion::FlowWithin {
                    asset: "USD".into(),
                    direction: FlowDirection::Out,
                    from_sequence: 2,
                    to_sequence: to,
                    bound,
                },
                vec![Claim::AggregateInWindow {
                    filter: Filter {
                        journal: None,
                        account: Some([2; 16]),
                        direction: Some(1),
                    },
                    t_lo: 2,
                    t_hi: to,
                    relation,
                    state_root: root,
                }],
            )
        };
        let ok = flow(
            Bound::AtMost(70),
            Relation::AtMost(schnorr::public_commitment(70)),
            20,
        );
        assert!(
            verify(&ok.to_bytes(), Some(pk), &chain(&ok))
                .unwrap()
                .passed()
        );
        let wrong_bound = flow(
            Bound::AtMost(70),
            Relation::AtMost(schnorr::public_commitment(71)),
            20,
        );
        assert!(
            verify(&wrong_bound.to_bytes(), Some(pk), &chain(&wrong_bound))
                .unwrap()
                .claims_consistent
                .is_err()
        );
        let past = flow(
            Bound::AtMost(70),
            Relation::AtMost(schnorr::public_commitment(70)),
            21,
        );
        assert!(
            verify(&past.to_bytes(), Some(pk), &chain(&past))
                .unwrap()
                .claims_consistent
                .is_err()
        );
        // A claim over another root.
        let unbound = issue(
            &sk,
            Assertion::OperationOccurred {
                operation: "did:aledgent:operation:o".into(),
            },
            vec![Claim::TransferExisted {
                transfer_id: [1; 16],
                state_root: [4; 32],
            }],
        );
        assert!(
            !verify(&unbound.to_bytes(), Some(pk), &chain(&unbound))
                .unwrap()
                .claims_bound
        );
    }
}
