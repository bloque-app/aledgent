// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Seeded genesis (`F-036`, amendment 0022 draft, `REQ-RC-4`, `REQ-RC-6`): a book that starts
//! from a sealed **seed** — identities, ledger accounts, aliases, opening balances and grants —
//! applied at genesis in bulk, through the same commands, instead of one command per record after
//! it.
//!
//! ## The seed
//!
//! A [`ChainSpec`] is a [`SpecHeader`] and a list of [`SpecRecord`]s in a fixed section order
//! ([`Section`]), each section sorted, every amount an exact integer of minor units (`REQ-AS-4`):
//!
//! | Section | Record | Applied as |
//! |---|---|---|
//! | identities | kind, `key` verifier (operator-held public material; one verifier may serve many identities, owner ruling 2026-10-08; a seeded service principal's own, `T-036-30`) | `open_identity` by the operator |
//! | accounts | owner, kind; custody is always delegated | `open_account` by the operator |
//! | aliases | (source, legacy reference) → account: the opening set's source map (`F-014`) | an alias of the book, keyed by a hash of the reference |
//! | openings | (account, asset, amount), in (account, asset) order: the opening set's rows | one `Opening` per non-zero row, the opening load's postings |
//! | operator grants | operator-surface kinds the operator gives itself | `grant_operator` |
//! | grants | a grant request: the operator's grant over `delegated_accounts`; amendment 0025's grants of the seed (`a0025-q3`, `a0025-q4`, R4): a delegated grant over an identity naming `open_account`, and delegated grants over its accounts with no parent | `grant_authority`, by the operator as the identities principal |
//! | issuance grants | a grant request and its justification | `grant_issuance` |
//!
//! The header names the book it is for (genesis's operation reference), the time every seeded
//! command carries, the registry (equal to genesis's), each section's count, the opening set's
//! commitment ([`OpeningCommit`]: set digest, reconciliation digest, totals) and the
//! pre-cutover history's ([`history::HistoryCommit`]).
//!
//! ## Chunks and the digest
//!
//! The seed is cut into [`SpecChunk`]s: chunk 0 is the header, every later chunk a run of records
//! within [`ChunkLimits`]. Each chunk's leaf is `blake2b-256("aledgent/chainspec/chunk/v1" ‖ index ‖
//! canonical chunk)`, the leaves form a Merkle tree in index order (`chainspec::merkle`), and the **seed
//! digest** is `blake2b-256("aledgent/chainspec/v1" ‖ version ‖ chunk count ‖ root)`. Genesis carries
//! the digest and the count ([`SpecCommitment`]); each chunk travels with its path, so the book
//! checks every chunk against what genesis committed, in order, before it applies anything of it.
//!
//! ## References
//!
//! Every identifier a seeded command mints derives from genesis's operation reference, the section
//! and the record's position in it ([`spec_op`]), so the builder knows every identity and account
//! before the book exists, and the same seed gives the same identifiers in any book founded on the
//! same genesis, and in no other.

pub mod history;
pub mod merkle;

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use cipher_clerk::crypto::blake2b_256;
use ledger_kernel::posting::OperationRef;

use crate::account::{AccountKind, Time};
use crate::asset::{AssetBinding, AssetId, AssetRegistry};
use crate::authority::{GrantKind, OperationKind, Subject, Window};
use crate::code::CodeManifest;
use crate::export::canon::{self, Canonical, Dec, DecodeError, Enc};
use crate::id::{AccountId, CredentialId, GrantId, IdentityId, OperationId};
use crate::identity::IdentityKind;
use crate::leaves::{LeafHost, LeafMap, Leaves, Slot, kind};
use crate::ledger::{AssetDecl, GrantRequest};
use crate::opening::{OpeningRow, OpeningSet, OpeningSource, Reconciliation, RequiredSources};
use crate::operator::OperatorOpKind;
use crate::principal::Verifier;
use crate::set::NonEmptySet;

pub use history::HistoryCommit;

/// A shared chainspec, as `Genesis::spec` holds it: an `Arc` where the target has atomics (every
/// host), an `Rc` in the core's guest, which has neither threads nor atomics and never holds one.
#[cfg(target_has_atomic = "ptr")]
pub type SpecRef = alloc::sync::Arc<ChainSpec>;
/// A shared chainspec (see the atomics build).
#[cfg(not(target_has_atomic = "ptr"))]
pub type SpecRef = alloc::rc::Rc<ChainSpec>;

/// The seed format this module reads and writes. Version 2 adds the header's genesis code
/// ([`GenesisCode`], amendment 0023) and settlement base ([`SettlementBase`], amendment 0022
/// option (e)). Version 3 adds the merge map's and the exclusion file's SHA-256
/// (`merges_sha256`, `excluded_sha256`: the account unification, rulings `au-8` and `au-19`).
/// Version 4 adds the reconciliation journal's SHA-256 (`reconcile_sha256`: every adjustment that
/// sets a chain-bound asset's openings equal to the chain, ruling `chain-balance-authority`).
pub const SPEC_VERSION: u16 = 4;

const SEED_DOMAIN: &[u8] = b"aledgent/chainspec/v1";
const CHUNK_DOMAIN: &[u8] = b"aledgent/chainspec/chunk/v1";
const NODE_DOMAIN: &[u8] = b"aledgent/chainspec/node/v1";
const OP_DOMAIN: &[u8] = b"aledgent/chainspec/op/v1";
const ALIAS_DOMAIN: &[u8] = b"aledgent/alias/v1";
const FILE_MAGIC: &[u8; 8] = b"ALGSPEC1";

/// What genesis carries of a seed: its digest and how many chunks it has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SpecCommitment {
    /// The seed digest (module documentation).
    pub digest: [u8; 32],
    /// The number of chunks, the header included.
    pub chunks: u32,
}

/// The seed's sections, in the order they must appear. The code enters every seeded reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Section {
    /// Identities.
    Identities = 1,
    /// Ledger accounts.
    Accounts = 2,
    /// Aliases: the opening set's source map.
    Aliases = 3,
    /// Opening rows.
    Openings = 4,
    /// Operator-surface grants to the operator itself.
    OperatorGrants = 5,
    /// Authority grants.
    Grants = 6,
    /// Issuance grants.
    IssuanceGrants = 7,
    /// Imported credentials: an identity's existing passkeys (`REQ-MG-7`, ruling `mn7`), after the
    /// operator's own grants, since an import is an operator operation of its own kind.
    Credentials = 8,
}

/// The section code of the chunks' own references.
const CHUNK_SECTION: u8 = 0xC0;

