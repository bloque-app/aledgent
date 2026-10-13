// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The delta record: the unit the core commits and the leaf store applies (`F-006` T-006-01,
//! `DEC-4`, `REQ-RT-1`, `REQ-RT-3`).
//!
//! ```text
//! DeltaRecord { sequence, prev_log_hash, roots_before, roots_after, kernel_delta, service_delta, ops }
//! log_hash = blake2b-256("aledgent/delta-log/v1" ‖ prev_log_hash ‖ canonical(record))
//! ```
//!
//! - `kernel_delta` is `F-001`'s [`BookDelta`]: the kernel's by-id puts, in key order, with the
//!   posting sequence numbers of the transfers it adds.
//! - `service_delta` is the library's own growing records, as leaves of a seventh tree, the
//!   **service tree** ([`ServiceLeaf`], `F-006` Proposal): the same sparse Merkle construction as
//!   the kernel's six (depth 128, 16-byte keys), with its own leaf tag.
//! - `ops` are the operation references the record commits.
//!
//! The record is the same whoever runs the core. A core in its own process commits it through the
//! leaf store; a core inside an Agent builds it and hands it to the host, which applies it. Either
//! way the hash chain and the roots make each record self-checking: applied to a store at
//! `roots_before` it must give `roots_after`, and the chain must reach the core's committed head.
//!
//! **The encoding is canonical.** Integers are big-endian, every map drains in key order, and
//! decoding refuses anything a re-encoding would not reproduce byte for byte (unsorted or repeated
//! keys, trailing bytes, an unknown version), so one record has one encoding and one log hash on
//! every run and every target. Kernel records inside are the kernel's own rkyv archives, which are
//! fixed-endian and fixed-width by the workspace's rkyv configuration.
//!
//! **A record fits one delta-ring row.** [`DeltaRecord::new`] refuses a record whose encoding is
//! over [`MAX_RECORD_BYTES`] (64 KiB, the Agent runtime's row limit). The limit applies whoever
//! runs the core, so a unit that is too large is refused the same way under both (`F-008` maps it
//! to `ERR-BatchTooLarge`).

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::num::NonZeroU64;

use cipher_clerk::crypto::blake2b_256;
use cipher_clerk::merkle::smt_leaf_hash;
use ledger_kernel::posting::OperationRef;
use ledger_kernel::store::codec::{decode_delta, encode_delta};
use ledger_kernel::store::{BookDelta, SubRoots, composite_root};

/// The largest encoded record: one row of the core's delta ring (64 KiB).
pub const MAX_RECORD_BYTES: usize = 64 * 1024;
/// Version byte of the record encoding.
pub const RECORD_VERSION: u8 = 2;
/// The `prev_log_hash` of the first record.
pub const GENESIS_LOG_HASH: [u8; 32] = [0; 32];
/// The first byte of every service leaf's content: the service tree's own leaf tag.
pub const SERVICE_LEAF_TAG: u8 = 0xA5;

/// Domain of the log hash chain.
const LOG_DOMAIN: &[u8] = b"aledgent/delta-log/v1";
/// Domain of the service-tree key derivation.
const SERVICE_KEY_DOMAIN: &[u8] = b"aledgent/service-key/v1";
/// Domain of the book root that folds the kernel root and the service root.
const BOOK_ROOT_DOMAIN: &[u8] = b"aledgent/book-root/v1";
/// The same with v2 (compact) trees (`DEC-25`): a v2 book root never equals
/// a v1 one, whatever the roots it folds.
const BOOK_ROOT_DOMAIN_V2: &[u8] = b"aledgent/book-root/v2";

/// The book root's domain in the build's root version.
pub fn book_root_domain() -> &'static [u8] {
    match ledger_kernel::compact_smt::Version::ACTIVE {
        ledger_kernel::compact_smt::Version::V1 => BOOK_ROOT_DOMAIN,
        ledger_kernel::compact_smt::Version::V2 => BOOK_ROOT_DOMAIN_V2,
    }
}

/// The seven tree roots of the book: the kernel's six sub-roots and the service root.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Roots {
    /// The kernel's six sub-roots, in composite order.
    pub kernel: SubRoots,
    /// The service tree's root.
    pub service: [u8; 32],
}

impl Roots {
    /// The roots of an empty book: every tree empty.
    pub fn empty() -> Self {
        let empty = ledger_kernel::compact_smt::Version::ACTIVE.empty_root();
        Self {
            kernel: [empty; 6],
            service: empty,
        }
    }

    /// The kernel's composite root, exactly as CipherClerk folds it.
    pub fn kernel_root(&self) -> [u8; 32] {
        composite_root(&self.kernel)
    }

