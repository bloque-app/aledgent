// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The asset registry and the one decimal boundary (`F-003` §2, `REQ-AS-1`–`REQ-AS-4`, `INV-7`,
//! `INV-24`).
//!
//! - **One registry.** [`AssetRegistry`] is the only type that holds an asset's precision. The
//!   service store holds exactly one of them and no constructor outside this crate exists, so a
//!   second registry cannot be formed:
//!
//!   ```compile_fail
//!   // INV-24: there is no way to form a second registry.
//!   let _ = ledger_core::asset::AssetRegistry::new();
//!   ```
//!
//! - **Normalised ids.** An [`AssetId`] is upper-case ASCII `[A-Z0-9]{2,12}`. Every boundary
//!   normalises through [`AssetId::parse`], so two spellings of one asset cannot be two assets
//!   (`REQ-AS-3`).
//! - **One boundary.** [`Minor::from_decimal`] is the only conversion from a decimal string to minor
//!   units in the workspace, and [`Minor::to_decimal`] the only one back (`REQ-AS-4`). It never
//!   rounds or truncates: more fractional digits than the asset's precision is
//!   `ERR-PrecisionExceeded`. Every internal amount is a [`Minor`] or a [`NonZeroMinor`], an exact
//!   `u64` count of minor units. There is no floating-point type anywhere (`INV-7`):
//!
//!   ```compile_fail
//!   // INV-7: a floating-point amount cannot be expressed.
//!   let _ = ledger_core::asset::Minor::new(1.5);
//!   ```
//!
//! - **Kernel tags.** Registration assigns the asset its kernel tag (CipherClerk's `ledger` field,
//!   [`AssetTag`]) once. Tags are never reused, because nothing is ever removed.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt;
use core::num::NonZeroU64;

use cipher_clerk::crypto::blake2b_256;
pub use ledger_kernel::account::AssetTag;

use crate::refusal::Refusal;

/// The shortest asset id.
pub const ASSET_ID_MIN: usize = 2;
/// The longest asset id.
pub const ASSET_ID_MAX: usize = 12;
/// The largest precision the registry accepts. With `u64` minor units, 10^18 minor units is about
/// 18.4 whole units at most: beyond it an asset could not hold even one whole unit exactly
/// (`F-003` Open points: the owner sets the real threshold per expected supply).
pub const MAX_PRECISION: u8 = 18;
/// Domain of the registry digest.
const DIGEST_DOMAIN: &[u8] = b"aledgent/asset-registry/v1";

/// An asset's identity, normalised: upper-case ASCII `[A-Z0-9]{2,12}` (`REQ-AS-3`).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AssetId {
    bytes: [u8; ASSET_ID_MAX],
    len: u8,
}

impl AssetId {
    /// Normalise a spelling: ASCII letters are upper-cased; anything outside `[A-Za-z0-9]`, or a
    /// length outside 2–12, names no asset and is `ERR-UnknownAsset`.
    pub fn parse(s: &str) -> Result<AssetId, Refusal> {
        let raw = s.as_bytes();
        if raw.len() < ASSET_ID_MIN || raw.len() > ASSET_ID_MAX {
            return Err(Refusal::UnknownAsset);
        }
        let mut bytes = [0u8; ASSET_ID_MAX];
        for (slot, c) in bytes.iter_mut().zip(raw) {
            if !c.is_ascii_alphanumeric() {
                return Err(Refusal::UnknownAsset);
            }
            *slot = c.to_ascii_uppercase();
        }
        Ok(AssetId {
            bytes,
            len: raw.len() as u8,
        })
    }

    /// The normalised spelling.
    pub fn as_str(&self) -> &str {
        // Only ASCII alphanumerics are ever stored.
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("")
    }
}

impl fmt::Display for AssetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for AssetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AssetId({})", self.as_str())
    }
}

/// The longest canonical spelling of an [`AssetLocation`].
pub const LOCATION_MAX: usize = 64;
/// The most fields a location may carry.
const LOCATION_FIELDS_MAX: usize = 4;
/// The longest variant name of a location.
const LOCATION_NAME_MAX: usize = 32;

