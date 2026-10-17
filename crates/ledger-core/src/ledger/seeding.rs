// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `apply_spec_chunk` (`F-036`, `REQ-RC-6`, `REQ-RC-4`, `REQ-RT-3`): one chunk of the seed genesis
//! committed, applied through the same commands as the records it carries.
//!
//! In order:
//!
//! 1. **The book has a seed, unsealed.** A book founded without one has nothing to apply, and a
//!    sealed one takes no more: `ERR-InvalidTransition`. A seed is only ever applied at genesis;
//!    there is no command that gives a live book one (`NFR-17`).
//! 2. **Exactly once, in order.** A chunk below the book's next index was applied: the call answers
//!    as it did and writes nothing. A chunk above it is `ERR-InvalidTransition`. So an interrupted
//!    seeding resumes at the first chunk not applied, and nothing applies twice.
//! 3. **The chunk is the committed one.** Its bytes and its path must reach the digest genesis
//!    committed, at its index ([`crate::chainspec::chunk_in_spec`]); otherwise
//!    `ERR-EvidenceInsufficient`, and nothing of it is read.
//! 4. **The caller is the operator** (`ERR-NotAuthorised` otherwise). The seal's guard lets it
//!    through for this call only.
//! 5. **The header** (chunk 0): the seed is for this book (its genesis minted the operator), its
//!    registry is the book's, and each asset of the opening set gets its opening position, as a
//!    load's beginning gives them.
//! 6. **The records**, each under its own reference ([`crate::chainspec::spec_op`]) and the header's
//!    time, through the command it stands for: `open_identity`, `open_account`, `grant_operator`,
//!    `grant_authority`, `grant_issuance`; aliases into the seed's record; the opening rows are
//!    checked as a load checks them, recorded as opened, and posted last, in batches of the
//!    deployment's `max_legs` counted across the whole set, each one linked chain under the pair's
//!    own kernel key ([`crate::opening::opening_key`]).
//! 7. **The chunk's record**, its audit record, and the seed's progress. Applying the last chunk
//!    **seals** the seed once every section's count is the header's and each opening position
//!    stands at minus its asset's total.
//!
//! A refused record takes the whole chunk back (the service store's writes; the kernel's postings
//! come last, after every check), and the book stays at the chunk before.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::vec;
use alloc::vec::Vec;

use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;

use super::{Accepted, Ctx, Ledger, OpenAccount, policy_used};
use crate::account::{AccountKind, Custody, LedgerAccount, Time};
use crate::asset::Minor;
use crate::audit::{AuditSubject, Justification};
use crate::chainspec::{
    self, ChunkBody, HeaderFacts, Section, SpecChunk, SpecProgress, SpecRecord, SpecWrite,
};
use crate::id::{Mint, OperationId};
use crate::opening::{OpeningRow, OpeningWrite};
use crate::principal::{Principal, resolve_unsealed};
use crate::refusal::Refusal;
use crate::state::{Command, OperationRecord, Write};

