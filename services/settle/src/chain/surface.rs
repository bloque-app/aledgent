// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The transaction surface of a Kreivo runtime, read from its live metadata (`F-027` T-027-12,
//! `REQ-SA-9`, amendment 0031): what every transaction the executor builds must carry, as the
//! runtime itself describes it.
//!
//! [`super::kreivo::Surface`] holds three parts. Two are read straight off the runtime: its
//! `transaction_version` and the identifiers of its transaction extensions for extension version
//! 0, in order. The third, [`fingerprint`], is a digest of **the types of what a transaction
//! carries**: each of those extensions' explicit and implicit data types, and the
//! `Contracts::call` call the executor submits (its pallet's index, the call's index, and its
//! arguments' names and types). [`super::kreivo::TxFormat`] pins the digest of each runtime it was
//! pinned against, so a runtime whose types changed matches no format, whatever its
//! `spec_version` says.
//!
//! **Why not subxt's own validation hash** (`subxt_metadata::utils::validation`, what subxt uses
//! to check static code against live metadata). It hashes a composite's fields as an XOR, on
//! purpose: subxt encodes by field name, so it does not care about their order. A SCALE encoder
//! written by position, as [`super::kreivo`] is, does. Two fields of the same type swapped would
//! leave subxt's hash unchanged and every byte of the transaction wrong. The digest here is order
//! sensitive throughout: fields in order, variants by index, arrays with their length. It reads
//! names (fields', variants') too, and nothing else: no type paths, no type parameters, no docs,
//! so a type moved between crates keeps its digest, and a field renamed does not.
//!
//! The digest is this crate's own and does not change with subxt's releases. The pinned values
//! are derived from Kreivo's live metadata at specs 132, 133, 134 and 136, kept as reduced fixtures
//! (`services/settle/fixtures/surface-<spec>.json`: the type registry cut down to the types the
//! digest reads, which [`fixture`] rebuilds from a runtime's metadata and checks against the
//! full metadata's digest).

use std::collections::BTreeMap;

use parity_scale_codec::{Compact, Decode, Encode};
use scale_info::form::PortableForm;
use scale_info::{Field, PortableRegistry, TypeDef, TypeDefPrimitive, Variant};

use super::blake2_256;
use super::kreivo::EXTENSION_VERSION;

/// The call the executor submits: `call` of the contracts pallet.
pub const CONTRACTS_CALL: &str = "call";

/// What the digest reads, with the types it names: each extension (its identifier, its explicit
/// and implicit types), the contracts pallet's index, and its `call` call. A reduced fixture is
/// this, SCALE-encoded, with its registry cut down to these types.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct SurfaceTypes {
    /// The type registry the ids below point into.
    pub registry: PortableRegistry,
    /// Each transaction extension of extension version 0, in order: its identifier, and the ids
    /// of its explicit and implicit types.
    pub extensions: Vec<(String, u32, u32)>,
    /// The contracts pallet's index: the first byte of the call.
    pub pallet_index: u8,
    /// The contracts pallet's `call` call: its index (the call's second byte) and its arguments.
    pub call: Variant<PortableForm>,
}

impl SurfaceTypes {
    /// What the digest reads in `metadata`, whose contracts pallet is named `contracts`. `None` if
    /// the metadata has no extensions of extension version 0, no such pallet or no `call` in it.
    pub fn of(metadata: &subxt::Metadata, contracts: &str) -> Option<SurfaceTypes> {
        let extensions = metadata
            .extrinsic()
            .transaction_extensions_by_version(EXTENSION_VERSION)?
            .map(|e| (e.identifier().to_owned(), e.extra_ty(), e.additional_ty()))
            .collect();
        let pallet = metadata.pallet_by_name(contracts)?;
        let call = pallet.call_variant_by_name(CONTRACTS_CALL)?.clone();
        Some(SurfaceTypes {
            registry: metadata.types().clone(),
            extensions,
            pallet_index: pallet.index(),
            call,
        })
    }

    /// The extensions' identifiers, in order.
    pub fn identifiers(&self) -> Vec<String> {
        self.extensions.iter().map(|(n, _, _)| n.clone()).collect()
    }

