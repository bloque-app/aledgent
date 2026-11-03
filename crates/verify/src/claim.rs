// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! CipherClerk's four disclosure claims, in their wire form: `domain ‖ public inputs ‖ 64-byte
//! signature` (CipherClerk's source; `cipher_clerk::disclosure`). The verifier parses them itself, so
//! it needs no kernel crate; a test holds every shape to CipherClerk's own `verify_*`.
//!
//! Each claim names the kernel's `state_root` it speaks of. In a disclosure that root must be the
//! checkpoint's kernel root ([`crate::check`]).

use alloc::vec::Vec;

use crate::schnorr;

/// `cipher-clerk/disclosure/balance-at-least/v1`.
pub const DOMAIN_BALANCE: &[u8] = b"cipher-clerk/disclosure/balance-at-least/v1";
/// `cipher-clerk/disclosure/transfer-existed/v1`.
pub const DOMAIN_EXISTED: &[u8] = b"cipher-clerk/disclosure/transfer-existed/v1";
/// `cipher-clerk/disclosure/transfer-amount-equals/v2`.
pub const DOMAIN_AMOUNT: &[u8] = b"cipher-clerk/disclosure/transfer-amount-equals/v2";
/// `cipher-clerk/disclosure/aggregate/v1`.
pub const DOMAIN_AGGREGATE: &[u8] = b"cipher-clerk/disclosure/aggregate/v1";

/// The relation of an aggregate claim, with the commitment to its bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Relation {
    /// The aggregate is at least the committed value.
    AtLeast([u8; 32]),
    /// The aggregate is at most the committed value.
    AtMost([u8; 32]),
    /// The aggregate equals the committed value.
    Equals([u8; 32]),
}

impl Relation {
    /// The commitment to the bound.
    pub const fn commitment(&self) -> &[u8; 32] {
        match self {
            Relation::AtLeast(c) | Relation::AtMost(c) | Relation::Equals(c) => c,
        }
    }
}

/// The filter of an aggregate claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Filter {
    /// A journal, if the claim is limited to one.
    pub journal: Option<[u8; 16]>,
    /// A kernel account, if the claim is limited to one.
    pub account: Option<[u8; 16]>,
    /// `1` debit, `2` credit, if the claim is limited to one side.
    pub direction: Option<u8>,
}

/// One claim's public inputs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Claim {
    /// A kernel account's balance on a layer is at least `threshold` at `state_root`.
    BalanceAtLeast {
        /// The kernel account.
        account: [u8; 16],
        /// The layer: `0` settled, `1` pending, `2` encumbrance.
        layer: u8,
        /// The threshold, in minor units.
        threshold: u64,
        /// The kernel root.
        state_root: [u8; 32],
    },
    /// A kernel transfer exists at `state_root`.
    TransferExisted {
        /// The transfer.
        transfer_id: [u8; 16],
        /// The kernel root.
        state_root: [u8; 32],
    },
    /// A kernel transfer entry's committed amount opens to `value`.
    TransferAmountEquals {
        /// The transfer.
        transfer_id: [u8; 16],
        /// The value, in minor units.
        value: u64,
        /// The entry's Pedersen commitment, as the kernel stores it.
        amount_commit: [u8; 32],
        /// The kernel root.
        state_root: [u8; 32],
    },
    /// The aggregate of the transfers that match `filter` in `[t_lo, t_hi]` satisfies `relation`.
    AggregateInWindow {
        /// Which transfers.
        filter: Filter,
        /// The window's low end.
        t_lo: u64,
        /// The window's high end.
        t_hi: u64,
        /// The relation and its bound.
        relation: Relation,
        /// The kernel root.
        state_root: [u8; 32],
    },
}

/// A claim and its signature, as a disclosure carries it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SignedClaim {
    /// What it claims.
    pub claim: Claim,
    /// The clerk's signature over [`Claim::public_bytes`].
    pub signature: [u8; 64],
}

fn take<const N: usize>(b: &mut &[u8]) -> Option<[u8; N]> {
    if b.len() < N {
        return None;
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&b[..N]);
    *b = &b[N..];
    Some(out)
}

fn opt16(b: &mut &[u8]) -> Option<Option<[u8; 16]>> {
    match take::<1>(b)?[0] {
        0 => Some(None),
        1 => Some(Some(take::<16>(b)?)),
        _ => None,
    }
}

