// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The service store's sections of an export, and its rebuild from them (`F-014` T-014-01,
//! T-014-03).
//!
//! Each map the store keeps is one section, its records in key order. Indexes the store derives
//! — accounts by owner, credentials by identity, grants by principal, the open-reservation totals, the operation store's
//! keys and deadlines, operator grants by principal, the journal's indexes and checkpoints — are
//! not exported: the rebuild derives them again, through the same code that keeps them. Where a
//! sub-store has its own write rules (the reservation index, the operator records, the journal,
//! the openings record), the rebuild replays its records through them.
//!
//! The audit log is not a section: the audit trail is retained apart (`REQ-AT-2`). An export
//! carries its head in the manifest, and the rebuild continues the chain from it.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use super::{Roles, State};
use crate::account::{LedgerAccount, Time};
use crate::anchor::{AnchorIncident, AnchorStore, Checkpoint, Comparison};
use crate::asset::Minor;
use crate::asset::{AssetBinding, AssetId, AssetKind, AssetRegistry, AssetStatus};
use crate::audit::AuditLog;
use crate::authority::{Grant, WindowKey};
use crate::custody::store::{CustodyState, Notice, PendingChange};
use crate::export::canon::{self, Canonical, Dec, DecodeError, Enc};
use crate::export::{AuditHead, Section};
use crate::id::{AccountId, GrantId, IdentityId, Mint, OperationId};
use crate::identity::Identity;
use crate::leaves::Slot;
use crate::lifecycle::store::{Notification, Operation, Operations, RequestEntry};
use crate::opening::{OpeningLoad, OpeningsState};
use crate::operator::{
    EvidenceSubmission, Incident, OperatorGrant, OperatorOpKind, OpsState, OpsWrite,
    PendingReversal, ResolutionCase,
};
use crate::party::{IntegratingParty, PartyTie};
use crate::principal::Credential;
use crate::query::journal::{Journal, PostingEntry};
use crate::reservation::{Closing, IndexChange, ReservationIndex, Status};
use crate::set::NonEmptySet;
use crate::settlement::store::SettlementStore;
use crate::state::OperationRecord;

/// The names of the service sections, in order.
pub const SERVICE_SECTIONS: [&str; 37] = [
    "service.registry",
    "service.roles",
    "service.identities",
    "service.accounts",
    "service.credentials",
    "service.grants",
    "service.windows",
    "service.records",
    "service.transitions",
    "service.reservations",
    "service.operations",
    "service.lifecycle-clock",
    "service.outbox",
    "service.requests",
    "service.settlement",
    "service.operator-grants",
    "service.reversals",
    "service.incidents",
    "service.journal",
    "service.opening-positions",
    "service.opening-loads",
    "service.custody-changes",
    "service.custody-successors",
    "service.custody-epochs",
    "service.custody-notices",
    "service.anchor-checkpoints",
    "service.anchor-incidents",
    "service.anchor-comparisons",
    "service.anchor-latest",
    "service.anchor-issued",
    "service.anchor-on-demand",
    "service.resolutions",
    "service.integrating-parties",
    "service.party-ties",
    "service.chainspec",
    "service.chainspec-aliases",
    "service.chainspec-openings",
];

/// How many sections an export written before seeded genesis has (`F-036`): it imports as a book
/// without a seed.
const SECTIONS_BEFORE_SEED: usize = 34;

/// One asset of the registry, with the kernel tag it was given.
struct AssetRecord {
    id: AssetId,
    tag: u32,
    binding: AssetBinding,
    precision: u8,
    kind: AssetKind,
    status: AssetStatus,
}

impl Canonical for AssetRecord {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.id)
            .put(&self.tag)
            .put(&self.binding)
            .put(&self.precision)
            .put(&self.kind)
            .put(&self.status);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(AssetRecord {
            id: d.get()?,
            tag: d.get()?,
            binding: d.get()?,
            precision: d.get()?,
            kind: d.get()?,
            status: d.get()?,
        })
    }
}

/// An operator-surface grant: its terms, and when it was revoked.
struct OperatorGrantRecord {
    minted: Mint,
    grantor: IdentityId,
    principal: IdentityId,
    kinds: NonEmptySet<OperatorOpKind>,
    granted_at: Time,
    expires: Option<Time>,
    revoked: Option<Time>,
}

impl Canonical for OperatorGrantRecord {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.minted)
            .put(&self.grantor)
            .put(&self.principal)
            .put(&self.kinds)
            .put(&self.granted_at)
            .put(&self.expires)
            .put(&self.revoked);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(OperatorGrantRecord {
            minted: d.get()?,
            grantor: d.get()?,
            principal: d.get()?,
            kinds: d.get()?,
            granted_at: d.get()?,
            expires: d.get()?,
            revoked: d.get()?,
        })
    }
}

