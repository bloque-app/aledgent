// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-036` — Seeded genesis (amendment 0022 draft, `REQ-RC-6`, Epic N): its registered cases
//! (`F-015` §6, §7).
//!
//! Each case founds its own book on a seed, in process, through `ledger-core`'s public surface:
//! the operator's path applies the chunks as `aledgent-core-host serve --seed` does
//! (`Ledger::apply_spec_chunk`, behind `InputBody::SpecChunk`). A case that builds its own deployment
//! proves nothing about the Agent target, and the runner says so (`Harness::exercised`). The
//! seeded roots against the same records as single commands, and the export of a seeded book,
//! need the library's insides: `ledger-core`'s `ledger::tests::seeding`.

// Case names end in `__<layer>`, so a test can be found by id and layer.
#![allow(non_snake_case)]

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;

use ledger_core::Refusal;
use ledger_core::account::AccountKind;
use ledger_core::asset::{AssetId, AssetKind, Minor, NonZeroMinor};
use ledger_core::authority::{GrantKind, OperationKind, Subject, Window};
use ledger_core::chainspec::{self, ChainSpec, ChunkLimits, SpecError, SpecRecord};
use ledger_core::id::OperationId;
use ledger_core::identity::IdentityKind;
use ledger_core::ledger::{AssetDecl, Ctx, Founded, Genesis, GrantRequest, Ledger};
use ledger_core::lifecycle::LifecycleConfig;
use ledger_core::opening::{
    OpeningRow, OpeningSet, OpeningSource, Reconciliation, RequiredSources, SourceExtract,
    SourceRow,
};
use ledger_core::principal::{Caller, Verifier};
use ledger_core::set::NonEmptySet;
use ledger_kernel::book::Book;
use ledger_kernel::keys::KeyRing;
use ledger_kernel::posting::OperationRef;
use ledger_kernel::store::MemStore;
use ledger_kernel::vault::MemVault;

use crate::case::{Case, Feature, Layer, Outcome, Port};
use crate::harness::Harness;

macro_rules! service_case {
    ($id:literal, $name:ident, $traces:expr) => {
        Case::run(
            $id,
            stringify!($name),
            Layer::Service,
            Feature::F036,
            Port::Service,
            $traces,
            $name,
        )
    };
}

/// `F-036`'s registered cases.
pub const CASES: &[Case] = &[
    service_case!(
        "AC-N2.1",
        ac_n2_1_a_seed_applies_in_order_and_seals__service,
        &["REQ-RC-6", "REQ-RC-4", "REQ-AU-7"]
    ),
    service_case!(
        "AC-N2.2",
        ac_n2_2_nothing_but_the_seed_before_the_seal__service,
        &["REQ-RC-6", "ERR-InitialStateNotSealed"]
    ),
    service_case!(
        "ERR-InitialStateNotSealed",
        err_initial_state_not_sealed__service,
        &["REQ-RC-6", "REQ-ER-1"]
    ),
    service_case!(
        "AC-N2.3",
        ac_n2_3_a_part_not_committed_or_applied_takes_no_effect__service,
        &["REQ-RC-6", "INV-18", "ERR-EvidenceInsufficient"]
    ),
    service_case!(
        "AC-N2.4",
        ac_n2_4_an_interrupted_seeding_resumes_to_the_same_book__service,
        &["REQ-RC-6", "REQ-RT-3"]
    ),
    service_case!(
        "AC-N2.5",
        ac_n2_5_a_divergent_opening_set_is_refused_before_any_part__service,
        &["REQ-RC-4", "REQ-RC-6", "NFR-9"]
    ),
];

type L = Ledger<MemStore, MemVault>;

fn g() -> OperationId {
    OperationId::from_ref(OperationRef([0x36; 16]))
}

fn copm() -> AssetId {
    AssetId::parse("COPM").expect("an asset symbol")
}

fn source() -> OpeningSource {
    OpeningSource([0x5A; 16])
}

fn required() -> RequiredSources {
    let mut r = RequiredSources::new();
    r.insert(copm(), NonEmptySet::one(source()));
    r
}

fn assets() -> Vec<AssetDecl> {
    vec![AssetDecl {
        id: copm(),
        location: None,
        precision: 2,
        kind: AssetKind::Stable,
        deprecated: false,
    }]
}