    /// The book root: `blake2b-256("aledgent/book-root/v1" ‖ kernel root ‖ service root)`. What a
    /// core commits beside its head.
    pub fn book_root(&self) -> [u8; 32] {
        blake2b_256(book_root_domain(), &[&self.kernel_root(), &self.service])
    }

    fn encode(&self, out: &mut Vec<u8>) {
        for r in &self.kernel {
            out.extend_from_slice(r);
        }
        out.extend_from_slice(&self.service);
    }
}

/// What a service leaf holds: the leaf's **kind**, one byte. The code is stable: it enters the
/// leaf's key and its content. The named kinds are `F-006`'s; the library's collections take the
/// codes of [`crate::leaves::kind`] (`F-005`), of which these are the first eleven. Code 0 is no
/// kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServiceKind(u8);

#[allow(non_upper_case_globals)]
impl ServiceKind {
    /// The asset registry.
    pub const Registry: ServiceKind = ServiceKind(crate::leaves::kind::REGISTRY);
    /// The book's standing roles.
    pub const Roles: ServiceKind = ServiceKind(crate::leaves::kind::ROLES);
    /// An identity.
    pub const Identity: ServiceKind = ServiceKind(crate::leaves::kind::IDENTITY);
    /// A ledger account.
    pub const LedgerAccount: ServiceKind = ServiceKind(crate::leaves::kind::LEDGER_ACCOUNT);
    /// An authority grant.
    pub const Grant: ServiceKind = ServiceKind(crate::leaves::kind::GRANT);
    /// A credential.
    pub const Credential: ServiceKind = ServiceKind(crate::leaves::kind::CREDENTIAL);
    /// A window counter of an authority's bounds.
    pub const Window: ServiceKind = ServiceKind(crate::leaves::kind::WINDOW);
    /// An operation's record (`F-007`).
    pub const Operation: ServiceKind = ServiceKind(crate::leaves::kind::OPERATION);
    /// A client key and the operation it named (`REQ-OP-10`).
    pub const ClientKey: ServiceKind = ServiceKind(crate::leaves::kind::CLIENT_KEY);
    /// A reservation of the open-reservation index (`F-004`).
    pub const Reservation: ServiceKind = ServiceKind(crate::leaves::kind::RESERVATION);
    /// An entry of the deadline index (`F-007`).
    pub const Deadline: ServiceKind = ServiceKind(crate::leaves::kind::DEADLINE);

    /// The named kinds, in code order.
    pub const ALL: [ServiceKind; 11] = [
        ServiceKind::Registry,
        ServiceKind::Roles,
        ServiceKind::Identity,
        ServiceKind::LedgerAccount,
        ServiceKind::Grant,
        ServiceKind::Credential,
        ServiceKind::Window,
        ServiceKind::Operation,
        ServiceKind::ClientKey,
        ServiceKind::Reservation,
        ServiceKind::Deadline,
    ];

    /// The stable code.
    pub const fn code(self) -> u8 {
        self.0
    }

    /// The kind with this code: every code but 0.
    pub fn from_code(code: u8) -> Option<Self> {
        (code != 0).then_some(ServiceKind(code))
    }
}

/// A relation a service leaf states, which the store indexes (`F-006` T-006-07). A fact is part of
/// the leaf's content, so the index is authenticated by the service root and can be recomputed
/// from the leaves alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Fact {
    /// A ledger account owned by an identity: the index of accounts by identity.
    OwnedBy {
        /// The owning identity.
        identity: [u8; 16],
    },
    /// An authority over an account, held by an identity: the store half of `INV-16`. Stated by
    /// a grant's leaf while the grant stands.
    Holder {
        /// The account the authority is over.
        account: [u8; 16],
        /// The identity that holds it.
        holder: [u8; 16],
    },
    /// Value of an account reserved in an asset: stated by an open reservation's leaf, summed
    /// into the account's reserved total (`REQ-BA-6`, `DEC-2`).
    Reserves {
        /// The payer's ledger account.
        account: [u8; 16],
        /// The asset's kernel tag.
        asset: u32,
        /// Minor units held.
        amount: NonZeroU64,
    },
}

impl Fact {
    const fn tag(&self) -> u8 {
        match self {
            Fact::OwnedBy { .. } => 1,
            Fact::Holder { .. } => 2,
            Fact::Reserves { .. } => 3,
        }
    }

    fn encode(&self, out: &mut Vec<u8>) {
        out.push(self.tag());
        match self {
            Fact::OwnedBy { identity } => out.extend_from_slice(identity),
            Fact::Holder { account, holder } => {
                out.extend_from_slice(account);
                out.extend_from_slice(holder);
            }
            Fact::Reserves {
                account,
                asset,
                amount,
            } => {
                out.extend_from_slice(account);
                out.extend_from_slice(&asset.to_be_bytes());
                out.extend_from_slice(&amount.get().to_be_bytes());
            }
        }
    }

