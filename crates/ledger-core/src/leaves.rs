// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The service store's collections, as leaves of the service tree (`F-005`, `F-006` decision 5,
//! `DEC-4`).
//!
//! Every collection the service store keeps is one of three wrappers, so the same library code
//! runs over the whole store and over a witnessed part of it:
//!
//! - [`Leaves`]: a map whose every entry is one leaf of the service tree, of the map's own
//!   **kind** (a const parameter, [`kind`]);
//! - [`Slot`]: one value that is one leaf, absent while it holds its default;
//! - [`Derived`]: an index derived from the leaves and read only by queries. It is not a leaf:
//!   the host keeps it, and a witnessed store has none.
//!
//! **Whole and partial.** A store built by [`crate::state::State::new`] (or an import) is *whole*:
//! every collection holds everything, as a `BTreeMap` does. A store built by
//! [`crate::state::State::partial`] holds only the leaves a witness gave it, each marked known,
//! present or absent. There, a read or a write of a key the witness does not cover, an iteration
//! over a collection the witness did not give whole, and any read of a [`Derived`] index **fail
//! closed**: the read answers as if absent, the collection remembers it ([`LeafMap::missed`]), and
//! the core Agent commits nothing. So a partial store never decides on a leaf it was not shown.
//!
//! **Recording.** A whole store records, while recording is on, every key it is asked for or
//! writes, and every collection it is asked to iterate: the read set the host witnesses for the
//! Agent. [`crate::Ledger`] turns recording on for the span of each outermost recorded call
//! (`crate::call`), so queries and the host's own reads are never witnessed.
//!
//! A leaf's key in the service tree is its kind's code, then 15 bytes of a hash of the kind and
//! the entry's canonical key ([`crate::delta::library_leaf`]): every kind is one subtree of the
//! service tree, so a witness can prove that it holds a kind whole.

use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;
use core::ops::RangeBounds;

use crate::export::canon::{self, Canonical, DecodeError};

use self::sync::{Flag, Lock};

/// Interior mutability for recording through `&self`. Where the target has atomics (the host),
/// a flag is atomic and a lock spins, so a whole store stays `Sync` for concurrent readers, who
/// never record and never take the lock; the core's guest has no atomics and one thread, and
/// uses cells.
mod sync {
    #[cfg(target_has_atomic = "8")]
    mod imp {
        use core::sync::atomic::{AtomicBool, Ordering};

        /// A boolean.
        #[derive(Debug, Default)]
        pub struct Flag(AtomicBool);

        impl Flag {
            pub fn new(v: bool) -> Self {
                Flag(AtomicBool::new(v))
            }
            pub fn get(&self) -> bool {
                self.0.load(Ordering::Relaxed)
            }
            pub fn set(&self, v: bool) {
                self.0.store(v, Ordering::Relaxed);
            }
        }

        /// A value behind a lock.
        #[derive(Debug, Default)]
        pub struct Lock<T>(spin::mutex::SpinMutex<T>);

        impl<T> Lock<T> {
            pub fn new(v: T) -> Self {
                Lock(spin::mutex::SpinMutex::new(v))
            }
            pub fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
                f(&mut self.0.lock())
            }
        }
    }

    #[cfg(not(target_has_atomic = "8"))]
    mod imp {
        use core::cell::{Cell, RefCell};

        /// A boolean.
        #[derive(Debug, Default)]
        pub struct Flag(Cell<bool>);

        impl Flag {
            pub fn new(v: bool) -> Self {
                Flag(Cell::new(v))
            }
            pub fn get(&self) -> bool {
                self.0.get()
            }
            pub fn set(&self, v: bool) {
                self.0.set(v);
            }
        }

        /// A value behind a lock.
        #[derive(Debug, Default)]
        pub struct Lock<T>(RefCell<T>);

        impl<T> Lock<T> {
            pub fn new(v: T) -> Self {
                Lock(RefCell::new(v))
            }
            pub fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> R {
                f(&mut self.0.borrow_mut())
            }
        }
    }

    pub use imp::{Flag, Lock};

    impl Clone for Flag {
        fn clone(&self) -> Self {
            Flag::new(self.get())
        }
    }
}

