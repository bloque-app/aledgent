// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Seeded genesis through the kernel (`F-036` T-036-03 to T-036-06, `REQ-RC-6`, `REQ-RC-4`,
//! `REQ-RT-3`): the seeded book is the book the same records make as single commands; a seeding
//! interrupted anywhere resumes to the same roots; nothing but the seeding is taken before the
//! seal; the export carries the seed's record.

use super::*;
use crate::chainspec::testing::{Synthetic, assets, copm, synthetic};
use crate::chainspec::{self, ChunkLimits, Section, SpecRecord};
use crate::leaves::{LeafHost, LeafMap, kind};
use crate::opening::{OpeningSet, RequiredSources};
use ledger_kernel::keys::KeyRing;
use ledger_kernel::posting::OperationRef;
use ledger_kernel::store::MemStore;
use ledger_kernel::vault::MemVault;

type L = Ledger<MemStore, MemVault>;

const MAX_LEGS: usize = 4;

fn g() -> OperationId {
    OperationId::from_ref(OperationRef([9; 16]))
}

fn limits() -> ChunkLimits {
    ChunkLimits {
        max_records: 9,
        max_bytes: 3 * 1024,
        opening_batch: MAX_LEGS as u32,
    }
}

fn fixture() -> Synthetic {
    synthetic(g(), 30, 13, limits())
}

fn lifecycle(required: &RequiredSources) -> crate::lifecycle::LifecycleConfig {
    let mut c = crate::lifecycle::LifecycleConfig::new([7; 16], 60, 3_600).with_max_legs(MAX_LEGS);
    for (asset, sources) in required {
        c = c.with_opening_sources(*asset, sources.clone());
    }
    c
}

/// A book founded on `seed` (or on nothing).
fn book(s: &Synthetic, seeded: bool) -> (L, Founded) {
    let book = Book::bootstrap(
        MemStore::new(),
        MemVault::new(),
        KeyRing::from_root([7; 32]),
    )
    .unwrap();
    let genesis = Genesis {
        assets: assets(),
        operator_verifier: Verifier::new(vec![0]),
        lifecycle: lifecycle(&s.required),
        chainspec: seeded.then(|| s.seed.commitment()),
        spec: None,
    };
    Ledger::genesis(book, genesis, g(), 1).unwrap()
}

fn operator(f: &Founded) -> Caller {
    Caller::new(f.operator, f.operator_credential)
}

fn apply(l: &mut L, f: &Founded, s: &Synthetic, chunks: core::ops::Range<u32>) {
    let bytes = s.seed.chunk_bytes();
    let paths = s.seed.paths();
    for i in chunks {
        let ctx = Ctx {
            caller: operator(f),
            op: chainspec::chunk_op(&g(), i),
            now: 50,
        };
        l.apply_spec_chunk(&ctx, &bytes[i as usize], &paths[i as usize])
            .unwrap_or_else(|e| panic!("chunk {i}: {e:?}"));
    }
}

fn seeded() -> (L, Founded, Synthetic) {
    let s = fixture();
    let (mut l, f) = book(&s, true);
    apply(&mut l, &f, &s, 0..s.seed.chunk_count());
    (l, f, s)
}

/// Every leaf of the service store, by (kind, key).
fn leaves(l: &L) -> BTreeMap<(u8, Vec<u8>), Vec<u8>> {
    let mut maps: Vec<&dyn LeafMap> = Vec::new();
    l.state().leaf_maps(&mut maps);
    let mut out = BTreeMap::new();
    for m in maps {
        for (k, v) in m.entries_encoded() {
            out.insert((m.kind(), k), v);
        }
    }
    out
}

