// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Opening balances (`F-014` T-014-06 to T-014-08, `REQ-RC-4`, `NFR-9`, `REQ-AS-4`, `INV-5`,
//! `REQ-AT-1`): the zero-divergence gate against every seeded divergence, the load, one opening
//! per (account, asset), and a load interrupted and resumed.

use std::collections::BTreeMap;

use conformance::tools::book::{
    self, BANK_SOURCE, LEDGER_SOURCE, RandomBook, opening_set, source_ref,
};
use ledger_core::Refusal;
use ledger_core::asset::{AssetId, Minor};
use ledger_core::audit::{AuditAction, AuditSubject};
use ledger_core::id::AccountId;
use ledger_core::ledger::Ctx;
use ledger_core::opening::{
    Divergence, OpeningRow, OpeningSet, Reconciliation, SourceExtract, SourceRow, reconcile,
};
use ledger_core::query::journal::PostingEntry;
use ledger_core::state::Command;
use ledger_kernel::posting::PostingKind;
use ledger_kernel::store::MemStore;
use ledger_kernel::vault::MemVault;

type Mem = RandomBook<MemStore, MemVault>;

fn usd() -> AssetId {
    book::assets()[0]
}

fn eur() -> AssetId {
    book::assets()[1]
}

fn fresh() -> Mem {
    RandomBook::new(MemStore::new(), MemVault::new(), 0x0E)
}

/// `n` new accounts, each with a USD row, every other one an EUR row too.
fn rows(b: &mut Mem, n: usize) -> Vec<OpeningRow> {
    let mut out = Vec::new();
    for i in 0..n {
        let h = b.person();
        out.push(OpeningRow {
            account: h.account,
            asset: usd(),
            amount: Minor::new(1_000 + i as u64 * 7),
        });
        if i % 2 == 0 {
            out.push(OpeningRow {
                account: h.account,
                asset: eur(),
                amount: Minor::new(50 + i as u64),
            });
        }
    }
    out.sort_by_key(|r| (r.account, r.asset));
    out
}

fn set_of(b: &Mem, rows: &[OpeningRow]) -> (OpeningSet, Reconciliation) {
    opening_set(rows, b.ledger.state().registry(), b.now())
}

fn load(
    b: &mut Mem,
    ctx: &Ctx,
    set: &OpeningSet,
    rec: &Reconciliation,
    max: Option<usize>,
) -> Result<ledger_core::ledger::operator::LoadReport, Refusal> {
    b.ledger
        .load_opening_balances(ctx, set, rec, b"cutover of the clean accounts", max)
        .map(|a| a.value)
}

fn available(b: &Mem, account: &AccountId, asset: &AssetId) -> i128 {
    b.ledger.balance_view(account, asset).unwrap().available
}

fn openings(b: &Mem) -> Vec<PostingEntry> {
    b.ledger
        .state()
        .journal()
        .iter()
        .filter(|e| e.kind == PostingKind::Opening)
        .cloned()
        .collect()
}

#[test]
fn req_rc_4_a_set_that_matches_every_source_loads() {
    let mut b = fresh();
    let rows = rows(&mut b, 9);
    let (set, rec) = set_of(&b, &rows);
    let ctx = b.ctx(b.operator());
    let r = load(&mut b, &ctx, &set, &rec, None).expect("a clean set loads");
    assert!(r.complete);
    assert_eq!(r.posted, rows.len() as u64);
    // One `Opening` posting per (account, asset), each crediting available.
    assert_eq!(openings(&b).len(), rows.len());
    for row in &rows {
        assert_eq!(
            available(&b, &row.account, &row.asset),
            i128::from(row.amount.get())
        );
    }
    // Against one named opening position per asset, standing at minus the total.
    for (asset, total) in set.totals() {
        let position = b.ledger.opening_position(&asset).expect("a position");
        assert_eq!(available(&b, &position, &asset), -(total as i128));
    }
    // INV-5: each asset still sums to zero, on openings and on commitments.
    for asset in book::assets() {
        assert_eq!(b.ledger.trial_balance(&asset), Ok(0));
    }
    let trial = b
        .ledger
        .view(b.now())
        .unwrap()
        .trial_balance(&b.operator(), None)
        .unwrap();
    assert!(trial.is_zero(), "{trial:?}");
    assert_eq!(b.ledger.reconcile().unwrap(), Vec::new());
    // REQ-AT-1: the load is on the audit trail, with its principal, justification and the
    // reconciliation it rests on.
    b.ship();
    let record = b
        .audit
        .iter()
        .find(|r| r.operation_ref == ctx.op)
        .expect("the load's audit record");
    assert_eq!(
        record.action,
        AuditAction::Command(Command::LoadOpeningBalances)
    );
    assert_eq!(record.principal(), Some(b.founded.operator));
    assert_eq!(
        record.justification.as_ref().unwrap().as_bytes(),
        b"cutover of the clean accounts"
    );
    assert!(
        record
            .subject
            .contains(&AuditSubject::Evidence(rec.digest()))
    );
    // The reconciliation is kept, so the gate can be re-checked later.
    let kept = b.ledger.state().openings().load(&ctx.op).unwrap();
    assert_eq!(kept.reconciliation, rec);
    assert_eq!(
        reconcile(
            &kept.set,
            &kept.reconciliation,
            &book::config().opening_sources,
            b.ledger.state().registry()
        ),
        Ok(())
    );
    // A repeat of the same request returns the same outcome and posts nothing.
    let again = load(&mut b, &ctx, &set, &rec, None).unwrap();
    assert_eq!(again, r);
    assert_eq!(openings(&b).len(), rows.len());
    // Another request under the same client key is refused.
    let other = Reconciliation {
        as_of: rec.as_of + 1,
        ..rec.clone()
    };
    assert_eq!(
        load(&mut b, &ctx, &set, &other, None),
        Err(Refusal::ClientKeyReused)
    );
}