/// The kinds of the service tree's leaves: the first byte of every leaf key. Stable: a code
/// enters every leaf's key and content. Codes 1 to 11 are the kinds `F-006` named; the core
/// binding owns [`kind::DELIVERED`].
#[allow(missing_docs)]
pub mod kind {
    // F-006's kinds (`crate::delta::ServiceKind`).
    pub const REGISTRY: u8 = 1;
    pub const ROLES: u8 = 2;
    pub const IDENTITY: u8 = 3;
    pub const LEDGER_ACCOUNT: u8 = 4;
    pub const GRANT: u8 = 5;
    pub const CREDENTIAL: u8 = 6;
    pub const WINDOW: u8 = 7;
    pub const OPERATION: u8 = 8;
    pub const CLIENT_KEY: u8 = 9;
    pub const RESERVATION: u8 = 10;
    pub const DEADLINE: u8 = 11;
    // The service store.
    pub const ACCOUNTS_BY_OWNER: u8 = 12;
    pub const CREDENTIAL_SET: u8 = 13;
    /// Retired: the grants each identity holds are a derived index now (`State::grants_of`), so a
    /// principal holding many grants never brings them all to the Agent. Never reuse the code.
    pub const GRANTS_BY_PRINCIPAL: u8 = 14;
    pub const ATTRIBUTION: u8 = 15;
    pub const ATTRIBUTED_TRANSITIONS: u8 = 16;
    pub const ACCOUNT_BY_REFERENCE: u8 = 17;
    pub const GRANTS_BY_SUBJECT: u8 = 18;
    pub const GRANTS_BY_ASSET_OR_IDENTITY: u8 = 19;
    /// The accounts in owned custody each identity owns (`State::holds_owned_custody`).
    pub const OWNED_CUSTODY_BY_OWNER: u8 = 22;
    // The reservation index.
    pub const RESERVED_TOTAL: u8 = 20;
    pub const OPEN_RESERVATIONS: u8 = 21;
    // The operation store.
    pub const TRANSITION_KEY: u8 = 30;
    pub const LIFECYCLE_CLOCK: u8 = 31;
    pub const NOTIFICATION: u8 = 32;
    // The settlement store.
    pub const SETTLEMENT_CONFIG: u8 = 40;
    pub const SETTLEMENT_SUPPORT: u8 = 41;
    pub const INGESTED: u8 = 42;
    pub const CORROBORATION: u8 = 43;
    pub const FINAL_POSITION: u8 = 44;
    pub const INGESTED_BY_OP: u8 = 45;
    pub const NAMING: u8 = 46;
    pub const BOUND: u8 = 47;
    pub const INSTRUCTION: u8 = 48;
    pub const FINALISED_HEAD: u8 = 49;
    pub const GAPS: u8 = 50;
    pub const SETTLEMENT_INCIDENTS: u8 = 51;
    pub const ADAPTER_STATUS: u8 = 52;
    pub const LIVE_INSTRUCTION: u8 = 53;
    pub const AWAITING_NONCE: u8 = 54;
    pub const ASSET_ATTESTATION: u8 = 55;
    pub const ASSET_BIND: u8 = 56;
    // Finding F18's class (`F-032`): what the core's recorded paths read by key instead of whole.
    pub const ASSET_BIND_REF: u8 = 57;
    pub const INGESTED_AT: u8 = 58;
    pub const POSITION_TALLY: u8 = 59;
    pub const POSITION_COUNTED: u8 = 39;
    // The operator records.
    pub const OPERATOR_GRANT: u8 = 60;
    pub const OPERATOR_GRANTS_BY_PRINCIPAL: u8 = 61;
    pub const REVERSAL: u8 = 62;
    pub const OPERATOR_INCIDENT: u8 = 63;
    pub const RESOLUTION_CASE: u8 = 64;
    // Integrating parties and the principals tied to them (`REQ-ID-7`).
    pub const INTEGRATING_PARTY: u8 = 65;
    pub const PARTY_TIE: u8 = 66;
    // The posting journal.
    pub const POSTING: u8 = 70;
    pub const LAST_POSTING: u8 = 71;
    pub const RUNNING_BALANCE: u8 = 72;
    // Custody.
    pub const CUSTODY_PENDING: u8 = 80;
    pub const CUSTODY_OPEN: u8 = 81;
    pub const SUCCESSOR: u8 = 82;
    pub const CUSTODY_EPOCH: u8 = 83;
    pub const CUSTODY_NOTICE: u8 = 84;
    // Openings.
    pub const OPENING_POSITION: u8 = 90;
    pub const OPENING_LOAD: u8 = 91;
    pub const OPENING_LOADED: u8 = 92;
    // The chainspec genesis (`F-036`).
    pub const CHAINSPEC: u8 = 93;
    pub const SPEC_ALIAS: u8 = 94;
    // Anchoring.
    pub const ANCHOR_CHECKPOINT: u8 = 100;
    pub const ANCHOR_INCIDENTS: u8 = 101;
    pub const ANCHOR_COMPARISONS: u8 = 102;
    pub const ANCHOR_LATEST: u8 = 103;
    pub const DISCLOSURE_ISSUED: u8 = 104;
    pub const DISCLOSURE_ON_DEMAND: u8 = 105;
    pub const ANCHOR_LAST: u8 = 106;
    pub const ANCHOR_COMPARED: u8 = 107;
    // The writer's upgrade-authority record (`F-037`, `T-037-05`).
    pub const UPGRADE_AUTHORITY: u8 = 110;
    // The core binding's own: a delivery recorded by the Agent (`REQ-RT-4`).
    pub const DELIVERED: u8 = 200;