#[test]
fn req_rc_6_a_seed_applies_chunk_by_chunk_and_seals() {
    let (l, f, s) = seeded();
    let p = l.spec_progress().unwrap();
    assert!(p.sealed);
    assert_eq!(p.applied, s.seed.chunk_count());
    assert!(s.seed.chunk_count() > 5, "the fixture spans several chunks");
    // Every identity, account and alias; every opening at its exact amount.
    for (i, id) in s.identities.iter().enumerate() {
        assert!(l.state().identity(id).is_some(), "identity {i}");
    }
    for a in &s.accounts {
        let acct = l.state().account(a).unwrap();
        assert_eq!(acct.custody(), Custody::Delegated);
        assert_eq!(acct.owner(), l.state().account(a).unwrap().owner());
    }
    for (i, a) in s.accounts.iter().enumerate() {
        let r = alloc::format!("legacy-{i:07}");
        assert_eq!(l.resolve_alias(&s.source, r.as_bytes()), Some(*a));
    }
    let set: OpeningSet = s.seed.opening_set().unwrap().unwrap();
    for row in set.rows() {
        let v = l.balance_view(&row.account, &copm()).unwrap();
        assert_eq!(v.available, i128::from(row.amount.get()));
    }
    let total: u128 = set.totals()[&copm()];
    let position = l.opening_position(&copm()).unwrap();
    assert_eq!(
        l.balance_view(&position, &copm()).unwrap().available,
        -(total as i128)
    );
    // After the seal the book is an ordinary one: the operator opens an identity.
    let ctx = Ctx {
        caller: operator(&f),
        op: OperationId::from_ref(OperationRef([0xEE; 16])),
        now: 60,
    };
    let mut l = l;
    assert!(
        l.open_identity(&ctx, IdentityKind::Person, None, Verifier::new(vec![1]))
            .is_ok()
    );
}

#[test]
fn req_rc_6_before_the_seal_only_the_seeding_is_taken() {
    let s = fixture();
    let (mut l, f) = book(&s, true);
    let try_open = |l: &mut L, n: u8| {
        let ctx = Ctx {
            caller: operator(&f),
            op: OperationId::from_ref(OperationRef([n; 16])),
            now: 60,
        };
        l.open_identity(&ctx, IdentityKind::Person, None, Verifier::new(vec![1]))
    };
    assert_eq!(
        try_open(&mut l, 0xE1).unwrap_err(),
        Refusal::InitialStateNotSealed
    );
    apply(&mut l, &f, &s, 0..3);
    assert_eq!(
        try_open(&mut l, 0xE2).unwrap_err(),
        Refusal::InitialStateNotSealed
    );
    // A seeded identity's own credential is refused too.
    let id = s.identities[1];
    let cred =
        crate::id::CredentialId::mint(&chainspec::spec_op(&g(), Section::Identities as u8, 1), 1);
    let ctx = Ctx {
        caller: Caller::new(id, cred),
        op: OperationId::from_ref(OperationRef([0xE3; 16])),
        now: 60,
    };
    assert_eq!(
        l.open_account(
            &ctx,
            OpenAccount {
                owner: id,
                kind: AccountKind::Customer,
                custody: Custody::Delegated,
                treasury_limits: BTreeMap::new(),
                acknowledged: None,
            }
        )
        .unwrap_err(),
        Refusal::InitialStateNotSealed
    );
    // Nor may that identity apply a chunk: only the operator seeds.
    let bytes = s.seed.chunk_bytes();
    let paths = s.seed.paths();
    assert_eq!(
        l.apply_spec_chunk(&ctx, &bytes[3], &paths[3]).unwrap_err(),
        Refusal::NotAuthorised
    );
    // A chunk out of order, or not the committed one.
    let op = Ctx {
        caller: operator(&f),
        op: chainspec::chunk_op(&g(), 5),
        now: 60,
    };
    assert_eq!(
        l.apply_spec_chunk(&op, &bytes[5], &paths[5]).unwrap_err(),
        Refusal::InvalidTransition
    );
    let mut forged = bytes[3].clone();
    let last = forged.len() - 1;
    forged[last] ^= 1;
    assert!(matches!(
        l.apply_spec_chunk(&op, &forged, &paths[3]).unwrap_err(),
        Refusal::EvidenceInsufficient | Refusal::MalformedCommand
    ));
    apply(&mut l, &f, &s, 3..s.seed.chunk_count());
    assert!(try_open(&mut l, 0xE4).is_ok());
}