/// Load `set` with `rec` and expect the gate to refuse it, with nothing posted and nothing
/// opened.
fn blocked(b: &mut Mem, set: &OpeningSet, rec: &Reconciliation) {
    let before = b.ledger.state().journal().len();
    let ctx = b.ctx(b.operator());
    assert_eq!(
        load(b, &ctx, set, rec, None),
        Err(Refusal::EvidenceInsufficient)
    );
    assert_eq!(b.ledger.state().journal().len(), before);
    assert!(b.ledger.state().openings().load(&ctx.op).is_none());
    assert_eq!(b.ledger.opening_position(&usd()), None);
}

fn source_mut(
    rec: &mut Reconciliation,
    s: ledger_core::opening::OpeningSource,
) -> &mut SourceExtract {
    rec.sources.iter_mut().find(|x| x.source == s).unwrap()
}

fn reseal(x: &mut SourceExtract) {
    *x = SourceExtract::new(x.source, x.rows.clone());
}

#[test]
fn req_rc_4_every_seeded_divergence_blocks_the_whole_set() {
    let mut b = fresh();
    let rows = rows(&mut b, 6);
    let (set, rec) = set_of(&b, &rows);
    let registry = b.ledger.state().registry().clone();
    let required = book::config().opening_sources;
    assert_eq!(reconcile(&set, &rec, &required, &registry), Ok(()));
    let gate = |rec: &Reconciliation| reconcile(&set, rec, &required, &registry).unwrap_err();

    // One minor unit of difference in one row of one source.
    let mut r = rec.clone();
    let x = source_mut(&mut r, BANK_SOURCE);
    let row = &mut x.rows[2];
    let asset = registry.get(&row.asset).unwrap();
    let off_by_one = Minor::from_decimal(&row.amount, asset).unwrap().get() + 1;
    row.amount = Minor::new(off_by_one).to_decimal(asset);
    reseal(x);
    assert!(matches!(gate(&r)[..], [Divergence::Amount { .. }]));
    blocked(&mut b, &set, &r);

    // A missing row.
    let mut r = rec.clone();
    let x = source_mut(&mut r, LEDGER_SOURCE);
    x.rows.remove(0);
    reseal(x);
    assert!(matches!(gate(&r)[..], [Divergence::Missing { .. }]));
    blocked(&mut b, &set, &r);

    // An extra row: a source states a balance the set does not hold (an EUR balance where the
    // set has none, on a mapped account).
    let mut r = rec.clone();
    let x = source_mut(&mut r, LEDGER_SOURCE);
    let no_eur = rows
        .iter()
        .position(|row| {
            row.asset == usd()
                && !rows
                    .iter()
                    .any(|o| o.account == row.account && o.asset == eur())
        })
        .unwrap();
    let account_index = {
        let mut seen: Vec<AccountId> = Vec::new();
        for row in &rows {
            if !seen.contains(&row.account) {
                seen.push(row.account);
            }
        }
        seen.iter()
            .position(|a| *a == rows[no_eur].account)
            .unwrap()
    };
    x.rows.push(SourceRow {
        source_ref: source_ref(&LEDGER_SOURCE, account_index),
        asset: eur(),
        amount: "1.00".into(),
    });
    reseal(x);
    assert!(matches!(gate(&r)[..], [Divergence::Extra { .. }]));
    blocked(&mut b, &set, &r);

    // A missing required source.
    let mut r = rec.clone();
    r.sources.retain(|x| x.source != BANK_SOURCE);
    assert!(matches!(
        gate(&r)[..],
        [Divergence::MissingSource { source, .. }] if source == BANK_SOURCE
    ));
    blocked(&mut b, &set, &r);

    // An amount with excess precision: it would need rounding, and is refused, never rounded.
    let mut r = rec.clone();
    let x = source_mut(&mut r, LEDGER_SOURCE);
    // Two fractional digits at precision 2: a third is excess, whatever its value.
    assert_eq!(x.rows[1].amount.split('.').nth(1).map(str::len), Some(2));
    x.rows[1].amount.push('1');
    reseal(x);
    assert!(matches!(gate(&r)[..], [Divergence::NotExact { .. }]));
    blocked(&mut b, &set, &r);

    // A reference the source map does not hold, a digest that does not match, and a
    // reconciliation of another set: each blocks too.
    let mut r = rec.clone();
    let x = source_mut(&mut r, BANK_SOURCE);
    x.rows[0].source_ref = b"unknown".to_vec();
    reseal(x);
    assert!(
        gate(&r)
            .iter()
            .any(|d| matches!(d, Divergence::Unmapped { .. }))
    );
    blocked(&mut b, &set, &r);
    let mut r = rec.clone();
    source_mut(&mut r, BANK_SOURCE).extract_digest = [0; 32];
    assert!(matches!(gate(&r)[..], [Divergence::ExtractDigest(_)]));
    blocked(&mut b, &set, &r);
    let mut r = rec.clone();
    r.set_digest = [1; 32];
    assert!(matches!(gate(&r)[..], [Divergence::SetDigest]));
    blocked(&mut b, &set, &r);

    // The clean set loads.
    let ctx = b.ctx(b.operator());
    assert!(load(&mut b, &ctx, &set, &rec, None).unwrap().complete);
}