    /// The kinds keyed by a ledger account: their canonical key starts with the account's 16
    /// bytes. Their leaves live in the account's partition, and their service keys carry it after
    /// the kind's code (`REQ-RT-9`, `F-031`, `crate::delta::service_key`). Kinds keyed by an
    /// operation follow when operation references carry their payer's partition (`F-031` notes).
    pub const ACCOUNT_SCOPED: [u8; 8] = [
        LEDGER_ACCOUNT,
        RESERVED_TOTAL,
        OPEN_RESERVATIONS,
        RUNNING_BALANCE,
        CUSTODY_OPEN,
        SUCCESSOR,
        CUSTODY_EPOCH,
        OPENING_LOADED,
    ];

    /// Whether `kind` is keyed by a ledger account ([`ACCOUNT_SCOPED`]).
    pub fn account_scoped(kind: u8) -> bool {
        ACCOUNT_SCOPED.contains(&kind)
    }
}

/// What a whole store recorded while recording was on.
#[derive(Clone, Debug)]
struct Recorded<K> {
    touched: BTreeSet<K>,
    whole: bool,
}

impl<K> Default for Recorded<K> {
    fn default() -> Self {
        Recorded {
            touched: BTreeSet::new(),
            whole: false,
        }
    }
}

/// Recording, and the fail-closed flag. Interior mutability: reads record through `&self`.
#[derive(Debug)]
struct Track<K> {
    on: Flag,
    rec: Lock<Recorded<K>>,
    missed: Flag,
}

impl<K: Clone> Clone for Track<K> {
    fn clone(&self) -> Self {
        Track {
            on: self.on.clone(),
            rec: Lock::new(self.rec.with(|r| r.clone())),
            missed: self.missed.clone(),
        }
    }
}

/// A partial collection's witness: the keys it covers, and whether it was given whole.
#[derive(Clone, Debug)]
struct Partial<K> {
    known: BTreeSet<K>,
    complete: bool,
    loaded_present: usize,
}

impl<K> Default for Partial<K> {
    fn default() -> Self {
        Partial {
            known: BTreeSet::new(),
            complete: false,
            loaded_present: 0,
        }
    }
}

/// A map whose entries are leaves of kind `KIND` (module documentation). Its methods are the
/// `BTreeMap` ones the store uses; each records and checks as the module says.
#[derive(Debug)]
pub struct Leaves<K, V, const KIND: u8> {
    map: BTreeMap<K, V>,
    partial: Option<Box<Partial<K>>>,
    track: Track<K>,
}

impl<K: Clone, V: Clone, const KIND: u8> Clone for Leaves<K, V, KIND> {
    fn clone(&self) -> Self {
        Leaves {
            map: self.map.clone(),
            partial: self.partial.clone(),
            track: self.track.clone(),
        }
    }
}

impl<K, V, const KIND: u8> Default for Leaves<K, V, KIND> {
    fn default() -> Self {
        Leaves {
            map: BTreeMap::new(),
            partial: None,
            track: Track {
                on: Flag::new(false),
                rec: Lock::new(Recorded::default()),
                missed: Flag::new(false),
            },
        }
    }
}