#[test]
fn nfr_17_a_book_founded_without_a_seed_takes_none_and_a_sealed_one_no_more() {
    let s = fixture();
    let (mut l, f) = book(&s, false);
    let bytes = s.seed.chunk_bytes();
    let paths = s.seed.paths();
    let ctx = Ctx {
        caller: operator(&f),
        op: chainspec::chunk_op(&g(), 0),
        now: 60,
    };
    assert_eq!(
        l.apply_spec_chunk(&ctx, &bytes[0], &paths[0]).unwrap_err(),
        Refusal::InvalidTransition
    );
    let (mut l, f, s) = seeded();
    let ctx = Ctx {
        caller: operator(&f),
        op: chainspec::chunk_op(&g(), 0),
        now: 60,
    };
    // A chunk applied before answers as it did, and writes nothing.
    let before = leaves(&l);
    let again = l.apply_spec_chunk(&ctx, &bytes[1], &paths[1]).unwrap();
    assert_eq!(again.value.index, 1);
    assert!(again.value.sealed);
    assert_eq!(leaves(&l), before);
    // No other seed can be committed: the record holds one, for ever.
    assert!(
        l.state
            .check(&[Write::ChainSpec(crate::chainspec::SpecWrite::Commit(
                s.seed.commitment()
            ))])
            .is_err()
    );
}

#[test]
fn req_rt_3_an_interrupted_seeding_resumes_to_the_same_roots() {
    let (whole, _, s) = seeded();
    let n = s.seed.chunk_count();
    for k in [1, 2, n / 2, n - 1] {
        let (mut l, f) = book(&s, true);
        apply(&mut l, &f, &s, 0..k);
        // The operator's tool re-sends from the start: what was applied answers, the rest applies.
        apply(&mut l, &f, &s, 0..n);
        assert_eq!(
            l.book().root().unwrap(),
            whole.book().root().unwrap(),
            "k={k}"
        );
        assert_eq!(leaves(&l), leaves(&whole), "k={k}");
        assert_eq!(l.state().chain_head(), whole.state().chain_head(), "k={k}");
    }
}

#[test]
fn req_rc_6_a_refused_record_takes_its_whole_chunk_back() {
    let s = fixture();
    // An account whose owner exists nowhere: the chunk that holds it is refused whole.
    let mut records = s.seed.records().to_vec();
    let i = records
        .iter()
        .position(|r| matches!(r, SpecRecord::Account { .. }))
        .unwrap();
    if let SpecRecord::Account { owner, .. } = &mut records[i + 3] {
        *owner = IdentityId::mint(&OperationId::from_ref(OperationRef([0xAB; 16])), 0);
    }
    let h = s.seed.header();
    let bad = chainspec::ChainSpec::build(
        h.genesis_op,
        h.at,
        h.assets.clone(),
        records,
        s.seed.reconciliation().cloned(),
        h.history,
        s.limits,
    );
    // The changed owner changes nothing the builder checks but the commitment; the alias and
    // opening set are unchanged, so it builds.
    let bad = Synthetic {
        seed: bad.unwrap(),
        ..fixture()
    };
    let (mut l, f) = book(&bad, true);
    let bytes = bad.seed.chunk_bytes();
    let paths = bad.seed.paths();
    let mut refused = None;
    for c in 0..bad.seed.chunk_count() {
        let ctx = Ctx {
            caller: operator(&f),
            op: chainspec::chunk_op(&g(), c),
            now: 60,
        };
        let before = leaves(&l);
        match l.apply_spec_chunk(&ctx, &bytes[c as usize], &paths[c as usize]) {
            Ok(_) => {}
            Err(e) => {
                assert_eq!(leaves(&l), before, "nothing of chunk {c} stays");
                assert_eq!(l.spec_progress().unwrap().applied, c);
                refused = Some(e);
                break;
            }
        }
    }
    assert_eq!(refused, Some(Refusal::NotAuthorised));
}