    fn decode(r: &mut Reader<'_>) -> Result<Self, RecordError> {
        let [tag] = r.array::<1>()?;
        Ok(match tag {
            1 => Fact::OwnedBy {
                identity: r.array()?,
            },
            2 => Fact::Holder {
                account: r.array()?,
                holder: r.array()?,
            },
            3 => Fact::Reserves {
                account: r.array()?,
                asset: u32::from_be_bytes(r.array()?),
                amount: NonZeroU64::new(u64::from_be_bytes(r.array()?))
                    .ok_or_else(|| codec("a reservation of zero"))?,
            },
            t => return Err(codec(&format!("fact tag {t}"))),
        })
    }
}

/// The 16-byte key of a service leaf: its kind's code, then
/// `blake2b-256("aledgent/service-key/v1" ‖ kind ‖ id)[..15]`. Every kind is one subtree of the
/// service tree (the one under its code), so a witness can show a kind whole (`F-005`).
///
/// A kind keyed by a ledger account ([`crate::leaves::kind::ACCOUNT_SCOPED`]) keeps the
/// partition its identifier carries ([`library_id`]) right after its code, then 13 bytes of the
/// digest: under a kind's subtree, each partition is one subtree (`REQ-RT-9`, `F-031`).
pub fn service_key(kind: ServiceKind, id: &[u8; 16]) -> [u8; 16] {
    let digest = blake2b_256(SERVICE_KEY_DOMAIN, &[&[kind.code()], id]);
    let mut key = [0u8; 16];
    key[0] = kind.code();
    if crate::leaves::kind::account_scoped(kind.code()) {
        key[1..3].copy_from_slice(&id[..2]);
        key[3..].copy_from_slice(&digest[..13]);
    } else {
        key[1..].copy_from_slice(&digest[..15]);
    }
    key
}

/// Domain of a library leaf's identifier.
const LIBRARY_ID_DOMAIN: &[u8] = b"aledgent/service-id/v1";

/// The identifier of the library's leaf of `kind` at canonical key `key` (`crate::leaves`):
/// `blake2b-256("aledgent/service-id/v1" ‖ kind ‖ key)[..16]`. For a kind keyed by a ledger
/// account, the account's partition, then the digest's first 14 bytes (`REQ-RT-9`).
pub fn library_id(kind: u8, key: &[u8]) -> [u8; 16] {
    let digest = blake2b_256(LIBRARY_ID_DOMAIN, &[&[kind], key]);
    match key.get(..16).and_then(|a| <&[u8; 16]>::try_from(a).ok()) {
        Some(account) if crate::leaves::kind::account_scoped(kind) => {
            ledger_kernel::partition::Partition::of(account).place(&digest)
        }
        _ => {
            let mut id = [0u8; 16];
            id.copy_from_slice(&digest[..16]);
            id
        }
    }
}

/// The service-tree key of the library's leaf of `kind` at canonical key `key`.
pub fn library_key(kind: u8, key: &[u8]) -> [u8; 16] {
    service_key(ServiceKind(kind), &library_id(kind, key))
}

/// The library's leaf of `kind` at canonical key `key`, holding the canonical `value`
/// (`crate::leaves`, `F-006` decision 5): no facts, and a body of the key then the value, each
/// length-prefixed.
pub fn library_leaf(kind: u8, key: &[u8], value: &[u8]) -> ServiceLeaf {
    let mut body = Vec::with_capacity(8 + key.len() + value.len());
    put_blob(&mut body, key);
    put_blob(&mut body, value);
    ServiceLeaf::new(ServiceKind(kind), library_id(kind, key), Vec::new(), body)
}

/// The canonical key and value a leaf built by [`library_leaf`] holds.
pub fn library_parts(leaf: &ServiceLeaf) -> Result<(Vec<u8>, Vec<u8>), RecordError> {
    let mut r = Reader { bytes: leaf.body() };
    let key = r.blob()?.to_vec();
    let value = r.blob()?.to_vec();
    r.finish()?;
    if !leaf.facts().is_empty() || library_id(leaf.kind().code(), &key) != leaf.id() {
        return Err(codec("not a library leaf"));
    }
    Ok((key, value))
}

/// The leaf hash of the library's leaf of `kind` at `key`: its [`ServiceLeaf::leaf_hash`] if it
/// holds `value`, the empty sentinel if it is absent.
pub fn library_leaf_hash(kind: u8, key: &[u8], value: Option<&[u8]>) -> [u8; 32] {
    value.map_or(cipher_clerk::merkle::SMT_EMPTY_LEAF, |v| {
        library_leaf(kind, key, v).leaf_hash()
    })
}

