// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Book-only assets and their binding to the settlement layer (amendment 0011, `DEC-20`, `F-029`;
//! `REQ-AS-2`, `REQ-AS-7`–`REQ-AS-9`).
//!
//! An asset is registered **book-only**, with its precision (`REQ-AS-1`). The settlement adapter
//! reads each settlement-layer asset its configuration maps ([`AssetLocation`]) at a finalised
//! height and submits what the layer states there as an [`AssetAttestation`], the way it submits
//! every other observation: what the layer recorded, never an outcome (`INV-11`). The core keeps the
//! latest attestation of each location.
//!
//! - **What is read.** The layer's `symbol` and `decimals` fields, and its status. A name is never
//!   read: the owner's ruling `d-assets-a` (2026-10-02), "the name shouldn't care". An absent
//!   metadata entry is "no metadata", never zero decimals.
//! - **Binding** ([`admit`]): the issuance authority asks for it, and the core refuses unless the
//!   latest fresh attestation of the location describes this asset: the adapter maps it there, the
//!   layer holds a live asset there whose symbol, normalised, is the asset's id (`REQ-AS-3`) and
//!   whose decimals are the registry's precision. It then emits a bind call ([`AssetBind`]) for the
//!   book's outstanding balance; the settlement layer checks that its holding equals it at the block
//!   that records the binding, and only a final `Bound` record binds the asset.
//! - **After binding** (`REQ-AS-9`): attestations of a bound asset are recorded and otherwise
//!   ignored. Nothing is held, nothing refused, and the registry never adopts what the layer says.

use alloc::vec::Vec;

use crate::account::Time;
use crate::asset::{Asset, AssetId, AssetLocation, Minor};
use crate::export::canon::{Canonical, Dec, DecodeError, Enc};
use crate::id::OperationId;

use super::Coordinate;

/// The longest symbol an attestation carries, in bytes.
pub const LAYER_TEXT_MAX: usize = 64;

/// An asset's status on the settlement layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum LayerAssetStatus {
    /// It moves.
    Live,
    /// Its root authority froze it: nothing moves.
    Frozen,
    /// It is being destroyed.
    Destroying,
}

/// What the settlement layer states about the asset at a location: its own fields for the symbol
/// and the decimals, and its status. Nothing is parsed out of any other text.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct LayerAssetFacts {
    /// Its symbol field, as the layer stores it.
    pub symbol: Vec<u8>,
    /// Its decimals field.
    pub decimals: u8,
    /// Its status.
    pub status: LayerAssetStatus,
}

/// The settlement adapter's attestation of one location: the asset its configuration maps there,
/// and what the layer states about it at a finalised height.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AssetAttestation {
    /// The location.
    pub location: AssetLocation,
    /// The asset the adapter's configuration maps at the location: the one it maps the location's
    /// movements to and from.
    pub asset: AssetId,
    /// The finalised height the facts were read at.
    pub height: u64,
    /// The facts; `None` when the layer holds no asset, or no metadata, at the location.
    pub facts: Option<LayerAssetFacts>,
    /// How long the attestation holds, capped at [`super::MAX_ADAPTER_LEASE`].
    pub lease: Time,
}

/// An attestation as the core recorded it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attested {
    /// The attestation.
    pub attestation: AssetAttestation,
    /// When the core recorded it.
    pub recorded_at: Time,
    /// The first time it no longer holds.
    pub fresh_until: Time,
}

impl Attested {
    /// Whether it still holds at `now`.
    pub const fn fresh(&self, now: Time) -> bool {
        now < self.fresh_until
    }
}

/// Why the layer's view of a location does not describe an asset, so that it cannot be bound
/// there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Mismatch {
    /// The layer holds no asset, or no metadata, at the location.
    Absent,
    /// The adapter maps another asset at the location.
    Mapping,
    /// The layer's symbol field, normalised, is not the asset's id.
    Symbol,
    /// The layer's decimals field is not the registry's precision.
    Decimals,
    /// The layer's asset is not live.
    Status,
}

impl Mismatch {
    /// Its stable spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Mismatch::Absent => "absent",
            Mismatch::Mapping => "mapping",
            Mismatch::Symbol => "symbol",
            Mismatch::Decimals => "decimals",
            Mismatch::Status => "status",
        }
    }
}

