// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Export and import of the whole book (`F-014` T-014-02, T-014-03, `REQ-RC-3`, `REQ-RT-8`).
//!
//! **Export** reads every fact of the book, bypassing the authority checks of `REQ-PV-1`: it is a
//! privileged read, and goes through the one instrumented mechanism (`REQ-PV-4`), which records
//! the operator and the justification in the audit store before anything is returned. It then
//! reads one snapshot pinned to the current posting sequence `S` — in process, `&mut self` holds
//! everything still — and requires that `S` is anchored with this very book root (`REQ-PR-7`,
//! `INV-19`), so the export can be checked against the public anchor and not only against the
//! exporter's word.
//!
//! **Import** is the only path that writes state outside a command. It is allowed only into an
//! empty deployment, only from a whole export, and only when every root matches. Everything is
//! checked before the first write, so a refused import leaves the target as it was:
//!
//! 1. the target store and vault are empty (`ERR`: [`ExportError::NotEmpty`]);
//! 2. the manifest and every section digest ([`ExportError::Digest`]), the deployment id, and the
//!    anchor at `S`;
//! 3. the leaves decode and their composite root is the manifest's; the service store rebuilds,
//!    and re-encodes to the same service root ([`ExportError::RootMismatch`]);
//! 4. the sealed key-custody step's secrets are this book's: the keys derive its journal, and the
//!    openings open every exported commitment ([`ExportError::Secrets`]);
//! 5. the audit store's head is the export's, so the trail continues ([`ExportError::AuditNotShipped`]);
//! 6. the caller holds operator authority of kind `ImportBook` in the imported book, with a
//!    justification.
//!
//! Then the leaves are committed off-kernel, as the bootstrap writes the journal (research 07
//! §1.1), the posting sequence continues where it was, and the import is recorded on the audit
//! trail as an operator intervention (`REQ-AT-1`). The record goes to the audit chain only, not
//! to any section: re-exporting the imported book gives the same sections and roots.

use alloc::vec;
use alloc::vec::Vec;

use ledger_kernel::book::{Book, journal_id_for};
use ledger_kernel::keys::KeyRing;
use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::{OpeningVault, VaultBackend};

use super::super::{Ctx, Ledger};
use super::operator_authority_in;
use crate::audit::{
    AuditAction, AuditClass, AuditNote, AuditOutcome, AuditSink, Justification, ReadKind,
};
use crate::export::kernel::Leaves;
use crate::export::{
    Anchors, Export, ExportError, FORMAT_VERSION, Manifest, Openings, SectionEntry, book_root,
    service_root,
};
use crate::lifecycle::LifecycleConfig;
use crate::operator::OperatorOpKind;
use crate::principal::resolve;
use crate::state::{Command, State, Write};

/// Where an import goes: an empty deployment, with the secrets the sealed key-custody step
/// carried.
pub struct ImportTarget<S: LeafStore, V: VaultBackend> {
    /// The target's leaf store: empty.
    pub store: S,
    /// The target's openings vault: empty.
    pub vault: V,
    /// The core's key ring, from the same root as the exporter's (`DEC-3`): sealed key custody.
    pub keys: KeyRing,
    /// The book's openings: sealed key custody (`REQ-RT-6`).
    pub openings: Openings,
    /// The target's deployment configuration. Its deployment id must be the export's.
    pub config: LifecycleConfig,
}

/// A refused or failed import. `parts` gives the target's store and vault back when nothing was
/// written; it is `None` only for [`ExportError::Partial`], and then the target must be discarded.
pub struct ImportFailure<S, V> {
    /// Why.
    pub error: ExportError,
    /// The untouched store and vault.
    pub parts: Option<(S, V)>,
}

impl<S, V> core::fmt::Debug for ImportFailure<S, V> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ImportFailure")
            .field("error", &self.error)
            .field("untouched", &self.parts.is_some())
            .finish()
    }
}