/// What one chunk came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpecReport {
    /// The chunk's index.
    pub index: u32,
    /// Chunks applied after it.
    pub applied: u32,
    /// Chunks the seed has.
    pub chunks: u32,
    /// Whether the seed is sealed.
    pub sealed: bool,
}

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    /// `apply_spec_chunk` (module documentation). `chunk` is the chunk's canonical bytes and
    /// `path` its path in the seed's tree. `ctx` names the caller; every reference and time the
    /// chunk uses derives from the seed.
    pub fn apply_spec_chunk(
        &mut self,
        ctx: &Ctx,
        chunk: &[u8],
        path: &[[u8; 32]],
    ) -> Result<Accepted<SpecReport>, Refusal> {
        let top = self.enter(|| crate::call::Call::ApplySpecChunk {
            ctx: *ctx,
            chunk: chunk.to_vec(),
            path: path.to_vec(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<SpecReport>, Refusal> = (|| {
            // 1. A seed, unsealed.
            let progress = self
                .state
                .chainspec()
                .progress()
                .cloned()
                .ok_or(Refusal::InvalidTransition)?;
            let decoded = SpecChunk::from_bytes(chunk).map_err(|_| Refusal::MalformedCommand)?;
            // 2. Exactly once, in order.
            if decoded.index < progress.applied {
                return self.spec_reply(&progress, decoded.index);
            }
            if progress.sealed || decoded.index != progress.applied {
                return Err(Refusal::InvalidTransition);
            }
            // 3. The committed chunk.
            if !chainspec::chunk_in_spec(&progress.commitment, decoded.index, chunk, path) {
                return Err(Refusal::EvidenceInsufficient);
            }
            // 4. The operator.
            let p = resolve_unsealed(&self.state, &ctx.caller)?;
            if !self.is_operator(&p) {
                return Err(Refusal::NotAuthorised);
            }
            self.state.begin_spec_chunk();
            let applied = self.spec_chunk(ctx, &p, progress, decoded);
            self.state.end_spec_chunk(applied.is_ok());
            applied
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// The seed's progress, if the book was founded on one.
    pub fn spec_progress(&self) -> Option<&SpecProgress> {
        self.state.chainspec().progress()
    }

    /// The account a prior system's reference names, if the seed gave it an alias.
    pub fn resolve_alias(
        &self,
        source: &crate::opening::OpeningSource,
        reference: &[u8],
    ) -> Option<crate::id::AccountId> {
        self.state
            .chainspec()
            .alias(&chainspec::alias_key(source, reference))
    }

    /// The reply for a chunk already applied: its record, nothing written.
    fn spec_reply(
        &self,
        progress: &SpecProgress,
        index: u32,
    ) -> Result<Accepted<SpecReport>, Refusal> {
        let g = progress
            .header
            .as_ref()
            .map(|h| h.genesis_op)
            .ok_or(Refusal::InvalidTransition)?;
        let record = self
            .state
            .record(&chainspec::chunk_op(&g, index))
            .ok_or(Refusal::InvalidTransition)?
            .clone();
        Ok(Accepted {
            value: SpecReport {
                index,
                applied: progress.applied,
                chunks: progress.commitment.chunks,
                sealed: progress.sealed,
            },
            record,
            status: None,
        })
    }

    /// Steps 5 to 7 (module documentation), inside the chunk's undo span.
    fn spec_chunk(
        &mut self,
        ctx: &Ctx,
        p: &Principal,
        mut progress: SpecProgress,
        chunk: SpecChunk,
    ) -> Result<Accepted<SpecReport>, Refusal> {
        let index = chunk.index;
        let mut accounts = Vec::new();
        match chunk.body {
            ChunkBody::Header(h) => {
                if index != 0 {
                    return Err(Refusal::MalformedCommand);
                }
                accounts = self.spec_header(p, &h)?;
                let code = h.genesis_code();
                progress.header = Some(HeaderFacts {
                    genesis_op: h.genesis_op,
                    at: h.at,
                    counts: h.counts,
                    opening: h.opening,
                    history: h.history,
                    code,
                    settlement: h.settlement,
                });
            }
            ChunkBody::Records(records) => {
                if index == 0 {
                    return Err(Refusal::MalformedCommand);
                }
                self.spec_records(ctx, p, &mut progress, records)?;
            }
        }
        let facts = progress.header.clone().ok_or(Refusal::InvalidTransition)?;
        progress.applied += 1;
        if progress.applied == progress.commitment.chunks {
            self.seal(&mut progress, &facts)?;
        }
        // 7. The chunk's record, its audit note, the progress.
        let chunk_ctx = Ctx {
            caller: ctx.caller,
            op: chainspec::chunk_op(&facts.genesis_op, index),
            now: facts.at,
        };
        let record = OperationRecord {
            op: chunk_ctx.op,
            command: Command::ApplyChainSpec,
            authorities: vec![policy_used(p)],
            accounts,
            postings: Vec::new(),
            at: facts.at,
        };
        let j = Justification::new(
            format!(
                "seeded genesis: chunk {index} of {} applied",
                progress.commitment.chunks
            )
            .as_bytes(),
        )?;
        let writes = [
            Write::PutRecord(record.clone()),
            Self::justified(
                &chunk_ctx,
                j,
                vec![AuditSubject::Evidence(progress.commitment.digest)],
            ),
            Write::ChainSpec(SpecWrite::Progress(Box::new(progress.clone()))),
        ];
        self.check(&writes)?;
        self.commit(&writes)?;
        Ok(Accepted {
            value: SpecReport {
                index,
                applied: progress.applied,
                chunks: progress.commitment.chunks,
                sealed: progress.sealed,
            },
            record,
            status: None,
        })
    }

    /// Step 5: the header. Returns the opening positions it opened.
    fn spec_header(
        &mut self,
        p: &Principal,
        h: &chainspec::SpecHeader,
    ) -> Result<Vec<crate::id::AccountId>, Refusal> {
        // Either readable format (`chainspec::readable`): a book founded on format 4 replays its
        // genesis as it was built. Only the core host refuses to drive one.
        if !chainspec::readable(h.version) {
            return Err(Refusal::MalformedCommand);
        }
        let roles = self.roles()?;
        if roles.operator != chainspec::operator_of(&h.genesis_op) || roles.operator != p.identity {
            return Err(Refusal::NotAuthorised);
        }
        let registry = chainspec::registry_of(&h.assets).map_err(|_| Refusal::UnknownAsset)?;
        if &registry != self.state.registry() {
            return Err(Refusal::UnknownAsset);
        }
        // The opening positions, as a load's beginning opens them.
        let mut positions = Vec::new();
        let Some(opening) = &h.opening else {
            return Ok(positions);
        };
        let op = chainspec::opening_op(&h.genesis_op);
        let mut writes = Vec::new();
        let mut ordinal = 0u32;
        for asset in opening.totals.keys() {
            self.state.active_asset(asset)?;
            match self.state.openings().position(asset) {
                Some(a) => positions.push(a),
                None => {
                    let account = LedgerAccount::new(
                        Mint::new(op, ordinal),
                        roles.operator,
                        AccountKind::Issuance,
                        Custody::Delegated,
                        h.at,
                        BTreeMap::new(),
                    )?;
                    ordinal += 1;
                    positions.push(account.id());
                    writes.push(Write::OpeningPosition {
                        asset: *asset,
                        account,
                    });
                }
            }
        }
        // The opening reference's own record: the postings the seed makes carry it.
        writes.push(Write::PutRecord(OperationRecord {
            op,
            command: Command::ApplyChainSpec,
            authorities: vec![policy_used(p)],
            accounts: positions.clone(),
            postings: Vec::new(),
            at: h.at,
        }));
        self.check(&writes)?;
        self.commit(&writes)?;
        Ok(positions)
    }

    /// Step 6: the records of one chunk.
    fn spec_records(
        &mut self,
        ctx: &Ctx,
        p: &Principal,
        progress: &mut SpecProgress,
        records: Vec<SpecRecord>,
    ) -> Result<(), Refusal> {
        let facts = progress.header.clone().ok_or(Refusal::InvalidTransition)?;
        let g = facts.genesis_op;
        let at: Time = facts.at;
        let mut aliases = Vec::new();
        let mut openings: Vec<(u32, OpeningRow)> = Vec::new();
        for r in records {
            let section = r.section();
            if (section as u8) < progress.section {
                return Err(Refusal::InvalidTransition);
            }
            progress.section = section as u8;
            let ordinal = progress.seen.get(section);
            if ordinal >= facts.counts.get(section) {
                return Err(Refusal::InvalidTransition);
            }
            *progress.seen.of(section) += 1;
            let rctx = Ctx {
                caller: ctx.caller,
                op: chainspec::spec_op(&g, section as u8, ordinal),
                now: at,
            };
            match r {
                SpecRecord::Identity { kind, verifier } => {
                    self.open_identity(&rctx, kind, None, verifier)?;
                }
                SpecRecord::Account { owner, kind } => {
                    self.open_account(
                        &rctx,
                        OpenAccount {
                            owner,
                            kind,
                            custody: Custody::Delegated,
                            treasury_limits: BTreeMap::new(),
                            acknowledged: None,
                        },
                    )?;
                }
                SpecRecord::Alias {
                    source,
                    reference,
                    account,
                } => aliases.push(Write::ChainSpec(SpecWrite::Alias {
                    key: chainspec::alias_key(&source, &reference),
                    account,
                })),
                SpecRecord::Opening(row) => {
                    let ordinal = u32::try_from(ordinal).map_err(|_| Refusal::InvalidTransition)?;
                    openings.push((ordinal, row));
                }
                SpecRecord::OperatorGrant {
                    kinds,
                    expires,
                    justification,
                } => {
                    self.grant_operator(&rctx, p.identity, kinds, expires, &justification)?;
                }
                SpecRecord::Grant(request) => {
                    self.grant_authority(&rctx, request)?;
                }
                SpecRecord::IssuanceGrant {
                    request,
                    justification,
                } => {
                    self.grant_issuance(&rctx, request, &justification)?;
                }
                SpecRecord::Credentials {
                    identity,
                    credentials,
                } => {
                    self.import_credentials(
                        &rctx,
                        crate::ledger::custody::ImportCredentials {
                            identity,
                            credentials: credentials
                                .into_iter()
                                .map(|verifier| crate::ledger::custody::ImportedCredential {
                                    verifier,
                                    scope: None,
                                })
                                .collect(),
                        },
                        chainspec::IMPORT_JUSTIFICATION,
                    )?;
                }
            }
        }
        if !aliases.is_empty() {
            self.check(&aliases)?;
            self.commit(&aliases)?;
        }
        if !openings.is_empty() {
            self.spec_openings(&g, at, progress, &openings)?;
        }
        Ok(())
    }

    /// The opening rows of one chunk: every row checked, every pair recorded as opened, then the
    /// non-zero rows posted in the deployment's batches, counted across the whole set.
    fn spec_openings(
        &mut self,
        g: &OperationId,
        at: Time,
        progress: &mut SpecProgress,
        rows: &[(u32, OpeningRow)],
    ) -> Result<(), Refusal> {
        let op = chainspec::opening_op(g);
        for (_, row) in rows {
            self.check_spec_opening(row)?;
        }
        let pairs = Write::Opening(OpeningWrite::Seeded {
            op,
            pairs: rows.iter().map(|(_, r)| (r.account, r.asset)).collect(),
        });
        self.check(core::slice::from_ref(&pairs))?;
        self.commit(core::slice::from_ref(&pairs))?;
        // The caller of the postings is the seed's: the opening reference is the operation.
        let ctx = Ctx {
            caller: crate::principal::Caller::new(
                chainspec::operator_of(g),
                chainspec::operator_credential_of(g),
            ),
            op,
            now: at,
        };
        let limit = self.config.max_legs.max(1) as u64;
        let mut batch: Vec<(u32, OpeningRow)> = Vec::new();
        let mut batch_id: Option<u64> = None;
        for (ordinal, row) in rows.iter().filter(|(_, r)| r.amount != Minor::ZERO) {
            let id = progress.opening_posted / limit;
            if batch_id.is_some_and(|b| b != id) {
                self.post_opening_chain(&ctx, &batch, None)?;
                batch.clear();
            }
            batch_id = Some(id);
            batch.push((*ordinal, *row));
            progress.opening_posted += 1;
        }
        if !batch.is_empty() {
            self.post_opening_chain(&ctx, &batch, None)?;
        }
        Ok(())
    }

    /// One opening row, as a load checks it: the account exists, can be credited and is not an
    /// issuance account; the asset is active; nothing has opened the pair, and the kernel holds no
    /// entry on it.
    fn check_spec_opening(&self, row: &OpeningRow) -> Result<(), Refusal> {
        let acct = self
            .state
            .account(&row.account)
            .ok_or(Refusal::UnknownAccount)?;
        acct.require_creditable()?;
        if acct.kind() == AccountKind::Issuance {
            return Err(Refusal::NotAuthorised);
        }
        let asset = *self.state.active_asset(&row.asset)?;
        if self
            .state
            .openings()
            .loaded_by(&row.account, &row.asset)
            .is_some()
        {
            return Err(Refusal::InvalidTransition);
        }
        match self.book.kernel_account(&acct.sub_account(&asset).id()) {
            Ok(None) => Ok(()),
            Ok(Some(_)) => Err(Refusal::InvalidTransition),
            Err(_) => Err(Refusal::NotAuthorised),
        }
    }

    /// The seal (module documentation, step 7): every count is the header's, and each opening
    /// position stands at minus its asset's total.
    fn seal(&mut self, progress: &mut SpecProgress, facts: &HeaderFacts) -> Result<(), Refusal> {
        if progress.seen != facts.counts {
            return Err(Refusal::EvidenceInsufficient);
        }
        if let Some(opening) = &facts.opening {
            for (asset, total) in &opening.totals {
                let position = self
                    .state
                    .openings()
                    .position(asset)
                    .and_then(|id| self.state.account(&id))
                    .ok_or(Refusal::EvidenceInsufficient)?
                    .clone();
                let a = *self.state.registry().get(asset)?;
                let net = match self.net(&position, &a) {
                    Ok(n) => n,
                    Err(f) => return Err(self.fault(f)),
                };
                let total = i128::try_from(*total).map_err(|_| Refusal::EvidenceInsufficient)?;
                if net != -total {
                    return Err(Refusal::EvidenceInsufficient);
                }
            }
        }
        progress.sealed = true;
        Ok(())
    }
}

/// The sections, for callers that count what a seed holds.
pub const SECTIONS: [Section; 8] = [
    Section::Identities,
    Section::Accounts,
    Section::Aliases,
    Section::Openings,
    Section::OperatorGrants,
    Section::Grants,
    Section::IssuanceGrants,
    Section::Credentials,
];