fn section<'a, T: Canonical + 'a>(name: &str, items: impl IntoIterator<Item = &'a T>) -> Section {
    let mut s = Section::new(name);
    for i in items {
        s.push(i);
    }
    s
}

/// Decode a section of keyed records, requiring the keys strictly increasing.
fn keyed<K: Ord, V: Canonical>(
    s: &Section,
    key: impl Fn(&V) -> K,
) -> Result<BTreeMap<K, V>, DecodeError> {
    let mut out = BTreeMap::new();
    for r in s.records() {
        let v: V = canon::from_bytes(r?)?;
        let k = key(&v);
        if out.last_key_value().is_some_and(|(last, _)| *last >= k) {
            return Err(DecodeError("records out of order"));
        }
        out.insert(k, v);
    }
    Ok(out)
}

fn bad<E>(_: E) -> DecodeError {
    DecodeError("the store's rules refuse the record")
}

impl State {
    /// The service store's sections, in [`SERVICE_SECTIONS`] order.
    pub(crate) fn sections(&self) -> Vec<Section> {
        let State {
            registry,
            identities,
            accounts,
            by_owner: _,
            owned_custody: _,
            credentials,
            sets: _,
            grants,
            by_principal: _,
            by_subject: _,
            by_asset_or_identity: _,
            by_ref: _,
            windows,
            records,
            transitions,
            reservations,
            roles,
            operations,
            settlement,
            audit: _,
            ops,
            journal,
            openings,
            custody,
            anchoring,
            chainspec: seed,
            applying_spec: _,
            spec_undo: _,
        } = self;
        let n = &SERVICE_SECTIONS;
        let mut assets: Vec<AssetRecord> = registry
            .get()
            .iter()
            .map(|a| AssetRecord {
                id: a.id(),
                tag: a.tag().0,
                binding: a.binding(),
                precision: a.precision(),
                kind: a.kind(),
                status: a.status(),
            })
            .collect();
        assets.sort_by_key(|a| a.tag);
        let mut settlement_section = Section::new(n[14]);
        for r in settlement.to_records() {
            settlement_section.push_bytes(&r);
        }
        let op_grants: Vec<OperatorGrantRecord> = ops
            .grants()
            .map(|g| OperatorGrantRecord {
                minted: g.minted(),
                grantor: g.grantor(),
                principal: g.principal(),
                kinds: g.kinds().clone(),
                granted_at: g.granted_at(),
                expires: g.expires(),
                revoked: g.revoked(),
            })
            .collect();
        let positions: Vec<(AssetId, AccountId)> =
            openings.positions().map(|(a, b)| (*a, *b)).collect();
        let loads: Vec<(OperationId, OpeningLoad)> =
            openings.loads().map(|(k, v)| (*k, v.clone())).collect();
        let transition_rows: Vec<(OperationId, Vec<OperationRecord>)> =
            transitions.iter().map(|(k, v)| (*k, v.clone())).collect();
        let reservation_rows: Vec<(OperationId, Status)> = reservations
            .iter()
            .map(|s| (s.reservation().op, *s))
            .collect();
        let successors: Vec<(AccountId, IdentityId)> =
            custody.successors().map(|(a, i)| (*a, *i)).collect();
        let epochs: Vec<(AccountId, u64)> = custody.epochs().map(|(a, e)| (*a, *e)).collect();
        let window_rows: Vec<(WindowKey, Minor)> = windows.iter().map(|(k, v)| (*k, *v)).collect();
        let seed_aliases: Vec<([u8; 32], AccountId)> =
            seed.aliases().map(|(k, v)| (*k, *v)).collect();
        let seeded_pairs: Vec<(AccountId, AssetId)> = seed
            .progress()
            .and_then(|p| p.header.as_ref())
            .map(|h| openings.opened_under(&crate::chainspec::opening_op(&h.genesis_op)))
            .unwrap_or_default();
        let issued: Vec<(OperationId, Vec<u8>)> = anchoring
            .all_issued()
            .map(|(k, v)| (*k, v.clone()))
            .collect();
        let on_demand: Vec<(OperationId, Option<u64>)> =
            anchoring.all_on_demand().map(|(k, v)| (*k, *v)).collect();
        alloc::vec![
            section(n[0], &assets),
            section(n[1], roles.get().iter()),
            section(n[2], identities.values()),
            section(n[3], accounts.values()),
            section(n[4], credentials.values()),
            section(n[5], grants.values()),
            section(n[6], &window_rows),
            section(n[7], records.values()),
            section(n[8], &transition_rows),
            section(n[9], &reservation_rows),
            section(n[10], operations.all()),
            section(n[11], core::iter::once(&operations.clock())),
            section(n[12], operations.outbox()),
            section(n[13], operations.requests()),
            settlement_section,
            section(n[15], &op_grants),
            section(n[16], ops.reversals()),
            section(n[17], ops.incidents()),
            section(n[18], journal.iter()),
            section(n[19], &positions),
            section(n[20], &loads),
            section(n[21], custody.changes()),
            section(n[22], &successors),
            section(n[23], &epochs),
            section(n[24], custody.outbox()),
            section(n[25], anchoring.checkpoints()),
            section(n[26], anchoring.incidents()),
            section(n[27], anchoring.comparisons()),
            section(n[28], anchoring.latest_comparison()),
            section(n[29], &issued),
            section(n[30], &on_demand),
            section(n[31], ops.resolutions()),
            section(n[32], ops.parties()),
            section(n[33], ops.ties()),
            section(n[34], seed.progress()),
            section(n[35], &seed_aliases),
            section(n[36], &seeded_pairs),
        ]
    }