/// One leaf of the service tree: a library record, the facts the store indexes, and the record's
/// canonical bytes. The bytes are the library's to define per kind (`F-005`); the store never
/// reads them.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ServiceLeaf {
    kind: ServiceKind,
    id: [u8; 16],
    facts: Vec<Fact>,
    body: Vec<u8>,
}

impl ServiceLeaf {
    /// A leaf. Facts are kept sorted and without repeats, so equal leaves encode equally.
    pub fn new(kind: ServiceKind, id: [u8; 16], facts: Vec<Fact>, body: Vec<u8>) -> Self {
        let facts: BTreeSet<Fact> = facts.into_iter().collect();
        Self {
            kind,
            id,
            facts: facts.into_iter().collect(),
            body,
        }
    }

    /// What the leaf holds.
    pub fn kind(&self) -> ServiceKind {
        self.kind
    }

    /// The record's own identifier.
    pub fn id(&self) -> [u8; 16] {
        self.id
    }

    /// The facts, sorted.
    pub fn facts(&self) -> &[Fact] {
        &self.facts
    }

    /// The record's canonical bytes.
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// The leaf's key in the service tree.
    pub fn key(&self) -> [u8; 16] {
        service_key(self.kind, &self.id)
    }

    /// The canonical encoding: kind ‖ id ‖ facts ‖ body.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(32 + self.body.len());
        out.push(self.kind.code());
        out.extend_from_slice(&self.id);
        let n = u16::try_from(self.facts.len()).expect("a leaf never states 65 536 facts");
        out.extend_from_slice(&n.to_be_bytes());
        for f in &self.facts {
            f.encode(&mut out);
        }
        put_blob(&mut out, &self.body);
        out
    }

    /// Decode a leaf. Unsorted or repeated facts, an unknown kind and trailing bytes are refused.
    pub fn decode(bytes: &[u8]) -> Result<Self, RecordError> {
        let mut r = Reader { bytes };
        let leaf = Self::read(&mut r)?;
        r.finish()?;
        Ok(leaf)
    }

    fn read(r: &mut Reader<'_>) -> Result<Self, RecordError> {
        let [code] = r.array::<1>()?;
        let kind =
            ServiceKind::from_code(code).ok_or_else(|| codec(&format!("service kind {code}")))?;
        let id = r.array()?;
        let n = u16::from_be_bytes(r.array()?);
        let mut facts: Vec<Fact> = Vec::with_capacity(usize::from(n));
        for _ in 0..n {
            let f = Fact::decode(r)?;
            if facts.last().is_some_and(|last| *last >= f) {
                return Err(codec("facts out of order"));
            }
            facts.push(f);
        }
        let body = r.blob()?.to_vec();
        Ok(Self {
            kind,
            id,
            facts,
            body,
        })
    }

    /// The content hashed into the service tree: the service leaf tag, then the encoding.
    pub fn leaf_content(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(33 + self.body.len());
        out.push(SERVICE_LEAF_TAG);
        out.extend_from_slice(&self.encode());
        out
    }

    /// The leaf hash, with the same construction as the kernel's trees.
    pub fn leaf_hash(&self) -> [u8; 32] {
        smt_leaf_hash(&self.leaf_content())
    }
}

/// The service tree's changes in one record, by key: puts and removals. A put inserts a leaf or
/// replaces the leaf of the same record. A removal empties a leaf of a derived index (`F-005`):
/// the library's records themselves are never removed (`REQ-LA-4`), but an index entry that stops
/// holding is.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ServiceDelta {
    puts: BTreeMap<[u8; 16], ServiceLeaf>,
    removes: BTreeMap<[u8; 16], (ServiceKind, [u8; 16])>,
}

impl ServiceDelta {
    /// An empty delta.
    pub fn new() -> Self {
        Self::default()
    }

    /// Put a leaf. A later put of the same record replaces an earlier one, and a removal; a put
    /// whose key is already taken by another record is refused (a key collision, fatal as for
    /// external ids).
    pub fn put(&mut self, leaf: ServiceLeaf) -> Result<(), RecordError> {
        let key = leaf.key();
        if let Some(prev) = self.puts.get(&key)
            && (prev.kind, prev.id) != (leaf.kind, leaf.id)
        {
            return Err(RecordError::ServiceKeyCollision { key });
        }
        if let Some(prev) = self.removes.get(&key)
            && *prev != (leaf.kind, leaf.id)
        {
            return Err(RecordError::ServiceKeyCollision { key });
        }
        self.removes.remove(&key);
        self.puts.insert(key, leaf);
        Ok(())
    }