impl Claim {
    /// The kernel root the claim speaks of.
    pub const fn state_root(&self) -> &[u8; 32] {
        match self {
            Claim::BalanceAtLeast { state_root, .. }
            | Claim::TransferExisted { state_root, .. }
            | Claim::TransferAmountEquals { state_root, .. }
            | Claim::AggregateInWindow { state_root, .. } => state_root,
        }
    }

    /// The bytes the clerk signs: CipherClerk's `public_bytes` for the shape.
    pub fn public_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Claim::BalanceAtLeast {
                account,
                layer,
                threshold,
                state_root,
            } => {
                out.extend_from_slice(DOMAIN_BALANCE);
                out.extend_from_slice(account);
                out.push(*layer);
                out.extend_from_slice(&threshold.to_le_bytes());
                out.extend_from_slice(state_root);
            }
            Claim::TransferExisted {
                transfer_id,
                state_root,
            } => {
                out.extend_from_slice(DOMAIN_EXISTED);
                out.extend_from_slice(transfer_id);
                out.extend_from_slice(state_root);
            }
            Claim::TransferAmountEquals {
                transfer_id,
                value,
                amount_commit,
                state_root,
            } => {
                out.extend_from_slice(DOMAIN_AMOUNT);
                out.extend_from_slice(transfer_id);
                out.extend_from_slice(&value.to_le_bytes());
                out.extend_from_slice(amount_commit);
                out.extend_from_slice(state_root);
            }
            Claim::AggregateInWindow {
                filter,
                t_lo,
                t_hi,
                relation,
                state_root,
            } => {
                out.extend_from_slice(DOMAIN_AGGREGATE);
                for id in [filter.journal, filter.account] {
                    match id {
                        Some(id) => {
                            out.push(1);
                            out.extend_from_slice(&id);
                        }
                        None => out.push(0),
                    }
                }
                out.push(filter.direction.unwrap_or(0));
                out.extend_from_slice(&t_lo.to_le_bytes());
                out.extend_from_slice(&t_hi.to_le_bytes());
                let (tag, c) = match relation {
                    Relation::AtLeast(c) => (1, c),
                    Relation::AtMost(c) => (2, c),
                    Relation::Equals(c) => (3, c),
                };
                out.push(tag);
                out.extend_from_slice(c);
                out.extend_from_slice(state_root);
            }
        }
        out
    }

    /// Parse the public inputs at the start of `b`; the rest is left in `b`.
    fn parse(b: &mut &[u8]) -> Option<Claim> {
        // The amount domain extends the existed one's prefix: check the longer first.
        if let Some(rest) = b.strip_prefix(DOMAIN_BALANCE) {
            *b = rest;
            let account = take::<16>(b)?;
            let layer = take::<1>(b)?[0];
            if layer > 2 {
                return None;
            }
            let threshold = u64::from_le_bytes(take::<8>(b)?);
            let state_root = take::<32>(b)?;
            return Some(Claim::BalanceAtLeast {
                account,
                layer,
                threshold,
                state_root,
            });
        }
        if let Some(rest) = b.strip_prefix(DOMAIN_AMOUNT) {
            *b = rest;
            let transfer_id = take::<16>(b)?;
            let value = u64::from_le_bytes(take::<8>(b)?);
            let amount_commit = take::<32>(b)?;
            let state_root = take::<32>(b)?;
            return Some(Claim::TransferAmountEquals {
                transfer_id,
                value,
                amount_commit,
                state_root,
            });
        }
        if let Some(rest) = b.strip_prefix(DOMAIN_EXISTED) {
            *b = rest;
            let transfer_id = take::<16>(b)?;
            let state_root = take::<32>(b)?;
            return Some(Claim::TransferExisted {
                transfer_id,
                state_root,
            });
        }
        if let Some(rest) = b.strip_prefix(DOMAIN_AGGREGATE) {
            *b = rest;
            let journal = opt16(b)?;
            let account = opt16(b)?;
            let direction = match take::<1>(b)?[0] {
                0 => None,
                d @ (1 | 2) => Some(d),
                _ => return None,
            };
            let t_lo = u64::from_le_bytes(take::<8>(b)?);
            let t_hi = u64::from_le_bytes(take::<8>(b)?);
            let tag = take::<1>(b)?[0];
            let c = take::<32>(b)?;
            let relation = match tag {
                1 => Relation::AtLeast(c),
                2 => Relation::AtMost(c),
                3 => Relation::Equals(c),
                _ => return None,
            };
            let state_root = take::<32>(b)?;
            return Some(Claim::AggregateInWindow {
                filter: Filter {
                    journal,
                    account,
                    direction,
                },
                t_lo,
                t_hi,
                relation,
                state_root,
            });
        }
        None
    }
}

