// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Opening balances: the opening set, its reconciliation, the zero-divergence gate, and the
//! service store's record of opening positions and loads (`F-014` T-014-06, T-014-07,
//! `REQ-RC-4`, `NFR-9`, `REQ-AS-4`).
//!
//! ## The set and its reconciliation
//!
//! An [`OpeningSet`] is one row per (account, asset) with its exact balance in minor units, and
//! the **source map** from each independent source's references to the ledger accounts they
//! name. Its digest covers both, so the map cannot change after the set was reconciled.
//!
//! A [`Reconciliation`] carries, for one set, what each independent source says: a
//! [`SourceExtract`] per source, its rows in the source's own references and in the source's own
//! decimal figures.
//!
//! ## The gate ([`reconcile`])
//!
//! The library checks, itself, with no "passed" flag taken on trust:
//!
//! - the reconciliation is of this set (its digest);
//! - every source the deployment requires for each asset of the set is present, once, and its
//!   extract's digest is the digest of its rows;
//! - each source's rows map, through the set's source map, onto exactly the set's (account, asset)
//!   keys in the assets that source is required for: nothing missing, nothing extra, nothing twice;
//! - every figure converts to minor units at the asset's precision through the one decimal
//!   boundary (`REQ-AS-4`): a figure that would need rounding is refused, never rounded;
//! - every amount equals the set's exactly: the difference is zero, at one minor unit resolution
//!   (`NFR-9`).
//!
//! There is no override. A set with any finding does not load (`REQ-RC-4`).

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;

use cipher_clerk::crypto::blake2b_256;

use crate::account::Time;
use crate::asset::{AssetId, AssetRegistry, Minor};
use crate::authority::AuthorityUsed;
use crate::export::canon::{self, Canonical, Dec, DecodeError, Enc};
use crate::id::{AccountId, OperationId};
use crate::leaves::{LeafHost, LeafMap, Leaves, kind};
use crate::set::NonEmptySet;

const SET_DOMAIN: &[u8] = b"aledgent/opening-set/v1";
const EXTRACT_DOMAIN: &[u8] = b"aledgent/opening-extract/v1";
const RECONCILIATION_DOMAIN: &[u8] = b"aledgent/opening-reconciliation/v1";
const OPENING_DOMAIN: &[u8] = b"aledgent/opening/v1";

/// The kernel's idempotency key of the opening of (`account`, `asset`):
/// `blake2b-256("aledgent/opening/v1" ‖ account ‖ asset)`. It depends on the pair alone, so the
/// kernel accepts one opening per pair for ever, whatever load carries it (`REQ-KER-6`).
pub fn opening_key(account: &AccountId, asset: &AssetId) -> [u8; 32] {
    blake2b_256(
        OPENING_DOMAIN,
        &[account.as_bytes(), asset.as_str().as_bytes()],
    )
}

/// An independent source of opening balances (`NFR-9`'s first axis): an opaque identifier the
/// deployment configures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct OpeningSource(pub [u8; 16]);

/// The sources a deployment requires, per asset: deployment configuration, never a caller's
/// input.
pub type RequiredSources = BTreeMap<AssetId, NonEmptySet<OpeningSource>>;

/// One opening balance: an account's exact balance in one asset, in minor units.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct OpeningRow {
    /// The ledger account.
    pub account: AccountId,
    /// The asset.
    pub asset: AssetId,
    /// Its balance, in minor units. Zero is reconciled and posts nothing.
    pub amount: Minor,
}

/// Why a set could not be built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetError {
    /// Two rows name the same (account, asset).
    DuplicatePair {
        /// The account.
        account: AccountId,
        /// The asset.
        asset: AssetId,
    },
    /// The set has no row.
    Empty,
}

/// An opening set: rows keyed by (account, asset), and the source map.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct OpeningSet {
    rows: Vec<OpeningRow>,
    source_map: BTreeMap<(OpeningSource, Vec<u8>), AccountId>,
}

impl OpeningSet {
    /// A set of `rows`, in (account, asset) order, with its source map. Two rows for one pair are
    /// refused.
    pub fn new(
        mut rows: Vec<OpeningRow>,
        source_map: BTreeMap<(OpeningSource, Vec<u8>), AccountId>,
    ) -> Result<OpeningSet, SetError> {
        if rows.is_empty() {
            return Err(SetError::Empty);
        }
        rows.sort_by_key(|r| (r.account, r.asset));
        for w in rows.windows(2) {
            if (w[0].account, w[0].asset) == (w[1].account, w[1].asset) {
                return Err(SetError::DuplicatePair {
                    account: w[0].account,
                    asset: w[0].asset,
                });
            }
        }
        Ok(OpeningSet { rows, source_map })
    }