    /// Rebuild the store from its sections, continuing the audit chain at `audit`.
    pub(crate) fn from_sections(
        sections: &[Section],
        audit: &AuditHead,
    ) -> Result<State, DecodeError> {
        if (sections.len() != SERVICE_SECTIONS.len() && sections.len() != SECTIONS_BEFORE_SEED)
            || sections
                .iter()
                .zip(SERVICE_SECTIONS)
                .any(|(s, n)| s.name() != n)
        {
            return Err(DecodeError("service sections"));
        }
        let s = |i: usize| &sections[i];
        let mut state = State::new();

        // The registry, re-registered in tag order: each asset must get back its tag.
        let mut registry = AssetRegistry::new();
        for a in s(0).decode_all::<AssetRecord>()? {
            let got = registry.register(a.id, a.precision, a.kind).map_err(bad)?;
            // Its binding, through the same moves a live registry makes (`REQ-AS-8`).
            if let Some(l) = a.binding.location() {
                registry
                    .set_binding(&a.id, crate::asset::AssetBinding::Binding(l))
                    .map_err(bad)?;
            }
            if a.binding.bound().is_some() {
                registry.set_binding(&a.id, a.binding).map_err(bad)?;
            }
            if got.tag().0 != a.tag {
                return Err(DecodeError("asset tag"));
            }
            if a.status == AssetStatus::Deprecated {
                registry.deprecate(&a.id).map_err(bad)?;
            }
        }
        state.registry = Slot::new(registry);

        let roles: Vec<Roles> = s(1).decode_all()?;
        if roles.len() > 1 {
            return Err(DecodeError("roles"));
        }
        state.roles.set(roles.first().copied());

        state.identities = keyed(s(2), |i: &Identity| i.id())?.into();
        state.accounts = keyed(s(3), |a: &LedgerAccount| a.id())?.into();
        for a in state.accounts.values() {
            state.by_owner.entry_or_default(a.owner()).insert(a.id());
            if a.custody() == crate::account::Custody::Owned {
                state
                    .owned_custody
                    .entry_or_default(a.owner())
                    .insert(a.id());
            }
        }
        state.credentials = keyed(s(4), |c: &Credential| c.id())?.into();
        for c in state.credentials.values() {
            state.sets.entry_or_default(c.identity()).insert(c.id());
        }
        for a in state.accounts.values() {
            if a.kind() != crate::account::AccountKind::Issuance {
                let r = crate::id::account_reference(&a.id());
                if !state.by_ref.contains_key(&r) {
                    state.by_ref.insert(r, a.id());
                }
            }
        }
        state.grants = keyed(s(5), |g: &Grant| g.id())?.into();
        for g in state.grants.values() {
            state
                .by_principal
                .write()
                .entry(g.principal())
                .or_default()
                .insert(g.id());
            if let Some(key) = super::subject_key(g) {
                state.by_subject.entry_or_default(key).insert(g.id());
            }
            if let Some(key) = super::asset_or_identity_key(g) {
                state
                    .by_asset_or_identity
                    .entry_or_default(key)
                    .insert(g.id());
            }
        }
        state.windows = keyed(s(6), |(k, _): &(WindowKey, Minor)| *k)?
            .into_iter()
            .map(|(k, (_, v))| (k, v))
            .collect();
        state.records = keyed(s(7), |r: &OperationRecord| r.op)?.into();
        state.transitions = keyed(s(8), |(k, _): &(OperationId, Vec<OperationRecord>)| *k)?
            .into_iter()
            .map(|(k, (_, v))| (k, v))
            .collect();

        // The reservation index, through its own rules: every reservation opened, then the
        // closed ones closed.
        let mut index = ReservationIndex::new();
        let rows = keyed(s(9), |(k, _): &(OperationId, Status)| *k)?;
        let mut closings: Vec<(OperationId, Closing)> = Vec::new();
        for (op, (_, status)) in &rows {
            let r = *status.reservation();
            if r.op != *op {
                return Err(DecodeError("reservation key"));
            }
            index.apply(&IndexChange::Open(r)).map_err(bad)?;
            if let Status::Closed(_, c) = status {
                closings.push((*op, *c));
            }
        }
        for (op, closing) in closings {
            index
                .apply(&IndexChange::Close { op, closing })
                .map_err(bad)?;
        }
        state.reservations = index;

        let ops: Vec<Operation> = s(10).decode_all()?;
        let clock: Vec<Option<Time>> = s(11).decode_all()?;
        let [clock] = clock.as_slice() else {
            return Err(DecodeError("lifecycle clock"));
        };
        let outbox: Vec<Notification> = s(12).decode_all()?;
        let requests: Vec<RequestEntry> = s(13).decode_all()?;
        state.operations = Operations::restore(ops, *clock, outbox, requests)?;

        let records: Vec<&[u8]> = s(14).records().collect::<Result<_, _>>()?;
        state.settlement = SettlementStore::from_records(&records)?;
        let operations = &state.operations;
        state
            .settlement
            .rebuild_live(|op| operations.get(op).is_some_and(|o| o.state().is_terminal()));
        state.settlement.rebuild_awaiting();
        state.settlement.rebuild_indexes();

        // Operator records, through their own rules.
        let mut ops_state = OpsState::new();
        let known = |i: &IdentityId| state.identities.contains_key(i);
        let taken = |g: &GrantId| state.grants.contains_key(g);
        let mut revocations = Vec::new();
        for g in s(15).decode_all::<OperatorGrantRecord>()? {
            let grant = OperatorGrant::new(
                g.minted,
                g.grantor,
                g.principal,
                g.kinds,
                g.granted_at,
                g.expires,
            );
            if let Some(at) = g.revoked {
                revocations.push((grant.id(), at));
            }
            ops_state
                .apply(&OpsWrite::PutGrant(grant), known, taken)
                .map_err(bad)?;
        }
        for (id, at) in revocations {
            ops_state
                .apply(&OpsWrite::Revoke(id, at), known, taken)
                .map_err(bad)?;
        }
        for p in s(16).decode_all::<PendingReversal>()? {
            let approval = p.approval;
            let proposal = p.proposal;
            ops_state
                .apply(
                    &OpsWrite::Propose(PendingReversal {
                        approval: None,
                        ..p
                    }),
                    known,
                    taken,
                )
                .map_err(bad)?;
            if let Some((approver, at)) = approval {
                ops_state
                    .apply(
                        &OpsWrite::Approve {
                            proposal,
                            approver,
                            at,
                        },
                        known,
                        taken,
                    )
                    .map_err(bad)?;
            }
        }
        for i in s(17).decode_all::<Incident>()? {
            ops_state
                .apply(&OpsWrite::Incident(i), known, taken)
                .map_err(bad)?;
        }
        // Resolution cases (`REQ-RC-5`), through the store's rules: each submission, then what the
        // adapter found, then a proposal.
        for c in s(31).decode_all::<ResolutionCase>()? {
            for e in &c.evidence {
                let finding = e.finding;
                ops_state
                    .apply(
                        &OpsWrite::SubmitEvidence(EvidenceSubmission {
                            finding: None,
                            ..e.clone()
                        }),
                        known,
                        taken,
                    )
                    .map_err(bad)?;
                if let Some(finding) = finding {
                    ops_state
                        .apply(
                            &OpsWrite::EvidenceFound {
                                op: c.op,
                                id: e.id,
                                finding,
                            },
                            known,
                            taken,
                        )
                        .map_err(bad)?;
                }
            }
            if let Some(proposed) = c.proposed {
                ops_state
                    .apply(
                        &OpsWrite::ProposeResolution { op: c.op, proposed },
                        known,
                        taken,
                    )
                    .map_err(bad)?;
            }
        }
        // Integrating parties, then the principals tied to them (`REQ-ID-7`), through the
        // store's rules: a party once, a principal tied at most once.
        for p in s(32).decode_all::<IntegratingParty>()? {
            ops_state
                .apply(&OpsWrite::RegisterParty(p), known, taken)
                .map_err(bad)?;
        }
        for t in s(33).decode_all::<PartyTie>()? {
            ops_state
                .apply(&OpsWrite::Tie(t), known, taken)
                .map_err(bad)?;
        }
        state.ops = ops_state;

        // The journal, through its own ordering rules; every index derived again.
        let mut journal = Journal::new();
        for r in s(18).records() {
            let entry: PostingEntry = canon::from_bytes(r?)?;
            journal.append_entry(entry).map_err(bad)?;
        }
        state.journal = journal;

        let positions = keyed(s(19), |(a, _): &(AssetId, AccountId)| *a)?
            .into_iter()
            .map(|(k, (_, v))| (k, v))
            .collect();
        let loads = keyed(s(20), |(k, _): &(OperationId, OpeningLoad)| *k)?
            .into_iter()
            .map(|(k, (_, v))| (k, v))
            .collect();
        // The seed's record (`F-036`): absent from an export made before it.
        let seeded = sections.len() > SECTIONS_BEFORE_SEED;
        let progress: Vec<crate::chainspec::SpecProgress> = if seeded {
            s(34).decode_all()?
        } else {
            Vec::new()
        };
        if progress.len() > 1 {
            return Err(DecodeError("seed progress"));
        }
        let progress = progress.into_iter().next();
        let seeded_pairs: Vec<(AccountId, AssetId)> = if seeded {
            s(36).decode_all()?
        } else {
            Vec::new()
        };
        let seeded_openings = match progress.as_ref().and_then(|p| p.header.as_ref()) {
            Some(h) if !seeded_pairs.is_empty() => {
                Some((crate::chainspec::opening_op(&h.genesis_op), seeded_pairs))
            }
            None if !seeded_pairs.is_empty() => return Err(DecodeError("seeded openings")),
            _ => None,
        };
        state.openings = OpeningsState::restore(positions, loads, seeded_openings).map_err(bad)?;
        let aliases: BTreeMap<[u8; 32], AccountId> = if seeded {
            keyed(s(35), |(k, _): &([u8; 32], AccountId)| *k)?
                .into_values()
                .collect()
        } else {
            BTreeMap::new()
        };
        for a in aliases.values() {
            if state.accounts.get(a).is_none() {
                return Err(DecodeError("an alias of no account"));
            }
        }
        state.chainspec = crate::chainspec::SpecState::restore(progress, aliases);

        state.custody = CustodyState::restore(
            s(21).decode_all::<PendingChange>()?,
            s(22).decode_all::<(AccountId, IdentityId)>()?,
            s(23).decode_all::<(AccountId, u64)>()?,
            s(24).decode_all::<Notice>()?,
        )?;

        let latest: Vec<Comparison> = s(28).decode_all()?;
        if latest.len() > 1 {
            return Err(DecodeError("latest comparison"));
        }
        state.anchoring = AnchorStore::restore(
            s(25).decode_all::<Checkpoint>()?,
            s(26).decode_all::<AnchorIncident>()?,
            s(27).decode_all::<Comparison>()?,
            latest.into_iter().next(),
            keyed(s(29), |(k, _): &(OperationId, Vec<u8>)| *k)?
                .into_values()
                .collect(),
            keyed(s(30), |(k, _): &(OperationId, Option<u64>)| *k)?
                .into_values()
                .collect(),
        )
        .map_err(bad)?;

        // The positions' tally must be what the inbox and the journal give at its cursor
        // (finding F18's class, `REQ-PR-5`): the next comparison starts from it.
        let (through, held) = state.settlement.tally();
        if state.tally_from_scratch(through) != *held {
            return Err(DecodeError("position tally"));
        }

        state.audit = AuditLog::continuing(audit.emitted, audit.head, audit.shipped);
        Ok(state)
    }

    /// The audit chain's head as it stands, whatever the audit store has acknowledged: what the
    /// core Agent continues the chain from on its next transition (`F-005`).
    pub fn chain_head(&self) -> AuditHead {
        AuditHead {
            emitted: self.audit.emitted(),
            head: self.audit.head_hash(),
            shipped: self.audit.shipped(),
        }
    }

    /// The audit chain's head, for the manifest: `None` while the outbox holds a record the
    /// audit store has not acknowledged.
    pub(crate) fn audit_head(&self) -> Option<AuditHead> {
        if self.audit.outbox().next().is_some() {
            return None;
        }
        Some(AuditHead {
            emitted: self.audit.emitted(),
            head: self.audit.head_hash(),
            shipped: self.audit.shipped(),
        })
    }
}