    /// Remove the leaf of record `id` of `kind`. It replaces an earlier put of the same record.
    pub fn remove(&mut self, kind: ServiceKind, id: [u8; 16]) -> Result<(), RecordError> {
        let key = service_key(kind, &id);
        if let Some(prev) = self.puts.get(&key)
            && (prev.kind, prev.id) != (kind, id)
        {
            return Err(RecordError::ServiceKeyCollision { key });
        }
        if let Some(prev) = self.removes.get(&key)
            && *prev != (kind, id)
        {
            return Err(RecordError::ServiceKeyCollision { key });
        }
        self.puts.remove(&key);
        self.removes.insert(key, (kind, id));
        Ok(())
    }

    /// The puts, in key order.
    pub fn puts(&self) -> impl Iterator<Item = (&[u8; 16], &ServiceLeaf)> {
        self.puts.iter()
    }

    /// The removals, in key order: the key, and the record's kind and identifier.
    pub fn removes(&self) -> impl Iterator<Item = (&[u8; 16], &(ServiceKind, [u8; 16]))> {
        self.removes.iter()
    }

    /// The number of changes.
    pub fn len(&self) -> usize {
        self.puts.len() + self.removes.len()
    }

    /// Whether the delta changes nothing.
    pub fn is_empty(&self) -> bool {
        self.puts.is_empty() && self.removes.is_empty()
    }
}

/// Why a record could not be built or read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordError {
    /// The encoding is over [`MAX_RECORD_BYTES`].
    TooLarge {
        /// The encoded size.
        bytes: usize,
    },
    /// Two records of the service tree share one key.
    ServiceKeyCollision {
        /// The key.
        key: [u8; 16],
    },
    /// The bytes are not a canonical record.
    Codec(String),
}

fn codec(what: &str) -> RecordError {
    RecordError::Codec(what.to_string())
}

/// One committed transition of the book (module documentation).
///
/// Built once, then read: the encoding and the log hash are computed at construction and never
/// change.
#[derive(Clone, PartialEq, Eq)]
pub struct DeltaRecord {
    sequence: u64,
    prev_log_hash: [u8; 32],
    roots_before: Roots,
    roots_after: Roots,
    kernel_delta: BookDelta,
    service_delta: ServiceDelta,
    ops: BTreeSet<OperationRef>,
    bytes: Vec<u8>,
    log_hash: [u8; 32],
}

impl core::fmt::Debug for DeltaRecord {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DeltaRecord")
            .field("sequence", &self.sequence)
            .field("bytes", &self.bytes.len())
            .field("log_hash", &self.log_hash)
            .finish_non_exhaustive()
    }
}

/// The parts of a record, for building one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordParts {
    /// The commit sequence number: 0, 1, 2… without a gap.
    pub sequence: u64,
    /// The log hash of the previous record, or [`GENESIS_LOG_HASH`].
    pub prev_log_hash: [u8; 32],
    /// The roots the record applies to.
    pub roots_before: Roots,
    /// The roots it yields.
    pub roots_after: Roots,
    /// The kernel's puts.
    pub kernel_delta: BookDelta,
    /// The service tree's puts.
    pub service_delta: ServiceDelta,
    /// The operations it commits.
    pub ops: BTreeSet<OperationRef>,
}