    /// The rows, in (account, asset) order.
    pub fn rows(&self) -> &[OpeningRow] {
        &self.rows
    }

    /// The source map.
    pub fn source_map(&self) -> &BTreeMap<(OpeningSource, Vec<u8>), AccountId> {
        &self.source_map
    }

    /// The set's total per asset, in minor units. Exact: a sum past `u128` cannot be reached with
    /// `u64` rows under 2^64 of them.
    pub fn totals(&self) -> BTreeMap<AssetId, u128> {
        let mut out: BTreeMap<AssetId, u128> = BTreeMap::new();
        for r in &self.rows {
            *out.entry(r.asset).or_default() += u128::from(r.amount.get());
        }
        out
    }

    /// The assets it loads.
    pub fn assets(&self) -> BTreeSet<AssetId> {
        self.rows.iter().map(|r| r.asset).collect()
    }

    /// `blake2b-256("aledgent/opening-set/v1" ‖ rows ‖ source map)`, over the canonical encoding.
    pub fn digest(&self) -> [u8; 32] {
        blake2b_256(SET_DOMAIN, &[&canon::to_bytes(self)])
    }
}

/// One row of a source's extract, in the source's own terms.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SourceRow {
    /// The source's own reference for the account.
    pub source_ref: Vec<u8>,
    /// The asset.
    pub asset: AssetId,
    /// The figure as the source states it: a decimal in major units, converted by the one decimal
    /// boundary (`REQ-AS-4`), never rounded.
    pub amount: String,
}

/// What one independent source says about the set's accounts.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SourceExtract {
    /// The source.
    pub source: OpeningSource,
    /// The digest of the extract's rows, as the source adapter delivered them. The gate
    /// recomputes it.
    pub extract_digest: [u8; 32],
    /// The rows.
    pub rows: Vec<SourceRow>,
}

impl SourceExtract {
    /// An extract of `rows`, with its digest.
    pub fn new(source: OpeningSource, rows: Vec<SourceRow>) -> SourceExtract {
        let extract_digest = SourceExtract::digest_of(&source, &rows);
        SourceExtract {
            source,
            extract_digest,
            rows,
        }
    }

    /// `blake2b-256("aledgent/opening-extract/v1" ‖ source ‖ rows)`.
    pub fn digest_of(source: &OpeningSource, rows: &[SourceRow]) -> [u8; 32] {
        let mut e = Enc::new();
        e.put(source).len(rows.len());
        for r in rows {
            e.put(r);
        }
        blake2b_256(EXTRACT_DOMAIN, &[e.as_bytes()])
    }
}

/// The reconciliation of one opening set against every independent source.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Reconciliation {
    /// The digest of the set reconciled.
    pub set_digest: [u8; 32],
    /// The time the sources' figures are as of. Openings carry the load's time; this is kept.
    pub as_of: Time,
    /// What each source says.
    pub sources: Vec<SourceExtract>,
}

impl Reconciliation {
    /// `blake2b-256("aledgent/opening-reconciliation/v1" ‖ reconciliation)`.
    pub fn digest(&self) -> [u8; 32] {
        blake2b_256(RECONCILIATION_DOMAIN, &[&canon::to_bytes(self)])
    }
}

