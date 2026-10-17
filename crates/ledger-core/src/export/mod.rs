// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Export and import of the whole book (`F-014`, `REQ-RC-3`, `REQ-RT-8`, `AC-K5.2`).
//!
//! ## What an export holds
//!
//! Everything needed to rebuild the book and every surface, at one posting sequence `S`:
//!
//! - the kernel's leaves of all six sub-trees, in the kernel's canonical bytes (its rkyv
//!   archives, the bytes its leaves hash), each transfer with its place in the posting sequence;
//! - the service store's records: the registry, identities, ledger accounts, credentials (their
//!   verifiers are public material, `REQ-AU-8`), grants and window counters, attribution records,
//!   the reservation index, operation records with their transitions, the client-key entries,
//!   undelivered notifications, the settlement store, operator records, the posting journal, and
//!   the opening positions and loads (`F-014`);
//! - counters in the [`Manifest`]: the posting sequence, and the head of the audit chain.
//!
//! It holds **no blinding, no opening and no key** (`REQ-RT-6`, `REQ-AU-8`, `NFR-8`): the openings
//! and the core's key root cross by a sealed key-custody step of their own ([`Openings`]), never in
//! these bytes. The audit trail is not in the export either: it is retained apart and stays where
//! it is (`REQ-AT-2`); the manifest names its head so an import can continue the same chain.
//!
//! ## Format (accepted proposal)
//!
//! A magic string, the [`Manifest`], then the sections in the manifest's order. A section is a
//! count and a stream of length-prefixed records in canonical form ([`canon`]), in key order. The
//! manifest holds the format version, the deployment id, `S`, the kernel's composite root, the
//! service root, the audit head, and each section's name, count and digest. The bytes are
//! deterministic: two exports of the same state are identical.
//!
//! The **service root** is the digest of the service sections' digests, in order. The core
//! Agent's service root is another: the root of `F-006`'s service tree, over the same records as
//! leaves with their derived indexes (`crate::leaves`, `F-005`). The manifest keeps the digest,
//! so an export does not depend on the tree's layout, and a book rebuilt from an export holds
//! the same leaves as the original (`tests/export_import.rs`). `TODO(F-014)`: carry the tree's
//! root as well, if an import is ever to be checked against a core Agent's root.

use alloc::string::String;
use alloc::vec::Vec;

use cipher_clerk::crypto::blake2b_256;

pub mod anchor;
pub mod canon;
pub mod compare;
pub mod kernel;
mod records;
pub mod rederive;

pub use anchor::{Anchors, Checkpoints};
pub use canon::{Canonical, Dec, DecodeError, Enc};
pub use kernel::{Leaves, Openings};

/// The format version this build writes and reads. 2: the settlement section carries the
/// settlement adapter's status declaration (`F-016`). 3: the anchor store's six sections, and an
/// observation's anchor fact (`F-017`). 4: an instruction's signing record and nonce fact, and
/// the `IncludedWithoutRecord` incident (the owner's ruling of 2026-09-24 on `F-010`/`F-016`).
/// 5: the venue an anchor lives in, on an observation's anchor fact and on a checkpoint, and the
/// `AnchorRefused` event kind (`F-027`). 6: amendment 0006: the `Reversed` state, the
/// `ApprovedReversal` trigger and the `Adjustment` posting kind withdrawn, the reversal operation
/// kind, and the resolution cases of `REQ-RC-5` (`service.resolutions`). 7: a verifier's scheme,
/// as a tag before its material (`DEC-19`, `F-028`). 8: integrating parties and the principals
/// tied to them (`REQ-ID-7`, amendment 0011: `service.integrating-parties`, `service.party-ties`),
/// and the commands that make them. 9: the latest final anchor in each notification; a final
/// checkpoint's block height (`AC-M2.4`).
/// 10: `DEC-20`: every asset's binding to the settlement layer in the registry; the settlement
/// section's attestations and asset binds (`F-029`). 11: a kernel account's placement in a
/// partition other than its subject's, after its balance rule (`REQ-RT-9`, `F-031`); kernel
/// account ids and transfer ids carry their partition. 12: a checkpoint names its construction and
/// its code binding (the book's genesis, the code version and the upgrade-authority mode; anchor
/// commitment `v3`, `T-037-03`). As for every version, this build reads only its own.
pub const FORMAT_VERSION: u32 = 12;