impl DeltaRecord {
    /// Build a record. Refused if its encoding is over [`MAX_RECORD_BYTES`].
    pub fn new(parts: RecordParts) -> Result<Self, RecordError> {
        let bytes = encode_parts(&parts);
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(RecordError::TooLarge { bytes: bytes.len() });
        }
        let log_hash = blake2b_256(LOG_DOMAIN, &[&parts.prev_log_hash, &bytes]);
        let RecordParts {
            sequence,
            prev_log_hash,
            roots_before,
            roots_after,
            kernel_delta,
            service_delta,
            ops,
        } = parts;
        Ok(Self {
            sequence,
            prev_log_hash,
            roots_before,
            roots_after,
            kernel_delta,
            service_delta,
            ops,
            bytes,
            log_hash,
        })
    }

    /// Decode a record from its canonical encoding. Anything a re-encoding would not reproduce is
    /// refused.
    pub fn decode(bytes: &[u8]) -> Result<Self, RecordError> {
        if bytes.len() > MAX_RECORD_BYTES {
            return Err(RecordError::TooLarge { bytes: bytes.len() });
        }
        let mut r = Reader { bytes };
        let [version] = r.array::<1>()?;
        if version != RECORD_VERSION {
            return Err(codec(&format!("record version {version}")));
        }
        let sequence = u64::from_be_bytes(r.array()?);
        let prev_log_hash = r.array()?;
        let roots_before = read_roots(&mut r)?;
        let roots_after = read_roots(&mut r)?;
        let kernel_bytes = r.blob()?;
        let kernel_delta = decode_delta(kernel_bytes).map_err(|e| RecordError::Codec(e.0))?;
        if encode_delta(&kernel_delta) != kernel_bytes {
            return Err(codec("kernel delta is not canonical"));
        }
        let mut service_delta = ServiceDelta::default();
        let mut last: Option<[u8; 16]> = None;
        for _ in 0..r.count()? {
            let leaf_bytes = r.blob()?;
            let leaf = ServiceLeaf::decode(leaf_bytes)?;
            let key = leaf.key();
            if last.is_some_and(|l| l >= key) {
                return Err(codec("service puts out of order"));
            }
            last = Some(key);
            service_delta.puts.insert(key, leaf);
        }
        let mut last: Option<[u8; 16]> = None;
        for _ in 0..r.count()? {
            let [code] = r.array::<1>()?;
            let kind = ServiceKind::from_code(code)
                .ok_or_else(|| codec(&format!("service kind {code}")))?;
            let id = r.array()?;
            let key = service_key(kind, &id);
            if last.is_some_and(|l| l >= key) {
                return Err(codec("service removals out of order"));
            }
            if service_delta.puts.contains_key(&key) {
                return Err(codec("a leaf both put and removed"));
            }
            last = Some(key);
            service_delta.removes.insert(key, (kind, id));
        }
        let mut ops = BTreeSet::new();
        let mut last_op: Option<OperationRef> = None;
        for _ in 0..r.count()? {
            let op = OperationRef(r.array()?);
            if last_op.is_some_and(|l| l >= op) {
                return Err(codec("operations out of order"));
            }
            last_op = Some(op);
            ops.insert(op);
        }
        r.finish()?;
        let record = Self::new(RecordParts {
            sequence,
            prev_log_hash,
            roots_before,
            roots_after,
            kernel_delta,
            service_delta,
            ops,
        })?;
        if record.bytes != bytes {
            return Err(codec("record is not canonical"));
        }
        Ok(record)
    }

    /// The commit sequence number.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// The previous record's log hash.
    pub fn prev_log_hash(&self) -> [u8; 32] {
        self.prev_log_hash
    }

    /// The roots the record applies to.
    pub fn roots_before(&self) -> &Roots {
        &self.roots_before
    }

    /// The roots it yields.
    pub fn roots_after(&self) -> &Roots {
        &self.roots_after
    }

    /// The kernel's puts.
    pub fn kernel_delta(&self) -> &BookDelta {
        &self.kernel_delta
    }

    /// The service tree's puts.
    pub fn service_delta(&self) -> &ServiceDelta {
        &self.service_delta
    }

    /// The operations it commits.
    pub fn ops(&self) -> &BTreeSet<OperationRef> {
        &self.ops
    }

    /// The canonical encoding.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// `blake2b-256("aledgent/delta-log/v1" ‖ prev_log_hash ‖ canonical(record))`.
    pub fn log_hash(&self) -> [u8; 32] {
        self.log_hash
    }

    /// Whether this record continues the chain whose head is `head`.
    pub fn follows(&self, head: &[u8; 32]) -> bool {
        &self.prev_log_hash == head
    }
}

fn encode_parts(p: &RecordParts) -> Vec<u8> {
    let kernel = encode_delta(&p.kernel_delta);
    let mut out = Vec::with_capacity(1 + 8 + 32 + 2 * 224 + 4 + kernel.len() + 64);
    out.push(RECORD_VERSION);
    out.extend_from_slice(&p.sequence.to_be_bytes());
    out.extend_from_slice(&p.prev_log_hash);
    p.roots_before.encode(&mut out);
    p.roots_after.encode(&mut out);
    put_blob(&mut out, &kernel);
    put_u32(&mut out, p.service_delta.puts.len());
    for leaf in p.service_delta.puts.values() {
        put_blob(&mut out, &leaf.encode());
    }
    put_u32(&mut out, p.service_delta.removes.len());
    for (kind, id) in p.service_delta.removes.values() {
        out.push(kind.code());
        out.extend_from_slice(id);
    }
    put_u32(&mut out, p.ops.len());
    for op in &p.ops {
        out.extend_from_slice(&op.0);
    }
    out
}

fn read_roots(r: &mut Reader<'_>) -> Result<Roots, RecordError> {
    let mut kernel = [[0u8; 32]; 6];
    for k in &mut kernel {
        *k = r.array()?;
    }
    Ok(Roots {
        kernel,
        service: r.array()?,
    })
}

fn put_u32(out: &mut Vec<u8>, n: usize) {
    let n = u32::try_from(n).expect("a record section never holds 2^32 items");
    out.extend_from_slice(&n.to_be_bytes());
}

fn put_blob(out: &mut Vec<u8>, blob: &[u8]) {
    put_u32(out, blob.len());
    out.extend_from_slice(blob);
}