/// One way a set diverges from its sources. Any one blocks the whole set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Divergence {
    /// The reconciliation is of another set.
    SetDigest,
    /// An asset of the set is not registered.
    UnknownAsset(AssetId),
    /// The deployment requires no source for an asset of the set: nothing can reconcile it.
    NoSourcesRequired(AssetId),
    /// A required source is not in the reconciliation.
    MissingSource {
        /// The source.
        source: OpeningSource,
        /// An asset it is required for.
        asset: AssetId,
    },
    /// A source appears twice.
    DuplicateSource(OpeningSource),
    /// A source that no asset of the set requires.
    UnexpectedSource(OpeningSource),
    /// A source's extract digest is not the digest of its rows.
    ExtractDigest(OpeningSource),
    /// A source's reference is not in the source map.
    Unmapped {
        /// The source.
        source: OpeningSource,
        /// Its reference.
        source_ref: Vec<u8>,
    },
    /// A figure is not exact at the asset's precision, or is not a decimal: it would need
    /// rounding, and is refused (`REQ-AS-4`).
    NotExact {
        /// The source.
        source: OpeningSource,
        /// The account.
        account: AccountId,
        /// The asset.
        asset: AssetId,
    },
    /// A set row the source does not state.
    Missing {
        /// The source.
        source: OpeningSource,
        /// The account.
        account: AccountId,
        /// The asset.
        asset: AssetId,
    },
    /// A source row the set does not hold, or in an asset the source is not required for.
    Extra {
        /// The source.
        source: OpeningSource,
        /// The account.
        account: AccountId,
        /// The asset.
        asset: AssetId,
    },
    /// The source states one (account, asset) twice.
    Duplicate {
        /// The source.
        source: OpeningSource,
        /// The account.
        account: AccountId,
        /// The asset.
        asset: AssetId,
    },
    /// The source's figure differs from the set's.
    Amount {
        /// The source.
        source: OpeningSource,
        /// The account.
        account: AccountId,
        /// The asset.
        asset: AssetId,
        /// The set's amount.
        set: Minor,
        /// The source's.
        stated: Minor,
    },
}

/// The zero-divergence gate (module documentation). `Ok` only when every finding list is empty;
/// otherwise every divergence found, in a stable order.
pub fn reconcile(
    set: &OpeningSet,
    rec: &Reconciliation,
    required: &RequiredSources,
    registry: &AssetRegistry,
) -> Result<(), Vec<Divergence>> {
    let mut found = Vec::new();
    if rec.set_digest != set.digest() {
        found.push(Divergence::SetDigest);
    }
    // What each source must state: the set's keys in the assets it is required for.
    let mut owed: BTreeMap<OpeningSource, BTreeSet<AssetId>> = BTreeMap::new();
    for asset in set.assets() {
        if registry.get(&asset).is_err() {
            found.push(Divergence::UnknownAsset(asset));
            continue;
        }
        match required.get(&asset) {
            None => found.push(Divergence::NoSourcesRequired(asset)),
            Some(sources) => {
                for s in sources.iter() {
                    owed.entry(*s).or_default().insert(asset);
                }
            }
        }
    }
    let mut seen: BTreeSet<OpeningSource> = BTreeSet::new();
    for x in &rec.sources {
        if !seen.insert(x.source) {
            found.push(Divergence::DuplicateSource(x.source));
        }
    }
    for (source, assets) in &owed {
        if !seen.contains(source) {
            for asset in assets {
                found.push(Divergence::MissingSource {
                    source: *source,
                    asset: *asset,
                });
            }
        }
    }
    let expected: BTreeMap<(AccountId, AssetId), Minor> = set
        .rows
        .iter()
        .map(|r| ((r.account, r.asset), r.amount))
        .collect();
    let mut checked: BTreeSet<OpeningSource> = BTreeSet::new();
    for x in &rec.sources {
        if !checked.insert(x.source) {
            continue;
        }
        let Some(assets) = owed.get(&x.source) else {
            found.push(Divergence::UnexpectedSource(x.source));
            continue;
        };
        if SourceExtract::digest_of(&x.source, &x.rows) != x.extract_digest {
            found.push(Divergence::ExtractDigest(x.source));
        }
        let mut stated: BTreeMap<(AccountId, AssetId), Option<Minor>> = BTreeMap::new();
        for row in &x.rows {
            let Some(account) = set
                .source_map
                .get(&(x.source, row.source_ref.clone()))
                .copied()
            else {
                found.push(Divergence::Unmapped {
                    source: x.source,
                    source_ref: row.source_ref.clone(),
                });
                continue;
            };
            let key = (account, row.asset);
            if !assets.contains(&row.asset) || !expected.contains_key(&key) {
                found.push(Divergence::Extra {
                    source: x.source,
                    account,
                    asset: row.asset,
                });
                continue;
            }
            let amount = registry
                .get(&row.asset)
                .ok()
                .and_then(|a| Minor::from_decimal(&row.amount, a).ok());
            if amount.is_none() {
                found.push(Divergence::NotExact {
                    source: x.source,
                    account,
                    asset: row.asset,
                });
            }
            if stated.insert(key, amount).is_some() {
                found.push(Divergence::Duplicate {
                    source: x.source,
                    account,
                    asset: row.asset,
                });
            }
        }
        for (key, want) in expected.iter().filter(|(k, _)| assets.contains(&k.1)) {
            match stated.get(key) {
                None => found.push(Divergence::Missing {
                    source: x.source,
                    account: key.0,
                    asset: key.1,
                }),
                Some(None) => {}
                Some(Some(got)) if got == want => {}
                Some(Some(got)) => found.push(Divergence::Amount {
                    source: x.source,
                    account: key.0,
                    asset: key.1,
                    set: *want,
                    stated: *got,
                }),
            }
        }
    }
    if found.is_empty() { Ok(()) } else { Err(found) }
}