/// How many records of each section.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SpecCounts {
    /// Identities.
    pub identities: u64,
    /// Ledger accounts.
    pub accounts: u64,
    /// Aliases.
    pub aliases: u64,
    /// Opening rows, zero rows included.
    pub openings: u64,
    /// Operator-surface grants.
    pub operator_grants: u64,
    /// Authority grants.
    pub grants: u64,
    /// Issuance grants.
    pub issuance_grants: u64,
    /// Identities with imported credentials.
    pub credentials: u64,
}

impl SpecCounts {
    pub(crate) fn of(&mut self, s: Section) -> &mut u64 {
        match s {
            Section::Identities => &mut self.identities,
            Section::Accounts => &mut self.accounts,
            Section::Aliases => &mut self.aliases,
            Section::Openings => &mut self.openings,
            Section::OperatorGrants => &mut self.operator_grants,
            Section::Grants => &mut self.grants,
            Section::IssuanceGrants => &mut self.issuance_grants,
            Section::Credentials => &mut self.credentials,
        }
    }

    /// The count of `s`.
    pub fn get(&self, s: Section) -> u64 {
        let mut c = *self;
        *c.of(s)
    }

    /// Every record.
    pub fn total(&self) -> u64 {
        self.identities
            + self.accounts
            + self.aliases
            + self.openings
            + self.operator_grants
            + self.grants
            + self.issuance_grants
            + self.credentials
    }
}

/// The opening set's commitment in the header (`F-014`, `REQ-RC-4`).
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct OpeningCommit {
    /// The digest of the opening set: the seed's opening rows and its aliases as the source map
    /// ([`OpeningSet::digest`]).
    pub set_digest: [u8; 32],
    /// The digest of its reconciliation against every required source
    /// ([`Reconciliation::digest`]).
    pub reconciliation_digest: [u8; 32],
    /// The set's total per asset, in minor units; every asset of the set, zero totals included.
    pub totals: BTreeMap<AssetId, u128>,
}

/// The code a chainspec's genesis is built and run under (amendment 0023, PR #201: "the
/// chainspec's header names the genesis code version *H₀* and the construction"): the whole
/// [`CodeManifest`], so a host checks its binaries against the header alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GenesisCode {
    /// The manifest; *H₀* is its [`CodeManifest::version`].
    pub manifest: CodeManifest,
    /// The construction of the state commitment the genesis is built in (`DEC-25`): one of the
    /// manifest's.
    pub construction: u8,
}

impl GenesisCode {
    /// *H₀*.
    pub fn version(&self) -> [u8; 32] {
        self.manifest.version()
    }
}

/// Where the settlement contract stands when the chainspec is built (amendment 0022, option (e)):
/// read from its `head()`, so that a re-founded book continues its anchor sequence. Carried and
/// committed; the anchor store and the adapter read it in `T-036-15`/`T-036-16`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SettlementBase {
    /// The contract instance.
    pub instance: u32,
    /// Its last anchor's number.
    pub last_anchor: u64,
    /// Its inbox's next sequence number.
    pub next_seq: u64,
}

/// The seed's first chunk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpecHeader {
    /// [`SPEC_VERSION`].
    pub version: u16,
    /// The operation reference of the genesis the seed is for.
    pub genesis_op: OperationId,
    /// The time every seeded command carries.
    pub at: Time,
    /// The registry, exactly genesis's.
    pub assets: Vec<AssetDecl>,
    /// Each section's count.
    pub counts: SpecCounts,
    /// The opening set, when the seed has opening rows.
    pub opening: Option<OpeningCommit>,
    /// The pre-cutover history (`seed-history-commitment`).
    pub history: Option<HistoryCommit>,
    /// The genesis code (format 2): what builds and runs this genesis. A chainspec without it names
    /// no code, and the core host refuses it unless told the code is not checked (tests, stress).
    pub code: Option<GenesisCode>,
    /// The settlement contract's head (format 2).
    pub settlement: Option<SettlementBase>,
    /// The SHA-256 of the identifier map `migrate chainspec build` writes beside the chainspec
    /// (`ids.jsonl`: each legacy identifier's new identity or account), so a consumer checks the
    /// file it imports against the book's genesis (format 2).
    pub ids_sha256: Option<[u8; 32]>,
    /// The SHA-256 of the merge map (`merges.jsonl`, `aledgent/merges/v1`): each empty legacy
    /// account absorbed into its identity's default account (ruling `au-8`, format 3).
    pub merges_sha256: Option<[u8; 32]>,
    /// The SHA-256 of the exclusion file (`excluded.jsonl`, `aledgent/excluded/v1`): each legacy
    /// identifier deliberately left out of the book (ruling `au-19`, format 3).
    pub excluded_sha256: Option<[u8; 32]>,
    /// The SHA-256 of the reconciliation journal (`reconcile.jsonl`, `aledgent/reconcile/v1`):
    /// each chain-bound asset's books, the postings that take them to the chain with their
    /// evidence, and the chain's balances at the window block (ruling `chain-balance-authority`,
    /// format 4).
    pub reconcile_sha256: Option<[u8; 32]>,
}

/// One record (module documentation). Its `Debug` names its section only: a verifier has no place
/// in a log (`REQ-AU-8`).
#[derive(Clone, PartialEq, Eq)]
pub enum SpecRecord {
    /// An identity, opened by the operator with a `key` verifier the operator holds.
    Identity {
        /// Its kind.
        kind: IdentityKind,
        /// Its first credential's verifier: public material only (`REQ-AU-8`).
        verifier: Verifier,
    },
    /// A ledger account in delegated custody.
    Account {
        /// The owning identity: a seeded identity, or the operator.
        owner: IdentityId,
        /// Its kind (never `Issuance`).
        kind: AccountKind,
    },
    /// An alias: a prior system's reference for a ledger account (`INV-17`: no personal data).
    Alias {
        /// The source whose reference it is.
        source: OpeningSource,
        /// The reference, as that source writes it.
        reference: Vec<u8>,
        /// The account.
        account: AccountId,
    },
    /// An opening row.
    Opening(OpeningRow),
    /// An operator-surface grant to the operator itself.
    OperatorGrant {
        /// What it covers.
        kinds: NonEmptySet<OperatorOpKind>,
        /// When it stops working.
        expires: Option<Time>,
        /// Its justification.
        justification: Vec<u8>,
    },
    /// An authority grant by the operator.
    Grant(GrantRequest),
    /// An issuance grant.
    IssuanceGrant {
        /// The grant.
        request: GrantRequest,
        /// Its justification.
        justification: Vec<u8>,
    },
    /// An identity's existing credentials, imported into its set beside the operator-held key
    /// (`REQ-MG-7`, `REQ-AU-20`): applied by `import_credentials`, each marked imported, with
    /// access to all of the identity's accounts. For the migration, its passkeys
    /// (`webauthn-es256`), from the environment's own extract (ruling `mn7`).
    Credentials {
        /// A seeded identity.
        identity: IdentityId,
        /// The verifiers: public material only (`REQ-AU-8`), at most `MAX_IMPORTED_CREDENTIALS`.
        credentials: Vec<Verifier>,
    },
}