#[test]
fn req_rc_4_the_seeded_book_is_the_book_the_same_records_make_as_commands() {
    let (a, _, s) = seeded();
    // The same records, one command each, under the same references and time, on a book
    // founded without a seed on the same genesis.
    let (mut b, f) = book(&s, false);
    let at = s.seed.header().at;
    let mut seen = chainspec::SpecCounts::default();
    let ctx_of = |section: Section, n: u64| Ctx {
        caller: operator(&f),
        op: chainspec::spec_op(&g(), section as u8, n),
        now: at,
    };
    let mut openings = Vec::new();
    for r in s.seed.records() {
        let section = r.section();
        let n = seen.get(section);
        *seen.of(section) += 1;
        let ctx = ctx_of(section, n);
        match r.clone() {
            SpecRecord::Identity { kind, verifier } => {
                b.open_identity(&ctx, kind, None, verifier).unwrap();
            }
            SpecRecord::Account { owner, kind } => {
                b.open_account(
                    &ctx,
                    OpenAccount {
                        owner,
                        kind,
                        custody: Custody::Delegated,
                        treasury_limits: BTreeMap::new(),
                        acknowledged: None,
                    },
                )
                .unwrap();
            }
            SpecRecord::Alias { .. } => {}
            SpecRecord::Opening(row) => openings.push(row),
            SpecRecord::OperatorGrant {
                kinds,
                expires,
                justification,
            } => {
                b.grant_operator(&ctx, f.operator, kinds, expires, &justification)
                    .unwrap();
                // The openings, as one load, once the operator holds the kind to load them.
                let set = s.seed.opening_set().unwrap().unwrap();
                let load = Ctx {
                    caller: operator(&f),
                    op: chainspec::opening_op(&g()),
                    now: at,
                };
                let r = b
                    .load_opening_balances(
                        &load,
                        &set,
                        s.seed.reconciliation().unwrap(),
                        b"the same set, as one load",
                        None,
                    )
                    .unwrap();
                assert!(r.value.complete);
            }
            SpecRecord::Grant(request) => {
                b.grant_authority(&ctx, request).unwrap();
            }
            SpecRecord::IssuanceGrant {
                request,
                justification,
            } => {
                b.grant_issuance(&ctx, request, &justification).unwrap();
            }
            SpecRecord::Credentials {
                identity,
                credentials,
            } => {
                b.import_credentials(
                    &ctx,
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
                )
                .unwrap();
            }
        }
    }
    assert!(s.seed.header().counts.credentials > 0);
    assert_eq!(openings.len() as u64, s.seed.header().counts.openings);
    // The kernel's book is the same, posting for posting.
    assert_eq!(a.book().root().unwrap(), b.book().root().unwrap());
    // The service store is the same, leaf for leaf, but for what only one of them has:
    // the seed's own record and aliases, the load's record of its set, and the records and
    // client-key entries of the chunks and of the opening reference (one load's in `b`, the
    // seed's in `a`).
    let own: alloc::collections::BTreeSet<Vec<u8>> = (0..s.seed.chunk_count())
        .map(|i| chainspec::chunk_op(&g(), i))
        .chain([chainspec::opening_op(&g())])
        .map(|op| crate::export::canon::to_bytes(&op))
        .collect();
    let keep = |m: BTreeMap<(u8, Vec<u8>), Vec<u8>>| -> BTreeMap<(u8, Vec<u8>), Vec<u8>> {
        m.into_iter()
            .filter(|((k, key), _)| {
                ![kind::CHAINSPEC, kind::SPEC_ALIAS, kind::OPENING_LOAD].contains(k)
                    && !([kind::ATTRIBUTION, kind::CLIENT_KEY].contains(k) && own.contains(key))
            })
            .collect()
    };
    let (la, lb) = (keep(leaves(&a)), keep(leaves(&b)));
    let only_a: Vec<_> = la
        .keys()
        .filter(|k| !lb.contains_key(*k))
        .map(|k| k.0)
        .collect();
    let only_b: Vec<_> = lb
        .keys()
        .filter(|k| !la.contains_key(*k))
        .map(|k| k.0)
        .collect();
    let differ: Vec<_> = la
        .iter()
        .filter(|(k, v)| lb.get(*k).is_some_and(|w| w != *v))
        .map(|(k, _)| k.0)
        .collect();
    assert!(
        only_a.is_empty() && only_b.is_empty() && differ.is_empty(),
        "kinds only seeded {only_a:?}, only by commands {only_b:?}, different {differ:?}"
    );
}