/// Where an asset lives on the settlement layer (`DEC-20`): the one location a bound asset of the
/// registry is tied to, by its binding (`REQ-AS-8`). Opaque to the book beyond its spelling: a variant name and up to four
/// unsigned integer fields, as the settlement layer's own textual form writes them (`Here(100)`,
/// `Sibling(1000, 50, 50000002)`).
///
/// Normalised at every boundary through [`AssetLocation::parse`], so two spellings of one
/// location cannot be two locations (`REQ-AS-3`): whitespace is dropped, leading zeros are dropped,
/// and fields are written back separated by `", "`. The name is case-sensitive, as the layer's is.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AssetLocation {
    bytes: [u8; LOCATION_MAX],
    len: u8,
}

impl AssetLocation {
    /// Normalise a spelling: `Name` or `Name(n, …)`, the name `[A-Za-z][A-Za-z0-9]*` of at most 32
    /// characters, at most four fields, each a decimal integer below 2^128. Anything else names no
    /// location and is `ERR-UnknownAsset`.
    pub fn parse(s: &str) -> Result<AssetLocation, Refusal> {
        let bad = Refusal::UnknownAsset;
        let t: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        let (name, fields) = match t.split_once('(') {
            Some((n, rest)) => (n, Some(rest.strip_suffix(')').ok_or(bad)?)),
            None => (t.as_str(), None),
        };
        let mut chars = name.bytes();
        if !chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            || !chars.all(|c| c.is_ascii_alphanumeric())
            || name.len() > LOCATION_NAME_MAX
        {
            return Err(bad);
        }
        let mut out = String::from(name);
        if let Some(fields) = fields {
            let parts: Vec<&str> = fields.split(',').collect();
            if parts.len() > LOCATION_FIELDS_MAX {
                return Err(bad);
            }
            out.push('(');
            for (i, p) in parts.iter().enumerate() {
                if p.is_empty() || !p.bytes().all(|c| c.is_ascii_digit()) {
                    return Err(bad);
                }
                let n: u128 = p.parse().map_err(|_| bad)?;
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&alloc::format!("{n}"));
            }
            out.push(')');
        }
        if out.len() > LOCATION_MAX {
            return Err(bad);
        }
        let mut bytes = [0u8; LOCATION_MAX];
        bytes[..out.len()].copy_from_slice(out.as_bytes());
        Ok(AssetLocation {
            bytes,
            len: out.len() as u8,
        })
    }

    /// The normalised spelling.
    pub fn as_str(&self) -> &str {
        // Only ASCII is ever stored.
        core::str::from_utf8(&self.bytes[..usize::from(self.len)]).unwrap_or("")
    }
}

impl fmt::Display for AssetLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for AssetLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "AssetLocation({})", self.as_str())
    }
}

#[cfg(feature = "serde")]
impl serde::Serialize for AssetLocation {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

#[cfg(feature = "serde")]
impl<'de> serde::Deserialize<'de> for AssetLocation {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <String as serde::Deserialize>::deserialize(d)?;
        AssetLocation::parse(&s).map_err(|_| serde::de::Error::custom("not an asset location"))
    }
}

/// What kind of value an asset is (SPEC §5.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum AssetKind {
    /// A fiat currency.
    Fiat,
    /// A stable-value token.
    Stable,
    /// The settlement layer's native asset.
    Native,
    /// Anything else.
    Other,
}

impl AssetKind {
    const fn code(self) -> u8 {
        match self {
            AssetKind::Fiat => 1,
            AssetKind::Stable => 2,
            AssetKind::Native => 3,
            AssetKind::Other => 4,
        }
    }
}

/// Whether an asset accepts new operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AssetStatus {
    /// It accepts new operations.
    Active,
    /// It is registered but accepts no new operations (`ERR-AssetDeprecated`).
    Deprecated,
}

impl AssetStatus {
    const fn code(self) -> u8 {
        match self {
            AssetStatus::Active => 1,
            AssetStatus::Deprecated => 2,
        }
    }
}

/// An asset's tie to the settlement layer (amendment 0011, `REQ-AS-7`, `REQ-AS-8`, `DEC-20`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AssetBinding {
    /// In the book only. Its value enters and leaves by issuance and redemption; its external
    /// legs are refused (`REQ-AS-7`).
    BookOnly,
    /// A binding to this location was asked for and is not final yet. Its issuance and
    /// redemption are refused, so the outstanding balance the layer is checking cannot move.
    Binding(AssetLocation),
    /// Bound to this location, for good (`REQ-AS-8`, `REQ-AS-9`).
    Bound(AssetLocation),
}