impl<K: PartialEq, V: PartialEq, const KIND: u8> PartialEq for Leaves<K, V, KIND> {
    fn eq(&self, other: &Self) -> bool {
        self.map == other.map
    }
}

impl<K: Eq, V: Eq, const KIND: u8> Eq for Leaves<K, V, KIND> {}

impl<K, V, const KIND: u8> From<BTreeMap<K, V>> for Leaves<K, V, KIND> {
    fn from(map: BTreeMap<K, V>) -> Self {
        Leaves {
            map,
            ..Leaves::default()
        }
    }
}

impl<K: Ord, V, const KIND: u8> FromIterator<(K, V)> for Leaves<K, V, KIND> {
    fn from_iter<I: IntoIterator<Item = (K, V)>>(iter: I) -> Self {
        Leaves::from(iter.into_iter().collect::<BTreeMap<K, V>>())
    }
}

impl<K: Ord + Clone, V, const KIND: u8> Leaves<K, V, KIND> {
    /// An empty, whole map.
    pub fn new() -> Self {
        Self::default()
    }

    /// A point access to `k`: recorded, and checked against the witness.
    fn point(&self, k: &K) {
        if self.track.on.get() {
            self.track.rec.with(|r| {
                r.touched.insert(k.clone());
            });
        }
        if let Some(p) = &self.partial
            && !p.complete
            && !p.known.contains(k)
        {
            self.track.missed.set(true);
        }
    }

    /// An access to the whole map: recorded, and checked against the witness.
    fn all(&self) {
        if self.track.on.get() {
            self.track.rec.with(|r| r.whole = true);
        }
        if self.partial.as_ref().is_some_and(|p| !p.complete) {
            self.track.missed.set(true);
        }
    }

    /// The value at `k`.
    pub fn get(&self, k: &K) -> Option<&V> {
        self.point(k);
        self.map.get(k)
    }

    /// Whether `k` has a value.
    pub fn contains_key(&self, k: &K) -> bool {
        self.point(k);
        self.map.contains_key(k)
    }

    /// The value at `k`, to change.
    pub fn get_mut(&mut self, k: &K) -> Option<&mut V> {
        self.point(k);
        self.map.get_mut(k)
    }

    /// Put `v` at `k`; the value it replaced.
    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        self.point(&k);
        self.map.insert(k, v)
    }

    /// Take the value at `k` out.
    pub fn remove(&mut self, k: &K) -> Option<V> {
        self.point(k);
        self.map.remove(k)
    }

    /// The value at `k`, put there as the default first if there is none.
    pub fn entry_or_default(&mut self, k: K) -> &mut V
    where
        V: Default,
    {
        self.point(&k);
        self.map.entry(k).or_default()
    }

    /// Every value, in key order.
    pub fn values(&self) -> impl DoubleEndedIterator<Item = &V> {
        self.all();
        self.map.values()
    }

    /// Every entry, in key order.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (&K, &V)> {
        self.all();
        self.map.iter()
    }

    /// Every key, in order.
    pub fn keys(&self) -> impl DoubleEndedIterator<Item = &K> {
        self.all();
        self.map.keys()
    }

    /// Every key, in order, read without recording a whole read: for a choice the caller then
    /// passes as data, so that what it chose, not the collection, is witnessed (a tick's due
    /// deadlines, finding F18 of `F-032`). On a partial store it fails closed, as any read of a
    /// collection the witness did not give whole: the core Agent never makes such a choice.
    pub fn keys_unrecorded(&self) -> impl DoubleEndedIterator<Item = &K> {
        if self.partial.as_ref().is_some_and(|p| !p.complete) {
            self.track.missed.set(true);
        }
        self.map.keys()
    }

    /// The entries in `range`.
    pub fn range<R: RangeBounds<K>>(&self, range: R) -> impl Iterator<Item = (&K, &V)> {
        self.all();
        self.map.range(range)
    }

    /// How many entries.
    pub fn len(&self) -> usize {
        self.all();
        self.map.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.all();
        self.map.is_empty()
    }

    /// The last entry.
    pub fn last_key_value(&self) -> Option<(&K, &V)> {
        self.all();
        self.map.last_key_value()
    }

    /// The first entry.
    pub fn first_key_value(&self) -> Option<(&K, &V)> {
        self.all();
        self.map.first_key_value()
    }

    /// How many entries this partial map holds beyond those it was given present: the entries
    /// the run created (net). Zero on a whole map.
    pub fn created(&self) -> usize {
        match &self.partial {
            Some(p) => self.map.len().saturating_sub(p.loaded_present),
            None => 0,
        }
    }
}