/// A seed of `n` identities with an account each, an alias per account, some openings, and the
/// operator's grant over the delegated accounts. `skew` adds to one source figure: a divergence.
fn seed(n: u64, skew: u64) -> ChainSpec {
    let mut records = Vec::new();
    for i in 0..n {
        records.push(SpecRecord::Identity {
            kind: if i == 0 {
                IdentityKind::Organisation
            } else {
                IdentityKind::Person
            },
            verifier: Verifier::new(vec![0x5E; 32]),
        });
    }
    let accounts: Vec<_> = (0..n).map(|i| chainspec::account_of(&g(), i)).collect();
    for i in 0..n {
        records.push(SpecRecord::Account {
            owner: chainspec::identity_of(&g(), i),
            kind: AccountKind::Customer,
        });
    }
    let mut aliases: Vec<(Vec<u8>, _)> = accounts
        .iter()
        .enumerate()
        .map(|(i, a)| (format!("legacy-{i:05}").into_bytes(), *a))
        .collect();
    aliases.sort();
    for (r, a) in &aliases {
        records.push(SpecRecord::Alias {
            source: source(),
            reference: r.clone(),
            account: *a,
        });
    }
    let mut rows: Vec<OpeningRow> = accounts
        .iter()
        .take((n / 2) as usize)
        .enumerate()
        .map(|(i, a)| OpeningRow {
            account: *a,
            asset: copm(),
            amount: Minor::new(1_000 + i as u64),
        })
        .collect();
    rows.sort_by_key(|r| (r.account, r.asset));
    records.extend(rows.iter().map(|r| SpecRecord::Opening(*r)));
    records.push(SpecRecord::Grant(GrantRequest {
        kind: GrantKind::Operator,
        principal: chainspec::operator_of(&g()),
        subject: Subject::DelegatedAccounts,
        ops: NonEmptySet::new(BTreeSet::from([OperationKind::Transfer])).expect("one"),
        per_operation: None,
        per_window: Some(Window {
            length: NonZeroU64::new(86_400_000).expect("non-zero"),
            limit: NonZeroMinor::new(Minor::new(1_000_000)).expect("non-zero"),
        }),
        assets: None,
        counterparties: None,
        parent: None,
        expires: None,
    }));
    let refs: BTreeMap<_, _> = aliases.iter().map(|(r, a)| (*a, r.clone())).collect();
    let set = OpeningSet::new(
        rows.clone(),
        aliases
            .iter()
            .map(|(r, a)| ((source(), r.clone()), *a))
            .collect(),
    )
    .expect("a set");
    let source_rows = rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let amount = r.amount.get() + if i == 0 { skew } else { 0 };
            SourceRow {
                source_ref: refs[&r.account].clone(),
                asset: r.asset,
                amount: format!("{}.{:02}", amount / 100, amount % 100),
            }
        })
        .collect();
    let rec = Reconciliation {
        set_digest: set.digest(),
        as_of: 1,
        sources: vec![SourceExtract::new(source(), source_rows)],
    };
    ChainSpec::build(
        g(),
        10,
        assets(),
        records,
        Some(rec),
        None,
        ChunkLimits {
            max_records: 7,
            max_bytes: 2048,
            opening_batch: 10,
        },
    )
    .expect("the seed")
}

fn book(s: &ChainSpec) -> Result<(L, Founded), String> {
    book_as(s, false)
}

/// A book founded on the chainspec's commitment; `built`: on the chainspec itself, its genesis
/// state derived natively (owner ruling `genesis-chainspec`).
fn book_as(s: &ChainSpec, built: bool) -> Result<(L, Founded), String> {
    let book = Book::bootstrap(
        MemStore::new(),
        MemVault::new(),
        KeyRing::from_root([3; 32]),
    )
    .map_err(|e| format!("bootstrap: {e:?}"))?;
    let mut lifecycle = LifecycleConfig::new([7; 16], 60, 3_600);
    for (a, srcs) in required() {
        lifecycle = lifecycle.with_opening_sources(a, srcs);
    }
    Ledger::genesis(
        book,
        Genesis {
            assets: assets(),
            operator_verifier: Verifier::new(vec![0x0F]),
            lifecycle,
            chainspec: Some(s.commitment()),
            spec: built.then(|| std::sync::Arc::new(s.clone())),
        },
        g(),
        1,
    )
    .map_err(|e| format!("genesis: {e:?}"))
}