#[test]
fn req_rc_4_an_asset_no_source_is_required_for_cannot_load() {
    let mut b = fresh();
    let h = b.person();
    let btc = book::assets()[2];
    let rows = [OpeningRow {
        account: h.account,
        asset: btc,
        amount: Minor::new(5),
    }];
    let set = OpeningSet::new(rows.to_vec(), BTreeMap::new()).unwrap();
    let rec = Reconciliation {
        set_digest: set.digest(),
        as_of: b.now(),
        sources: Vec::new(),
    };
    assert_eq!(
        reconcile(
            &set,
            &rec,
            &book::config().opening_sources,
            b.ledger.state().registry()
        ),
        Err(vec![Divergence::NoSourcesRequired(btc)])
    );
    blocked(&mut b, &set, &rec);
}

#[test]
fn req_rc_4_one_opening_per_pair_even_under_a_new_client_key() {
    let mut b = fresh();
    let rows = rows(&mut b, 3);
    let (set, rec) = set_of(&b, &rows);
    let ctx = b.ctx(b.operator());
    load(&mut b, &ctx, &set, &rec, None).unwrap();
    // The same set under a new client key: refused, nothing posted.
    let ctx2 = b.ctx(b.operator());
    assert_eq!(
        load(&mut b, &ctx2, &set, &rec, None),
        Err(Refusal::InvalidTransition)
    );
    assert_eq!(openings(&b).len(), rows.len());
    // A set of one pair already loaded, with another amount: refused too.
    let one = [OpeningRow {
        amount: Minor::new(1),
        ..rows[0]
    }];
    let (set1, rec1) = set_of(&b, &one);
    let ctx3 = b.ctx(b.operator());
    assert_eq!(
        load(&mut b, &ctx3, &set1, &rec1, None),
        Err(Refusal::InvalidTransition)
    );
    // An account that has any entry already cannot be opened: no history is replayed.
    let h = b.person();
    b.issue(h.account, usd(), 10);
    let used = [OpeningRow {
        account: h.account,
        asset: usd(),
        amount: Minor::new(10),
    }];
    let (set2, rec2) = set_of(&b, &used);
    let ctx4 = b.ctx(b.operator());
    assert_eq!(
        load(&mut b, &ctx4, &set2, &rec2, None),
        Err(Refusal::InvalidTransition)
    );
}