/// One load of an opening set: its reconciliation, kept so the gate can be re-checked later, and
/// how far it has posted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpeningLoad {
    /// The set.
    pub set: OpeningSet,
    /// Its reconciliation.
    pub reconciliation: Reconciliation,
    /// The operator authority that loaded it.
    pub by: AuthorityUsed,
    /// When the load began.
    pub at: Time,
    /// Every pair posted so far, with its posting sequence number.
    pub posted: BTreeMap<(AccountId, AssetId), u64>,
    /// Whether the load is complete: every non-zero row posted, and the totals checked.
    pub complete: bool,
}

/// A change to the openings record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpeningWrite {
    /// Begin a load under operation `op`.
    Begin {
        /// The load's operation reference.
        op: OperationId,
        /// The load, with nothing posted.
        load: OpeningLoad,
    },
    /// Record pairs posted by a load.
    Posted {
        /// The load.
        op: OperationId,
        /// Each pair with its posting sequence number.
        pairs: Vec<((AccountId, AssetId), u64)>,
    },
    /// Mark a load complete.
    Complete(OperationId),
    /// Record pairs a seeded genesis opens under `op` (`F-036`): the seed's opening set is
    /// committed by its digest, not kept as a load, so only the pairs are recorded, each once for
    /// ever, as a load's are.
    Seeded {
        /// The seed's opening reference.
        op: OperationId,
        /// The pairs, zero rows included.
        pairs: Vec<(AccountId, AssetId)>,
    },
}

/// Why the openings record refused a change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpeningRefusal {
    /// A load under this operation exists.
    Exists,
    /// No such load.
    Missing,
    /// The load is complete: nothing more is posted under it.
    Complete,
    /// A pair was loaded by another load, or its posting is recorded already: one opening per
    /// (account, asset) (`REQ-RC-4`).
    AlreadyLoaded,
    /// A posted pair is not a non-zero row of the load's set.
    NotInSet,
    /// A load marked complete before every non-zero row was posted.
    Incomplete,
    /// An opening position for this asset exists, or its account is not the operator's
    /// `Issuance` account.
    Position,
}

/// How to undo one applied change.
#[derive(Clone, Debug)]
pub(crate) enum OpeningUndo {
    Unbegin(OperationId),
    Unpost(OperationId, Vec<(AccountId, AssetId)>),
    Uncomplete(OperationId),
    Unposition(AssetId),
    Unseed(Vec<(AccountId, AssetId)>),
}

/// The openings record in the service store: each asset's opening position, and every load.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OpeningsState {
    positions: Leaves<AssetId, AccountId, { kind::OPENING_POSITION }>,
    loads: Leaves<OperationId, OpeningLoad, { kind::OPENING_LOAD }>,
    loaded: Leaves<(AccountId, AssetId), OperationId, { kind::OPENING_LOADED }>,
}

impl LeafHost for OpeningsState {
    fn leaf_maps<'a>(&'a self, out: &mut Vec<&'a dyn LeafMap>) {
        out.push(&self.positions);
        out.push(&self.loads);
        out.push(&self.loaded);
    }
    fn leaf_maps_mut<'a>(&'a mut self, out: &mut Vec<&'a mut dyn LeafMap>) {
        out.push(&mut self.positions);
        out.push(&mut self.loads);
        out.push(&mut self.loaded);
    }
}

impl OpeningsState {
    /// Nothing loaded.
    pub fn new() -> OpeningsState {
        OpeningsState::default()
    }

    /// The opening position of `asset`, if it has one.
    pub fn position(&self, asset: &AssetId) -> Option<AccountId> {
        self.positions.get(asset).copied()
    }

    /// Every opening position.
    pub fn positions(&self) -> impl Iterator<Item = (&AssetId, &AccountId)> {
        self.positions.iter()
    }