impl<K: Ord + Clone, T: Ord, const KIND: u8> Leaves<K, BTreeSet<T>, KIND> {
    /// Take `item` out of the set at `k`, and the set out of the map once it is empty: an index
    /// entry that holds nothing is no leaf, so the service tree is the same however the store
    /// came to hold what it holds.
    pub fn remove_from(&mut self, k: &K, item: &T) {
        if let Some(set) = self.get_mut(k) {
            set.remove(item);
            if set.is_empty() {
                self.remove(k);
            }
        }
    }
}

impl<K: Ord + Clone + Canonical, V: Canonical, const KIND: u8> LeafMap for Leaves<K, V, KIND> {
    fn kind(&self) -> u8 {
        KIND
    }

    fn record(&self, on: bool) {
        self.track.on.set(on);
    }

    fn take_recorded(&self) -> (Vec<Vec<u8>>, bool) {
        let r = self.track.rec.with(core::mem::take);
        (r.touched.iter().map(canon::to_bytes).collect(), r.whole)
    }

    #[cfg(target_has_atomic = "8")]
    fn leaf_count(&self) -> u64 {
        self.map.len() as u64
    }

    fn keys_encoded(&self) -> Vec<Vec<u8>> {
        self.map.keys().map(canon::to_bytes).collect()
    }

    fn value_encoded(&self, key: &[u8]) -> Result<Option<Vec<u8>>, DecodeError> {
        let k: K = canon::from_bytes(key)?;
        Ok(self.map.get(&k).map(canon::to_bytes))
    }

    fn entries_encoded(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.map
            .iter()
            .map(|(k, v)| (canon::to_bytes(k), canon::to_bytes(v)))
            .collect()
    }

    fn visit_encoded(&self, f: &mut dyn FnMut(Vec<u8>, Vec<u8>)) {
        for (k, v) in &self.map {
            f(canon::to_bytes(k), canon::to_bytes(v));
        }
    }

    fn make_partial(&mut self) {
        self.map.clear();
        self.partial = Some(Box::default());
    }

    fn load(&mut self, key: &[u8], value: Option<&[u8]>) -> Result<(), DecodeError> {
        let p = self
            .partial
            .as_mut()
            .ok_or(DecodeError("a leaf loaded into a whole store"))?;
        let k: K = canon::from_bytes(key)?;
        if canon::to_bytes(&k) != key {
            return Err(DecodeError("a leaf key that is not canonical"));
        }
        if !p.known.insert(k.clone()) {
            return Err(DecodeError("a leaf witnessed twice"));
        }
        if let Some(v) = value {
            let v: V = canon::from_bytes(v)?;
            p.loaded_present += 1;
            self.map.insert(k, v);
        }
        Ok(())
    }

    fn set_complete(&mut self) {
        if let Some(p) = self.partial.as_mut() {
            p.complete = true;
        }
    }

    fn missed(&self) -> bool {
        self.track.missed.get()
    }

    fn witnessed(&self) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
        let Some(p) = &self.partial else {
            return Vec::new();
        };
        let keys: BTreeSet<&K> = p.known.iter().chain(self.map.keys()).collect();
        keys.into_iter()
            .map(|k| (canon::to_bytes(k), self.map.get(k).map(canon::to_bytes)))
            .collect()
    }
}

/// One value that is one leaf of kind `KIND`, absent while it holds `T::default()`.
#[derive(Debug)]
pub struct Slot<T, const KIND: u8> {
    value: T,
    /// Partial: whether the witness covers it.
    partial: Option<bool>,
    track: Track<()>,
}

impl<T: Clone, const KIND: u8> Clone for Slot<T, KIND> {
    fn clone(&self) -> Self {
        Slot {
            value: self.value.clone(),
            partial: self.partial,
            track: self.track.clone(),
        }
    }
}

impl<T: Default, const KIND: u8> Default for Slot<T, KIND> {
    fn default() -> Self {
        Slot::new(T::default())
    }
}

impl<T: PartialEq, const KIND: u8> PartialEq for Slot<T, KIND> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<T: Eq, const KIND: u8> Eq for Slot<T, KIND> {}