impl AssetBinding {
    /// The location it names, if any.
    pub const fn location(&self) -> Option<AssetLocation> {
        match self {
            AssetBinding::BookOnly => None,
            AssetBinding::Binding(l) | AssetBinding::Bound(l) => Some(*l),
        }
    }

    /// The location it is bound to, once the binding is final.
    pub const fn bound(&self) -> Option<AssetLocation> {
        match self {
            AssetBinding::Bound(l) => Some(*l),
            _ => None,
        }
    }

    const fn code(&self) -> u8 {
        match self {
            AssetBinding::BookOnly => 0,
            AssetBinding::Binding(_) => 1,
            AssetBinding::Bound(_) => 2,
        }
    }
}

impl crate::export::canon::Canonical for AssetBinding {
    fn encode(&self, e: &mut crate::export::canon::Enc) {
        e.put(&self.code());
        if let Some(l) = self.location() {
            e.put(&l);
        }
    }
    fn decode(
        d: &mut crate::export::canon::Dec<'_>,
    ) -> Result<Self, crate::export::canon::DecodeError> {
        Ok(match d.get::<u8>()? {
            0 => AssetBinding::BookOnly,
            1 => AssetBinding::Binding(d.get()?),
            2 => AssetBinding::Bound(d.get()?),
            _ => return Err(crate::export::canon::DecodeError("asset binding")),
        })
    }
}

/// A registered asset. Its fields are read-only outside the registry: the precision lives here
/// and nowhere else (`REQ-AS-1`, `INV-24`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Asset {
    id: AssetId,
    tag: AssetTag,
    binding: AssetBinding,
    precision: u8,
    kind: AssetKind,
    status: AssetStatus,
}

impl Asset {
    /// Its normalised id.
    pub const fn id(&self) -> AssetId {
        self.id
    }

    /// Its kernel tag.
    pub const fn tag(&self) -> AssetTag {
        self.tag
    }

    /// Its tie to the settlement layer: book-only, being bound, or bound (`REQ-AS-8`).
    pub const fn binding(&self) -> AssetBinding {
        self.binding
    }

    /// Minor units per unit, as a power of ten.
    pub const fn precision(&self) -> u8 {
        self.precision
    }

    /// Its kind.
    pub const fn kind(&self) -> AssetKind {
        self.kind
    }

    /// Whether it accepts new operations.
    pub const fn status(&self) -> AssetStatus {
        self.status
    }

    /// `Ok` if the asset accepts new operations, else `ERR-AssetDeprecated`.
    pub const fn require_active(&self) -> Result<&Asset, Refusal> {
        match self.status {
            AssetStatus::Active => Ok(self),
            AssetStatus::Deprecated => Err(Refusal::AssetDeprecated),
        }
    }
}

/// Why the registry refused a change. These are refusals of the registry's own rules, reached only
/// through a reviewed change (`REQ-AS-2`, `F-013`), never through a holder's command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegistryError {
    /// An asset with this id is already registered. Precision, tag and kind never change.
    AlreadyRegistered(AssetId),
    /// No asset with this id is registered.
    NotRegistered(AssetId),
    /// Another asset is bound, or being bound, to this settlement-layer location (`DEC-20`): one
    /// location, one asset.
    LocationTaken(AssetLocation),
    /// The asset is not book-only: it is bound, or a binding is in flight (`REQ-AS-8`).
    NotBookOnly(AssetId),
    /// No binding of this asset is in flight.
    NotBinding(AssetId),
    /// The precision is above [`MAX_PRECISION`].
    PrecisionOutOfRange(u8),
    /// Every kernel tag has been assigned.
    TagsExhausted,
}

/// The one asset registry (`REQ-AS-1`, `INV-24`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssetRegistry {
    assets: BTreeMap<AssetId, Asset>,
    by_tag: BTreeMap<AssetTag, AssetId>,
    by_location: BTreeMap<AssetLocation, AssetId>,
    next_tag: u32,
}