    /// Whether `account` is an opening position.
    pub fn is_position(&self, account: &AccountId) -> bool {
        self.positions.values().any(|a| a == account)
    }

    /// A load.
    pub fn load(&self, op: &OperationId) -> Option<&OpeningLoad> {
        self.loads.get(op)
    }

    /// Every load, by operation.
    pub fn loads(&self) -> impl Iterator<Item = (&OperationId, &OpeningLoad)> {
        self.loads.iter()
    }

    /// Every pair opened under `op`, in pair order: a seeded genesis's (`F-036`).
    pub fn opened_under(&self, op: &OperationId) -> Vec<(AccountId, AssetId)> {
        self.loaded
            .iter()
            .filter(|(_, o)| *o == op)
            .map(|(p, _)| *p)
            .collect()
    }

    /// The load that holds (`account`, `asset`), if any.
    pub fn loaded_by(&self, account: &AccountId, asset: &AssetId) -> Option<OperationId> {
        self.loaded.get(&(*account, *asset)).copied()
    }

    /// Register the opening position of `asset`. The service store checks the account.
    pub(crate) fn put_position(
        &mut self,
        asset: AssetId,
        account: AccountId,
    ) -> Result<OpeningUndo, OpeningRefusal> {
        if self.positions.contains_key(&asset) {
            return Err(OpeningRefusal::Position);
        }
        self.positions.insert(asset, account);
        Ok(OpeningUndo::Unposition(asset))
    }

    pub(crate) fn apply(&mut self, w: &OpeningWrite) -> Result<OpeningUndo, OpeningRefusal> {
        match w {
            OpeningWrite::Begin { op, load } => {
                if self.loads.contains_key(op) {
                    return Err(OpeningRefusal::Exists);
                }
                if !load.posted.is_empty() || load.complete {
                    return Err(OpeningRefusal::NotInSet);
                }
                if load
                    .set
                    .rows
                    .iter()
                    .any(|r| self.loaded.contains_key(&(r.account, r.asset)))
                {
                    return Err(OpeningRefusal::AlreadyLoaded);
                }
                for r in &load.set.rows {
                    self.loaded.insert((r.account, r.asset), *op);
                }
                self.loads.insert(*op, load.clone());
                Ok(OpeningUndo::Unbegin(*op))
            }
            OpeningWrite::Posted { op, pairs } => {
                let load = self.loads.get(op).ok_or(OpeningRefusal::Missing)?;
                if load.complete {
                    return Err(OpeningRefusal::Complete);
                }
                let mut seen = BTreeSet::new();
                for (pair, _) in pairs {
                    let row = load
                        .set
                        .rows
                        .binary_search_by_key(pair, |r| (r.account, r.asset))
                        .ok()
                        .map(|i| load.set.rows[i]);
                    if row.is_none_or(|r| r.amount == Minor::ZERO) {
                        return Err(OpeningRefusal::NotInSet);
                    }
                    if load.posted.contains_key(pair) || !seen.insert(*pair) {
                        return Err(OpeningRefusal::AlreadyLoaded);
                    }
                }
                let load = self.loads.get_mut(op).ok_or(OpeningRefusal::Missing)?;
                for (pair, seq) in pairs {
                    load.posted.insert(*pair, *seq);
                }
                Ok(OpeningUndo::Unpost(
                    *op,
                    pairs.iter().map(|(p, _)| *p).collect(),
                ))
            }
            OpeningWrite::Seeded { op, pairs } => {
                let mut seen = BTreeSet::new();
                for p in pairs {
                    if self.loaded.contains_key(p) || !seen.insert(*p) {
                        return Err(OpeningRefusal::AlreadyLoaded);
                    }
                }
                for p in pairs {
                    self.loaded.insert(*p, *op);
                }
                Ok(OpeningUndo::Unseed(pairs.clone()))
            }
            OpeningWrite::Complete(op) => {
                let load = self.loads.get(op).ok_or(OpeningRefusal::Missing)?;
                if load.complete {
                    return Err(OpeningRefusal::Complete);
                }
                let all = load
                    .set
                    .rows
                    .iter()
                    .filter(|r| r.amount != Minor::ZERO)
                    .all(|r| load.posted.contains_key(&(r.account, r.asset)));
                if !all {
                    return Err(OpeningRefusal::Incomplete);
                }
                if let Some(l) = self.loads.get_mut(op) {
                    l.complete = true;
                }
                Ok(OpeningUndo::Uncomplete(*op))
            }
        }
    }