    /// The digest of these types. See the module documentation.
    pub fn fingerprint(&self) -> [u8; 32] {
        let mut seen = BTreeMap::new();
        let mut b = b"aledgent/kreivo-transaction-surface/1".to_vec();
        Compact(u32::try_from(self.extensions.len()).unwrap_or(u32::MAX)).encode_to(&mut b);
        for (name, explicit, implicit) in &self.extensions {
            name.encode_to(&mut b);
            b.extend_from_slice(&shape(&self.registry, *explicit, &mut seen));
            b.extend_from_slice(&shape(&self.registry, *implicit, &mut seen));
        }
        self.pallet_index.encode_to(&mut b);
        variant(&self.registry, &self.call, &mut seen, &mut b);
        blake2_256(&b)
    }

    /// The same types with the registry cut down to what the digest reads, the ids renumbered:
    /// a fixture small enough to keep, whose [`SurfaceTypes::fingerprint`] is this one's.
    pub fn reduced(&self) -> SurfaceTypes {
        let mut keep: std::collections::BTreeSet<u32> = self
            .extensions
            .iter()
            .flat_map(|(_, e, i)| [*e, *i])
            .collect();
        keep.extend(self.call.fields.iter().map(|f| f.ty.id));
        let mut registry = self.registry.clone();
        let map = registry.retain(|id| keep.contains(&id));
        let to = |id: u32| map.get(&id).copied().unwrap_or(u32::MAX);
        let mut call = self.call.clone();
        call.fields = call
            .fields
            .into_iter()
            .map(|f| Field {
                ty: to(f.ty.id).into(),
                docs: Vec::new(),
                ..f
            })
            .collect();
        call.docs = Vec::new();
        SurfaceTypes {
            registry,
            extensions: self
                .extensions
                .iter()
                .map(|(n, e, i)| (n.clone(), to(*e), to(*i)))
                .collect(),
            pallet_index: self.pallet_index,
            call,
        }
    }
}

/// The digest of what the metadata's transaction surface carries ([`SurfaceTypes::fingerprint`]);
/// `None` when it has none to read ([`SurfaceTypes::of`]).
pub fn fingerprint(metadata: &subxt::Metadata, contracts: &str) -> Option<[u8; 32]> {
    SurfaceTypes::of(metadata, contracts).map(|s| s.fingerprint())
}

/// A reduced fixture of `metadata`'s surface (`Option<OpaqueMetadata>` bytes as
/// `Metadata_metadata_at_version` returns them), checked to digest as the full metadata does.
pub fn fixture(opaque: &[u8], contracts: &str) -> Result<SurfaceTypes, String> {
    let bytes = Option::<Vec<u8>>::decode(&mut &opaque[..])
        .map_err(|e| e.to_string())?
        .ok_or("no metadata")?;
    let metadata = subxt::Metadata::decode(&mut &bytes[..]).map_err(|e| e.to_string())?;
    let full = SurfaceTypes::of(&metadata, contracts).ok_or("no transaction surface")?;
    let reduced = full.reduced();
    if reduced.fingerprint() != full.fingerprint() {
        return Err("the reduced registry digests otherwise than the full one".into());
    }
    Ok(reduced)
}

/// The digest of the type `id`: its shape as SCALE encodes it, with field and variant names.
/// A type met again while its own digest is being taken (a recursive type) is a fixed marker.
fn shape(
    registry: &PortableRegistry,
    id: u32,
    seen: &mut BTreeMap<u32, Option<[u8; 32]>>,
) -> [u8; 32] {
    match seen.get(&id) {
        Some(Some(h)) => return *h,
        Some(None) => return blake2_256(b"recursive"),
        None => {}
    }
    seen.insert(id, None);
    let Some(ty) = registry.resolve(id) else {
        return blake2_256(b"missing");
    };
    let mut b = Vec::new();
    match &ty.type_def {
        TypeDef::Composite(c) => {
            b.push(0);
            fields(registry, &c.fields, seen, &mut b);
        }
        TypeDef::Variant(v) => {
            b.push(1);
            let mut variants: Vec<&Variant<PortableForm>> = v.variants.iter().collect();
            variants.sort_by_key(|v| v.index);
            Compact(u32::try_from(variants.len()).unwrap_or(u32::MAX)).encode_to(&mut b);
            for v in variants {
                variant(registry, v, seen, &mut b);
            }
        }
        TypeDef::Sequence(s) => {
            b.push(2);
            b.extend_from_slice(&shape(registry, s.type_param.id, seen));
        }
        TypeDef::Array(a) => {
            b.push(3);
            a.len.encode_to(&mut b);
            b.extend_from_slice(&shape(registry, a.type_param.id, seen));
        }
        TypeDef::Tuple(t) => {
            b.push(4);
            Compact(u32::try_from(t.fields.len()).unwrap_or(u32::MAX)).encode_to(&mut b);
            for f in &t.fields {
                b.extend_from_slice(&shape(registry, f.id, seen));
            }
        }
        TypeDef::Primitive(p) => {
            b.push(5);
            b.push(primitive(p));
        }
        TypeDef::Compact(c) => {
            b.push(6);
            b.extend_from_slice(&shape(registry, c.type_param.id, seen));
        }
        TypeDef::BitSequence(s) => {
            b.push(7);
            b.extend_from_slice(&shape(registry, s.bit_order_type.id, seen));
            b.extend_from_slice(&shape(registry, s.bit_store_type.id, seen));
        }
    }
    let h = blake2_256(&b);
    seen.insert(id, Some(h));
    h
}