impl Default for AssetRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// The registry as one leaf of the service tree (`crate::leaves`): every asset in tag order, then
/// the next tag. Decoding refuses anything but that order, a repeated identifier, a precision out
/// of range, and a next tag not past every tag.
impl crate::export::canon::Canonical for AssetRegistry {
    fn encode(&self, e: &mut crate::export::canon::Enc) {
        e.len(self.by_tag.len());
        for id in self.by_tag.values() {
            let a = &self.assets[id];
            e.put(&a.id)
                .put(&a.tag)
                .put(&a.binding)
                .put(&a.precision)
                .put(&a.kind)
                .put(&a.status);
        }
        e.put(&self.next_tag);
    }
    fn decode(
        d: &mut crate::export::canon::Dec<'_>,
    ) -> Result<Self, crate::export::canon::DecodeError> {
        use crate::export::canon::DecodeError;
        let mut reg = AssetRegistry::new();
        let n = d.len()?;
        let mut last = 0u32;
        for _ in 0..n {
            let a = Asset {
                id: d.get()?,
                tag: d.get()?,
                binding: d.get()?,
                precision: d.get()?,
                kind: d.get()?,
                status: d.get()?,
            };
            if a.tag.0 <= last
                || a.precision > MAX_PRECISION
                || reg.assets.contains_key(&a.id)
                || a.binding
                    .location()
                    .is_some_and(|l| reg.by_location.contains_key(&l))
            {
                return Err(DecodeError("registry"));
            }
            last = a.tag.0;
            reg.by_tag.insert(a.tag, a.id);
            if let Some(l) = a.binding.location() {
                reg.by_location.insert(l, a.id);
            }
            reg.assets.insert(a.id, a);
        }
        reg.next_tag = d.get()?;
        if reg.next_tag <= last {
            return Err(DecodeError("registry"));
        }
        Ok(reg)
    }
}

impl AssetRegistry {
    /// An empty registry. Crate-private: the service store holds the only one.
    pub(crate) fn new() -> Self {
        Self {
            assets: BTreeMap::new(),
            by_tag: BTreeMap::new(),
            by_location: BTreeMap::new(),
            next_tag: 1,
        }
    }

    /// Register an asset book-only, with exactly one precision, and assign its kernel tag
    /// (`REQ-AS-1`, `REQ-AS-7`).
    pub(crate) fn register(
        &mut self,
        id: AssetId,
        precision: u8,
        kind: AssetKind,
    ) -> Result<Asset, RegistryError> {
        if self.assets.contains_key(&id) {
            return Err(RegistryError::AlreadyRegistered(id));
        }
        if precision > MAX_PRECISION {
            return Err(RegistryError::PrecisionOutOfRange(precision));
        }
        let tag = AssetTag(self.next_tag);
        let next = self
            .next_tag
            .checked_add(1)
            .ok_or(RegistryError::TagsExhausted)?;
        let asset = Asset {
            id,
            tag,
            binding: AssetBinding::BookOnly,
            precision,
            kind,
            status: AssetStatus::Active,
        };
        self.assets.insert(id, asset);
        self.by_tag.insert(tag, id);
        self.next_tag = next;
        Ok(asset)
    }

    /// Move an asset's binding (`REQ-AS-8`): `BookOnly → Binding(location)` when one is asked
    /// for, `Binding → Bound` when it is final, `Binding → BookOnly` when the layer refused it.
    /// Returns the binding it had, for a store rollback. Every other move is refused: a binding
    /// is asked for once at a time, and never undone.
    pub(crate) fn set_binding(
        &mut self,
        id: &AssetId,
        to: AssetBinding,
    ) -> Result<AssetBinding, RegistryError> {
        let asset = self
            .assets
            .get(id)
            .ok_or(RegistryError::NotRegistered(*id))?;
        let from = asset.binding;
        match (from, to) {
            (AssetBinding::BookOnly, AssetBinding::Binding(l)) => {
                if self.by_location.contains_key(&l) {
                    return Err(RegistryError::LocationTaken(l));
                }
                self.by_location.insert(l, *id);
            }
            (AssetBinding::Binding(a), AssetBinding::Bound(b)) if a == b => {}
            (AssetBinding::Binding(l), AssetBinding::BookOnly) => {
                self.by_location.remove(&l);
            }
            (AssetBinding::Binding(_), _) => return Err(RegistryError::NotBinding(*id)),
            _ => return Err(RegistryError::NotBookOnly(*id)),
        }
        if let Some(a) = self.assets.get_mut(id) {
            a.binding = to;
        }
        Ok(from)
    }

    /// Undo [`AssetRegistry::set_binding`] made in the same store batch (store rollback only).
    pub(crate) fn restore_binding(&mut self, id: &AssetId, binding: AssetBinding) {
        let Some(a) = self.assets.get_mut(id) else {
            return;
        };
        if let Some(l) = a.binding.location() {
            self.by_location.remove(&l);
        }
        a.binding = binding;
        if let Some(l) = binding.location() {
            self.by_location.insert(l, *id);
        }
    }