    pub(crate) fn undo(&mut self, u: OpeningUndo) {
        match u {
            OpeningUndo::Unbegin(op) => {
                if let Some(load) = self.loads.remove(&op) {
                    for r in &load.set.rows {
                        self.loaded.remove(&(r.account, r.asset));
                    }
                }
            }
            OpeningUndo::Unpost(op, pairs) => {
                if let Some(load) = self.loads.get_mut(&op) {
                    for p in pairs {
                        load.posted.remove(&p);
                    }
                }
            }
            OpeningUndo::Uncomplete(op) => {
                if let Some(load) = self.loads.get_mut(&op) {
                    load.complete = false;
                }
            }
            OpeningUndo::Unposition(asset) => {
                self.positions.remove(&asset);
            }
            OpeningUndo::Unseed(pairs) => {
                for p in pairs {
                    self.loaded.remove(&p);
                }
            }
        }
    }

    /// Rebuild the record from its positions and loads (an import, `F-014` T-014-03). Every load
    /// is begun, posted and completed through the same rules as when it was made.
    pub(crate) fn restore(
        positions: BTreeMap<AssetId, AccountId>,
        loads: BTreeMap<OperationId, OpeningLoad>,
        seeded: Option<(OperationId, Vec<(AccountId, AssetId)>)>,
    ) -> Result<OpeningsState, OpeningRefusal> {
        let mut out = OpeningsState::default();
        if let Some((op, pairs)) = seeded {
            out.apply(&OpeningWrite::Seeded { op, pairs })?;
        }
        for (asset, account) in positions {
            out.positions.insert(asset, account);
        }
        for (op, load) in loads {
            let begun = OpeningLoad {
                posted: BTreeMap::new(),
                complete: false,
                ..load.clone()
            };
            out.apply(&OpeningWrite::Begin { op, load: begun })?;
            let pairs: Vec<((AccountId, AssetId), u64)> =
                load.posted.iter().map(|(k, v)| (*k, *v)).collect();
            if !pairs.is_empty() {
                out.apply(&OpeningWrite::Posted { op, pairs })?;
            }
            if load.complete {
                out.apply(&OpeningWrite::Complete(op))?;
            }
        }
        Ok(out)
    }
}

// ------------------------------------------------------------------ canonical forms

impl Canonical for OpeningSource {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.0);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(OpeningSource(d.get()?))
    }
}

impl Canonical for OpeningRow {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.account).put(&self.asset).put(&self.amount);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(OpeningRow {
            account: d.get()?,
            asset: d.get()?,
            amount: d.get()?,
        })
    }
}

impl Canonical for OpeningSet {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.rows).put(&self.source_map);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        let rows: Vec<OpeningRow> = d.get()?;
        let map = d.get()?;
        let set = OpeningSet::new(rows.clone(), map).map_err(|_| DecodeError("opening set"))?;
        // Canonical: the rows as given are already in order.
        if set.rows != rows {
            return Err(DecodeError("opening rows out of order"));
        }
        Ok(set)
    }
}

impl Canonical for String {
    fn encode(&self, e: &mut Enc) {
        e.bytes(self.as_bytes());
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        let s = core::str::from_utf8(d.bytes()?).map_err(|_| DecodeError("text"))?;
        Ok(String::from(s))
    }
}

impl Canonical for SourceRow {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.source_ref).put(&self.asset).put(&self.amount);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(SourceRow {
            source_ref: d.get()?,
            asset: d.get()?,
            amount: d.get()?,
        })
    }
}

impl Canonical for SourceExtract {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.source)
            .put(&self.extract_digest)
            .put(&self.rows);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(SourceExtract {
            source: d.get()?,
            extract_digest: d.get()?,
            rows: d.get()?,
        })
    }
}

impl Canonical for Reconciliation {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.set_digest).put(&self.as_of).put(&self.sources);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Reconciliation {
            set_digest: d.get()?,
            as_of: d.get()?,
            sources: d.get()?,
        })
    }
}

impl Canonical for OpeningLoad {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.set)
            .put(&self.reconciliation)
            .put(&self.by)
            .put(&self.at)
            .put(&self.posted)
            .put(&self.complete);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(OpeningLoad {
            set: d.get()?,
            reconciliation: d.get()?,
            by: d.get()?,
            at: d.get()?,
            posted: d.get()?,
            complete: d.get()?,
        })
    }
}