/// The justification every seeded import carries (`REQ-OS-1`).
pub const IMPORT_JUSTIFICATION: &[u8] =
    b"chainspec genesis: the identity's existing credentials (REQ-MG-7)";

impl core::fmt::Debug for SpecRecord {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "SpecRecord({:?})", self.section())
    }
}

impl SpecRecord {
    /// Its section.
    pub fn section(&self) -> Section {
        match self {
            SpecRecord::Identity { .. } => Section::Identities,
            SpecRecord::Account { .. } => Section::Accounts,
            SpecRecord::Alias { .. } => Section::Aliases,
            SpecRecord::Opening(_) => Section::Openings,
            SpecRecord::OperatorGrant { .. } => Section::OperatorGrants,
            SpecRecord::Grant(_) => Section::Grants,
            SpecRecord::IssuanceGrant { .. } => Section::IssuanceGrants,
            SpecRecord::Credentials { .. } => Section::Credentials,
        }
    }
}

/// What a chunk carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChunkBody {
    /// Chunk 0.
    Header(Box<SpecHeader>),
    /// Every later chunk.
    Records(Vec<SpecRecord>),
}

/// One chunk: its index and its body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpecChunk {
    /// Its index; the header is 0.
    pub index: u32,
    /// Its body.
    pub body: ChunkBody,
}

impl SpecChunk {
    /// Its canonical bytes: what travels, and what its leaf is over.
    pub fn to_bytes(&self) -> Vec<u8> {
        canon::to_bytes(self)
    }

    /// A chunk from its canonical bytes.
    pub fn from_bytes(b: &[u8]) -> Result<SpecChunk, DecodeError> {
        canon::from_bytes(b)
    }
}

/// The leaf of a chunk, from its canonical bytes and its index.
pub fn chunk_leaf(index: u32, bytes: &[u8]) -> [u8; 32] {
    blake2b_256(CHUNK_DOMAIN, &[&index.to_be_bytes(), bytes])
}

/// The seed digest of a tree of `chunks` leaves with this root.
pub fn spec_digest(chunks: u32, root: &[u8; 32]) -> [u8; 32] {
    blake2b_256(
        SEED_DOMAIN,
        &[&SPEC_VERSION.to_be_bytes(), &chunks.to_be_bytes(), root],
    )
}

/// Whether the chunk with these canonical bytes is chunk `index` of the committed seed, through
/// `path` (module documentation).
pub fn chunk_in_spec(
    commitment: &SpecCommitment,
    index: u32,
    bytes: &[u8],
    path: &[[u8; 32]],
) -> bool {
    merkle::fold(
        NODE_DOMAIN,
        chunk_leaf(index, bytes),
        u64::from(index),
        u64::from(commitment.chunks),
        path,
    )
    .is_some_and(|root| spec_digest(commitment.chunks, &root) == commitment.digest)
}

/// The header of a committed seed, checked against genesis's commitment: what a history proof is
/// checked against, by anyone holding the seed's first chunk and its path.
pub fn verify_header(
    commitment: &SpecCommitment,
    header_chunk: &[u8],
    path: &[[u8; 32]],
) -> Result<SpecHeader, SpecError> {
    if !chunk_in_spec(commitment, 0, header_chunk, path) {
        return Err(SpecError::NotInSeed);
    }
    match SpecChunk::from_bytes(header_chunk).map_err(SpecError::Decode)? {
        SpecChunk {
            index: 0,
            body: ChunkBody::Header(h),
        } => Ok(*h),
        _ => Err(SpecError::Order),
    }
}

/// The operation reference of record `ordinal` of `section`, in the book founded by
/// `genesis_op`: `blake2b-256("aledgent/chainspec/op/v1" ‖ genesis op ‖ section ‖ ordinal)`, first 16
/// bytes.
pub fn spec_op(genesis_op: &OperationId, section: u8, ordinal: u64) -> OperationId {
    let d = blake2b_256(
        OP_DOMAIN,
        &[genesis_op.as_bytes(), &[section], &ordinal.to_be_bytes()],
    );
    let mut b = [0u8; 16];
    b.copy_from_slice(&d[..16]);
    OperationId::from_ref(OperationRef(b))
}

/// The reference of chunk `index`'s own record.
pub fn chunk_op(genesis_op: &OperationId, index: u32) -> OperationId {
    spec_op(genesis_op, CHUNK_SECTION, u64::from(index))
}

/// The reference every opening of the seed is posted under: one for the whole set, as one load's.
pub fn opening_op(genesis_op: &OperationId) -> OperationId {
    spec_op(genesis_op, Section::Openings as u8, 0)
}

/// The operator's identity in the book founded by `genesis_op` (genesis mints it first).
pub fn operator_of(genesis_op: &OperationId) -> IdentityId {
    IdentityId::mint(genesis_op, 0)
}

/// The operator's first credential in that book.
pub fn operator_credential_of(genesis_op: &OperationId) -> CredentialId {
    CredentialId::mint(genesis_op, 1)
}

/// The identity seeded identity `ordinal` becomes.
pub fn identity_of(genesis_op: &OperationId, ordinal: u64) -> IdentityId {
    IdentityId::mint(&spec_op(genesis_op, Section::Identities as u8, ordinal), 0)
}

/// The first credential of seeded identity `ordinal`: what a seeded principal (a service, `T-036-30`)
/// authenticates as.
pub fn identity_credential_of(genesis_op: &OperationId, ordinal: u64) -> CredentialId {
    CredentialId::mint(&spec_op(genesis_op, Section::Identities as u8, ordinal), 1)
}

/// The ledger account seeded account `ordinal` becomes.
pub fn account_of(genesis_op: &OperationId, ordinal: u64) -> AccountId {
    AccountId::mint(&spec_op(genesis_op, Section::Accounts as u8, ordinal), 0)
}