/// What an import made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Imported {
    /// The posting sequence the book continues from.
    pub sequence: u64,
    /// The book root imported.
    pub book_root: [u8; 32],
}

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    /// The book as a checkpoint sees it: the posting sequence, and the book root of the kernel's
    /// composite root and the service root (`crate::export::book_root`).
    pub fn checkpoint(&self) -> Result<(u64, [u8; 32]), ExportError> {
        let store = self.book.store();
        let kernel = store.root().map_err(|_| ExportError::Inconsistent)?;
        let sequence = store
            .next_posting_seq()
            .map_err(|_| ExportError::Inconsistent)?;
        let service = service_root(&self.state.sections());
        Ok((sequence, book_root(&kernel, &service)))
    }

    /// Export the whole book at the current posting sequence `S` (module documentation).
    ///
    /// A privileged read (`REQ-PV-4`): the operator needs the `PrivilegedRead` operator kind and
    /// a justification, and the read's record reaches the audit store before anything is
    /// returned. `S` must be anchored with the current book root. Returns no blinding, opening
    /// or key.
    pub fn export_book(
        &mut self,
        ctx: &Ctx,
        anchors: &dyn Anchors,
        justification: &[u8],
        sink: &mut dyn AuditSink,
    ) -> Result<Export, ExportError> {
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Export, ExportError> = (|| {
            // An export does not carry the writer's upgrade-authority record yet (`T-037-05`;
            // `T-037-17` carries it): a book under any mode but its genesis one, or with a change
            // of it scheduled, is not exported at all, rather than imported under its genesis mode.
            if self.state.upgrade_authority().is_some()
                || self.authority() != crate::code::authority::GENESIS
                || self.authority_next().is_some()
            {
                return Err(ExportError::UpgradeAuthority);
            }
            self.privileged_read(ctx, ReadKind::Export, Vec::new(), justification, sink)
                .map_err(ExportError::Refused)?;
            // The export reads the store: an open batch is committed first (`T-031-19`).
            self.book.flush().map_err(|_| ExportError::Inconsistent)?;
            // Everything the audit store has not acknowledged would be lost to the trail.
            let audit = self
                .state
                .audit_head()
                .ok_or(ExportError::AuditNotShipped)?;
            let store = self.book.store();
            let last = store
                .last_committed()
                .ok()
                .flatten()
                .ok_or(ExportError::Inconsistent)?;
            let sequence = store
                .next_posting_seq()
                .map_err(|_| ExportError::Inconsistent)?;
            let kernel_root = store.root().map_err(|_| ExportError::Inconsistent)?;
            let leaves = Leaves::from_log(store, last).map_err(|_| ExportError::Inconsistent)?;
            if leaves.root() != kernel_root || leaves.next_posting() != sequence {
                return Err(ExportError::Inconsistent);
            }
            let service = self.state.sections();
            let service_root = service_root(&service);
            if anchors.anchored(sequence) != Some(book_root(&kernel_root, &service_root)) {
                return Err(ExportError::NotAnchored);
            }
            let mut sections = leaves.sections();
            sections.extend(service);
            let manifest = Manifest {
                version: FORMAT_VERSION,
                deployment: self.config.deployment,
                sequence,
                kernel_root,
                service_root,
                audit,
                sections: sections
                    .iter()
                    .map(|s| SectionEntry {
                        name: s.name().into(),
                        count: s.count(),
                        digest: s.digest(),
                    })
                    .collect(),
            };
            Ok(Export { manifest, sections })
        })();
        r
    }

    /// Import an export into an empty deployment (module documentation). Every check runs before
    /// the first write; a refusal gives the target back untouched.
    pub fn import_book(
        export: &Export,
        target: ImportTarget<S, V>,
        anchors: &dyn Anchors,
        ctx: &Ctx,
        justification: &[u8],
        sink: &mut dyn AuditSink,
    ) -> Result<(Ledger<S, V>, Imported), ImportFailure<S, V>> {
        let ImportTarget {
            store,
            vault,
            keys,
            openings,
            config,
        } = target;
        let checked = check_import(
            export,
            &store,
            &vault,
            &keys,
            &openings,
            &config,
            anchors,
            ctx,
            justification,
            &*sink,
        );
        let (leaves, mut state, note) = match checked {
            Ok(x) => x,
            Err(error) => {
                return Err(ImportFailure {
                    error,
                    parts: Some((store, vault)),
                });
            }
        };
        let partial = |_| ImportFailure {
            error: ExportError::Partial,
            parts: None,
        };
        let book =
            Book::import(store, vault, keys, &leaves.chunks(), openings.rows()).map_err(partial)?;
        if book.root().map_err(partial)? != export.manifest.kernel_root
            || book.verify_openings().is_err()
        {
            return Err(ImportFailure {
                error: ExportError::Partial,
                parts: None,
            });
        }
        // The import's own record, on the audit chain only (`REQ-AT-1`).
        state
            .commit(&[Write::Audit(note)])
            .map_err(|_| ImportFailure {
                error: ExportError::Partial,
                parts: None,
            })?;
        let mut ledger = Ledger {
            book,
            state,
            last_fault: None,
            internal_faults: (0, 0),
            config,
            fault: None,
            calls: None,
            code: ([0; 32], [0; 32]),
            input: 0,
            way_out: None,
            calls_in_input: (0, 0),
            writer: crate::code::authority::Position::default(),
        };
        // At least once: a record the store does not take now stays in the outbox.
        let _ = ledger.ship_audit(sink);
        Ok((
            ledger,
            Imported {
                sequence: export.manifest.sequence,
                book_root: export.manifest.book_root(),
            },
        ))
    }
}