#[test]
fn t_014_01_the_export_carries_the_seed_and_rebuilds_it() {
    let (l, _, _) = seeded();
    let sections = l.state().sections();
    let head = l.state().chain_head();
    let back = State::from_sections(&sections, &head).unwrap();
    assert_eq!(back.sections(), sections);
    assert_eq!(back.chainspec(), l.state().chainspec());
    // An export made before seeded genesis (34 sections) still imports, as a book without one.
    let older: Vec<_> = sections[..34].to_vec();
    let back = State::from_sections(&older, &head).unwrap();
    assert!(back.chainspec().progress().is_none());
}

#[test]
fn f_018_the_signals_show_the_seeding() {
    // Two records a chunk: the operator's grants and the grants after them fall in chunks of
    // their own, so the operator holds `ReadSignals` before the last chunk.
    let s = synthetic(
        g(),
        30,
        13,
        ChunkLimits {
            max_records: 2,
            ..limits()
        },
    );
    let (mut l, f) = book(&s, true);
    apply(&mut l, &f, &s, 0..s.seed.chunk_count() - 1);
    let p = l.spec_progress().unwrap();
    assert!(!p.sealed);
    assert_eq!(p.applied, s.seed.chunk_count() - 1);
    // The operator gave itself `ReadSignals` in the seed's operator grants: the signals read
    // through the seal's guard.
    let sig = l.signals(&operator(&f), 70).unwrap();
    let seeding = sig.seeding.unwrap();
    assert_eq!(
        (seeding.applied, seeding.chunks, seeding.sealed),
        (s.seed.chunk_count() - 1, s.seed.chunk_count(), false)
    );
}

/// A book founded on the chainspec itself (owner ruling `genesis-chainspec`).
fn built(s: &Synthetic) -> Result<(L, Founded), GenesisError> {
    let book = Book::bootstrap(
        MemStore::new(),
        MemVault::new(),
        KeyRing::from_root([7; 32]),
    )
    .unwrap();
    Ledger::genesis(
        book,
        Genesis {
            assets: assets(),
            operator_verifier: Verifier::new(vec![0]),
            lifecycle: lifecycle(&s.required),
            chainspec: Some(s.seed.commitment()),
            spec: Some(alloc::sync::Arc::new(s.seed.clone())),
        },
        g(),
        1,
    )
}

#[test]
fn genesis_chainspec_the_built_genesis_is_the_chunk_by_chunk_book_and_born_sealed() {
    let (a, _, s) = seeded();
    let (b, _) = built(&s).unwrap();
    assert!(b.spec_progress().unwrap().sealed);
    assert_eq!(a.book().root().unwrap(), b.book().root().unwrap());
    assert_eq!(leaves(&a), leaves(&b));
    assert_eq!(a.state().chain_head(), b.state().chain_head());
    // Born sealed: an ordinary command is decided at once.
    let (mut b, f) = built(&s).unwrap();
    let ctx = Ctx {
        caller: operator(&f),
        op: OperationId::from_ref(OperationRef([0xE5; 16])),
        now: 60,
    };
    assert!(
        b.open_identity(&ctx, IdentityKind::Person, None, Verifier::new(vec![1]))
            .is_ok()
    );
}

#[test]
fn a0023_the_book_keeps_the_genesis_code_version() {
    let mut s = fixture();
    let manifest = crate::code::sample(b"core.vos", b"runtime.pvm", b"core-host");
    let h0 = manifest.version();
    s.seed = s
        .seed
        .with_code(chainspec::GenesisCode {
            manifest,
            construction: 2,
        })
        .unwrap();
    let (b, _) = built(&s).unwrap();
    let facts = b.spec_progress().unwrap().header.clone().unwrap();
    assert_eq!(facts.code, Some((h0, 2)));
}