/// The grant seeded grant `ordinal` of `section` becomes.
pub fn grant_of(genesis_op: &OperationId, section: Section, ordinal: u64) -> GrantId {
    GrantId::mint(&spec_op(genesis_op, section as u8, ordinal), 0)
}

/// An alias's key in the book: `blake2b-256("aledgent/alias/v1" ‖ source ‖ reference)`. The book
/// keeps the key, never the reference.
pub fn alias_key(source: &OpeningSource, reference: &[u8]) -> [u8; 32] {
    blake2b_256(ALIAS_DOMAIN, &[&source.0, reference])
}

/// How a seed is cut into chunks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkLimits {
    /// The most records one chunk carries.
    pub max_records: u32,
    /// The most bytes one chunk's canonical encoding takes.
    pub max_bytes: u32,
    /// The deployment's batch of openings (`max_legs`): a chunk of openings ends only where a
    /// batch does, so the postings are linked exactly as one load of the same set links them.
    pub opening_batch: u32,
}

impl ChunkLimits {
    /// The defaults: what one input of the core Agent holds at a large book's volume. The guest's
    /// 256 KiB heap binds first: a chunk's witness brings every leaf its records create, about
    /// 20 KB of heap a record at a book of about a hundred thousand identities, so six records
    /// (`core-binding/tests/heap.rs`). A guest built with `heap-4m`
    /// takes up to about 28, where the 192 KiB of caller blobs one invocation carries binds: set
    /// `max_records` to 24 for it.
    pub const DEFAULT: ChunkLimits = ChunkLimits {
        max_records: 6,
        max_bytes: 11 * 1024,
        opening_batch: 10,
    };
}

impl Default for ChunkLimits {
    fn default() -> Self {
        ChunkLimits::DEFAULT
    }
}

/// Why a seed did not build, decode or check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpecError {
    /// Bytes did not decode.
    Decode(DecodeError),
    /// Another version.
    Version,
    /// A section out of order, or a record out of order within its section.
    Order,
    /// A count that is not the records'.
    Counts,
    /// An account record of the `Issuance` kind.
    IssuanceAccount,
    /// Two aliases with one key, or two opening rows for one pair.
    Duplicate,
    /// The opening commitment is not the seed's rows and aliases, or is missing or extra.
    OpeningCommit,
    /// The reconciliation is missing, or is not the one the header commits.
    Reconciliation,
    /// The opening set does not reconcile with its sources (`F-014`'s gate): the findings.
    Divergent(Vec<crate::opening::Divergence>),
    /// The header's registry does not build.
    Registry,
    /// A chunk is not in the committed seed.
    NotInSeed,
    /// The limits cannot hold one record.
    Limits,
    /// The genesis code's construction is not one its manifest reads, or its generation is not 0.
    Code,
    /// An imported credential set that is empty, too large, repeats a verifier or holds one not
    /// well formed.
    Credentials,
}

/// A whole seed: what `migrate seed build` writes and an operator applies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainSpec {
    header: SpecHeader,
    records: Vec<SpecRecord>,
    /// Records per chunk after the header, in order.
    cuts: Vec<u32>,
    /// The opening set's reconciliation, when it has openings: kept with the seed so that anyone
    /// with the seed re-runs the gate. It never crosses into the book.
    reconciliation: Option<Reconciliation>,
}

impl ChainSpec {
    /// Build a seed: check the records' order, count them, commit the opening set and cut the
    /// chunks. `reconciliation` is required when there are opening rows.
    pub fn build(
        genesis_op: OperationId,
        at: Time,
        assets: Vec<AssetDecl>,
        records: Vec<SpecRecord>,
        reconciliation: Option<Reconciliation>,
        history: Option<HistoryCommit>,
        limits: ChunkLimits,
    ) -> Result<ChainSpec, SpecError> {
        let counts = check_order(&records)?;
        let opening = match opening_set(&records)? {
            None => None,
            Some(set) => {
                let rec = reconciliation.as_ref().ok_or(SpecError::Reconciliation)?;
                Some(OpeningCommit {
                    set_digest: set.digest(),
                    reconciliation_digest: rec.digest(),
                    totals: set.totals(),
                })
            }
        };
        let header = SpecHeader {
            version: SPEC_VERSION,
            genesis_op,
            at,
            assets,
            counts,
            opening,
            history,
            code: None,
            settlement: None,
            ids_sha256: None,
            merges_sha256: None,
            excluded_sha256: None,
            reconcile_sha256: None,
        };
        registry_of(&header.assets)?;
        let cuts = cut(&records, limits)?;
        let reconciliation = if header.opening.is_some() {
            reconciliation
        } else {
            None
        };
        let seed = ChainSpec {
            header,
            records,
            cuts,
            reconciliation,
        };
        seed.check()?;
        Ok(seed)
    }

    /// The header.
    pub fn header(&self) -> &SpecHeader {
        &self.header
    }

    /// The same seed naming its genesis code (format 2). The construction must be one the manifest
    /// reads.
    pub fn with_code(mut self, code: GenesisCode) -> Result<ChainSpec, SpecError> {
        self.header.code = Some(code);
        self.check()?;
        Ok(self)
    }

    /// The same seed committing its identifier map's SHA-256 (format 2).
    pub fn with_ids(mut self, sha256: [u8; 32]) -> ChainSpec {
        self.header.ids_sha256 = Some(sha256);
        self
    }

    /// The same seed committing its merge map's SHA-256 (format 3, ruling `au-8`).
    pub fn with_merges(mut self, sha256: [u8; 32]) -> ChainSpec {
        self.header.merges_sha256 = Some(sha256);
        self
    }

    /// The same seed committing its exclusion file's SHA-256 (format 3, ruling `au-19`).
    pub fn with_excluded(mut self, sha256: [u8; 32]) -> ChainSpec {
        self.header.excluded_sha256 = Some(sha256);
        self
    }

    /// The same seed committing its reconciliation journal's SHA-256 (format 4, ruling
    /// `chain-balance-authority`).
    pub fn with_reconcile(mut self, sha256: [u8; 32]) -> ChainSpec {
        self.header.reconcile_sha256 = Some(sha256);
        self
    }

    /// The same seed with the settlement contract's head (format 2).
    pub fn with_settlement(mut self, base: SettlementBase) -> ChainSpec {
        self.header.settlement = Some(base);
        self
    }

    /// The records, in order.
    pub fn records(&self) -> &[SpecRecord] {
        &self.records
    }

    /// The opening set's reconciliation, if the seed has openings.
    pub fn reconciliation(&self) -> Option<&Reconciliation> {
        self.reconciliation.as_ref()
    }

