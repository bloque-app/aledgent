// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A synthetic seed for the library's tests (`F-036`): the shape of the migration's, small.

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::vec;
use alloc::vec::Vec;
use core::num::NonZeroU64;

use super::*;
use crate::asset::{AssetKind, Minor, NonZeroMinor};
use crate::opening::{SourceExtract, SourceRow};

/// A passkey verifier, distinct per `(i, k)` (the library checks its shape only).
pub(crate) fn passkey(i: u64, k: u8) -> Verifier {
    let mut public_key = [0x22; 65];
    public_key[0] = 0x04;
    public_key[1..9].copy_from_slice(&i.to_be_bytes());
    public_key[9] = k;
    Verifier::of(
        crate::scheme::Scheme::WebAuthnEs256,
        crate::scheme::PasskeyKey {
            handle: [k; 32],
            public_key,
            rp_id: "bloque.app".into(),
            backup_eligible: true,
            sign_count: 0,
        }
        .encode(),
    )
}

/// The test seed and what a test needs beside it.
pub(crate) struct Synthetic {
    pub seed: ChainSpec,
    pub limits: ChunkLimits,
    pub required: RequiredSources,
    pub identities: Vec<IdentityId>,
    pub accounts: Vec<AccountId>,
    pub source: OpeningSource,
    /// The seeded service principals, with [`synthetic_with_services`]: the custodian, then the issuer.
    pub services: Vec<IdentityId>,
}

/// The per-window bound of the services' grants of the seed (`T-036-30`).
pub(crate) const SERVICE_LIMIT: u64 = 1_000;

/// The asset the openings are in.
pub(crate) fn copm() -> AssetId {
    AssetId::parse("COPM").unwrap()
}

/// The registry: COPM, book-only.
pub(crate) fn assets() -> Vec<AssetDecl> {
    vec![AssetDecl {
        id: copm(),
        location: None,
        precision: 2,
        kind: AssetKind::Stable,
        deprecated: false,
    }]
}

/// The verifier every seeded identity takes: one operator-held key (owner ruling 2026-10-08).
pub(crate) fn verifier() -> Verifier {
    Verifier::new(vec![0x5E; 32])
}

/// `n` identities (the first an organisation: the system owners' principal), one account each, an alias
/// per account, `openings` non-zero openings and two zero ones, the operator's grants.
pub(crate) fn synthetic(g: OperationId, n: u64, openings: u64, limits: ChunkLimits) -> Synthetic {
    synthetic_seed(g, n, openings, limits, false)
}

/// [`synthetic`], with amendment 0025's grants of the seed (`T-036-30`, `a0025-q3`, `a0025-q4`):
/// two service principals seeded after the estate's identities (the custodian, then the issuer, each with its
/// own verifier), a delegated grant to the custodian over every identity (`open_account`, `transfer`),
/// and a delegated grant to the issuer over every account (`transfer`, COPM), each bounded per window,
/// all issued by the operator as the identities principal.
pub(crate) fn synthetic_with_services(
    g: OperationId,
    n: u64,
    openings: u64,
    limits: ChunkLimits,
) -> Synthetic {
    synthetic_seed(g, n, openings, limits, true)
}