fn fields(
    registry: &PortableRegistry,
    fields: &[Field<PortableForm>],
    seen: &mut BTreeMap<u32, Option<[u8; 32]>>,
    b: &mut Vec<u8>,
) {
    Compact(u32::try_from(fields.len()).unwrap_or(u32::MAX)).encode_to(b);
    for f in fields {
        f.name.encode_to(b);
        b.extend_from_slice(&shape(registry, f.ty.id, seen));
    }
}

fn variant(
    registry: &PortableRegistry,
    v: &Variant<PortableForm>,
    seen: &mut BTreeMap<u32, Option<[u8; 32]>>,
    b: &mut Vec<u8>,
) {
    v.index.encode_to(b);
    v.name.encode_to(b);
    fields(registry, &v.fields, seen, b);
}

fn primitive(p: &TypeDefPrimitive) -> u8 {
    match p {
        TypeDefPrimitive::Bool => 0,
        TypeDefPrimitive::Char => 1,
        TypeDefPrimitive::Str => 2,
        TypeDefPrimitive::U8 => 3,
        TypeDefPrimitive::U16 => 4,
        TypeDefPrimitive::U32 => 5,
        TypeDefPrimitive::U64 => 6,
        TypeDefPrimitive::U128 => 7,
        TypeDefPrimitive::U256 => 8,
        TypeDefPrimitive::I8 => 9,
        TypeDefPrimitive::I16 => 10,
        TypeDefPrimitive::I32 => 11,
        TypeDefPrimitive::I64 => 12,
        TypeDefPrimitive::I128 => 13,
        TypeDefPrimitive::I256 => 14,
    }
}

#[cfg(test)]
mod tests {
    use super::super::kreivo::{FORMAT_132, FORMAT_133, Surface, TxFormat};
    use super::*;