    /// How many chunks, the header included.
    pub fn chunk_count(&self) -> u32 {
        self.cuts.len() as u32 + 1
    }

    /// Chunk `index`.
    pub fn chunk(&self, index: u32) -> Option<SpecChunk> {
        if index == 0 {
            return Some(SpecChunk {
                index: 0,
                body: ChunkBody::Header(Box::new(self.header.clone())),
            });
        }
        let i = usize::try_from(index - 1).ok()?;
        let n = *self.cuts.get(i)? as usize;
        let start: usize = self.cuts[..i].iter().map(|c| *c as usize).sum();
        Some(SpecChunk {
            index,
            body: ChunkBody::Records(self.records[start..start + n].to_vec()),
        })
    }

    /// Every chunk's canonical bytes, in order (one pass over the records).
    pub fn chunk_bytes(&self) -> Vec<Vec<u8>> {
        let mut out = Vec::with_capacity(self.cuts.len() + 1);
        out.push(
            SpecChunk {
                index: 0,
                body: ChunkBody::Header(Box::new(self.header.clone())),
            }
            .to_bytes(),
        );
        let mut start = 0usize;
        for (i, n) in self.cuts.iter().enumerate() {
            let n = *n as usize;
            out.push(
                SpecChunk {
                    index: i as u32 + 1,
                    body: ChunkBody::Records(self.records[start..start + n].to_vec()),
                }
                .to_bytes(),
            );
            start += n;
        }
        out
    }

    fn leaves(&self) -> Vec<[u8; 32]> {
        self.chunk_bytes()
            .iter()
            .enumerate()
            .map(|(i, b)| chunk_leaf(i as u32, b))
            .collect()
    }

    /// The seed digest.
    pub fn digest(&self) -> [u8; 32] {
        let root = merkle::root(NODE_DOMAIN, &self.leaves()).unwrap_or([0; 32]);
        spec_digest(self.chunk_count(), &root)
    }

    /// What genesis carries.
    pub fn commitment(&self) -> SpecCommitment {
        SpecCommitment {
            digest: self.digest(),
            chunks: self.chunk_count(),
        }
    }

    /// Every chunk's path, in order (the tree built once).
    pub fn paths(&self) -> Vec<Vec<[u8; 32]>> {
        merkle::paths(NODE_DOMAIN, &self.leaves())
    }

    /// Every chunk, in order, with its index, canonical bytes and path, one at a time: memory of
    /// the tree's nodes (64 bytes a chunk) and one chunk, never every chunk's bytes or path at once
    /// (`F-036`: the native genesis of a large chainspec). Stops at the first error `f` returns.
    pub fn try_for_each_chunk<E>(
        &self,
        mut f: impl FnMut(u32, &[u8], &[[u8; 32]]) -> Result<(), E>,
    ) -> Result<(), E> {
        let mut leaves = Vec::with_capacity(self.cuts.len() + 1);
        self.visit_chunks(&mut |i, b| leaves.push(chunk_leaf(i, b)));
        let levels = merkle::levels(NODE_DOMAIN, leaves);
        let mut out = Ok(());
        self.visit_chunks(&mut |i, b| {
            if out.is_ok() {
                out = f(i, b, &merkle::path_in(&levels, i as usize));
            }
        });
        out
    }

    /// Each chunk's index and canonical bytes, built one at a time.
    fn visit_chunks(&self, f: &mut dyn FnMut(u32, &[u8])) {
        let header = SpecChunk {
            index: 0,
            body: ChunkBody::Header(Box::new(self.header.clone())),
        };
        f(0, &header.to_bytes());
        let mut start = 0usize;
        for (i, n) in self.cuts.iter().enumerate() {
            let n = *n as usize;
            let c = SpecChunk {
                index: i as u32 + 1,
                body: ChunkBody::Records(self.records[start..start + n].to_vec()),
            };
            f(i as u32 + 1, &c.to_bytes());
            start += n;
        }
    }

    /// The opening set: the rows and the aliases as its source map.
    pub fn opening_set(&self) -> Result<Option<OpeningSet>, SpecError> {
        opening_set(&self.records)
    }

    /// The seed's own consistency: version, order, counts, the opening commitment against the
    /// rows and aliases, and the reconciliation against its digest.
    pub fn check(&self) -> Result<(), SpecError> {
        if self.header.version != SPEC_VERSION {
            return Err(SpecError::Version);
        }
        // The genesis code is *H₀*: generation 0 (amendment 0033, D1), in a construction it reads.
        if let Some(c) = &self.header.code
            && (!c.manifest.constructions.contains(&c.construction) || c.manifest.generation != 0)
        {
            return Err(SpecError::Code);
        }
        if check_order(&self.records)? != self.header.counts {
            return Err(SpecError::Counts);
        }
        if self.cuts.iter().map(|c| u64::from(*c)).sum::<u64>() != self.records.len() as u64
            || self.cuts.contains(&0)
        {
            return Err(SpecError::Counts);
        }
        match (opening_set(&self.records)?, &self.header.opening) {
            (None, None) => {}
            (Some(set), Some(c)) => {
                if set.digest() != c.set_digest || set.totals() != c.totals {
                    return Err(SpecError::OpeningCommit);
                }
                match &self.reconciliation {
                    Some(r) if r.digest() == c.reconciliation_digest => {}
                    _ => return Err(SpecError::Reconciliation),
                }
            }
            _ => return Err(SpecError::OpeningCommit),
        }
        Ok(())
    }

    /// `F-014`'s zero-divergence gate over the seed's opening set (`REQ-RC-4`): the set against its reconciliation, the deployment's required sources and the
    /// header's registry. No override. A seed without openings passes.
    pub fn gate(&self, required: &RequiredSources) -> Result<(), SpecError> {
        self.check()?;
        let Some(set) = opening_set(&self.records)? else {
            return Ok(());
        };
        let rec = self
            .reconciliation
            .as_ref()
            .ok_or(SpecError::Reconciliation)?;
        let registry = registry_of(&self.header.assets)?;
        crate::opening::reconcile(&set, rec, required, &registry).map_err(SpecError::Divergent)
    }