/// Whether the layer's facts in `att` describe the registered `asset` (`REQ-AS-8`): the adapter
/// maps it at the attested location; the layer holds a live asset with metadata there; its symbol
/// field, normalised, is the asset's id (`REQ-AS-3`); and its decimals field is the registry's
/// precision (`REQ-AS-1`). No name is read.
pub fn admit(asset: &Asset, att: &AssetAttestation) -> Result<(), Mismatch> {
    if att.asset != asset.id() {
        return Err(Mismatch::Mapping);
    }
    let f = att.facts.as_ref().ok_or(Mismatch::Absent)?;
    if f.symbol.is_empty() {
        return Err(Mismatch::Absent);
    }
    let symbol = core::str::from_utf8(&f.symbol).map_err(|_| Mismatch::Symbol)?;
    if AssetId::parse(symbol).ok() != Some(asset.id()) {
        return Err(Mismatch::Symbol);
    }
    if f.decimals != asset.precision() {
        return Err(Mismatch::Decimals);
    }
    if f.status != LayerAssetStatus::Live {
        return Err(Mismatch::Status);
    }
    Ok(())
}

/// What the core made of an attestation. Informative only: nothing is held on it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum AttestationVerdict {
    /// The adapter maps no registered asset there: recorded.
    Unregistered,
    /// A book-only asset, which a binding to this location would admit, or the first reason it
    /// would not.
    BookOnly {
        /// The asset.
        asset: AssetId,
        /// Why a binding would be refused, if it would be.
        mismatch: Option<Mismatch>,
    },
    /// A binding to this location is in flight.
    Binding {
        /// The asset.
        asset: AssetId,
    },
    /// The asset is bound; what the layer says now is ignored (`REQ-AS-9`).
    Bound {
        /// The asset.
        asset: AssetId,
    },
}

/// Where a binding stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum BindStatus {
    /// In the call outbox, until the layer's record of it is final.
    Pending,
    /// The layer recorded the binding, and the record is final: the asset is bound.
    Bound,
    /// The layer refused it, and the refusal is final: the asset is book-only again.
    Refused,
}

/// One binding of a book-only asset (`REQ-AS-8`): what the book asked the layer to record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AssetBind {
    /// The `bind_asset` command's operation: the call's reference.
    pub reference: OperationId,
    /// The asset.
    pub asset: AssetId,
    /// Its location on the layer.
    pub location: AssetLocation,
    /// The book's outstanding balance of the asset when the binding was asked for: what the
    /// layer's holding must equal.
    pub amount: Minor,
    /// When it was asked for.
    pub requested_at: Time,
    /// Where it stands.
    pub status: BindStatus,
    /// The layer's final record of its outcome, once there is one.
    pub coordinate: Option<Coordinate>,
}

// ------------------------------------------------------------------ canonical forms (F-014)

fn text(d: &mut Dec<'_>) -> Result<Vec<u8>, DecodeError> {
    let b = d.bytes()?;
    if b.len() > LAYER_TEXT_MAX {
        return Err(DecodeError("layer text"));
    }
    Ok(b.to_vec())
}

impl Canonical for LayerAssetStatus {
    fn encode(&self, e: &mut Enc) {
        let code: u8 = match self {
            LayerAssetStatus::Live => 1,
            LayerAssetStatus::Frozen => 2,
            LayerAssetStatus::Destroying => 3,
        };
        e.put(&code);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        match d.get::<u8>()? {
            1 => Ok(LayerAssetStatus::Live),
            2 => Ok(LayerAssetStatus::Frozen),
            3 => Ok(LayerAssetStatus::Destroying),
            _ => Err(DecodeError("layer asset status")),
        }
    }
}

impl Canonical for LayerAssetFacts {
    fn encode(&self, e: &mut Enc) {
        e.bytes(&self.symbol);
        e.put(&self.decimals).put(&self.status);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(LayerAssetFacts {
            symbol: text(d)?,
            decimals: d.get()?,
            status: d.get()?,
        })
    }
}

impl Canonical for AssetAttestation {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.location)
            .put(&self.asset)
            .put(&self.height)
            .put(&self.facts)
            .put(&self.lease);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(AssetAttestation {
            location: d.get()?,
            asset: d.get()?,
            height: d.get()?,
            facts: d.get()?,
            lease: d.get()?,
        })
    }
}

impl Canonical for Attested {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.attestation)
            .put(&self.recorded_at)
            .put(&self.fresh_until);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Attested {
            attestation: d.get()?,
            recorded_at: d.get()?,
            fresh_until: d.get()?,
        })
    }
}

