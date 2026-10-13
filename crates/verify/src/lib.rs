// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! # verify
//!
//! aledgent's anchor and disclosure verifier (`F-017`, `DEC-12`, `DEC-15`: `crates/verify`).
//!
//! A recipient of a disclosure checks it with this crate alone, **without aledgent taking part**
//! (`REQ-DS-3`, `REQ-PR-4`, `AC-I2.1`, `AC-I3.1`). Its inputs are the disclosure's bytes, the
//! deployment's clerk public key (given, or read from the deployment's instance record on the
//! settlement layer), and a public read of the settlement layer ([`AnchorSource`]). It depends on
//! no aledgent crate: the formats it reads are defined here, and aledgent's core builds them with
//! this crate.
//!
//! ## What it checks ([`verify()`])
//!
//! 1. The format and the version ([`envelope`]).
//! 2. Both signatures: the envelope's, over every field, and each claim's, in CipherClerk's wire
//!    form ([`claim`]); every claim is bound to the checkpoint's kernel root and says what the
//!    assertion says.
//! 3. The anchor, read at a finalized block of the settlement layer.
//! 4. The commitment, recomputed from the checkpoint the disclosure names ([`commitment()`]) and
//!    compared with the anchor's.
//! 5. It shows the assertion, the assurance level and the signed-assertion label.
//!
//! **What a pass means** (`REQ-PR-3`, `REQ-PR-6`): aledgent asserted this fact about a book
//! state it committed to publicly and cannot later deny having held. A disclosure today is a
//! **signed assertion**: evidence of aledgent's claim, not of its truth. The anchor proves
//! non-revision, never solvency.
//!
//! ## Map
//!
//! - [`commitment()`]: the anchor commitment, `blake2b-256("aledgent/anchor/v1" ‖ instance ‖
//!   number ‖ entry sequence ‖ kernel root)`.
//! - [`schnorr`]: the clerk's signature: CipherClerk's Schnorr over Ristretto255.
//! - [`claim`]: CipherClerk's four disclosure claims, parsed from their wire form.
//! - [`envelope`]: the disclosure envelope and its one canonical encoding.
//! - [`source`]: [`AnchorSource`], the public read, and [`MemAnchors`], a fake.
//! - [`check`]: [`verify()`] and its [`Report`].
//! - `rpc` (feature `rpc`): the settlement layer read over a node's JSON-RPC.
//! - `cli` (feature `std`): the command-line verifier's logic; the `aledgent-verify` binary
//!   (feature `cli`) is a thin wrapper around it.

#![no_std]

extern crate alloc;
#[cfg(any(feature = "std", test))]
extern crate std;

pub mod check;
pub mod claim;
#[cfg(feature = "std")]
pub mod cli;
pub mod envelope;
mod hash;
pub mod schnorr;
pub mod source;

#[cfg(feature = "rpc")]
pub mod rpc;

pub use check::{AnchorCheck, Report, VerifyError, verify};
pub use envelope::{
    Assertion, Assurance, AssuranceKind, BalanceKind, Bound, CheckpointRef, Disclosure,
    FlowDirection, Recipient,
};
pub use hash::{blake2b_256, blake2b_512};
pub use source::{AnchorSource, MemAnchors, PublicAnchor, Venue};

/// The domain of the anchor commitment of construction 1 (CipherClerk's full-depth tree).
pub const COMMITMENT_DOMAIN: &[u8] = b"aledgent/anchor/v1";

/// The domain of the anchor commitment of every later construction (`REQ-PR-10`): the
/// construction's number is the first byte it hashes.
pub const COMMITMENT_DOMAIN_V2: &[u8] = b"aledgent/anchor/v2";

/// Construction 1 of the state commitment: CipherClerk's full-depth sparse Merkle trees.
pub const CONSTRUCTION_V1: u8 = 1;

/// Construction 2 of the state commitment: compact sparse Merkle trees (`DEC-25`), the book's
/// construction from genesis.
pub const CONSTRUCTION_V2: u8 = 2;