impl<T, const KIND: u8> Slot<T, KIND> {
    /// A whole slot holding `value`.
    pub fn new(value: T) -> Self {
        Slot {
            value,
            partial: None,
            track: Track {
                on: Flag::new(false),
                rec: Lock::new(Recorded::default()),
                missed: Flag::new(false),
            },
        }
    }

    fn touch(&self) {
        if self.track.on.get() {
            self.track.rec.with(|r| {
                r.touched.insert(());
            });
        }
        if self.partial == Some(false) {
            self.track.missed.set(true);
        }
    }

    /// The value.
    pub fn get(&self) -> &T {
        self.touch();
        &self.value
    }

    /// The value, to change.
    pub fn get_mut(&mut self) -> &mut T {
        self.touch();
        &mut self.value
    }

    /// Put `value`; the one it replaced.
    pub fn set(&mut self, value: T) -> T {
        self.touch();
        core::mem::replace(&mut self.value, value)
    }
}

impl<T: Canonical + Default + PartialEq, const KIND: u8> LeafMap for Slot<T, KIND> {
    fn kind(&self) -> u8 {
        KIND
    }

    fn record(&self, on: bool) {
        self.track.on.set(on);
    }

    fn take_recorded(&self) -> (Vec<Vec<u8>>, bool) {
        let r = self.track.rec.with(core::mem::take);
        (r.touched.iter().map(|()| Vec::new()).collect(), false)
    }

    #[cfg(target_has_atomic = "8")]
    fn leaf_count(&self) -> u64 {
        u64::from(self.value != T::default())
    }

    fn keys_encoded(&self) -> Vec<Vec<u8>> {
        if self.value == T::default() {
            Vec::new()
        } else {
            alloc::vec![Vec::new()]
        }
    }

    fn value_encoded(&self, key: &[u8]) -> Result<Option<Vec<u8>>, DecodeError> {
        if !key.is_empty() {
            return Err(DecodeError("a slot's key is empty"));
        }
        Ok((self.value != T::default()).then(|| canon::to_bytes(&self.value)))
    }

    fn entries_encoded(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.keys_encoded()
            .into_iter()
            .map(|k| (k, canon::to_bytes(&self.value)))
            .collect()
    }

    fn make_partial(&mut self) {
        self.value = T::default();
        self.partial = Some(false);
    }

    fn load(&mut self, key: &[u8], value: Option<&[u8]>) -> Result<(), DecodeError> {
        if !key.is_empty() {
            return Err(DecodeError("a slot's key is empty"));
        }
        match self.partial {
            Some(false) => {}
            Some(true) => return Err(DecodeError("a leaf witnessed twice")),
            None => return Err(DecodeError("a leaf loaded into a whole store")),
        }
        if let Some(v) = value {
            let v: T = canon::from_bytes(v)?;
            if v == T::default() {
                return Err(DecodeError("a slot leaf holding its default"));
            }
            self.value = v;
        }
        self.partial = Some(true);
        Ok(())
    }

    fn set_complete(&mut self) {
        // A slot is one leaf: witnessing it whole is witnessing it.
        if self.partial == Some(false) {
            self.partial = Some(true);
        }
    }

    fn missed(&self) -> bool {
        self.track.missed.get()
    }

    fn witnessed(&self) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
        match self.partial {
            Some(true) => alloc::vec![(
                Vec::new(),
                (self.value != T::default()).then(|| canon::to_bytes(&self.value)),
            )],
            _ => Vec::new(),
        }
    }
}

/// An index derived from the leaves for queries only (module documentation). Not a leaf: in a
/// partial store it is empty, and reading it fails closed.
#[derive(Clone, Debug, Default)]
pub struct Derived<T> {
    value: T,
    partial: bool,
    missed: Flag,
}

impl<T: PartialEq> PartialEq for Derived<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<T: Eq> Eq for Derived<T> {}

impl<T: Default> Derived<T> {
    /// The index, to read. On a partial store, the read fails closed.
    pub fn read(&self) -> &T {
        if self.partial {
            self.missed.set(true);
        }
        &self.value
    }

    /// The index, to keep up to date. On a partial store the writes land in an index nobody may
    /// read.
    pub fn write(&mut self) -> &mut T {
        &mut self.value
    }
}