/// The bytes every export starts with.
pub const MAGIC: &[u8; 16] = b"aledgent-export\0";

const SECTION_DOMAIN: &[u8] = b"aledgent/export/section/v1";
const SERVICE_ROOT_DOMAIN: &[u8] = b"aledgent/export/service-root/v1";

/// The book root: `blake2b-256("aledgent/book-root/v1" ‖ kernel root ‖ service root)`, as
/// `F-006` folds it. What an anchored checkpoint commits to.
pub fn book_root(kernel_root: &[u8; 32], service_root: &[u8; 32]) -> [u8; 32] {
    // The same domain as `F-006`'s (`crate::delta::Roots::book_root`), by root version.
    blake2b_256(
        crate::delta::book_root_domain(),
        &[kernel_root, service_root],
    )
}

/// One section: a name, and its records in canonical form and key order.
#[derive(Clone, PartialEq, Eq)]
pub struct Section {
    name: String,
    count: u64,
    bytes: Vec<u8>,
}

impl core::fmt::Debug for Section {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Section")
            .field("name", &self.name)
            .field("count", &self.count)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

impl Section {
    /// An empty section.
    pub fn new(name: &str) -> Section {
        Section {
            name: String::from(name),
            count: 0,
            bytes: Vec::new(),
        }
    }

    /// Append one record, already in canonical form.
    pub fn push_bytes(&mut self, record: &[u8]) {
        let n = u32::try_from(record.len()).expect("a record is under 4 GiB");
        self.bytes.extend_from_slice(&n.to_be_bytes());
        self.bytes.extend_from_slice(record);
        self.count += 1;
    }

    /// Append one record.
    pub fn push<T: Canonical>(&mut self, record: &T) {
        let mut e = Enc::new();
        record.encode(&mut e);
        self.push_bytes(e.as_bytes());
    }

    /// Its name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// How many records it holds.
    pub fn count(&self) -> u64 {
        self.count
    }

    /// Its records, in order.
    pub fn records(&self) -> Records<'_> {
        Records {
            rest: &self.bytes,
            left: self.count,
        }
    }

    /// Decode every record as a `T`, in order.
    pub fn decode_all<T: Canonical>(&self) -> Result<Vec<T>, DecodeError> {
        let mut out = Vec::with_capacity(usize::try_from(self.count).unwrap_or(0).min(1 << 20));
        for r in self.records() {
            out.push(canon::from_bytes(r?)?);
        }
        Ok(out)
    }

    /// Its digest: `blake2b-256("aledgent/export/section/v1" ‖ name ‖ count ‖ records)`.
    pub fn digest(&self) -> [u8; 32] {
        let mut name = Enc::new();
        name.bytes(self.name.as_bytes());
        blake2b_256(
            SECTION_DOMAIN,
            &[name.as_bytes(), &self.count.to_be_bytes(), &self.bytes],
        )
    }

    /// Its size in bytes, records and their prefixes.
    pub fn size(&self) -> usize {
        self.bytes.len()
    }

    /// Change one byte of the section, for the refusal tests. Never used by the library.
    #[cfg(any(test, feature = "conformance-ports"))]
    pub fn tamper(&mut self, at: usize) {
        if let Some(b) = self.bytes.get_mut(at) {
            *b ^= 0x01;
        }
    }
}

/// The records of a section, in order.
#[derive(Clone, Debug)]
pub struct Records<'a> {
    rest: &'a [u8],
    left: u64,
}

impl<'a> Iterator for Records<'a> {
    type Item = Result<&'a [u8], DecodeError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let mut d = Dec::new(self.rest);
        let r = d.bytes().inspect(|b| {
            self.rest = &self.rest[4 + b.len()..];
        });
        if r.is_err() {
            self.left = 0;
        }
        Some(r)
    }
}

/// The audit chain's head at export (`REQ-AT-2`): the audit trail is not exported, and an import
/// continues the same chain from here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuditHead {
    /// How many records the book has emitted: the next record's sequence number.
    pub emitted: u64,
    /// The hash of the last record emitted (zeroes before the first).
    pub head: [u8; 32],
    /// The last record the audit store acknowledged. An export is taken with nothing unshipped,
    /// so this is the last record emitted.
    pub shipped: Option<(u64, [u8; 32])>,
}