/// Every check of an import, before anything is written: the leaves, the rebuilt service store,
/// and the import's audit note.
#[allow(clippy::too_many_arguments)]
fn check_import<S: LeafStore, V: VaultBackend>(
    export: &Export,
    store: &S,
    vault: &V,
    keys: &KeyRing,
    openings: &Openings,
    config: &LifecycleConfig,
    anchors: &dyn Anchors,
    ctx: &Ctx,
    justification: &[u8],
    sink: &dyn AuditSink,
) -> Result<(Leaves, State, AuditNote), ExportError> {
    // 1. An empty target.
    let vault_empty = vault
        .read()
        .map(|image| image.seq.is_none() && image.rows.is_empty())
        .unwrap_or(false);
    if !matches!(store.last_committed(), Ok(None)) || !vault_empty {
        return Err(ExportError::NotEmpty);
    }
    // 2. The layout, every digest, the deployment, the anchor.
    export.verify()?;
    let m = &export.manifest;
    if m.deployment != config.deployment {
        return Err(ExportError::Deployment);
    }
    if anchors.anchored(m.sequence) != Some(m.book_root()) {
        return Err(ExportError::NotAnchored);
    }
    // 3. Both roots, recomputed.
    let leaves = Leaves::from_sections(export.kernel_sections())?;
    if leaves.root() != m.kernel_root || leaves.next_posting() != m.sequence {
        return Err(ExportError::RootMismatch);
    }
    let state = State::from_sections(export.service_sections(), &m.audit)?;
    if service_root(&state.sections()) != m.service_root {
        return Err(ExportError::RootMismatch);
    }
    // 4. The sealed secrets are this book's.
    let journal = journal_id_for(&keys.registrar_key());
    if leaves.journals.len() != 1 || !leaves.journals.contains_key(&journal) {
        return Err(ExportError::Secrets);
    }
    openings.check(&leaves).map_err(|_| ExportError::Secrets)?;
    // 5. The audit trail continues from the export's head.
    let head = m.audit.emitted.checked_sub(1).map(|s| (s, m.audit.head));
    if m.audit.shipped != head || sink.head().ok() != Some(head) {
        return Err(ExportError::AuditNotShipped);
    }
    // 6. Operator authority of its own kind, in the book being imported.
    let p = resolve(&state, &ctx.caller).map_err(ExportError::Refused)?;
    let used = operator_authority_in(&state, &p, OperatorOpKind::ImportBook, ctx.now)
        .map_err(ExportError::Refused)?;
    let justification = Justification::new(justification).map_err(ExportError::Refused)?;
    let note = AuditNote {
        op: ctx.op,
        class: Some(AuditClass::of(Command::ImportBook)),
        action: Some(AuditAction::Command(Command::ImportBook)),
        authorities: vec![used],
        subjects: Vec::new(),
        justification: Some(justification),
        outcome: AuditOutcome::Accepted,
        at: ctx.now,
    };
    Ok((leaves, state, note))
}

/// The openings of a ledger's vault, for the sealed key-custody step in process.
pub fn openings_of<S: LeafStore, V: VaultBackend>(ledger: &Ledger<S, V>) -> Openings {
    let vault: &OpeningVault<V> = ledger.book.vault();
    Openings::from_vault(vault)
}
