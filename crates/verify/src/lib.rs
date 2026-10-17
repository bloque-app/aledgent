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
//! - [`commitment()`]: the anchor commitment of a `v1` or `v2` checkpoint, `blake2b-256("aledgent/
//!   anchor/v1" ‖ instance ‖ number ‖ entry sequence ‖ kernel root)` and its `v2` form; and
//!   [`commitment_v3()`], the `v3` form that also binds the book's genesis, its code version and its
//!   upgrade-authority mode ([`CodeBinding`]; `REQ-RT-15`, `T-037-03`). [`checkpoint_commitment()`]
//!   is either, as a checkpoint names it. All are aledgent's fields alone: nothing of a platform
//!   (`REQ-RT-19`).
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

/// The domain of the `v3` anchor commitment (`T-037-03`, `DEC-26` (a), rulings `a0023-q3` and
/// `a0023-q4`): it binds the construction, the book's genesis, the code version and the
/// upgrade-authority mode beside what `v2` binds.
pub const COMMITMENT_DOMAIN_V3: &[u8] = b"aledgent/anchor/v3";

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

/// What a `v3` checkpoint binds beside a `v2` one (`REQ-RT-15`, `REQ-RT-18`, `T-037-03`): the
/// book's genesis (amendment 0022's seed digest), the code version that produced the anchored
/// state, and the digest of the upgrade-authority mode in force. Every field is aledgent's own: a
/// digest aledgent computes, never a platform's identity, slot or encoding (`REQ-RT-19`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CodeBinding {
    /// The book's genesis: its chainspec's seed digest; zeros for a book founded without one.
    pub genesis: [u8; 32],
    /// The code version: the digest of the writer's code manifest (`aledgent/code/v1`).
    pub code: [u8; 32],
    /// The digest of the upgrade-authority mode in force.
    pub authority: [u8; 32],
}

/// The `v3` anchor commitment (`DEC-26` (a)):
/// `blake2b-256("aledgent/anchor/v3" ‖ construction ‖ genesis ‖ code ‖ authority ‖ instance ‖
/// number ‖ entry_sequence ‖ kernel_root)`, the integers big-endian. Still 32 bytes, and still
/// revealing nothing of the book (`REQ-PR-2`, ruling `a0023-q4`).
pub fn commitment_v3(
    construction: u8,
    binding: &CodeBinding,
    instance: u32,
    number: u64,
    entry_sequence: u64,
    kernel_root: &[u8; 32],
) -> [u8; 32] {
    blake2b_256(
        COMMITMENT_DOMAIN_V3,
        &[
            &[construction],
            &binding.genesis,
            &binding.code,
            &binding.authority,
            &instance.to_be_bytes(),
            &number.to_be_bytes(),
            &entry_sequence.to_be_bytes(),
            kernel_root,
        ],
    )
}

/// A checkpoint's commitment, in the form it names: `v3` ([`commitment_v3()`]) when it carries a
/// [`CodeBinding`], else the `v1` or `v2` form ([`commitment()`]), so that every earlier anchor
/// still checks (`REQ-PR-10`).
pub fn checkpoint_commitment(
    construction: u8,
    binding: Option<&CodeBinding>,
    instance: u32,
    number: u64,
    entry_sequence: u64,
    kernel_root: &[u8; 32],
) -> [u8; 32] {
    match binding {
        Some(b) => commitment_v3(
            construction,
            b,
            instance,
            number,
            entry_sequence,
            kernel_root,
        ),
        None => commitment(construction, instance, number, entry_sequence, kernel_root),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `T-037-03`: the `v3` commitment binds the code, the construction and the mode, and the
    /// genesis, beside every field `v2` binds; a checkpoint without a binding keeps its `v2` form.
    #[test]
    fn t037_03_the_v3_commitment_binds_code_construction_and_mode() {
        let b = CodeBinding {
            genesis: [1; 32],
            code: [2; 32],
            authority: [3; 32],
        };
        let base = commitment_v3(CONSTRUCTION_V2, &b, 1, 2, 3, &[4; 32]);
        let other = |f: &dyn Fn(&mut CodeBinding)| {
            let mut x = b;
            f(&mut x);
            commitment_v3(CONSTRUCTION_V2, &x, 1, 2, 3, &[4; 32])
        };
        assert_ne!(base, other(&|x| x.genesis[0] ^= 1));
        assert_ne!(base, other(&|x| x.code[0] ^= 1));
        assert_ne!(base, other(&|x| x.authority[0] ^= 1));
        assert_ne!(base, commitment_v3(CONSTRUCTION_V1, &b, 1, 2, 3, &[4; 32]));
        assert_ne!(base, commitment_v3(CONSTRUCTION_V2, &b, 9, 2, 3, &[4; 32]));
        assert_ne!(base, commitment_v3(CONSTRUCTION_V2, &b, 1, 9, 3, &[4; 32]));
        assert_ne!(base, commitment_v3(CONSTRUCTION_V2, &b, 1, 2, 9, &[4; 32]));
        assert_ne!(base, commitment_v3(CONSTRUCTION_V2, &b, 1, 2, 3, &[9; 32]));
        // Its own domain: never a `v2` commitment of the same checkpoint.
        assert_ne!(base, commitment(CONSTRUCTION_V2, 1, 2, 3, &[4; 32]));
        assert_eq!(
            checkpoint_commitment(CONSTRUCTION_V2, Some(&b), 1, 2, 3, &[4; 32]),
            base
        );
        // `v2` anchors still check: no binding is the `v2` form, unchanged.
        for c in [CONSTRUCTION_V1, CONSTRUCTION_V2] {
            assert_eq!(
                checkpoint_commitment(c, None, 1, 2, 3, &[4; 32]),
                commitment(c, 1, 2, 3, &[4; 32])
            );
        }
        // The layout, spelled out.
        let theirs = cipher_clerk::crypto::blake2b_256(
            b"aledgent/anchor/v3",
            &[
                &[CONSTRUCTION_V2],
                &[1; 32],
                &[2; 32],
                &[3; 32],
                &1u32.to_be_bytes(),
                &2u64.to_be_bytes(),
                &3u64.to_be_bytes(),
                &[4; 32],
            ],
        );
        assert_eq!(base, theirs);
    }

    /// `T-037-03`: the commitment is computed in a crate that links no VOS crate. This crate
    /// depends on no aledgent crate and no runtime; the root workspace, which it belongs to, links
    /// no VOS crate at all (`tools/ci/check-no-vos.sh` gates the manifests).
    #[test]
    fn t037_03_the_commitment_links_no_vos_crate() {
        let lock = include_str!("../../../Cargo.lock");
        assert!(lock.contains("name = \"verify\""));
        assert!(
            !lock.contains("name = \"vos"),
            "a VOS crate is in the workspace that builds the commitment"
        );
        let manifest = include_str!("../Cargo.toml");
        assert!(
            !manifest.lines().any(|l| l.trim_start().starts_with("vos")),
            "this crate names a VOS crate"
        );
    }

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