fn operator(f: &Founded) -> Ctx {
    Ctx {
        caller: Caller::new(f.operator, f.operator_credential),
        op: g(),
        now: 10,
    }
}

/// Apply chunks `from..to`, as the operator's path does.
fn apply(l: &mut L, f: &Founded, s: &ChainSpec, from: u32, to: u32) -> Result<(), String> {
    let (bytes, paths) = (s.chunk_bytes(), s.paths());
    for i in from..to {
        l.apply_spec_chunk(&operator(f), &bytes[i as usize], &paths[i as usize])
            .map_err(|e| format!("chunk {i}: {}", e.code()))?;
    }
    Ok(())
}

fn ac_n2_1_a_seed_applies_in_order_and_seals__service(_: &mut dyn Harness) -> Outcome {
    let s = seed(20, 0);
    // The genesis state derived from the chainspec, and the same chainspec applied chunk by chunk:
    // one book, sealed.
    let (l, f) = match book_as(&s, true) {
        Ok(x) => x,
        Err(e) => return Outcome::Fail(e),
    };
    let (mut parts, pf) = match book(&s) {
        Ok(x) => x,
        Err(e) => return Outcome::Fail(e),
    };
    if let Err(e) = apply(&mut parts, &pf, &s, 0, s.chunk_count()) {
        return Outcome::Fail(e);
    }
    if !l.spec_progress().is_some_and(|p| p.sealed) {
        return Outcome::Fail("the derived genesis is not sealed".into());
    }
    if l.checkpoint().ok() != parts.checkpoint().ok() {
        return Outcome::Fail("the derived genesis is not the book the chunks make".into());
    }
    for i in 0..20 {
        let a = chainspec::account_of(&g(), i);
        let Some(acct) = l.state().account(&a) else {
            return Outcome::Fail(format!("account {i} is missing"));
        };
        if acct.owner() != chainspec::identity_of(&g(), i) {
            return Outcome::Fail(format!("account {i} has another owner"));
        }
        // Attributed to the operator (`REQ-AU-7`).
        let op = chainspec::spec_op(&g(), chainspec::Section::Accounts as u8, i);
        if !l
            .state()
            .record(&op)
            .is_some_and(|r| r.authorities.iter().any(|u| u.principal == f.operator))
        {
            return Outcome::Fail(format!("account {i} is not attributed to the operator"));
        }
    }
    let set = s.opening_set().ok().flatten().expect("openings");
    for row in set.rows() {
        match l.balance_view(&row.account, &copm()) {
            Ok(v) if v.available == i128::from(row.amount.get()) => {}
            other => return Outcome::Fail(format!("an opening reads {other:?}")),
        }
    }
    Outcome::Pass
}

fn try_open(l: &mut L, f: &Founded) -> Result<(), Refusal> {
    let ctx = Ctx {
        op: OperationId::from_ref(OperationRef([0xEE; 16])),
        ..operator(f)
    };
    l.open_identity(&ctx, IdentityKind::Person, None, Verifier::new(vec![1]))
        .map(|_| ())
}

fn ac_n2_2_nothing_but_the_seed_before_the_seal__service(_: &mut dyn Harness) -> Outcome {
    let s = seed(12, 0);
    let (mut l, f) = match book(&s) {
        Ok(x) => x,
        Err(e) => return Outcome::Fail(e),
    };
    for stop in [0, 2, s.chunk_count() - 1] {
        let from = l.spec_progress().map_or(0, |p| p.applied);
        if let Err(e) = apply(&mut l, &f, &s, from, stop) {
            return Outcome::Fail(e);
        }
        let before = l.checkpoint().ok();
        match try_open(&mut l, &f) {
            Err(Refusal::InitialStateNotSealed) => {}
            other => return Outcome::Fail(format!("at chunk {stop}: {other:?}")),
        }
        if l.checkpoint().ok() != before {
            return Outcome::Fail("a refused command changed the book".into());
        }
    }
    if let Err(e) = apply(&mut l, &f, &s, s.chunk_count() - 1, s.chunk_count()) {
        return Outcome::Fail(e);
    }
    match try_open(&mut l, &f) {
        Ok(()) => Outcome::Pass,
        Err(e) => Outcome::Fail(format!("after the seal: {}", e.code())),
    }
}