impl<T: Default> LeafMap for Derived<T> {
    fn kind(&self) -> u8 {
        0
    }
    fn record(&self, _on: bool) {}
    fn take_recorded(&self) -> (Vec<Vec<u8>>, bool) {
        (Vec::new(), false)
    }
    #[cfg(target_has_atomic = "8")]
    fn leaf_count(&self) -> u64 {
        0
    }
    fn keys_encoded(&self) -> Vec<Vec<u8>> {
        Vec::new()
    }
    fn value_encoded(&self, _key: &[u8]) -> Result<Option<Vec<u8>>, DecodeError> {
        Err(DecodeError("a derived index has no leaves"))
    }
    fn entries_encoded(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        Vec::new()
    }
    fn make_partial(&mut self) {
        self.value = T::default();
        self.partial = true;
    }
    fn load(&mut self, _key: &[u8], _value: Option<&[u8]>) -> Result<(), DecodeError> {
        Err(DecodeError("a derived index has no leaves"))
    }
    fn set_complete(&mut self) {}
    fn missed(&self) -> bool {
        self.missed.get()
    }
    fn witnessed(&self) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
        Vec::new()
    }
}

/// One collection of the service store, seen as leaves (module documentation). Keys and values
/// cross as their canonical encodings (`crate::export::canon`). Kind 0 is a [`Derived`] index,
/// which has no leaves.
pub trait LeafMap {
    /// The kind of its leaves.
    fn kind(&self) -> u8;
    /// Turn recording on or off; what was recorded stays until taken.
    fn record(&self, on: bool);
    /// The keys recorded, and whether the whole collection was read; recording starts afresh.
    fn take_recorded(&self) -> (Vec<Vec<u8>>, bool);
    /// How many leaves it holds, from the collection's own count: no leaf is visited, and
    /// nothing is recorded (the book's size, `T-018-11`). Zero for a [`Derived`] index; a partial
    /// store counts only what its witness gave and its run created. The host's alone (it has
    /// atomics; the core's guest has none), so the guest's code is not changed by it.
    #[cfg(target_has_atomic = "8")]
    fn leaf_count(&self) -> u64;
    /// Every key present (a whole store).
    fn keys_encoded(&self) -> Vec<Vec<u8>>;
    /// The value at a key, as it stands.
    fn value_encoded(&self, key: &[u8]) -> Result<Option<Vec<u8>>, DecodeError>;
    /// Every entry present (a whole store).
    fn entries_encoded(&self) -> Vec<(Vec<u8>, Vec<u8>)>;
    /// Every entry present, one at a time, in key order: what a host streams into its service
    /// tree without holding every leaf's encoding at once (`F-036`).
    fn visit_encoded(&self, f: &mut dyn FnMut(Vec<u8>, Vec<u8>)) {
        for (k, v) in self.entries_encoded() {
            f(k, v);
        }
    }
    /// Empty it and make it partial: it holds only what is loaded.
    fn make_partial(&mut self);
    /// Load one witnessed leaf: its key, and its value if present.
    fn load(&mut self, key: &[u8], value: Option<&[u8]>) -> Result<(), DecodeError>;
    /// The witness gave this collection whole.
    fn set_complete(&mut self);
    /// Whether a read or a write went outside the witness.
    fn missed(&self) -> bool;
    /// A partial collection's witnessed keys and the keys it created, with their values as they
    /// stand: what the service root after is computed from.
    fn witnessed(&self) -> Vec<(Vec<u8>, Option<Vec<u8>>)>;
}

/// Something that holds collections of the service store.
pub trait LeafHost {
    /// Each collection, once.
    fn leaf_maps<'a>(&'a self, out: &mut Vec<&'a dyn LeafMap>);
    /// Each collection, once, to change.
    fn leaf_maps_mut<'a>(&'a mut self, out: &mut Vec<&'a mut dyn LeafMap>);
}

/// A map's canonical encoding is its `BTreeMap`'s, so an export (`F-014`) reads the same bytes
/// whatever holds the map. Encoding reads it whole; decoding gives a whole map.
impl<K: Ord + Clone + Canonical, V: Canonical, const KIND: u8> Canonical for Leaves<K, V, KIND> {
    fn encode(&self, e: &mut canon::Enc) {
        self.all();
        self.map.encode(e);
    }
    fn decode(d: &mut canon::Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Leaves {
            map: BTreeMap::decode(d)?,
            ..Leaves::default()
        })
    }
}