    /// The seed file: a magic, then the header, the records, the cuts and the reconciliation,
    /// canonically.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.raw(FILE_MAGIC)
            .put(&self.header)
            .put(&self.records)
            .put(&self.cuts)
            .put(&self.reconciliation);
        e.into_bytes()
    }

    /// A seed file, checked ([`ChainSpec::check`]).
    pub fn from_bytes(b: &[u8]) -> Result<ChainSpec, SpecError> {
        let rest = b
            .strip_prefix(FILE_MAGIC.as_slice())
            .ok_or(SpecError::Decode(DecodeError("not a seed file")))?;
        let mut d = Dec::new(rest);
        let seed = ChainSpec {
            header: d.get().map_err(SpecError::Decode)?,
            records: d.get().map_err(SpecError::Decode)?,
            cuts: d.get().map_err(SpecError::Decode)?,
            reconciliation: d.get().map_err(SpecError::Decode)?,
        };
        d.finish().map_err(SpecError::Decode)?;
        seed.check()?;
        Ok(seed)
    }
}

/// The registry genesis builds from `assets`, through the same moves.
pub fn registry_of(assets: &[AssetDecl]) -> Result<AssetRegistry, SpecError> {
    let mut r = AssetRegistry::new();
    for a in assets {
        r.register(a.id, a.precision, a.kind)
            .map_err(|_| SpecError::Registry)?;
        if let Some(l) = a.location {
            r.set_binding(&a.id, AssetBinding::Binding(l))
                .map_err(|_| SpecError::Registry)?;
            r.set_binding(&a.id, AssetBinding::Bound(l))
                .map_err(|_| SpecError::Registry)?;
        }
        if a.deprecated {
            r.deprecate(&a.id).map_err(|_| SpecError::Registry)?;
        }
    }
    Ok(r)
}

/// Check the sections' order and each section's own order; count the records.
fn check_order(records: &[SpecRecord]) -> Result<SpecCounts, SpecError> {
    let mut counts = SpecCounts::default();
    let mut last: Option<&SpecRecord> = None;
    for r in records {
        if let Some(p) = last {
            match p.section().cmp(&r.section()) {
                core::cmp::Ordering::Greater => return Err(SpecError::Order),
                core::cmp::Ordering::Less => {}
                core::cmp::Ordering::Equal => match (p, r) {
                    (
                        SpecRecord::Alias {
                            source: a,
                            reference: ra,
                            ..
                        },
                        SpecRecord::Alias {
                            source: b,
                            reference: rb,
                            ..
                        },
                    ) => match (a, ra).cmp(&(b, rb)) {
                        core::cmp::Ordering::Less => {}
                        core::cmp::Ordering::Equal => return Err(SpecError::Duplicate),
                        core::cmp::Ordering::Greater => return Err(SpecError::Order),
                    },
                    (
                        SpecRecord::Credentials { identity: a, .. },
                        SpecRecord::Credentials { identity: b, .. },
                    ) => match a.cmp(b) {
                        core::cmp::Ordering::Less => {}
                        core::cmp::Ordering::Equal => return Err(SpecError::Duplicate),
                        core::cmp::Ordering::Greater => return Err(SpecError::Order),
                    },
                    (SpecRecord::Opening(a), SpecRecord::Opening(b)) => {
                        match (a.account, a.asset).cmp(&(b.account, b.asset)) {
                            core::cmp::Ordering::Less => {}
                            core::cmp::Ordering::Equal => return Err(SpecError::Duplicate),
                            core::cmp::Ordering::Greater => return Err(SpecError::Order),
                        }
                    }
                    _ => {}
                },
            }
        }
        if let SpecRecord::Account {
            kind: AccountKind::Issuance,
            ..
        } = r
        {
            return Err(SpecError::IssuanceAccount);
        }
        if let SpecRecord::Credentials { credentials, .. } = r
            && (credentials.is_empty()
                || credentials.len() > crate::ledger::custody::MAX_IMPORTED_CREDENTIALS
                || !credentials.iter().all(Verifier::well_formed)
                || credentials
                    .iter()
                    .enumerate()
                    .any(|(i, c)| credentials[..i].contains(c)))
        {
            return Err(SpecError::Credentials);
        }
        *counts.of(r.section()) += 1;
        last = Some(r);
    }
    Ok(counts)
}

/// The opening set of `records`, or `None` without opening rows.
fn opening_set(records: &[SpecRecord]) -> Result<Option<OpeningSet>, SpecError> {
    let mut rows = Vec::new();
    let mut map = BTreeMap::new();
    for r in records {
        match r {
            SpecRecord::Opening(row) => rows.push(*row),
            SpecRecord::Alias {
                source,
                reference,
                account,
            } => {
                map.insert((*source, reference.clone()), *account);
            }
            _ => {}
        }
    }
    if rows.is_empty() {
        return Ok(None);
    }
    OpeningSet::new(rows, map)
        .map(Some)
        .map_err(|_| SpecError::Duplicate)
}

/// The encoded size of one record in a chunk.
fn record_bytes(r: &SpecRecord) -> u32 {
    u32::try_from(canon::to_bytes(r).len()).unwrap_or(u32::MAX)
}

/// Cut `records` into chunks within `limits` (`ChunkLimits::opening_batch` keeps opening batches
/// whole).
fn cut(records: &[SpecRecord], limits: ChunkLimits) -> Result<Vec<u32>, SpecError> {
    // A chunk's fixed part: the index, the body's tag and the records' count.
    const FIXED: u32 = 4 + 1 + 4;
    if limits.max_records == 0 || limits.max_bytes <= FIXED {
        return Err(SpecError::Limits);
    }
    let batch = u64::from(limits.opening_batch.max(1));
    let mut cuts = Vec::new();
    let (mut n, mut bytes) = (0u32, FIXED);
    // Where the current chunk may end without splitting an opening batch: records in it so far.
    let mut aligned = 0u32;
    let mut posted = 0u64;
    let mut i = 0usize;
    while i < records.len() {
        let r = &records[i];
        let b = record_bytes(r);
        if FIXED + b > limits.max_bytes {
            return Err(SpecError::Limits);
        }
        if n > 0 && (n + 1 > limits.max_records || bytes + b > limits.max_bytes) {
            // Close the chunk; with openings in it, at the last batch boundary if there is one.
            let close = if aligned > 0 { aligned } else { n };
            cuts.push(close);
            let back = (n - close) as usize;
            i -= back;
            // Re-count the non-zero openings the rewound records had counted.
            for r in &records[i..i + back] {
                if let SpecRecord::Opening(row) = r
                    && row.amount.get() != 0
                {
                    posted -= 1;
                }
            }
            n = 0;
            bytes = FIXED;
            aligned = 0;
            continue;
        }
        n += 1;
        bytes += b;
        if let SpecRecord::Opening(row) = r
            && row.amount.get() != 0
        {
            posted += 1;
        }
        // Aligned: after any record that is not an opening, or after an opening that ends a batch
        // (and before the next non-zero one).
        let next_is_opening = matches!(records.get(i + 1), Some(SpecRecord::Opening(_)));
        let is_opening = matches!(r, SpecRecord::Opening(_));
        if !is_opening || !next_is_opening || posted % batch == 0 {
            aligned = n;
        }
        i += 1;
    }
    if n > 0 {
        cuts.push(n);
    }
    Ok(cuts)
}

