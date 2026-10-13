// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The call envelope and the call outbox (`F-027` T-027-01, `DEC-18`, `REQ-SO-5`).
//!
//! Every call the book makes on the settlement layer — an external movement or an anchor —
//! leaves the core as a [`CallEnvelope`]: **a reference, a target, a call and an expiry**. The
//! form is the same for every agent and every venue: the executor (today `services/settle`, later
//! a VOS host extension) maps it onto whatever the current venue takes (the settlement contract's
//! `execute`, later the Spaces pallet), and reports the one outcome the layer recorded under the
//! reference as an observation. Switching venue changes the executor, never the book.
//!
//! - **The reference** is what the layer records the call's outcome under (`REQ-SL-8`): for a
//!   withdrawal, the operation's 16 bytes followed by 16 zero bytes ([`operation_reference`]), the
//!   form the adapter has used since `F-016`; for an anchor, a digest of the deployment's instance
//!   and the anchor's number ([`anchor_reference`]), which names no operation of the book.
//! - **The target** is the deployment's position on the settlement layer the call acts for.
//! - **The call** is what the layer does: pay out a reservation, or store an anchor. Nothing
//!   else: an anchor is a number and 32 bytes (`REQ-PR-2`, `INV-17`).
//! - **The expiry** is the settlement-layer height after which the call can no longer take
//!   effect (`REQ-SL-4`), for a withdrawal. An anchor has none: its transaction is still mortal,
//!   and one that provably did not execute is emitted again.
//!
//! [`CallEnvelope::withdrawal`] takes an [`OutboundInstruction`], which only a reservation
//! yields (`INV-13`), and [`CallEnvelope::anchor`] an [`AnchorInstruction`], which only a
//! checkpoint yields. There is no other constructor of an envelope's call with a movement in it.
//!
//! The **call outbox** ([`crate::ledger::Ledger::call_outbox`]) is every call not yet taken
//! over by the executor: the withdrawal instructions it has not acknowledged, and the checkpoint
//! in flight until it is final, with whether the executor holds it. After a restart it hands the
//! same envelopes under the same references (`REQ-SO-7`).

use cipher_clerk::crypto::blake2b_256;

use super::{Destination, OutboundInstruction, instruction_digest};
use crate::anchor::AnchorInstruction;
use crate::asset::{AssetId, AssetLocation, Minor, NonZeroMinor};
use crate::id::OperationId;
use crate::settlement::assets::AssetBind;
use ledger_kernel::posting::OperationRef;

/// The domain of an anchor's reference.
pub const ANCHOR_REFERENCE_DOMAIN: &[u8] = b"aledgent/anchor-reference/v1";

/// A call's reference: the 32 bytes the settlement layer records the call's outcome under.
pub type CallReference = [u8; 32];

/// The reference of operation `op`'s call: its 16 bytes, then 16 zero bytes.
pub fn operation_reference(op: &OperationId) -> CallReference {
    let mut r = [0u8; 32];
    r[..16].copy_from_slice(op.as_bytes());
    r
}

/// The operation a reference names, if it has an operation's form ([`operation_reference`]).
pub fn operation_of(reference: &CallReference) -> Option<OperationId> {
    if reference[16..] != [0u8; 16] {
        return None;
    }
    let mut b = [0u8; 16];
    b.copy_from_slice(&reference[..16]);
    Some(OperationId::from_ref(OperationRef(b)))
}

/// The reference of anchor `number` of the deployment's `instance`:
/// `blake2b-256("aledgent/anchor-reference/v1" ‖ instance ‖ number)`, the integers big-endian.
/// It is the same for every emission of the anchor, and names no operation.
pub fn anchor_reference(instance: u32, number: u64) -> CallReference {
    blake2b_256(
        ANCHOR_REFERENCE_DOMAIN,
        &[&instance.to_be_bytes(), &number.to_be_bytes()],
    )
}

/// The position on the settlement layer a call acts for: the deployment's instance.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Target {
    /// The deployment's instance on the settlement layer.
    pub instance: u32,
}

/// What a call does on the settlement layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum SettlementCall {
    /// Pay out a reservation from the deployment's position (`REQ-SO-5`): the reservation's asset
    /// and exact amount, to the instruction's destination.
    Withdraw {
        /// The asset.
        asset: AssetId,
        /// The exact amount, in minor units: the reservation's.
        amount: NonZeroMinor,
        /// Where it goes.
        to: Destination,
    },
    /// Store the deployment's anchor `number` (`REQ-PR-2`): a number and 32 bytes, nothing else.
    Anchor {
        /// The anchor number.
        number: u64,
        /// The commitment.
        commitment: [u8; 32],
    },
    /// Bind a book-only asset (amendment 0011, `REQ-AS-8`): the layer records its holding of the
    /// asset at `location` as the deployment's position only if it equals `amount` exactly.
    Bind {
        /// The book's asset.
        asset: AssetId,
        /// Its location on the layer.
        location: AssetLocation,
        /// The book's outstanding balance of it. May be zero.
        amount: Minor,
    },
}