impl Canonical for AuditHead {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.emitted).put(&self.head).put(&self.shipped);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(AuditHead {
            emitted: d.get()?,
            head: d.get()?,
            shipped: d.get()?,
        })
    }
}

/// A section's entry in the manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SectionEntry {
    /// Its name.
    pub name: String,
    /// How many records it holds.
    pub count: u64,
    /// Its digest.
    pub digest: [u8; 32],
}

impl Canonical for SectionEntry {
    fn encode(&self, e: &mut Enc) {
        e.bytes(self.name.as_bytes())
            .put(&self.count)
            .put(&self.digest);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        let name = core::str::from_utf8(d.bytes()?).map_err(|_| DecodeError("section name"))?;
        Ok(SectionEntry {
            name: String::from(name),
            count: d.get()?,
            digest: d.get()?,
        })
    }
}

/// What an export says about itself (accepted proposal of `F-014`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    /// The format version.
    pub version: u32,
    /// The deployment's identity, which operation references are derived under (`F-007` §7).
    pub deployment: [u8; 16],
    /// `S`: the posting sequence the export is taken at. Every posting below it is in the export,
    /// none at or above it.
    pub sequence: u64,
    /// The kernel's composite root at `S`.
    pub kernel_root: [u8; 32],
    /// The service root at `S`: the digest of the service sections.
    pub service_root: [u8; 32],
    /// The audit chain's head.
    pub audit: AuditHead,
    /// Every section, in the order the export holds them.
    pub sections: Vec<SectionEntry>,
}

impl Manifest {
    /// The book root at `S`: what the anchor at `S` commits to.
    pub fn book_root(&self) -> [u8; 32] {
        book_root(&self.kernel_root, &self.service_root)
    }
}

impl Canonical for Manifest {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.version)
            .put(&self.deployment)
            .put(&self.sequence)
            .put(&self.kernel_root)
            .put(&self.service_root)
            .put(&self.audit)
            .put(&self.sections);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Manifest {
            version: d.get()?,
            deployment: d.get()?,
            sequence: d.get()?,
            kernel_root: d.get()?,
            service_root: d.get()?,
            audit: d.get()?,
            sections: d.get()?,
        })
    }
}

/// The service root of a list of service sections: the digest of their names, counts and
/// digests, in order.
pub fn service_root(sections: &[Section]) -> [u8; 32] {
    let mut e = Enc::new();
    for s in sections {
        e.bytes(s.name.as_bytes()).put(&s.count).put(&s.digest());
    }
    blake2b_256(SERVICE_ROOT_DOMAIN, &[e.as_bytes()])
}

/// The names of the kernel's sections, in order.
pub const KERNEL_SECTIONS: [&str; 6] = [
    "kernel.accounts",
    "kernel.transfers",
    "kernel.journals",
    "kernel.external-ids",
    "kernel.voided",
    "kernel.pending",
];

/// A whole export: the manifest and its sections, kernel sections first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Export {
    /// The manifest.
    pub manifest: Manifest,
    /// The sections, in the manifest's order.
    pub sections: Vec<Section>,
}

impl Export {
    /// The kernel's sections.
    pub fn kernel_sections(&self) -> &[Section] {
        let n = KERNEL_SECTIONS.len().min(self.sections.len());
        &self.sections[..n]
    }

    /// The service sections.
    pub fn service_sections(&self) -> &[Section] {
        let n = KERNEL_SECTIONS.len().min(self.sections.len());
        &self.sections[n..]
    }

    /// The export's bytes: the magic, the manifest, then each section as its count and a
    /// length-prefixed stream of records.
    pub fn to_bytes(&self) -> Vec<u8> {
        let manifest = canon::to_bytes(&self.manifest);
        let mut out = Vec::with_capacity(
            MAGIC.len()
                + 4
                + manifest.len()
                + self.sections.iter().map(|s| 16 + s.size()).sum::<usize>(),
        );
        out.extend_from_slice(MAGIC);
        let n = u32::try_from(manifest.len()).expect("a manifest is small");
        out.extend_from_slice(&n.to_be_bytes());
        out.extend_from_slice(&manifest);
        for s in &self.sections {
            out.extend_from_slice(&s.count.to_be_bytes());
            out.extend_from_slice(&(s.bytes.len() as u64).to_be_bytes());
            out.extend_from_slice(&s.bytes);
        }
        out
    }