    /// Mark an asset deprecated. Nothing else about it changes, and it is never removed.
    pub(crate) fn deprecate(&mut self, id: &AssetId) -> Result<AssetStatus, RegistryError> {
        let asset = self
            .assets
            .get_mut(id)
            .ok_or(RegistryError::NotRegistered(*id))?;
        let before = asset.status;
        asset.status = AssetStatus::Deprecated;
        Ok(before)
    }

    /// Undo a deprecation made in the same store batch (store rollback only).
    pub(crate) fn restore_status(&mut self, id: &AssetId, status: AssetStatus) {
        if let Some(a) = self.assets.get_mut(id) {
            a.status = status;
        }
    }

    /// Undo a registration made in the same store batch (store rollback only).
    pub(crate) fn unregister_last(&mut self, id: &AssetId) {
        if let Some(a) = self.assets.remove(id) {
            self.by_tag.remove(&a.tag);
            if let Some(l) = a.binding.location() {
                self.by_location.remove(&l);
            }
            self.next_tag = a.tag.0;
        }
    }

    /// The registered asset, or `ERR-UnknownAsset`.
    pub fn get(&self, id: &AssetId) -> Result<&Asset, Refusal> {
        self.assets.get(id).ok_or(Refusal::UnknownAsset)
    }

    /// The registered asset if it accepts new operations: `ERR-UnknownAsset` or
    /// `ERR-AssetDeprecated` otherwise.
    pub fn active(&self, id: &AssetId) -> Result<&Asset, Refusal> {
        self.get(id)?.require_active()
    }

    /// The asset with this kernel tag.
    pub fn by_tag(&self, tag: AssetTag) -> Option<&Asset> {
        self.by_tag.get(&tag).and_then(|id| self.assets.get(id))
    }

    /// The asset bound, or being bound, to this settlement-layer location, if one is (`DEC-20`).
    pub fn at_location(&self, location: &AssetLocation) -> Option<&Asset> {
        self.by_location
            .get(location)
            .and_then(|id| self.assets.get(id))
    }

    /// Every registered asset, by id.
    pub fn iter(&self) -> impl Iterator<Item = &Asset> {
        self.assets.values()
    }

    /// How many assets are registered.
    pub fn len(&self) -> usize {
        self.assets.len()
    }

    /// Whether no asset is registered.
    pub fn is_empty(&self) -> bool {
        self.assets.is_empty()
    }

    /// A digest of the whole registry: id, tag, binding, precision, kind and status of every
    /// asset, in id order.
    pub fn digest(&self) -> [u8; 32] {
        let mut buf: Vec<u8> = Vec::with_capacity(self.assets.len() * 20);
        for a in self.assets.values() {
            buf.push(a.id.len);
            buf.extend_from_slice(a.id.as_str().as_bytes());
            buf.extend_from_slice(&a.tag.0.to_be_bytes());
            buf.push(a.binding.code());
            if let Some(l) = a.binding.location() {
                buf.push(l.len);
                buf.extend_from_slice(l.as_str().as_bytes());
            }
            buf.push(a.precision);
            buf.push(a.kind.code());
            buf.push(a.status.code());
        }
        blake2b_256(DIGEST_DOMAIN, &[&buf])
    }
}

/// An exact count of minor units of one asset (`INV-7`). It may be zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Minor(u64);

impl Minor {
    /// Zero minor units.
    pub const ZERO: Minor = Minor(0);

    /// A count of minor units.
    pub const fn new(units: u64) -> Minor {
        Minor(units)
    }

    /// The count.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The sum, or `None` past `u64::MAX`. Never wraps.
    pub const fn checked_add(self, other: Minor) -> Option<Minor> {
        match self.0.checked_add(other.0) {
            Some(v) => Some(Minor(v)),
            None => None,
        }
    }