// ------------------------------------------------------------------ the book's record

/// What the book keeps of its seed: the commitment genesis took, and how far applying it got.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct SpecProgress {
    /// What genesis committed.
    pub commitment: SpecCommitment,
    /// Chunks applied: the next chunk's index.
    pub applied: u32,
    /// Whether the last chunk was applied and its checks held.
    pub sealed: bool,
    /// What the header said, once applied.
    pub header: Option<HeaderFacts>,
    /// Records applied so far, per section.
    pub seen: SpecCounts,
    /// The last section a record was applied from.
    pub section: u8,
    /// Non-zero opening rows posted so far.
    pub opening_posted: u64,
}

/// What the book keeps of the header.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeaderFacts {
    /// The genesis it is for.
    pub genesis_op: OperationId,
    /// The seeded commands' time.
    pub at: Time,
    /// The counts.
    pub counts: SpecCounts,
    /// The opening set's commitment.
    pub opening: Option<OpeningCommit>,
    /// The history's commitment: stored, never read by the book.
    pub history: Option<HistoryCommit>,
    /// The genesis code version *H₀* and construction, when the header names them.
    pub code: Option<([u8; 32], u8)>,
    /// The settlement contract's head, when the header carries it.
    pub settlement: Option<SettlementBase>,
}

impl SpecProgress {
    /// Whether the book refuses everything but its seeding.
    pub fn open(&self) -> bool {
        !self.sealed
    }
}

/// A change of the seed's record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpecWrite {
    /// Genesis commits a seed.
    Commit(SpecCommitment),
    /// A chunk was applied: the progress after it.
    Progress(Box<SpecProgress>),
    /// An alias.
    Alias {
        /// Its key ([`alias_key`]).
        key: [u8; 32],
        /// The account.
        account: AccountId,
    },
}

/// Why the seed's record refused a write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpecRefusal {
    /// A second commitment, or one on a book that has no seed.
    Commitment,
    /// Progress that does not follow the last: not the next chunk, another commitment, or after
    /// the seal.
    Progress,
    /// An alias key held already.
    Alias,
}

/// How to undo one change.
#[derive(Clone, Debug)]
pub(crate) enum SpecUndo {
    Progress(Option<Box<SpecProgress>>),
    Alias([u8; 32]),
}

/// The seed's record in the service store: its progress (one leaf) and the aliases (one leaf
/// each).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpecState {
    progress: Slot<Option<SpecProgress>, { kind::CHAINSPEC }>,
    aliases: Leaves<[u8; 32], AccountId, { kind::SPEC_ALIAS }>,
}

impl LeafHost for SpecState {
    fn leaf_maps<'a>(&'a self, out: &mut Vec<&'a dyn LeafMap>) {
        out.push(&self.progress);
        out.push(&self.aliases);
    }
    fn leaf_maps_mut<'a>(&'a mut self, out: &mut Vec<&'a mut dyn LeafMap>) {
        out.push(&mut self.progress);
        out.push(&mut self.aliases);
    }
}

impl SpecState {
    /// No seed.
    pub fn new() -> SpecState {
        SpecState::default()
    }

    /// The progress, if genesis committed a seed.
    pub fn progress(&self) -> Option<&SpecProgress> {
        self.progress.get().as_ref()
    }

    /// The account an alias names.
    pub fn alias(&self, key: &[u8; 32]) -> Option<AccountId> {
        self.aliases.get(key).copied()
    }

    /// Every alias, by key.
    pub fn aliases(&self) -> impl Iterator<Item = (&[u8; 32], &AccountId)> {
        self.aliases.iter()
    }

    pub(crate) fn apply(&mut self, w: &SpecWrite) -> Result<SpecUndo, SpecRefusal> {
        match w {
            SpecWrite::Commit(c) => {
                if self.progress.get().is_some() {
                    return Err(SpecRefusal::Commitment);
                }
                let before = self.progress.set(Some(SpecProgress {
                    commitment: *c,
                    ..SpecProgress::default()
                }));
                Ok(SpecUndo::Progress(before.map(Box::new)))
            }
            SpecWrite::Progress(p) => {
                let ok = match self.progress.get() {
                    Some(old) => {
                        !old.sealed
                            && old.commitment == p.commitment
                            && p.applied == old.applied + 1
                            && p.applied <= p.commitment.chunks
                            && (!p.sealed || p.applied == p.commitment.chunks)
                    }
                    None => false,
                };
                if !ok {
                    return Err(SpecRefusal::Progress);
                }
                let before = self.progress.set(Some((**p).clone()));
                Ok(SpecUndo::Progress(before.map(Box::new)))
            }
            SpecWrite::Alias { key, account } => {
                if self.aliases.contains_key(key) {
                    return Err(SpecRefusal::Alias);
                }
                self.aliases.insert(*key, *account);
                Ok(SpecUndo::Alias(*key))
            }
        }
    }

    pub(crate) fn undo(&mut self, u: SpecUndo) {
        match u {
            SpecUndo::Progress(before) => {
                self.progress.set(before.map(|b| *b));
            }
            SpecUndo::Alias(key) => {
                self.aliases.remove(&key);
            }
        }
    }

    /// Rebuild from an export's sections (`F-014`).
    pub(crate) fn restore(
        progress: Option<SpecProgress>,
        aliases: BTreeMap<[u8; 32], AccountId>,
    ) -> SpecState {
        let mut s = SpecState::new();
        s.progress.set(progress);
        for (k, v) in aliases {
            s.aliases.insert(k, v);
        }
        s
    }
}

// ------------------------------------------------------------------ canonical forms

fn put_section(e: &mut Enc, s: Section) {
    e.tag(s as u8);
}

fn put_grant_request(e: &mut Enc, g: &GrantRequest) {
    e.put(&g.kind)
        .put(&g.principal)
        .put(&g.subject)
        .put(&g.ops)
        .put(&g.per_operation)
        .put(&g.per_window)
        .put(&g.assets)
        .put(&g.counterparties)
        .put(&g.parent)
        .put(&g.expires);
}