impl SignedClaim {
    /// Parse a claim's whole wire form: its public inputs and its 64-byte signature, nothing
    /// more. `None` if the bytes are anything else.
    pub fn from_wire(wire: &[u8]) -> Option<SignedClaim> {
        let mut b = wire;
        let claim = Claim::parse(&mut b)?;
        let signature = take::<64>(&mut b)?;
        if !b.is_empty() {
            return None;
        }
        Some(SignedClaim { claim, signature })
    }

    /// The claim's wire form.
    pub fn to_wire(&self) -> Vec<u8> {
        let mut out = self.claim.public_bytes();
        out.extend_from_slice(&self.signature);
        out
    }

    /// Whether the signature is `key`'s over the claim's public inputs.
    pub fn verify(&self, key: &[u8; 32]) -> bool {
        schnorr::verify(key, &self.claim.public_bytes(), &self.signature)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cipher_clerk::crypto::{Amount, AuthKey, SecretKey};
    use cipher_clerk::disclosure::{self, AggregateClaim, AggregateFilter, DisclosureProof};
    use cipher_clerk::ids::{AccountId, TransferId};
    use cipher_clerk::types::{Direction, Layer};

    fn clerk() -> (SecretKey, AuthKey) {
        let sk = SecretKey::from_bytes([9u8; 32]).unwrap();
        let pk = sk.public_key();
        (sk, pk)
    }

    fn check(p: &DisclosureProof, pk: &AuthKey) -> SignedClaim {
        let c = SignedClaim::from_wire(&p.bytes).expect("parses");
        assert!(c.verify(&pk.0));
        assert_eq!(c.to_wire(), p.bytes);
        let mut bad = p.bytes.clone();
        let n = bad.len();
        bad[n - 70] ^= 1;
        assert!(SignedClaim::from_wire(&bad).is_none_or(|c| !c.verify(&pk.0)));
        c
    }

    #[test]
    fn req_ds_3_every_shape_is_cipher_clerks_wire_form() {
        let (sk, pk) = clerk();
        let root = [5u8; 32];
        let p =
            disclosure::prove_balance_at_least(AccountId([1; 16]), Layer::Settled, 40, root, &sk);
        assert_eq!(
            check(&p, &pk).claim,
            Claim::BalanceAtLeast {
                account: [1; 16],
                layer: 0,
                threshold: 40,
                state_root: root
            }
        );
        let p = disclosure::prove_transfer_existed(TransferId([2; 16]), root, &sk);
        assert!(matches!(
            check(&p, &pk).claim,
            Claim::TransferExisted { .. }
        ));
        let commit = Amount([3; 32]);
        let p =
            disclosure::prove_transfer_amount_equals(TransferId([2; 16]), 17, commit, root, &sk);
        assert!(disclosure::verify_transfer_amount_equals(
            &p,
            TransferId([2; 16]),
            17,
            commit,
            root,
            &pk
        ));
        assert!(matches!(
            check(&p, &pk).claim,
            Claim::TransferAmountEquals { value: 17, .. }
        ));
        let filter = AggregateFilter {
            journal: None,
            account: Some(AccountId([4; 16])),
            direction: Some(Direction::Credit),
        };
        let p = disclosure::prove_aggregate_in_window(
            filter,
            3,
            9,
            AggregateClaim::AtMost(Amount([6; 32])),
            root,
            &sk,
        );
        let c = check(&p, &pk);
        assert_eq!(
            c.claim,
            Claim::AggregateInWindow {
                filter: Filter {
                    journal: None,
                    account: Some([4; 16]),
                    direction: Some(2)
                },
                t_lo: 3,
                t_hi: 9,
                relation: Relation::AtMost([6; 32]),
                state_root: root
            }
        );
    }

    #[test]
    fn trailing_or_short_bytes_are_not_a_claim() {
        let (sk, _) = clerk();
        let p = disclosure::prove_transfer_existed(TransferId([2; 16]), [0; 32], &sk);
        let mut long = p.bytes.clone();
        long.push(0);
        assert!(SignedClaim::from_wire(&long).is_none());
        assert!(SignedClaim::from_wire(&p.bytes[..p.bytes.len() - 1]).is_none());
        assert!(SignedClaim::from_wire(b"cipher-clerk/disclosure/other").is_none());
    }
}
