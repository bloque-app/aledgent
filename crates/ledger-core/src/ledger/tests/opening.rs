// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Unit tests of opening balances that need the library's insides (`F-014` T-014-06): a second
//! opening of a pair, built by the library's own builder under a new operation and sent straight
//! to the kernel past every library check, is refused by the kernel.

use super::operator::grant_kinds;
use super::*;
use crate::opening::{
    OpeningRow, OpeningSet, OpeningSource, Reconciliation, SourceExtract, SourceRow,
};
use crate::operator::OperatorOpKind;
use ledger_kernel::book::{EventClass, Step, Unit};

#[test]
fn req_rc_4_a_second_opening_of_a_pair_is_refused_at_the_kernel() {
    let mut fx = Fx::new();
    let source = OpeningSource([1; 16]);
    fx.ledger.config = fx
        .ledger
        .config
        .clone()
        .with_opening_sources(usd(), NonEmptySet::one(source));
    let operator = fx.founded.operator;
    grant_kinds(&mut fx, operator, &[OperatorOpKind::LoadOpeningBalances]);
    let p = fx.person();
    let acct = fx.account(&p, Custody::Delegated);
    let row = OpeningRow {
        account: acct,
        asset: usd(),
        amount: Minor::new(500),
    };
    let mut map = BTreeMap::new();
    map.insert((source, b"a".to_vec()), acct);
    let set = OpeningSet::new(vec![row], map).unwrap();
    let rec = Reconciliation {
        set_digest: set.digest(),
        as_of: 1,
        sources: vec![SourceExtract::new(
            source,
            vec![SourceRow {
                source_ref: b"a".to_vec(),
                asset: usd(),
                amount: "5.00".into(),
            }],
        )],
    };
    let ctx = fx.ctx(fx.operator());
    let loaded = fx
        .ledger
        .load_opening_balances(&ctx, &set, &rec, b"load", None)
        .unwrap();
    assert!(loaded.value.complete);

    // The same pair, under another operation: a new client key, a new load.
    let ctx2 = fx.ctx(fx.operator());
    let draft = fx.ledger.opening_draft(&ctx2, 0, &row).unwrap();
    assert_ne!(draft.meta().operation, ctx.op.to_ref());
    let unit = Unit {
        steps: vec![Step {
            creations: Vec::new(),
            chain: vec![draft],
        }],
    };
    let applied = fx.ledger.book.apply(&unit, 20).unwrap();
    assert_eq!(applied.steps[0].events, vec![EventClass::Duplicate]);
    assert!(!applied.steps[0].applied);
    assert_eq!(
        fx.ledger.balance_view(&acct, &usd()).unwrap().available,
        500
    );
}