#[test]
fn genesis_chainspec_another_chainspec_than_genesis_commits_founds_nothing() {
    let s = fixture();
    let other = synthetic(g(), 31, 13, limits());
    let book = Book::bootstrap(
        MemStore::new(),
        MemVault::new(),
        KeyRing::from_root([7; 32]),
    )
    .unwrap();
    let r = Ledger::genesis(
        book,
        Genesis {
            assets: assets(),
            operator_verifier: Verifier::new(vec![0]),
            lifecycle: lifecycle(&s.required),
            chainspec: Some(s.seed.commitment()),
            spec: Some(alloc::sync::Arc::new(other.seed)),
        },
        g(),
        1,
    );
    assert!(matches!(r, Err(GenesisError::ChainSpec(None))));
}

#[test]
fn mn7_imported_passkeys_join_the_identitys_set_beside_the_operator_key() {
    let (b, _) = built(&fixture()).unwrap();
    let s = fixture();
    let mut imported = 0;
    for r in s.seed.records() {
        if let SpecRecord::Credentials {
            identity,
            credentials,
        } = r
        {
            let set: Vec<_> = b.state().credentials_of(identity).collect();
            // The operator-held key, then each passkey, marked imported.
            assert_eq!(set.len(), 1 + credentials.len());
            for v in credentials {
                let c = set.iter().find(|c| c.verifier() == v).unwrap();
                assert_eq!(c.marker(), crate::principal::CredentialMarker::Imported);
                assert_eq!(c.verifier().scheme(), crate::scheme::Scheme::WebAuthnEs256);
            }
            imported += credentials.len();
        }
    }
    assert!(imported > 0);
    // An identity without one keeps the operator key only.
    let without = s.identities[2];
    assert_eq!(b.state().credentials_of(&without).count(), 1);
}

#[test]
fn mn7_a_credential_set_that_is_empty_or_repeats_is_refused() {
    let s = fixture();
    let h = s.seed.header();
    for bad in [
        Vec::new(),
        vec![
            chainspec::testing::passkey(1, 0),
            chainspec::testing::passkey(1, 0),
        ],
    ] {
        let mut records = s.seed.records().to_vec();
        let i = records
            .iter()
            .position(|r| matches!(r, SpecRecord::Credentials { .. }))
            .unwrap();
        if let SpecRecord::Credentials { credentials, .. } = &mut records[i] {
            *credentials = bad;
        }
        let r = chainspec::ChainSpec::build(
            h.genesis_op,
            h.at,
            h.assets.clone(),
            records,
            s.seed.reconciliation().cloned(),
            h.history,
            s.limits,
        );
        assert_eq!(r.unwrap_err(), chainspec::SpecError::Credentials);
    }
}

// ------------------------------------------------- T-036-30: amendment 0025's grants of the seed