    /// A fixture's surface, its digest taken again from its registry.
    fn fixture_surface(spec: u32) -> (Surface, SurfaceTypes) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("fixtures/surface-{spec}.json"));
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(json["spec_version"], spec);
        let types =
            SurfaceTypes::decode(&mut &hex::decode(json["surface"].as_str().unwrap()).unwrap()[..])
                .unwrap();
        assert_eq!(
            hex::encode(types.fingerprint()),
            json["fingerprint"].as_str().unwrap(),
            "the digest of fixtures/surface-{spec}.json changed"
        );
        let surface = Surface {
            transaction_version: u32::try_from(json["transaction_version"].as_u64().unwrap())
                .unwrap(),
            extensions: types.identifiers(),
            types: Some(types.fingerprint()),
        };
        (surface, types)
    }

    /// `T-027-12`, `AC-E4.3`, `AC-E4.4`: the pinned formats' surfaces are those Kreivo's live
    /// metadata describes. Spec 133 and spec 134 (Kreivo `0.17.1`, enacted on mainnet on
    /// 2026-10-14) carry the same surface, so 134 is signed for in 133's format with no entry of
    /// its own; spec 132's differs in all three parts. Spec 136 (Kreivo `0.18.1`, live on mainnet
    /// from 2026-10-27) differs from 134 in its `transaction_version` alone (14), and is signed for
    /// in 133's format, which lists 14 since it was pinned against 0.18.1's crates
    /// (`kreivo-pin-136`).
    #[test]
    fn t_027_12_the_pinned_surfaces_are_kreivos_own() {
        let (s132, _) = fixture_surface(132);
        let (s133, _) = fixture_surface(133);
        let (s134, t134) = fixture_surface(134);
        let (s136, t136) = fixture_surface(136);
        assert_eq!(TxFormat::by_surface(&s132), Some(FORMAT_132));
        assert_eq!(TxFormat::by_surface(&s133), Some(FORMAT_133));
        assert_eq!(TxFormat::by_surface(&s134), Some(FORMAT_133));
        assert_eq!(TxFormat::by_surface(&s136), Some(FORMAT_133));
        assert_eq!(s133, s134);
        assert_eq!(
            (s134.transaction_version, s136.transaction_version),
            (13, 14)
        );
        assert_eq!(
            (&s134.extensions, s134.types),
            (&s136.extensions, s136.types)
        );
        assert_eq!((t136.pallet_index, t136.call.index), (80, 6));
        assert_ne!(s132.transaction_version, s133.transaction_version);
        assert_ne!(s132.extensions, s133.extensions);
        assert_ne!(s132.types, s133.types);
        // The call is Contracts (pallet 80) `call` (call 6), as `kreivo::contracts_call` encodes it.
        assert_eq!((t134.pallet_index, t134.call.index), (80, 6));
    }

    /// `T-027-12`: the digest sees what SCALE sees. A field order swapped, a variant renumbered, an
    /// array resized, a name changed: each is another digest. subxt's own validation hash is blind
    /// to the first (it XORs a composite's fields).
    #[test]
    fn t_027_12_the_digest_is_order_sensitive() {
        let (_, t) = fixture_surface(134);
        let base = t.fingerprint();
        // The call's arguments, two of them swapped.
        let mut swapped = t.clone();
        swapped.call.fields.swap(1, 2);
        assert_ne!(swapped.fingerprint(), base);
        // The call's index.
        let mut renumbered = t.clone();
        renumbered.call.index = 7;
        assert_ne!(renumbered.fingerprint(), base);
        // The pallet's index.
        let mut moved = t.clone();
        moved.pallet_index = 81;
        assert_ne!(moved.fingerprint(), base);
        // An extension's name, and two extensions in the other order.
        let mut renamed = t.clone();
        renamed.extensions[0].0 = "PassAuthenticateV2".into();
        assert_ne!(renamed.fingerprint(), base);
        let mut reordered = t.clone();
        reordered.extensions.swap(6, 7);
        assert_ne!(reordered.fingerprint(), base);
        // An extension whose explicit data is another's type.
        let mut retyped = t.clone();
        retyped.extensions[6].1 = retyped.extensions[8].1;
        assert_ne!(retyped.fingerprint(), base);
        // Unchanged: the same digest, and a reduced copy digests the same.
        assert_eq!(t.clone().fingerprint(), base);
        assert_eq!(t.reduced().fingerprint(), base);
    }

    /// Writes `fixtures/surface-<spec>.json` from Kreivo metadata saved as
    /// `Metadata_metadata_at_version(15)`'s answer (`ALEDGENT_SURFACE_METADATA=<spec>=<file>,…`).
    /// Run by hand when a format is added; the fixtures it writes are what the other tests read.
    #[test]
    #[ignore = "reads metadata saved from a live runtime; run by hand to add a fixture"]
    fn t_027_12_write_surface_fixtures() {
        let list = std::env::var("ALEDGENT_SURFACE_METADATA").unwrap();
        for item in list.split(',') {
            let (spec, rest) = item.split_once('=').unwrap();
            let (tv, file) = rest.split_once('=').unwrap();
            let opaque = std::fs::read(file).unwrap();
            let reduced = fixture(&opaque, "Contracts").unwrap();
            let json = serde_json::json!({
                "spec_version": spec.parse::<u32>().unwrap(),
                "transaction_version": tv.parse::<u32>().unwrap(),
                "extensions": reduced.identifiers(),
                "fingerprint": hex::encode(reduced.fingerprint()),
                "surface": hex::encode(reduced.encode()),
            });
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("fixtures/surface-{spec}.json"));
            std::fs::write(&path, serde_json::to_string_pretty(&json).unwrap() + "\n").unwrap();
            println!("{spec}: {}", hex::encode(reduced.fingerprint()));
        }
    }
}