    /// Read an export's bytes. The layout is checked here; digests and roots are the import's to
    /// check ([`Export::verify`]).
    pub fn from_bytes(bytes: &[u8]) -> Result<Export, DecodeError> {
        let mut d = Dec::new(bytes);
        if d.raw(MAGIC.len())? != MAGIC {
            return Err(DecodeError("not an export"));
        }
        let manifest: Manifest = canon::from_bytes(d.bytes()?)?;
        if manifest.version != FORMAT_VERSION {
            return Err(DecodeError("format version"));
        }
        let mut sections = Vec::with_capacity(manifest.sections.len());
        for entry in &manifest.sections {
            let count: u64 = d.get()?;
            let len: u64 = d.get()?;
            let len = usize::try_from(len).map_err(|_| DecodeError("section length"))?;
            sections.push(Section {
                name: entry.name.clone(),
                count,
                bytes: d.raw(len)?.to_vec(),
            });
        }
        d.finish()?;
        Ok(Export { manifest, sections })
    }

    /// Check the layout against the manifest: the format version, the kernel sections first and in
    /// order, and every section's name, count and digest. Then that the service root is the digest
    /// of the service sections. Nothing is decoded yet.
    pub fn verify(&self) -> Result<(), ExportError> {
        if self.manifest.version != FORMAT_VERSION {
            return Err(ExportError::Format);
        }
        if self.sections.len() != self.manifest.sections.len()
            || self.sections.len() < KERNEL_SECTIONS.len()
        {
            return Err(ExportError::Format);
        }
        for (i, name) in KERNEL_SECTIONS.iter().enumerate() {
            if self.sections[i].name != *name {
                return Err(ExportError::Format);
            }
        }
        for (s, entry) in self.sections.iter().zip(&self.manifest.sections) {
            if s.name != entry.name || s.count != entry.count || s.digest() != entry.digest {
                return Err(ExportError::Digest);
            }
        }
        if service_root(self.service_sections()) != self.manifest.service_root {
            return Err(ExportError::Digest);
        }
        Ok(())
    }

    /// A section by name.
    pub fn section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.name == name)
    }

    /// A section by name, to change it in a refusal test. Never used by the library.
    #[cfg(any(test, feature = "conformance-ports"))]
    pub fn section_mut(&mut self, name: &str) -> Option<&mut Section> {
        self.sections.iter_mut().find(|s| s.name == name)
    }
}

/// Why an export could not be taken, or an import could not be made. An import that fails leaves
/// the target as it found it, except [`ExportError::Partial`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExportError {
    /// The book root at the current sequence is not the anchored one: export runs right after an
    /// anchored checkpoint (`REQ-PR-7`, `INV-19`), and an import checks the anchor too.
    NotAnchored,
    /// The audit outbox holds records the audit store has not acknowledged, or the audit store's
    /// head is not the export's: the trail would not continue.
    AuditNotShipped,
    /// The target deployment is not empty: import never merges.
    NotEmpty,
    /// The bytes are not an export of this format.
    Format,
    /// A section's name, count or digest, or the service root, is not the manifest's.
    Digest,
    /// A section did not decode, or decoded to something the store's rules refuse.
    Decode(DecodeError),
    /// The recomputed kernel or service root is not the manifest's.
    RootMismatch,
    /// The deployment is not the export's: operation references would not be the same.
    Deployment,
    /// The openings do not open the exported commitments, or the key root derives another
    /// journal: the sealed key-custody step did not carry this book's secrets.
    Secrets,
    /// The book's own state disagrees with itself (a stored root, a log): nothing was exported.
    Inconsistent,
    /// The command was refused, with this condition.
    Refused(crate::Refusal),
    /// The store or the vault failed after the import began writing: the target is not empty any
    /// more, and must be discarded.
    Partial,
}

impl From<DecodeError> for ExportError {
    fn from(e: DecodeError) -> Self {
        ExportError::Decode(e)
    }
}