/// `T-036-30` (`REQ-ID-6`, `REQ-RC-6`, amendment 0025 `a0025-q3`, `a0025-q4`, R4): the seed opens the
/// service principals and carries their grants, issued by the operator as the identities principal.
/// Once sealed, the custodian opens an account for a migrated identity and the issuer moves value from a
/// migrated account, with no grant made after genesis; each within its bound, and nothing broader.
#[test]
fn t_036_30_seeded_services_act_under_the_seeds_grants_with_no_runtime_grant() {
    use crate::authority::GrantKind;
    use chainspec::testing::{SERVICE_LIMIT, synthetic_with_services};
    let s = synthetic_with_services(g(), 30, 13, limits());
    let (mut l, f) = book(&s, true);
    apply(&mut l, &f, &s, 0..s.seed.chunk_count());
    assert!(l.spec_progress().unwrap().sealed);
    // The native genesis is the same book (owner ruling `genesis-chainspec`).
    let (b, _) = built(&s).unwrap();
    assert_eq!(l.book().root().unwrap(), b.book().root().unwrap());
    assert_eq!(leaves(&l), leaves(&b));

    let n = s.identities.len() as u64;
    let (custodian, issuer) = (s.services[0], s.services[1]);
    for (k, who) in s.services.iter().enumerate() {
        let i = l.state().identity(who).expect("a seeded service principal");
        assert_eq!(i.kind(), IdentityKind::Organisation);
        assert!(
            l.state()
                .credentials_of(who)
                .any(|c| c.id() == chainspec::identity_credential_of(&g(), n + k as u64))
        );
    }
    // The grants: the operator's (ordinal 0), one over every identity to the custodian, then one over
    // every account to the issuer; all delegated, by the operator, with no parent.
    let grant = |ordinal: u64| {
        l.state()
            .grant(&chainspec::grant_of(&g(), Section::Grants, ordinal))
            .unwrap()
            .clone()
    };
    for (k, i) in s.identities.iter().enumerate() {
        let gr = grant(1 + k as u64);
        assert_eq!(
            (
                gr.kind(),
                gr.grantor(),
                gr.principal(),
                gr.subject(),
                gr.parent()
            ),
            (
                GrantKind::Delegated,
                f.operator,
                custodian,
                Subject::Identity(*i),
                None
            )
        );
        assert!(gr.ops().contains(&OperationKind::OpenAccount));
        assert_eq!(
            gr.bounds().per_window().map(|w| w.limit.get().get()),
            Some(SERVICE_LIMIT)
        );
    }
    for (k, a) in s.accounts.iter().enumerate() {
        let gr = grant(1 + n + k as u64);
        assert_eq!(
            (
                gr.kind(),
                gr.grantor(),
                gr.principal(),
                gr.subject(),
                gr.parent()
            ),
            (
                GrantKind::Delegated,
                f.operator,
                issuer,
                Subject::Account(*a),
                None
            )
        );
        assert_eq!(gr.bounds().assets(), Some(&NonEmptySet::one(copm())));
    }

    let as_ = |who: IdentityId, k: u64, op: u8| Ctx {
        caller: Caller::new(who, chainspec::identity_credential_of(&g(), n + k)),
        op: OperationId::from_ref(OperationRef([op; 16])),
        now: 100,
    };
    // The custodian opens a Customer account in delegated custody for a migrated identity.
    let user = s.identities[3];
    let opened = l
        .open_account(
            &as_(custodian, 0, 0xF1),
            OpenAccount {
                owner: user,
                kind: AccountKind::Customer,
                custody: Custody::Delegated,
                treasury_limits: BTreeMap::new(),
                acknowledged: None,
            },
        )
        .expect("the custodian opens an account under the seed's grant over the identity");
    assert_eq!(
        opened.record.authorities[0].grant,
        Some(chainspec::grant_of(&g(), Section::Grants, 1 + 3))
    );
    assert_eq!(l.state().account(&opened.value).unwrap().owner(), user);
    // The issuer moves value from a migrated account, within its bound, and not beyond it.
    let (from, to) = (s.accounts[5], s.accounts[6]);
    let t = |amount: u64| Transfer::single(from, to, copm(), Minor::new(amount)).until(10_000);
    l.transfer(&as_(issuer, 1, 0xF2), &t(300))
        .expect("the issuer transfers under the seed's grant over the account");
    assert_eq!(
        l.transfer(&as_(issuer, 1, 0xF3), &t(SERVICE_LIMIT)).err(),
        Some(Refusal::BoundExceeded),
        "the window's limit, the seed's own"
    );
    // Nothing broader: the custodian's grant over the identity reaches no account, the issuer opens none.
    assert_eq!(
        l.transfer(&as_(custodian, 0, 0xF4), &t(1)).err(),
        Some(Refusal::UnknownAccount)
    );
    assert!(
        l.open_account(
            &as_(issuer, 1, 0xF5),
            OpenAccount {
                owner: user,
                kind: AccountKind::Customer,
                custody: Custody::Delegated,
                treasury_limits: BTreeMap::new(),
                acknowledged: None,
            },
        )
        .is_err()
    );
    // The account the custodian opened: the issuer reaches it only through a grant the custodian makes under its
    // own (R4), as at runtime.
    let new = opened.value;
    let narrow = GrantRequest {
        kind: GrantKind::Delegated,
        principal: issuer,
        subject: Subject::Account(new),
        ops: NonEmptySet::one(OperationKind::Transfer),
        per_operation: None,
        per_window: Some(crate::authority::Window {
            length: NonZeroU64::new(86_400_000).unwrap(),
            limit: NonZeroMinor::new(Minor::new(10)).unwrap(),
        }),
        assets: None,
        counterparties: None,
        parent: Some(chainspec::grant_of(&g(), Section::Grants, 1 + 3)),
        expires: None,
    };
    l.grant_authority(&as_(custodian, 0, 0xF6), narrow).expect(
        "the custodian grants the issuer over the account it opened, under the seed's grant",
    );
}