    /// The only conversion from a human-readable decimal to minor units (`REQ-AS-4`).
    ///
    /// Accepts `<digits>` or `<digits>.<digits>`: no sign, exponent, separator or whitespace.
    /// More fractional digits than the asset's precision is `ERR-PrecisionExceeded`; the value is
    /// never rounded or truncated. A string that is not such a decimal, or whose value does not fit
    /// in `u64` minor units, cannot be represented exactly either, and is refused the same way:
    /// SPEC §10's list is closed and has no other entry for it (`NOTES.md`).
    pub fn from_decimal(s: &str, asset: &Asset) -> Result<Minor, Refusal> {
        let (whole, frac) = match s.split_once('.') {
            Some((w, f)) => (w, Some(f)),
            None => (s, None),
        };
        let all_digits = |p: &str| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit());
        if !all_digits(whole) || frac.is_some_and(|f| !all_digits(f)) {
            return Err(Refusal::PrecisionExceeded);
        }
        let frac = frac.unwrap_or("");
        let precision = usize::from(asset.precision);
        if frac.len() > precision {
            return Err(Refusal::PrecisionExceeded);
        }
        let mut value: u64 = 0;
        let digits = whole
            .bytes()
            .chain(frac.bytes())
            .chain(core::iter::repeat_n(b'0', precision - frac.len()));
        for c in digits {
            value = value
                .checked_mul(10)
                .and_then(|v| v.checked_add(u64::from(c - b'0')))
                .ok_or(Refusal::PrecisionExceeded)?;
        }
        Ok(Minor(value))
    }

    /// The same boundary, the other way: minor units as a decimal with exactly the asset's
    /// precision in fractional digits. `from_decimal(to_decimal(m)) == m` for every `m`.
    pub fn to_decimal(self, asset: &Asset) -> String {
        let precision = usize::from(asset.precision);
        let digits = alloc::format!("{:0>width$}", self.0, width = precision + 1);
        if precision == 0 {
            return digits;
        }
        let (whole, frac) = digits.split_at(digits.len() - precision);
        alloc::format!("{whole}.{frac}")
    }
}

/// An exact, non-zero count of minor units: the amount of a movement (`ERR-ZeroAmount`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct NonZeroMinor(NonZeroU64);

impl NonZeroMinor {
    /// The amount of a movement, or `ERR-ZeroAmount`.
    pub const fn new(amount: Minor) -> Result<NonZeroMinor, Refusal> {
        match NonZeroU64::new(amount.0) {
            Some(v) => Ok(NonZeroMinor(v)),
            None => Err(Refusal::ZeroAmount),
        }
    }

    /// The count, as the kernel binding takes it.
    pub const fn get(self) -> NonZeroU64 {
        self.0
    }