/// A call of the book on the settlement layer. See the module documentation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CallEnvelope {
    /// What the layer records the call's outcome under.
    pub reference: CallReference,
    /// The position the call acts for.
    pub target: Target,
    /// What it does.
    pub call: SettlementCall,
    /// The layer height after which it can no longer take effect; `None` for an anchor.
    pub expiry: Option<u64>,
}

impl CallEnvelope {
    /// The envelope of an outbound instruction of the deployment's `instance`.
    pub fn withdrawal(instance: u32, i: &OutboundInstruction) -> CallEnvelope {
        CallEnvelope {
            reference: operation_reference(&i.operation_ref()),
            target: Target { instance },
            call: SettlementCall::Withdraw {
                asset: i.asset(),
                amount: i.amount(),
                to: i.destination(),
            },
            expiry: Some(i.expiry()),
        }
    }

    /// The envelope of a checkpoint's anchor instruction, of the deployment's `instance`.
    pub fn anchor(instance: u32, a: &AnchorInstruction) -> CallEnvelope {
        CallEnvelope {
            reference: anchor_reference(instance, a.number()),
            target: Target { instance },
            call: SettlementCall::Anchor {
                number: a.number(),
                commitment: a.commitment(),
            },
            expiry: None,
        }
    }

    /// The envelope of a pending binding, of the deployment's `instance`: under the
    /// `bind_asset` command's operation, with no expiry (its transaction is mortal; one that
    /// provably did not execute is signed again).
    pub fn bind(instance: u32, b: &AssetBind) -> CallEnvelope {
        CallEnvelope {
            reference: operation_reference(&b.reference),
            target: Target { instance },
            call: SettlementCall::Bind {
                asset: b.asset,
                location: b.location,
                amount: b.amount,
            },
            expiry: None,
        }
    }

    /// The operation a withdrawal pays out.
    pub fn operation(&self) -> Option<OperationId> {
        match self.call {
            SettlementCall::Withdraw { .. } => operation_of(&self.reference),
            SettlementCall::Anchor { .. } | SettlementCall::Bind { .. } => None,
        }
    }

    /// A withdrawal's instruction identity, recomputed from the envelope alone
    /// ([`OutboundInstruction::digest`]): what the executor's acknowledgement names. `None` for an
    /// anchor, or for a withdrawal whose reference is not an operation's or that has no expiry.
    pub fn instruction(&self) -> Option<[u8; 32]> {
        match self.call {
            SettlementCall::Withdraw { asset, amount, to } => Some(instruction_digest(
                &operation_of(&self.reference)?,
                &asset,
                amount,
                to,
                self.expiry?,
            )),
            SettlementCall::Anchor { .. } | SettlementCall::Bind { .. } => None,
        }
    }
}

/// One entry of the call outbox: an envelope, and whether the executor holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OutboxCall {
    /// The call.
    pub envelope: CallEnvelope,
    /// Whether the executor has acknowledged it: always `false` for a withdrawal (an acknowledged
    /// instruction leaves the outbox); for the checkpoint in flight, whether it is `Submitted`.
    pub acknowledged: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::Minor;

    fn op(b: u8) -> OperationId {
        OperationId::from_ref(OperationRef([b; 16]))
    }

    #[test]
    fn t_027_01_references_name_an_operation_or_an_anchor_never_both() {
        let r = operation_reference(&op(7));
        assert_eq!(&r[..16], &[7u8; 16]);
        assert_eq!(&r[16..], &[0u8; 16]);
        assert_eq!(operation_of(&r), Some(op(7)));
        let a = anchor_reference(1, 2);
        assert_ne!(a, anchor_reference(1, 3));
        assert_ne!(a, anchor_reference(2, 2));
        assert_eq!(a, anchor_reference(1, 2));
        assert_eq!(operation_of(&a), None, "an anchor names no operation");
    }

    #[test]
    fn t_027_01_a_withdrawal_envelope_recomputes_its_instruction() {
        let asset = AssetId::parse("USD").unwrap();
        let amount = NonZeroMinor::new(Minor::new(250)).unwrap();
        let to = Destination::Account([0xab; 32]);
        let e = CallEnvelope {
            reference: operation_reference(&op(3)),
            target: Target { instance: 1 },
            call: SettlementCall::Withdraw { asset, amount, to },
            expiry: Some(40),
        };
        assert_eq!(e.operation(), Some(op(3)));
        assert_eq!(
            e.instruction(),
            Some(instruction_digest(&op(3), &asset, amount, to, 40))
        );
        // Any other expiry is another instruction.
        let later = CallEnvelope {
            expiry: Some(41),
            ..e
        };
        assert_ne!(later.instruction(), e.instruction());
        // A reference that is not an operation's names no instruction.
        let foreign = CallEnvelope {
            reference: [0xff; 32],
            ..e
        };
        assert_eq!(foreign.instruction(), None);
    }
}