fn err_initial_state_not_sealed__service(_: &mut dyn Harness) -> Outcome {
    let s = seed(4, 0);
    let (mut l, f) = match book(&s) {
        Ok(x) => x,
        Err(e) => return Outcome::Fail(e),
    };
    match try_open(&mut l, &f) {
        Err(r) if r.code() == "ERR-InitialStateNotSealed" => Outcome::Pass,
        other => Outcome::Fail(format!("{other:?}")),
    }
}

fn ac_n2_3_a_part_not_committed_or_applied_takes_no_effect__service(
    _: &mut dyn Harness,
) -> Outcome {
    let s = seed(12, 0);
    let (mut l, f) = match book(&s) {
        Ok(x) => x,
        Err(e) => return Outcome::Fail(e),
    };
    if let Err(e) = apply(&mut l, &f, &s, 0, 3) {
        return Outcome::Fail(e);
    }
    let (bytes, paths) = (s.chunk_bytes(), s.paths());
    let before = l.checkpoint().ok();
    // Another seed's chunk at the next index: not the committed one.
    let other = seed(13, 0);
    let (ob, op) = (other.chunk_bytes(), other.paths());
    match l.apply_spec_chunk(&operator(&f), &ob[3], &op[3]) {
        Err(Refusal::EvidenceInsufficient) => {}
        r => return Outcome::Fail(format!("another seed's chunk: {:?}", r.map(|a| a.value))),
    }
    // A chunk past the next one.
    match l.apply_spec_chunk(&operator(&f), &bytes[5], &paths[5]) {
        Err(Refusal::InvalidTransition) => {}
        r => return Outcome::Fail(format!("a chunk out of order: {:?}", r.map(|a| a.value))),
    }
    // A chunk applied before: answered as it was, nothing written.
    match l.apply_spec_chunk(&operator(&f), &bytes[1], &paths[1]) {
        Ok(a) if a.value.index == 1 && a.value.applied == 3 => {}
        r => return Outcome::Fail(format!("a chunk again: {:?}", r.map(|a| a.value))),
    }
    if l.checkpoint().ok() != before {
        return Outcome::Fail("a part that takes no effect changed the book".into());
    }
    Outcome::Pass
}

fn ac_n2_4_an_interrupted_seeding_resumes_to_the_same_book__service(
    _: &mut dyn Harness,
) -> Outcome {
    let s = seed(16, 0);
    let n = s.chunk_count();
    let whole = match book(&s).and_then(|(mut l, f)| {
        apply(&mut l, &f, &s, 0, n)?;
        l.checkpoint().map_err(|e| format!("{e:?}"))
    }) {
        Ok(c) => c,
        Err(e) => return Outcome::Fail(e),
    };
    for k in 1..n {
        let resumed = book(&s).and_then(|(mut l, f)| {
            apply(&mut l, &f, &s, 0, k)?;
            apply(&mut l, &f, &s, 0, n)?;
            l.checkpoint().map_err(|e| format!("{e:?}"))
        });
        match resumed {
            Ok(c) if c == whole => {}
            Ok(_) => return Outcome::Fail(format!("interrupted after {k}: another book")),
            Err(e) => return Outcome::Fail(e),
        }
    }
    Outcome::Pass
}

fn ac_n2_5_a_divergent_opening_set_is_refused_before_any_part__service(
    _: &mut dyn Harness,
) -> Outcome {
    if let Err(e) = seed(12, 0).gate(&required()) {
        return Outcome::Fail(format!("a reconciled seed fails the gate: {e:?}"));
    }
    // One minor unit more in one source figure: the gate refuses, and the operator's path does
    // not apply a part (`aledgent-core-host serve --seed` runs the gate first).
    match seed(12, 1).gate(&required()) {
        Err(SpecError::Divergent(found)) if !found.is_empty() => Outcome::Pass,
        other => Outcome::Fail(format!("{other:?}")),
    }
}