/// Whether this verifier knows construction `c` (`REQ-PR-10`: every construction anchored in stays
/// checkable).
pub const fn construction_known(c: u8) -> bool {
    c == CONSTRUCTION_V1 || c == CONSTRUCTION_V2
}

/// The label every signed-assertion disclosure carries and the verifier prints (`REQ-PR-6`,
/// `AC-I2.3`).
pub const SIGNED_ASSERTION_LABEL: &str =
    "Signed assertion. Evidence of aledgent's claim, not of its truth.";

/// The anchor commitment (`F-017`, `INV-19`, `REQ-PR-2`, `REQ-PR-10`): for construction 1,
/// `blake2b-256("aledgent/anchor/v1" ‖ instance ‖ number ‖ entry_sequence ‖ kernel_root)`; for every
/// later construction `c`, `blake2b-256("aledgent/anchor/v2" ‖ c ‖ instance ‖ number ‖
/// entry_sequence ‖ kernel_root)`. The integers are big-endian.
///
/// It binds the construction of the state commitment, the deployment's instance, the anchor's
/// number, the sequence of the last entry the checkpoint covers, and the kernel's composite root at
/// that sequence. It reveals no account, identity, amount, counterparty or asset: 32 bytes, a hash of
/// a hash (`AC-I1.1`).
pub fn commitment(
    construction: u8,
    instance: u32,
    number: u64,
    entry_sequence: u64,
    kernel_root: &[u8; 32],
) -> [u8; 32] {
    let (instance, number, entry_sequence) = (
        instance.to_be_bytes(),
        number.to_be_bytes(),
        entry_sequence.to_be_bytes(),
    );
    if construction == CONSTRUCTION_V1 {
        return blake2b_256(
            COMMITMENT_DOMAIN,
            &[&instance, &number, &entry_sequence, kernel_root],
        );
    }
    blake2b_256(
        COMMITMENT_DOMAIN_V2,
        &[
            &[construction],
            &instance,
            &number,
            &entry_sequence,
            kernel_root,
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_commitment_binds_every_field() {
        for c in [CONSTRUCTION_V1, CONSTRUCTION_V2] {
            let base = commitment(c, 1, 2, 3, &[4; 32]);
            assert_ne!(base, commitment(c, 9, 2, 3, &[4; 32]));
            assert_ne!(base, commitment(c, 1, 9, 3, &[4; 32]));
            assert_ne!(base, commitment(c, 1, 2, 9, &[4; 32]));
            assert_ne!(base, commitment(c, 1, 2, 3, &[9; 32]));
            assert_eq!(base, commitment(c, 1, 2, 3, &[4; 32]));
        }
    }

    /// `AC-J1.5` (`REQ-PR-10`): the commitment binds the construction it names; the same
    /// checkpoint with another construction named gives another commitment.
    #[test]
    fn ac_j1_5_the_commitment_binds_the_construction() {
        let v1 = commitment(CONSTRUCTION_V1, 1, 2, 3, &[4; 32]);
        let v2 = commitment(CONSTRUCTION_V2, 1, 2, 3, &[4; 32]);
        assert_ne!(v1, v2);
        assert_ne!(v2, commitment(3, 1, 2, 3, &[4; 32]));
        assert!(construction_known(CONSTRUCTION_V1) && construction_known(CONSTRUCTION_V2));
        assert!(!construction_known(0) && !construction_known(3));
    }

    #[test]
    fn the_commitment_is_cipher_clerks_domain_hash() {
        let root = [7u8; 32];
        let ours = commitment(CONSTRUCTION_V1, 3, 4, 5, &root);
        let theirs = cipher_clerk::crypto::blake2b_256(
            COMMITMENT_DOMAIN,
            &[
                &3u32.to_be_bytes(),
                &4u64.to_be_bytes(),
                &5u64.to_be_bytes(),
                &root,
            ],
        );
        assert_eq!(ours, theirs);
    }
}