#[test]
fn t_014_08_an_interrupted_load_resumes_with_no_double_posting() {
    let mut b = fresh();
    // 16 accounts: 24 rows, three batches at the deployment's limit of 10.
    let rows = rows(&mut b, 16);
    assert_eq!(rows.len(), 24);
    let (set, rec) = set_of(&b, &rows);
    let ctx = b.ctx(b.operator());
    let first = load(&mut b, &ctx, &set, &rec, Some(1)).unwrap();
    assert_eq!((first.posted, first.complete), (10, false));
    assert_eq!(openings(&b).len(), 10);
    // The totals are not there yet: the load is not complete.
    let position = b.ledger.opening_position(&usd()).unwrap();
    let total = set.totals()[&usd()] as i128;
    assert!(available(&b, &position, &usd()) > -total);
    // The same request resumes where it stopped.
    let second = load(&mut b, &ctx, &set, &rec, Some(1)).unwrap();
    assert_eq!((second.posted, second.complete), (20, false));
    let done = load(&mut b, &ctx, &set, &rec, None).unwrap();
    assert_eq!((done.posted, done.complete), (24, true));
    // Every pair exactly once.
    let postings = openings(&b);
    assert_eq!(postings.len(), rows.len());
    let mut credited: BTreeMap<(AccountId, AssetId), u64> = BTreeMap::new();
    for p in &postings {
        assert_eq!(p.operation, ctx.op);
        for l in p.lines.iter().filter(|l| l.account != position) {
            *credited.entry((l.account, l.asset)).or_default() += l.amount.minor().get();
        }
    }
    for row in &rows {
        assert_eq!(credited[&(row.account, row.asset)], row.amount.get());
    }
    assert_eq!(available(&b, &position, &usd()), -total);
    for asset in book::assets() {
        assert_eq!(b.ledger.trial_balance(&asset), Ok(0));
    }
}

#[test]
fn t_014_08_a_load_stopped_by_a_refusal_stays_incomplete_and_posts_nothing_twice() {
    let mut b = fresh();
    let rows = rows(&mut b, 12);
    let (set, rec) = set_of(&b, &rows);
    let ctx = b.ctx(b.operator());
    load(&mut b, &ctx, &set, &rec, Some(1)).unwrap();
    // A pending row's account is closed between two calls.
    let load_now = b.ledger.state().openings().load(&ctx.op).unwrap().clone();
    let pending = rows
        .iter()
        .rev()
        .find(|r| !load_now.posted.contains_key(&(r.account, r.asset)))
        .unwrap();
    let owner = b
        .accounts
        .iter()
        .find(|h| h.account == pending.account)
        .unwrap()
        .owner;
    let cctx = b.ctx(owner);
    b.ledger.close_account(&cctx, pending.account).unwrap();
    let before = openings(&b).len();
    assert_eq!(
        load(&mut b, &ctx, &set, &rec, None),
        Err(Refusal::AccountClosed)
    );
    let l = b.ledger.state().openings().load(&ctx.op).unwrap();
    assert!(!l.complete);
    assert_eq!(openings(&b).len(), before);
    for asset in book::assets() {
        assert_eq!(b.ledger.trial_balance(&asset), Ok(0));
    }
}

#[test]
fn a_zero_row_is_reconciled_posts_nothing_and_closes_the_pair() {
    let mut b = fresh();
    let h = b.person();
    let rows = [OpeningRow {
        account: h.account,
        asset: usd(),
        amount: Minor::ZERO,
    }];
    let (set, rec) = set_of(&b, &rows);
    let ctx = b.ctx(b.operator());
    let r = load(&mut b, &ctx, &set, &rec, None).unwrap();
    assert!(r.complete);
    assert_eq!(r.posted, 0);
    assert!(openings(&b).is_empty());
    // The pair was loaded, at zero: a later opening of it is refused.
    let later = [OpeningRow {
        amount: Minor::new(3),
        ..rows[0]
    }];
    let (set2, rec2) = set_of(&b, &later);
    let ctx2 = b.ctx(b.operator());
    assert_eq!(
        load(&mut b, &ctx2, &set2, &rec2, None),
        Err(Refusal::InvalidTransition)
    );
}

#[test]
fn req_os_1_the_load_needs_its_own_operator_kind_and_a_justification() {
    let mut b = fresh();
    let rows = rows(&mut b, 2);
    let (set, rec) = set_of(&b, &rows);
    let stranger = b.accounts[0].owner;
    let ctx = b.ctx(stranger);
    assert_eq!(
        b.ledger
            .load_opening_balances(&ctx, &set, &rec, b"x", None)
            .map(|a| a.value),
        Err(Refusal::NotAuthorised)
    );
    let ctx = b.ctx(b.operator());
    assert_eq!(
        b.ledger
            .load_opening_balances(&ctx, &set, &rec, b"", None)
            .map(|a| a.value),
        Err(Refusal::JustificationRequired)
    );
    assert!(openings(&b).is_empty());
}