/// `T-018-11` (the trigger of `T-036-17`): the signals count a seeded book's records in its
/// chainspec's unit, from the collections' own counts: its chainspec's records and the rows
/// genesis itself writes; one more for each later command; and the leaves the replica holds.
#[test]
fn t_018_11_the_book_size_is_its_chainspecs_records_and_one_more_a_command() {
    let s = fixture();
    let genesis = s.seed.records().len() as u64;
    // Beside the chainspec's records, genesis writes the operator's identity, the issuance
    // counterparty and an opening position per asset of the opening set.
    let positions = s.seed.header().opening.as_ref().unwrap().totals.len() as u64;
    let own = 2 + positions;
    let (a, fa, _) = seeded();
    let (mut b, f) = built(&s).unwrap();
    for (l, f) in [(&a, &fa), (&b, &f)] {
        let size = l.signals(&operator(f), 60).unwrap().book;
        assert_eq!(size.genesis_records, Some(genesis));
        assert_eq!(size.records, genesis + own);
        assert_eq!(size.leaves, leaves(l).len() as u64);
        // Each collection's count is the leaves a visit finds of its kind.
        let mut visited: BTreeMap<u8, u64> = BTreeMap::new();
        l.state()
            .for_each_leaf(&mut |k, _, _| *visited.entry(k).or_default() += 1);
        assert!(visited.len() > 10);
        for k in l.state().leaf_kinds() {
            let n = visited.get(&k).copied().unwrap_or(0);
            assert_eq!(l.state().leaf_count_of(k), n, "kind {k}");
        }
    }
    let before = b.signals(&operator(&f), 60).unwrap().book;
    let ctx = Ctx {
        caller: operator(&f),
        op: OperationId::from_ref(OperationRef([0xE6; 16])),
        now: 60,
    };
    let person = b
        .open_identity(&ctx, IdentityKind::Person, None, Verifier::new(vec![1]))
        .unwrap()
        .value
        .identity;
    let after = b.signals(&operator(&f), 61).unwrap().book;
    assert_eq!(after.records, before.records + 1);
    // A grant is one more.
    let ctx = Ctx {
        caller: operator(&f),
        op: OperationId::from_ref(OperationRef([0xE8; 16])),
        now: 61,
    };
    b.grant_operator(
        &ctx,
        person,
        crate::set::NonEmptySet::new([crate::operator::OperatorOpKind::ReadSignals].into())
            .unwrap(),
        None,
        b"the signals",
    )
    .unwrap();
    let granted = b.signals(&operator(&f), 62).unwrap().book;
    assert_eq!(granted.records, after.records + 1);
    assert_eq!(after.genesis_records, before.genesis_records);
    assert!(after.leaves > before.leaves);
    assert_eq!(granted.leaves, leaves(&b).len() as u64);
    // A book founded without a chainspec has no genesis records: its own rows, and the
    // operator-surface grant that lets the operator read the signals.
    let (mut u, fu) = book(&s, false);
    let ctx = Ctx {
        caller: operator(&fu),
        op: OperationId::from_ref(OperationRef([0xE7; 16])),
        now: 60,
    };
    u.grant_operator(
        &ctx,
        fu.operator,
        crate::set::NonEmptySet::new([crate::operator::OperatorOpKind::ReadSignals].into())
            .unwrap(),
        None,
        b"the signals",
    )
    .unwrap();
    let size = u.signals(&operator(&fu), 60).unwrap().book;
    assert_eq!(size.genesis_records, None);
    assert_eq!(size.records, 3);
    assert_eq!(size.leaves, leaves(&u).len() as u64);
}