impl Canonical for BindStatus {
    fn encode(&self, e: &mut Enc) {
        let code: u8 = match self {
            BindStatus::Pending => 1,
            BindStatus::Bound => 2,
            BindStatus::Refused => 3,
        };
        e.put(&code);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(match d.get::<u8>()? {
            1 => BindStatus::Pending,
            2 => BindStatus::Bound,
            3 => BindStatus::Refused,
            _ => return Err(DecodeError("bind status")),
        })
    }
}

impl Canonical for AssetBind {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.reference)
            .put(&self.asset)
            .put(&self.location)
            .put(&self.amount)
            .put(&self.requested_at)
            .put(&self.status)
            .put(&self.coordinate);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(AssetBind {
            reference: d.get()?,
            asset: d.get()?,
            location: d.get()?,
            amount: d.get()?,
            requested_at: d.get()?,
            status: d.get()?,
            coordinate: d.get()?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset::tests::loc;
    use crate::asset::{AssetKind, AssetRegistry};

    fn copm() -> Asset {
        let mut r = AssetRegistry::new();
        r.register(AssetId::parse("COPM").unwrap(), 2, AssetKind::Stable)
            .unwrap()
    }

    /// Kreivo mainnet's COPM at `Here(100)`, as read on 2026-09-30 (spec 133): symbol `COPM`,
    /// name `COPM/2`, decimals 2, live.
    fn kreivo_copm() -> AssetAttestation {
        AssetAttestation {
            location: loc(100),
            asset: AssetId::parse("COPM").unwrap(),
            height: 39_074_703,
            facts: Some(LayerAssetFacts {
                symbol: b"COPM".to_vec(),
                decimals: 2,
                status: LayerAssetStatus::Live,
            }),
            lease: 60_000,
        }
    }

    fn with(f: impl FnOnce(&mut LayerAssetFacts)) -> AssetAttestation {
        let mut a = kreivo_copm();
        f(a.facts.as_mut().unwrap());
        a
    }

    #[test]
    fn ac_f5_2_symbol_and_precision_come_from_their_own_fields() {
        assert_eq!(admit(&copm(), &kreivo_copm()), Ok(()));
        // A symbol in another case is the same asset (REQ-AS-3).
        assert_eq!(
            admit(&copm(), &with(|f| f.symbol = b"copm".to_vec())),
            Ok(())
        );
        // Other decimals than the registry's: refused, whatever any name says.
        assert_eq!(
            admit(&copm(), &with(|f| f.decimals = 6)),
            Err(Mismatch::Decimals)
        );
        assert_eq!(
            admit(&copm(), &with(|f| f.symbol = b"COP".to_vec())),
            Err(Mismatch::Symbol)
        );
        assert_eq!(
            admit(&copm(), &with(|f| f.symbol = b"COPM/2".to_vec())),
            Err(Mismatch::Symbol),
            "a name-shaped symbol is not parsed"
        );
        assert_eq!(
            admit(&copm(), &with(|f| f.status = LayerAssetStatus::Frozen)),
            Err(Mismatch::Status)
        );
    }

    #[test]
    fn ac_f5_2_a_location_without_metadata_or_mapped_elsewhere_is_refused() {
        // Kreivo's dUSD (Sibling(1000, 50, 50000002)) and Here(103) have no metadata.
        let mut none = kreivo_copm();
        none.facts = None;
        assert_eq!(admit(&copm(), &none), Err(Mismatch::Absent));
        assert_eq!(
            admit(&copm(), &with(|f| f.symbol = Vec::new())),
            Err(Mismatch::Absent)
        );
        // The adapter maps TSTA there, not COPM.
        let mut other = kreivo_copm();
        other.asset = AssetId::parse("TSTA").unwrap();
        assert_eq!(admit(&copm(), &other), Err(Mismatch::Mapping));
    }

    #[test]
    fn the_canonical_forms_round_trip() {
        let a = kreivo_copm();
        assert_eq!(
            crate::export::canon::from_bytes::<AssetAttestation>(&crate::export::canon::to_bytes(
                &a
            )),
            Ok(a)
        );
        let b = AssetBind {
            reference: OperationId::from_ref(ledger_kernel::posting::OperationRef([7; 16])),
            asset: AssetId::parse("COPM").unwrap(),
            location: loc(100),
            amount: Minor::new(95_147_535),
            requested_at: 9,
            status: BindStatus::Bound,
            coordinate: Some(Coordinate {
                instance: 1,
                seq: 4,
                digest: [3; 32],
            }),
        };
        assert_eq!(
            crate::export::canon::from_bytes::<AssetBind>(&crate::export::canon::to_bytes(&b)),
            Ok(b)
        );
    }
}