struct Reader<'a> {
    bytes: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], RecordError> {
        if self.bytes.len() < n {
            return Err(codec("truncated"));
        }
        let (head, tail) = self.bytes.split_at(n);
        self.bytes = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], RecordError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn count(&mut self) -> Result<usize, RecordError> {
        Ok(u32::from_be_bytes(self.array()?) as usize)
    }

    fn blob(&mut self) -> Result<&'a [u8], RecordError> {
        let n = self.count()?;
        self.take(n)
    }

    fn finish(&self) -> Result<(), RecordError> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(codec("trailing bytes"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use cipher_clerk::crypto::AuthKey;
    use cipher_clerk::ids::ExternalId;
    use cipher_clerk::{Account, AccountId, Direction, Journal, JournalId, Transfer, TransferId};
    use ledger_kernel::store::PostingRecord;

    fn sample_parts() -> RecordParts {
        let mut kernel_delta = BookDelta::default();
        let journal = JournalId([2; 16]);
        let a = Account::new(
            AccountId([1; 16]),
            journal,
            AuthKey([3; 32]),
            7u32,
            1u16,
            Direction::Credit,
        );
        kernel_delta.accounts.insert(a.id, a);
        let t = Transfer::draft(TransferId([4; 16]), journal, Vec::new());
        kernel_delta.transfers.insert(t.id, t);
        kernel_delta
            .journals
            .insert(journal, Journal::new(journal, AuthKey([5; 32]), 0));
        kernel_delta.external_ids.insert(ExternalId([6; 32]));
        kernel_delta.postings.push(PostingRecord {
            seq: 0,
            transfer: TransferId([4; 16]),
        });
        let mut service_delta = ServiceDelta::new();
        service_delta
            .put(ServiceLeaf::new(
                ServiceKind::LedgerAccount,
                [7; 16],
                vec![Fact::OwnedBy { identity: [8; 16] }],
                b"account".to_vec(),
            ))
            .unwrap();
        service_delta
            .put(ServiceLeaf::new(
                ServiceKind::Reservation,
                [9; 16],
                vec![Fact::Reserves {
                    account: [7; 16],
                    asset: 1,
                    amount: NonZeroU64::new(250).unwrap(),
                }],
                Vec::new(),
            ))
            .unwrap();
        let mut ops = BTreeSet::new();
        ops.insert(OperationRef([0x0A; 16]));
        ops.insert(OperationRef([0x0B; 16]));
        let mut after = Roots::empty();
        after.service = [0xEE; 32];
        RecordParts {
            sequence: 3,
            prev_log_hash: [0x11; 32],
            roots_before: Roots::empty(),
            roots_after: after,
            kernel_delta,
            service_delta,
            ops,
        }
    }

    #[test]
    fn a_record_round_trips_and_keeps_its_hash() {
        let rec = DeltaRecord::new(sample_parts()).unwrap();
        let back = DeltaRecord::decode(rec.bytes()).unwrap();
        assert_eq!(back, rec);
        assert_eq!(back.log_hash(), rec.log_hash());
        assert!(rec.follows(&[0x11; 32]));
        assert_eq!(rec.service_delta().len(), 2);
    }

    /// T-006-01: the encoding does not depend on the run or the target. Pinned by a golden digest
    /// of the sample record's bytes: a change of layout, endianness or width changes it.
    #[test]
    fn req_rt_3_the_encoding_is_deterministic_golden() {
        let a = DeltaRecord::new(sample_parts()).unwrap();
        let b = DeltaRecord::new(sample_parts()).unwrap();
        assert_eq!(a.bytes(), b.bytes());
        let digest = blake2b_256(b"golden", &[a.bytes()]);
        assert_eq!(
            hex(&digest),
            match ledger_kernel::compact_smt::Version::ACTIVE {
                ledger_kernel::compact_smt::Version::V1 => GOLDEN_SAMPLE_DIGEST,
                // The sample's `roots_before` is the empty book's, whose roots differ in v2.
                ledger_kernel::compact_smt::Version::V2 => GOLDEN_SAMPLE_DIGEST_V2,
            },
            "the record encoding changed: bump RECORD_VERSION and re-pin"
        );
    }

    const GOLDEN_SAMPLE_DIGEST_V2: &str =
        "dc6d25626f4ed95a2df96670077bedf661b3269dbc1837a15623c6a6e958c7eb";
    const GOLDEN_SAMPLE_DIGEST: &str =
        "af2daf22490ab7177e07df75f209ee0eacffb0d7b7c1c1185a1658b64ce06f4d";

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn a_record_over_64_kib_cannot_be_built() {
        let mut parts = sample_parts();
        let mut delta = ServiceDelta::new();
        delta
            .put(ServiceLeaf::new(
                ServiceKind::Operation,
                [1; 16],
                Vec::new(),
                vec![0u8; MAX_RECORD_BYTES],
            ))
            .unwrap();
        parts.service_delta = delta;
        assert!(matches!(
            DeltaRecord::new(parts),
            Err(RecordError::TooLarge { .. })
        ));
    }

    #[test]
    fn non_canonical_bytes_are_refused() {
        let rec = DeltaRecord::new(sample_parts()).unwrap();
        let mut padded = rec.bytes().to_vec();
        padded.push(0);
        assert!(DeltaRecord::decode(&padded).is_err());
        assert!(DeltaRecord::decode(&rec.bytes()[..rec.bytes().len() - 1]).is_err());
        let mut version = rec.bytes().to_vec();
        version[0] = 1;
        assert!(DeltaRecord::decode(&version).is_err());
        // Two service leaves swapped: the same content in a non-canonical order.
        let leaves: Vec<ServiceLeaf> = rec.service_delta().puts().map(|(_, l)| l.clone()).collect();
        let mut swapped = Vec::new();
        swapped.push(RECORD_VERSION);
        swapped.extend_from_slice(&rec.sequence().to_be_bytes());
        swapped.extend_from_slice(&rec.prev_log_hash());
        rec.roots_before().encode(&mut swapped);
        rec.roots_after().encode(&mut swapped);
        put_blob(&mut swapped, &encode_delta(rec.kernel_delta()));
        put_u32(&mut swapped, 2);
        put_blob(&mut swapped, &leaves[1].encode());
        put_blob(&mut swapped, &leaves[0].encode());
        put_u32(&mut swapped, 0);
        put_u32(&mut swapped, rec.ops().len());
        for op in rec.ops() {
            swapped.extend_from_slice(&op.0);
        }
        assert_eq!(
            DeltaRecord::decode(&swapped),
            Err(codec("service puts out of order"))
        );
    }

    #[test]
    fn service_leaves_are_canonical_and_keyed_by_kind_and_id() {
        let f1 = Fact::OwnedBy { identity: [1; 16] };
        let f2 = Fact::Holder {
            account: [2; 16],
            holder: [3; 16],
        };
        let a = ServiceLeaf::new(ServiceKind::Grant, [4; 16], vec![f2, f1, f2], Vec::new());
        let b = ServiceLeaf::new(ServiceKind::Grant, [4; 16], vec![f1, f2], Vec::new());
        assert_eq!(a, b);
        assert_eq!(a.encode(), b.encode());
        assert_eq!(ServiceLeaf::decode(&a.encode()).unwrap(), a);
        assert_ne!(
            service_key(ServiceKind::Grant, &[4; 16]),
            service_key(ServiceKind::Credential, &[4; 16])
        );
        assert_eq!(a.leaf_content()[0], SERVICE_LEAF_TAG);
        let mut d = ServiceDelta::new();
        d.put(a.clone()).unwrap();
        d.put(ServiceLeaf::new(
            ServiceKind::Grant,
            [4; 16],
            Vec::new(),
            b"revoked".to_vec(),
        ))
        .unwrap();
        assert_eq!(d.len(), 1);
    }

    /// `F-005`: removals cross in the record, canonically; a later put of the same record
    /// replaces a removal and the other way round; every kind is the subtree under its code.
    #[test]
    fn removals_round_trip_and_keys_lead_with_their_kind() {
        let mut parts = sample_parts();
        let mut d = parts.service_delta.clone();
        d.remove(ServiceKind::Deadline, [7; 16]).unwrap();
        d.remove(ServiceKind::Deadline, [8; 16]).unwrap();
        d.put(ServiceLeaf::new(
            ServiceKind::Deadline,
            [8; 16],
            Vec::new(),
            b"back".to_vec(),
        ))
        .unwrap();
        assert_eq!(d.removes().count(), 1);
        parts.service_delta = d;
        let rec = DeltaRecord::new(parts).unwrap();
        let back = DeltaRecord::decode(rec.bytes()).unwrap();
        assert_eq!(back, rec);
        assert_eq!(back.service_delta().removes().count(), 1);
        for k in ServiceKind::ALL {
            assert_eq!(service_key(k, &[3; 16])[0], k.code());
        }
        assert_eq!(ServiceKind::from_code(0), None);
        let leaf = library_leaf(20, b"key", b"value");
        assert_eq!(leaf.key(), library_key(20, b"key"));
        assert_eq!(
            library_parts(&leaf).unwrap(),
            (b"key".to_vec(), b"value".to_vec())
        );
        assert_eq!(
            library_leaf_hash(20, b"key", None),
            cipher_clerk::merkle::SMT_EMPTY_LEAF
        );
    }
}