/// A slot's canonical encoding is its value's.
impl<T: Canonical, const KIND: u8> Canonical for Slot<T, KIND> {
    fn encode(&self, e: &mut canon::Enc) {
        self.touch();
        self.value.encode(e);
    }
    fn decode(d: &mut canon::Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Slot::new(T::decode(d)?))
    }
}

impl Canonical for () {
    fn encode(&self, _e: &mut canon::Enc) {}
    fn decode(_d: &mut canon::Dec<'_>) -> Result<Self, DecodeError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type M = Leaves<u64, u64, 7>;

    #[test]
    fn a_whole_map_records_what_it_is_asked() {
        let mut m = M::new();
        m.insert(1, 10);
        m.record(true);
        assert_eq!(m.get(&1), Some(&10));
        assert!(!m.contains_key(&2));
        m.insert(3, 30);
        let (keys, whole) = m.take_recorded();
        assert_eq!(
            keys,
            alloc::vec![
                canon::to_bytes(&1u64),
                canon::to_bytes(&2u64),
                canon::to_bytes(&3u64)
            ]
        );
        assert!(!whole);
        let _ = m.len();
        assert!(m.take_recorded().1);
        m.record(false);
        let _ = m.get(&9);
        assert!(m.take_recorded().0.is_empty());
        assert!(!m.missed());
    }

    /// Finding F18 of `F-032`: a choice made on the whole store is not recorded as a whole read
    /// (only what is then read by key is), and a partial store never makes it.
    #[test]
    fn an_unrecorded_scan_is_not_witnessed_and_fails_closed_on_a_partial_map() {
        let mut m = M::new();
        m.insert(1, 10);
        m.insert(2, 20);
        m.record(true);
        assert_eq!(m.keys_unrecorded().count(), 2);
        assert_eq!(m.take_recorded(), (Vec::new(), false));
        assert!(!m.missed());
        let mut p = M::new();
        p.make_partial();
        p.load(&canon::to_bytes(&1u64), Some(&canon::to_bytes(&10u64)))
            .unwrap();
        let _ = p.keys_unrecorded().count();
        assert!(p.missed());
    }

    #[test]
    fn a_partial_map_fails_closed_outside_its_witness() {
        let mut m = M::new();
        m.make_partial();
        m.load(&canon::to_bytes(&1u64), Some(&canon::to_bytes(&10u64)))
            .unwrap();
        m.load(&canon::to_bytes(&2u64), None).unwrap();
        assert_eq!(m.get(&1), Some(&10));
        assert_eq!(m.get(&2), None);
        assert!(!m.missed());
        m.insert(2, 20);
        assert!(!m.missed());
        assert_eq!(m.created(), 1);
        assert_eq!(m.get(&3), None);
        assert!(m.missed());
        let mut m = M::new();
        m.make_partial();
        let _ = m.values().count();
        assert!(m.missed());
        let mut m = M::new();
        m.make_partial();
        m.set_complete();
        let _ = m.values().count();
        m.insert(5, 5);
        assert!(!m.missed());
        assert_eq!(
            m.witnessed(),
            alloc::vec![(canon::to_bytes(&5u64), Some(canon::to_bytes(&5u64)))]
        );
    }

    #[test]
    fn a_leaf_is_witnessed_once_and_canonically() {
        let mut m = M::new();
        m.make_partial();
        m.load(&canon::to_bytes(&1u64), None).unwrap();
        assert!(m.load(&canon::to_bytes(&1u64), None).is_err());
        assert!(m.load(&[0, 1], None).is_err());
        let mut s: Slot<u64, 9> = Slot::default();
        s.make_partial();
        assert!(s.load(&[], Some(&canon::to_bytes(&0u64))).is_err());
        let _ = s.get();
        assert!(s.missed());
    }

    #[test]
    fn a_derived_index_is_not_read_in_a_partial_store() {
        let mut d: Derived<Vec<u8>> = Derived::default();
        d.write().push(1);
        assert_eq!(d.read(), &alloc::vec![1]);
        assert!(!LeafMap::missed(&d));
        d.make_partial();
        d.write().push(2);
        assert!(!LeafMap::missed(&d));
        let _ = d.read();
        assert!(LeafMap::missed(&d));
    }
}