fn synthetic_seed(
    g: OperationId,
    n: u64,
    openings: u64,
    limits: ChunkLimits,
    with_services: bool,
) -> Synthetic {
    let source = OpeningSource([0xA1; 16]);
    let mut records = Vec::new();
    let identities: Vec<IdentityId> = (0..n).map(|i| identity_of(&g, i)).collect();
    let accounts: Vec<AccountId> = (0..n).map(|i| account_of(&g, i)).collect();
    for i in 0..n {
        records.push(SpecRecord::Identity {
            kind: if i == 0 {
                IdentityKind::Organisation
            } else {
                IdentityKind::Person
            },
            verifier: verifier(),
        });
    }
    let services: Vec<IdentityId> = if with_services {
        (n..n + 2).map(|i| identity_of(&g, i)).collect()
    } else {
        Vec::new()
    };
    for k in 0..services.len() {
        records.push(SpecRecord::Identity {
            kind: IdentityKind::Organisation,
            verifier: Verifier::new(vec![0x3A + k as u8; 32]),
        });
    }
    for owner in &identities {
        records.push(SpecRecord::Account {
            owner: *owner,
            kind: AccountKind::Customer,
        });
    }
    let mut aliases: Vec<(Vec<u8>, AccountId)> = accounts
        .iter()
        .enumerate()
        .map(|(i, a)| (format!("legacy-{i:07}").into_bytes(), *a))
        .collect();
    aliases.sort();
    for (reference, account) in &aliases {
        records.push(SpecRecord::Alias {
            source,
            reference: reference.clone(),
            account: *account,
        });
    }
    let mut rows: Vec<OpeningRow> = (0..(openings + 2).min(n) as usize)
        .map(|i| OpeningRow {
            account: accounts[i],
            asset: copm(),
            amount: Minor::new(if i < 2 { 0 } else { 100 * i as u64 + 7 }),
        })
        .collect();
    rows.sort_by_key(|r| (r.account, r.asset));
    for r in &rows {
        records.push(SpecRecord::Opening(*r));
    }
    let operator = operator_of(&g);
    records.push(SpecRecord::OperatorGrant {
        kinds: NonEmptySet::new(BTreeSet::from([
            OperatorOpKind::IssuanceAuthority,
            OperatorOpKind::ImportCredentials,
            OperatorOpKind::LoadOpeningBalances,
            OperatorOpKind::ReadSignals,
        ]))
        .unwrap(),
        expires: None,
        justification: b"seeded genesis: the operator's own kinds".to_vec(),
    });
    let window = Window {
        length: NonZeroU64::new(86_400_000).unwrap(),
        limit: NonZeroMinor::new(Minor::new(1_000_000_000)).unwrap(),
    };
    records.push(SpecRecord::Grant(GrantRequest {
        kind: GrantKind::Operator,
        principal: operator,
        subject: Subject::DelegatedAccounts,
        ops: NonEmptySet::new(BTreeSet::from([
            OperationKind::Transfer,
            OperationKind::Reserve,
            OperationKind::Capture,
            OperationKind::Release,
        ]))
        .unwrap(),
        per_operation: None,
        per_window: Some(window),
        assets: None,
        counterparties: None,
        parent: None,
        expires: None,
    }));
    if with_services {
        let bound = Window {
            length: NonZeroU64::new(86_400_000).unwrap(),
            limit: NonZeroMinor::new(Minor::new(SERVICE_LIMIT)).unwrap(),
        };
        let grant = |principal, subject, ops: &[OperationKind], assets| {
            SpecRecord::Grant(GrantRequest {
                kind: GrantKind::Delegated,
                principal,
                subject,
                ops: NonEmptySet::new(ops.iter().copied().collect()).unwrap(),
                per_operation: None,
                per_window: Some(bound),
                assets,
                counterparties: None,
                parent: None,
                expires: None,
            })
        };
        for i in &identities {
            records.push(grant(
                services[0],
                Subject::Identity(*i),
                &[OperationKind::OpenAccount, OperationKind::Transfer],
                None,
            ));
        }
        for a in &accounts {
            records.push(grant(
                services[1],
                Subject::Account(*a),
                &[OperationKind::Transfer],
                Some(NonEmptySet::one(copm())),
            ));
        }
    }
    records.push(SpecRecord::IssuanceGrant {
        request: GrantRequest {
            kind: GrantKind::Issuance,
            principal: identities[0],
            subject: Subject::Asset(copm()),
            ops: NonEmptySet::new(BTreeSet::from([
                OperationKind::Issue,
                OperationKind::Redeem,
            ]))
            .unwrap(),
            per_operation: None,
            per_window: Some(window),
            assets: Some(NonEmptySet::one(copm())),
            counterparties: None,
            parent: None,
            expires: None,
        },
        justification: b"seeded genesis: issuance authority over COPM".to_vec(),
    });
    // Every third identity's existing passkeys: one, or two for every sixth (ruling `mn7`).
    let mut imports: Vec<(IdentityId, Vec<Verifier>)> = identities
        .iter()
        .enumerate()
        .skip(1)
        .step_by(3)
        .map(|(i, identity)| {
            let n = if i % 2 == 0 { 2 } else { 1 };
            (*identity, (0..n).map(|k| passkey(i as u64, k)).collect())
        })
        .collect();
    imports.sort_by_key(|(i, _)| *i);
    for (identity, credentials) in imports {
        records.push(SpecRecord::Credentials {
            identity,
            credentials,
        });
    }
    // The reconciliation: one source, stating every row in its own decimal figures.
    let by_account: BTreeMap<AccountId, Vec<u8>> =
        aliases.iter().map(|(r, a)| (*a, r.clone())).collect();
    let set = OpeningSet::new(
        rows.clone(),
        aliases
            .iter()
            .map(|(r, a)| ((source, r.clone()), *a))
            .collect(),
    )
    .unwrap();
    let extract = SourceExtract::new(
        source,
        rows.iter()
            .map(|r| SourceRow {
                source_ref: by_account[&r.account].clone(),
                asset: r.asset,
                amount: format!("{}.{:02}", r.amount.get() / 100, r.amount.get() % 100),
            })
            .collect(),
    );
    let rec = Reconciliation {
        set_digest: set.digest(),
        as_of: 5,
        sources: vec![extract],
    };
    let history = history::HistoryTree::new(&[]).unwrap().commit([0x77; 32]);
    let seed =
        ChainSpec::build(g, 50, assets(), records, Some(rec), Some(history), limits).unwrap();
    let mut required = RequiredSources::new();
    required.insert(copm(), NonEmptySet::one(source));
    Synthetic {
        seed,
        limits,
        required,
        identities,
        accounts,
        source,
        services,
    }
}