fn get_grant_request(d: &mut Dec<'_>) -> Result<GrantRequest, DecodeError> {
    let kind: GrantKind = d.get()?;
    let principal = d.get()?;
    let subject: Subject = d.get()?;
    let ops: NonEmptySet<OperationKind> = d.get()?;
    let per_operation = d.get()?;
    let per_window: Option<Window> = d.get()?;
    Ok(GrantRequest {
        kind,
        principal,
        subject,
        ops,
        per_operation,
        per_window,
        assets: d.get()?,
        counterparties: d.get()?,
        parent: d.get()?,
        expires: d.get()?,
    })
}

impl Canonical for SpecRecord {
    fn encode(&self, e: &mut Enc) {
        put_section(e, self.section());
        match self {
            SpecRecord::Identity { kind, verifier } => {
                e.put(kind).put(verifier);
            }
            SpecRecord::Account { owner, kind } => {
                e.put(owner).put(kind);
            }
            SpecRecord::Alias {
                source,
                reference,
                account,
            } => {
                e.put(source).bytes(reference).put(account);
            }
            SpecRecord::Opening(row) => {
                e.put(row);
            }
            SpecRecord::OperatorGrant {
                kinds,
                expires,
                justification,
            } => {
                e.put(kinds).put(expires).bytes(justification);
            }
            SpecRecord::Grant(g) => put_grant_request(e, g),
            SpecRecord::IssuanceGrant {
                request,
                justification,
            } => {
                put_grant_request(e, request);
                e.bytes(justification);
            }
            SpecRecord::Credentials {
                identity,
                credentials,
            } => {
                e.put(identity).put(credentials);
            }
        }
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(match d.tag()? {
            1 => SpecRecord::Identity {
                kind: d.get()?,
                verifier: d.get()?,
            },
            2 => SpecRecord::Account {
                owner: d.get()?,
                kind: d.get()?,
            },
            3 => SpecRecord::Alias {
                source: d.get()?,
                reference: d.bytes()?.to_vec(),
                account: d.get()?,
            },
            4 => SpecRecord::Opening(d.get()?),
            5 => SpecRecord::OperatorGrant {
                kinds: d.get()?,
                expires: d.get()?,
                justification: d.bytes()?.to_vec(),
            },
            6 => SpecRecord::Grant(get_grant_request(d)?),
            7 => SpecRecord::IssuanceGrant {
                request: get_grant_request(d)?,
                justification: d.bytes()?.to_vec(),
            },
            8 => SpecRecord::Credentials {
                identity: d.get()?,
                credentials: d.get()?,
            },
            _ => return Err(DecodeError("seed record")),
        })
    }
}

impl Canonical for AssetDecl {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.id)
            .put(&self.location)
            .put(&self.precision)
            .put(&self.kind)
            .put(&self.deprecated);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(AssetDecl {
            id: d.get()?,
            location: d.get()?,
            precision: d.get()?,
            kind: d.get()?,
            deprecated: d.get()?,
        })
    }
}

impl Canonical for SpecCounts {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.identities)
            .put(&self.accounts)
            .put(&self.aliases)
            .put(&self.openings)
            .put(&self.operator_grants)
            .put(&self.grants)
            .put(&self.issuance_grants)
            .put(&self.credentials);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(SpecCounts {
            identities: d.get()?,
            accounts: d.get()?,
            aliases: d.get()?,
            openings: d.get()?,
            operator_grants: d.get()?,
            grants: d.get()?,
            issuance_grants: d.get()?,
            credentials: d.get()?,
        })
    }
}

impl Canonical for OpeningCommit {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.set_digest)
            .put(&self.reconciliation_digest)
            .put(&self.totals);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(OpeningCommit {
            set_digest: d.get()?,
            reconciliation_digest: d.get()?,
            totals: d.get()?,
        })
    }
}

impl Canonical for SpecHeader {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.version)
            .put(&self.genesis_op)
            .put(&self.at)
            .put(&self.assets)
            .put(&self.counts)
            .put(&self.opening)
            .put(&self.history)
            .put(&self.code)
            .put(&self.settlement)
            .put(&self.ids_sha256)
            .put(&self.merges_sha256)
            .put(&self.excluded_sha256)
            .put(&self.reconcile_sha256);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(SpecHeader {
            version: d.get()?,
            genesis_op: d.get()?,
            at: d.get()?,
            assets: d.get()?,
            counts: d.get()?,
            opening: d.get()?,
            history: d.get()?,
            code: d.get()?,
            settlement: d.get()?,
            ids_sha256: d.get()?,
            merges_sha256: d.get()?,
            excluded_sha256: d.get()?,
            reconcile_sha256: d.get()?,
        })
    }
}

impl Canonical for SpecChunk {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.index);
        match &self.body {
            ChunkBody::Header(h) => {
                e.tag(0).put(&**h);
            }
            ChunkBody::Records(r) => {
                e.tag(1).put(r);
            }
        }
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        let index = d.get()?;
        let body = match d.tag()? {
            0 => ChunkBody::Header(Box::new(d.get()?)),
            1 => ChunkBody::Records(d.get()?),
            _ => return Err(DecodeError("seed chunk")),
        };
        Ok(SpecChunk { index, body })
    }
}

impl Canonical for SpecCommitment {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.digest).put(&self.chunks);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(SpecCommitment {
            digest: d.get()?,
            chunks: d.get()?,
        })
    }
}

impl Canonical for HeaderFacts {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.genesis_op)
            .put(&self.at)
            .put(&self.counts)
            .put(&self.opening)
            .put(&self.history)
            .put(&self.code)
            .put(&self.settlement);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(HeaderFacts {
            genesis_op: d.get()?,
            at: d.get()?,
            counts: d.get()?,
            opening: d.get()?,
            history: d.get()?,
            code: d.get()?,
            settlement: d.get()?,
        })
    }
}

impl Canonical for GenesisCode {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.manifest).put(&self.construction);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(GenesisCode {
            manifest: d.get()?,
            construction: d.get()?,
        })
    }
}

impl Canonical for SettlementBase {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.instance)
            .put(&self.last_anchor)
            .put(&self.next_seq);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(SettlementBase {
            instance: d.get()?,
            last_anchor: d.get()?,
            next_seq: d.get()?,
        })
    }
}

impl Canonical for SpecProgress {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.commitment)
            .put(&self.applied)
            .put(&self.sealed)
            .put(&self.header)
            .put(&self.seen)
            .put(&self.section)
            .put(&self.opening_posted);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(SpecProgress {
            commitment: d.get()?,
            applied: d.get()?,
            sealed: d.get()?,
            header: d.get()?,
            seen: d.get()?,
            section: d.get()?,
            opening_posted: d.get()?,
        })
    }
}

#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;