    /// The count as a possibly-zero amount.
    pub const fn minor(self) -> Minor {
        Minor(self.0.get())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use alloc::format;
    use proptest::prelude::*;

    /// A test location, `Here(n)`.
    pub(crate) fn loc(n: u32) -> AssetLocation {
        AssetLocation::parse(&format!("Here({n})")).unwrap()
    }

    pub(crate) fn registry() -> (AssetRegistry, Asset, Asset) {
        let mut r = AssetRegistry::new();
        let usd = r
            .register(AssetId::parse("USD").unwrap(), 2, AssetKind::Fiat)
            .unwrap();
        let btc = r
            .register(AssetId::parse("btc").unwrap(), 8, AssetKind::Native)
            .unwrap();
        (r, usd, btc)
    }

    #[test]
    fn req_as_3_spellings_normalise_to_one_asset() {
        let (mut r, usd, _) = registry();
        assert_eq!(AssetId::parse("usd").unwrap(), usd.id());
        assert_eq!(AssetId::parse("uSd").unwrap(), usd.id());
        assert_eq!(
            r.register(AssetId::parse("usd").unwrap(), 6, AssetKind::Stable),
            Err(RegistryError::AlreadyRegistered(usd.id()))
        );
        for bad in ["", "U", "US D", "US-D", "USDUSDUSDUSDX", "ÜSD", "U$D"] {
            assert_eq!(AssetId::parse(bad), Err(Refusal::UnknownAsset), "{bad}");
        }
    }

    #[test]
    fn dec_20_req_as_3_two_spellings_of_a_location_are_one_location() {
        let a = AssetLocation::parse("Sibling(1000, 50, 50000002)").unwrap();
        for same in [
            "Sibling(1000,50,50000002)",
            " Sibling( 01000 , 050,50000002 ) ",
        ] {
            assert_eq!(AssetLocation::parse(same).unwrap(), a, "{same}");
        }
        assert_eq!(a.as_str(), "Sibling(1000, 50, 50000002)");
        assert_eq!(
            AssetLocation::parse("Here(100)").unwrap().as_str(),
            "Here(100)"
        );
        assert_eq!(
            AssetLocation::parse("PolkadotNativeDOT").unwrap().as_str(),
            "PolkadotNativeDOT"
        );
        // The name is the layer's, case and all.
        assert_ne!(AssetLocation::parse("here(100)").unwrap(), loc(100));
        for bad in [
            "",
            "(1)",
            "1Here(1)",
            "Here(",
            "Here()",
            "Here(1,)",
            "Here(-1)",
            "Here(1.5)",
            "Here(1)x",
            "Here(1,2,3,4,5)",
            "Here(340282366920938463463374607431768211456)",
            "Ünïcode(1)",
        ] {
            assert_eq!(
                AssetLocation::parse(bad),
                Err(Refusal::UnknownAsset),
                "{bad}"
            );
        }
    }

    #[test]
    fn req_as_8_one_location_one_asset_and_a_binding_moves_once() {
        let (mut r, usd, btc) = registry();
        // Registered book-only (REQ-AS-7).
        assert_eq!(usd.binding(), AssetBinding::BookOnly);
        assert_eq!(r.at_location(&loc(1)), None);
        let before = r.digest();
        assert_eq!(
            r.set_binding(&usd.id(), AssetBinding::Binding(loc(1))),
            Ok(AssetBinding::BookOnly)
        );
        assert_ne!(r.digest(), before, "the binding is part of the digest");
        assert_eq!(r.at_location(&loc(1)).map(|a| a.id()), Some(usd.id()));
        // One location, one asset, even while the first binding is in flight.
        assert_eq!(
            r.set_binding(&btc.id(), AssetBinding::Binding(loc(1))),
            Err(RegistryError::LocationTaken(loc(1)))
        );
        // A binding is asked for once at a time.
        assert_eq!(
            r.set_binding(&usd.id(), AssetBinding::Binding(loc(2))),
            Err(RegistryError::NotBinding(usd.id()))
        );
        // Refused by the layer: book-only again, and the location is free.
        assert_eq!(
            r.set_binding(&usd.id(), AssetBinding::BookOnly),
            Ok(AssetBinding::Binding(loc(1)))
        );
        assert_eq!(r.at_location(&loc(1)), None);
        assert_eq!(r.digest(), before);
        // Asked again, then final: bound for good.
        r.set_binding(&usd.id(), AssetBinding::Binding(loc(1)))
            .unwrap();
        assert_eq!(
            r.set_binding(&usd.id(), AssetBinding::Bound(loc(2))),
            Err(RegistryError::NotBinding(usd.id()))
        );
        r.set_binding(&usd.id(), AssetBinding::Bound(loc(1)))
            .unwrap();
        for to in [
            AssetBinding::BookOnly,
            AssetBinding::Binding(loc(3)),
            AssetBinding::Bound(loc(1)),
        ] {
            assert_eq!(
                r.set_binding(&usd.id(), to),
                Err(RegistryError::NotBookOnly(usd.id())),
                "{to:?}"
            );
        }
        // The rollback restores the index too.
        r.restore_binding(&usd.id(), AssetBinding::BookOnly);
        assert_eq!(r.at_location(&loc(1)), None);
        assert_eq!(r.digest(), before);
    }

    #[test]
    fn req_as_8_the_registry_round_trips_every_binding() {
        use crate::export::canon::Canonical;
        let (mut r, usd, btc) = registry();
        r.set_binding(&usd.id(), AssetBinding::Binding(loc(4)))
            .unwrap();
        r.set_binding(&btc.id(), AssetBinding::Binding(loc(5)))
            .unwrap();
        r.set_binding(&btc.id(), AssetBinding::Bound(loc(5)))
            .unwrap();
        let mut e = crate::export::canon::Enc::new();
        r.encode(&mut e);
        let bytes = e.into_bytes();
        let mut d = crate::export::canon::Dec::new(&bytes);
        let back = AssetRegistry::decode(&mut d).unwrap();
        assert_eq!(back, r);
        assert_eq!(back.at_location(&loc(5)).map(|a| a.id()), Some(btc.id()));
    }

    #[test]
    fn inv_24_tags_are_assigned_once_and_precision_lives_only_here() {
        let (mut r, usd, btc) = registry();
        assert_eq!(usd.tag(), AssetTag(1));
        assert_eq!(btc.tag(), AssetTag(2));
        assert_eq!(r.by_tag(AssetTag(2)).unwrap().id(), btc.id());
        let before = r.digest();
        assert_eq!(r.deprecate(&usd.id()), Ok(AssetStatus::Active));
        assert_ne!(r.digest(), before);
        let usd_after = *r.get(&usd.id()).unwrap();
        assert_eq!(usd_after.precision(), 2);
        assert_eq!(usd_after.tag(), AssetTag(1));
        assert_eq!(r.active(&usd.id()), Err(Refusal::AssetDeprecated));
        assert_eq!(
            r.get(&AssetId::parse("EUR").unwrap()),
            Err(Refusal::UnknownAsset)
        );
        assert_eq!(
            r.register(AssetId::parse("ETH").unwrap(), 19, AssetKind::Native),
            Err(RegistryError::PrecisionOutOfRange(19))
        );
        let eth = r
            .register(AssetId::parse("ETH").unwrap(), 18, AssetKind::Native)
            .unwrap();
        assert_eq!(eth.tag(), AssetTag(3));
    }

    #[test]
    fn ac_f3_1_excess_precision_is_refused_not_rounded() {
        let (_, usd, btc) = registry();
        assert_eq!(Minor::from_decimal("12.34", &usd), Ok(Minor(1234)));
        assert_eq!(Minor::from_decimal("12.3", &usd), Ok(Minor(1230)));
        assert_eq!(Minor::from_decimal("12", &usd), Ok(Minor(1200)));
        assert_eq!(Minor::from_decimal("0.00000001", &btc), Ok(Minor(1)));
        for bad in [
            "12.345", "0.001", "12.340", "1e2", "-1", "+1", " 1", "1.", ".5", "", "1,5",
        ] {
            assert_eq!(
                Minor::from_decimal(bad, &usd),
                Err(Refusal::PrecisionExceeded),
                "{bad}"
            );
        }
        assert_eq!(
            Minor::from_decimal("184467440737095516.15", &usd),
            Ok(Minor(u64::MAX))
        );
        assert_eq!(
            Minor::from_decimal("184467440737095516.16", &usd),
            Err(Refusal::PrecisionExceeded)
        );
    }

    #[test]
    fn err_zero_amount_is_not_a_movement() {
        assert_eq!(NonZeroMinor::new(Minor::ZERO), Err(Refusal::ZeroAmount));
        assert_eq!(NonZeroMinor::new(Minor(5)).unwrap().get().get(), 5);
    }

    proptest! {
        // REQ-AS-4: from_decimal never rounds. It accepts exactly the strings with at most
        // `precision` fractional digits whose value fits, and then the value is exact.
        #[test]
        fn req_as_4_from_decimal_never_rounds(
            whole in "[0-9]{1,22}",
            frac in proptest::option::of("[0-9]{0,20}"),
            precision in 0u8..=MAX_PRECISION,
        ) {
            let mut r = AssetRegistry::new();
            let asset = r.register(AssetId::parse("AA").unwrap(), precision, AssetKind::Other).unwrap();
            let s = match &frac { Some(f) => format!("{whole}.{f}"), None => whole.clone() };
            let got = Minor::from_decimal(&s, &asset);
            let f = frac.clone().unwrap_or_default();
            let well_formed = frac.as_ref().is_none_or(|f| !f.is_empty());
            // The exact value in minor units, in u128 arithmetic, when the string can carry it.
            let exact: Option<u128> = if well_formed && f.len() <= usize::from(precision) {
                let scaled = format!("{whole}{f}{}", "0".repeat(usize::from(precision) - f.len()));
                scaled.parse::<u128>().ok()
            } else { None };
            match (exact, got) {
                (Some(v), Ok(m)) => {
                    prop_assert_eq!(u128::from(m.get()), v);
                    prop_assert_eq!(Minor::from_decimal(&m.to_decimal(&asset), &asset), Ok(m));
                }
                (Some(v), Err(e)) => {
                    prop_assert!(v > u128::from(u64::MAX));
                    prop_assert_eq!(e, Refusal::PrecisionExceeded);
                }
                (None, got) => prop_assert_eq!(got, Err(Refusal::PrecisionExceeded)),
            }
        }

        #[test]
        fn to_decimal_round_trips(units in any::<u64>(), precision in 0u8..=MAX_PRECISION) {
            let mut r = AssetRegistry::new();
            let asset = r.register(AssetId::parse("AA").unwrap(), precision, AssetKind::Other).unwrap();
            let s = Minor(units).to_decimal(&asset);
            prop_assert_eq!(Minor::from_decimal(&s, &asset), Ok(Minor(units)));
        }
    }
}
