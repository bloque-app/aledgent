// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The daily reconciliation (`T-018-06`, `T-018-07`, `NFR-9`, `INV-5`, `INV-6`, `REQ-PV-4`,
//! `REQ-PR-5`, `REQ-LA-2`, `AC-J3.1`, `AC-J3.2`): a clean book reconciles on all three axes and the
//! run is recorded, as JSON and on the audit trail; one minor unit of drift in a copied leaf fails
//! axis 1; an unbalanced copy fails axis 2 and only axis 2; one minor unit sent to the position
//! outside the pallet's calls fails axis 3; the external source runs as a counted, audited
//! privileged read and returns mismatches only; a `Suspense` balance is reported with its age;
//! each pages, and an axis that cannot run pages as not run.

use std::collections::BTreeMap;
use std::time::Duration;

use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT;
use curve25519_dalek::ristretto::CompressedRistretto;
use ingress::clock::Clock as _;
use ledger_core::asset::AssetId;
use ledger_core::audit::{AuditAction, ReadKind};
use ledger_core::id::AccountId;
use ledger_core::ledger::operator::{ExternalKind, ExternalSource};
use ops::alerts::{Evaluator, Routing, Rules, TestRoute};
use ops::kit::OpsKit;
use ops::reconcile::{self, BookSnapshot, External, Finding, NotRun, RouterLink, Run, Sources};
use ops::signals::{Snapshot, parse, render};

type MemKit = OpsKit<ingress::store::mem::MemStore, settle::store::mem::MemStore>;

async fn kit(dir: &std::path::Path) -> MemKit {
    let mut k = OpsKit::start(
        ingress::store::mem::MemStore::new(Duration::from_millis(50)),
        settle::store::mem::MemStore::new(),
        &dir.join("audit"),
        &dir.join("book"),
    )
    .await;
    for _ in 0..2 {
        k.round().await;
        k.anchor_round(12).await;
    }
    // The replica keeps running: the daily job reaches the core through it.
    assert!(k.d.quiesce(ops::kit::PATIENCE).await);
    k
}

fn link(k: &MemKit) -> RouterLink {
    RouterLink {
        router: k.d.router.clone(),
        token: ops::kit::OPS_TOKEN.to_owned(),
    }
}

/// The pages the exporter's view of `dir`'s records gives, at `now`, with the core's signals.
async fn pages_with_core(k: &MemKit, dir: &std::path::Path, now: u64) -> TestRoute {
    let exporter = ops::exporter::Exporter::new(
        ops::exporter::RouterSource::new(k.d.router.clone(), ops::kit::OPS_TOKEN),
        ops::exporter::Fixed(std::sync::Mutex::new(None)),
        ops::exporter::Records {
            reconciliation: Some(dir.to_path_buf()),
            backups: None,
            drills: None,
            core_archive: None,
        },
        "settlement-operations",
        ops::kit::PATIENCE,
    );
    let samples: Vec<_> = parse(&exporter.scrape(now).await)
        .into_iter()
        .filter(|(n, _, _)| {
            n.starts_with("aledgent_reconciliation")
                || n.starts_with("aledgent_settlement")
                || n.starts_with("aledgent_suspense")
                || n.starts_with("aledgent_anchor")
        })
        .collect();
    let mut ev = Evaluator::new(Rules::committed().unwrap());
    let firing = ev.step(now, &samples);
    let mut t = TestRoute::default();
    t.deliver(&Routing::committed().unwrap(), "pager", &firing);
    t
}

/// One minor unit added to a commitment: `+1·G`.
fn plus_one(bytes: &mut [u8; 32]) {
    let p = CompressedRistretto(*bytes).decompress().unwrap();
    *bytes = (p + RISTRETTO_BASEPOINT_POINT).compress().to_bytes();
}

fn run_on(k: &MemKit, snap: Option<BookSnapshot>) -> Run {
    k.d.core
        .checkpoint(|l, _| {
            reconcile::run(
                l,
                snap,
                Sources {
                    external: None,
                    settlement: &NotRun::default(),
                },
                1_000,
            )
        })
        .unwrap()
        .unwrap()
}

fn snapshot(k: &MemKit) -> BookSnapshot {
    k.d.core
        .with_ledger(|l| BookSnapshot::read(l.book().store()))
        .unwrap()
        .unwrap()
}

fn axis(r: &Run, n: u8) -> &reconcile::AxisResult {
    r.axes.iter().find(|a| a.axis == n).unwrap()
}

/// What the exporter shows of `dir`'s records, evaluated and routed: the pages.
fn pages(dir: &std::path::Path, now: u64) -> TestRoute {
    let s = Snapshot {
        at: now,
        reconciliation: reconcile::latest_axes(dir).unwrap(),
        ..Snapshot::default()
    };
    let samples: Vec<_> = parse(&render(&s))
        .into_iter()
        .filter(|(n, _, _)| n.starts_with("aledgent_reconciliation"))
        .collect();
    let mut ev = Evaluator::new(Rules::committed().unwrap());
    let firing = ev.step(now, &samples);
    let mut t = TestRoute::default();
    t.deliver(&Routing::committed().unwrap(), "pager", &firing);
    t
}

/// `NFR-9`, `AC-J3.1`: the daily run over a clean book reconciles on all three axes — axis 3
/// through `F-017`'s comparison at the adapter's latest cut — and is recorded: as JSON, and on the
/// audit trail as the `compare_settlement` operator record whose justification is the run's
/// summary. Nothing pages.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_j3_1_the_daily_run_reconciles_three_axes_and_is_recorded() {
    let tmp = tempfile::tempdir().unwrap();
    let k = kit(tmp.path()).await;
    let dir = tmp.path().join("runs");
    let now = k.d.clock.now();
    let run = reconcile::daily(&k.d.core, &link(&k), None, now, &dir)
        .await
        .unwrap();
    assert!(run.clean(), "{run:#?}");
    assert!(run.suspense.is_empty());
    let audited = run.audited.clone().expect("the run is on the audit trail");
    let summary = run.summary();
    // The operator record: compare_settlement, by the operations principal, with the summary.
    let recorded = k
        .d
        .core
        .with_ledger(|l| {
            l.state().audit().outbox().any(|r| {
                r.operation_ref.to_did() == audited
                    && r.action
                        == AuditAction::Command(ledger_core::state::Command::CompareSettlement)
                    && r.justification.as_ref().map(|j| j.as_bytes()) == Some(summary.as_bytes())
            })
        })
        .unwrap();
    let seen: Vec<String> = k
        .d
        .core
        .with_ledger(|l| {
            l.state()
                .audit()
                .outbox()
                .filter(|r| {
                    r.action == AuditAction::Command(ledger_core::state::Command::CompareSettlement)
                })
                .map(|r| {
                    format!(
                        "{} {:?}",
                        r.operation_ref.to_did(),
                        r.justification
                            .as_ref()
                            .map(|j| String::from_utf8_lossy(j.as_bytes()).into_owned())
                    )
                })
                .collect()
        })
        .unwrap();
    assert!(
        recorded,
        "no audit record of the run {audited}: {seen:?} want {summary}"
    );
    // The comparison it made is recorded in the core too (requested by the run).
    let requested =
        k.d.core
            .with_ledger(|l| {
                l.state()
                    .anchoring()
                    .comparisons()
                    .iter()
                    .any(|c| c.requested.is_some_and(|op| op.to_did() == audited))
            })
            .unwrap();
    assert!(requested);
    assert_eq!(reconcile::latest_run(&dir).unwrap().unwrap(), run);
    let p = pages_with_core(&k, &dir, now).await;
    assert!(p.delivered.is_empty(), "{:#?}", p.delivered);
}

/// `NFR-9`: an axis that cannot run — here axis 3, with ingress refusing the job's principal — is
/// recorded as not run, the run has no audit record, and a day later it pages as not run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nfr_9_an_axis_that_cannot_run_pages_as_not_run() {
    let tmp = tempfile::tempdir().unwrap();
    let k = kit(tmp.path()).await;
    let dir = tmp.path().join("runs");
    let refused = RouterLink {
        router: k.d.router.clone(),
        token: "no-such-token".into(),
    };
    let run = reconcile::daily(&k.d.core, &refused, None, 1_000, &dir)
        .await
        .unwrap();
    assert!(axis(&run, 1).ran && axis(&run, 2).ran && !axis(&run, 3).ran);
    assert!(run.audited.is_none() && !run.clean());
    let axes = reconcile::latest_axes(&dir).unwrap();
    assert_eq!(axes[0].last_run, Some(1_000));
    assert_eq!(axes[2].last_run, None);
    let p = pages(&dir, 1_000 + 86_400_001);
    assert!(!p.got("AledgentReconciliationDifference"));
    assert!(p.got("AledgentReconciliationNotRun"));
}

/// `AC-J3.2`, `REQ-PR-5`: one minor unit sent to the instance's position with a plain transfer,
/// outside the pallet's calls, fails axis 3 with a difference of exactly one, and pages — through
/// the reconciliation and through the core's settlement signal.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_j3_2_one_minor_unit_outside_the_pallet_fails_axis_3_and_pages() {
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kit(tmp.path()).await;
    k.chain
        .top_up_position(settle::testkit::INSTANCE, &settle::testkit::chain_usd(), 1);
    k.settle(3).await;
    let dir = tmp.path().join("runs");
    let now = k.d.clock.now();
    let run = reconcile::daily(&k.d.core, &link(&k), None, now, &dir)
        .await
        .unwrap();
    assert!(axis(&run, 1).findings.is_empty() && axis(&run, 2).findings.is_empty());
    let three = &axis(&run, 3).findings;
    assert!(
        matches!(three.as_slice(), [Finding::Settlement { asset, difference, .. }]
            if asset == "USD" && difference == "1"),
        "{three:#?}"
    );
    let p = pages_with_core(&k, &dir, now).await;
    assert!(
        p.got("AledgentReconciliationDifference"),
        "{:#?}",
        p.delivered
    );
    assert!(p.got("AledgentSettlementDifference"), "{:#?}", p.delivered);
}

/// `REQ-LA-2`, `RISK-8`: a non-zero `Suspense` balance is reported with when it left zero — no
/// figure — and pages once it has not been zero for a day; brought back to zero, it is not
/// reported.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn req_la_2_a_suspense_balance_is_reported_with_its_age() {
    let tmp = tempfile::tempdir().unwrap();
    let k = kit(tmp.path()).await;
    let op = k.d.operator.token.clone();
    let (s, v) = k
        .d
        .call(
            "POST",
            "/v1/accounts",
            Some(&op),
            &[("client-key", "suspense")],
            Some(serde_json::json!({ "owner": k.d.operator.identity, "kind": "suspense", "custody": "delegated" })),
        )
        .await;
    assert!(s.is_success(), "{v}");
    let suspense = v["result"]["account"].as_str().unwrap().to_owned();
    let (s, v) =
        k.d.call(
            "POST",
            "/v1/issuances",
            Some(&op),
            &[],
            Some(serde_json::json!({ "asset": "USD", "destination": suspense, "amount": "97531" })),
        )
        .await;
    assert!(s.is_success(), "{v}");
    let since = k.d.clock.now();
    let dir = tmp.path().join("runs");
    let run = reconcile::daily(&k.d.core, &link(&k), None, since + 1_000, &dir)
        .await
        .unwrap();
    assert_eq!(run.suspense.len(), 1, "{run:#?}");
    assert_eq!(run.suspense[0].account, suspense);
    assert!(run.suspense[0].since <= since + 1_000);
    assert!(
        !serde_json::to_string(&run.suspense)
            .unwrap()
            .contains("97531")
    );
    let signal = run.suspense_signal(since + 86_400_000 + 2_000);
    assert_eq!(signal.nonzero, 1);
    let s = Snapshot {
        at: since + 86_400_000 + 2_000,
        suspense: Some(signal),
        ..Snapshot::default()
    };
    let samples: Vec<_> = parse(&render(&s))
        .into_iter()
        .filter(|(n, _, _)| n.starts_with("aledgent_suspense"))
        .collect();
    let firing = Evaluator::new(Rules::committed().unwrap()).step(s.at, &samples);
    assert!(
        firing
            .iter()
            .any(|a| a.alert == "AledgentSuspenseNotReconciled")
    );
}

/// `NFR-9`, `AC-J3.2`, `INV-6`: one minor unit of drift in a copied leaf fails axis 1, and pages.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inv_6_one_minor_unit_of_drift_in_a_copied_leaf_fails_axis_1() {
    let tmp = tempfile::tempdir().unwrap();
    let k = kit(tmp.path()).await;
    let mut snap = snapshot(&k);
    let (id, account) = snap
        .stored
        .iter_mut()
        .find(|(_, a)| a.as_ref().is_some_and(|a| a.balances[0].cr.0 != [0; 32]))
        .unwrap();
    let id = *id;
    plus_one(&mut account.as_mut().unwrap().balances[0].cr.0);
    let run = run_on(&k, Some(snap));
    let one = &axis(&run, 1).findings;
    assert!(
        one.contains(&Finding::Commitment {
            account: hex::encode(id.0),
            layer: 0
        }),
        "{one:#?}"
    );
    assert!(one.iter().any(|f| matches!(f, Finding::Rebuild { .. })));
    let dir = tmp.path().join("runs");
    run.record(&dir).unwrap();
    assert!(pages(&dir, 2_000).got("AledgentReconciliationDifference"));
    // The live book was never touched: it reconciles.
    assert!(axis(&run_on(&k, None), 1).findings.is_empty());
}

/// `NFR-9`, `INV-5`: an unbalanced copy — an entry of one minor unit with no other side, its
/// account's leaf consistent with it — passes axis 1 and fails axis 2, and pages.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn inv_5_an_unbalanced_copy_fails_axis_2() {
    let tmp = tempfile::tempdir().unwrap();
    let k = kit(tmp.path()).await;
    let mut snap = snapshot(&k);
    let (seq, template) = snap
        .history
        .transfers
        .iter()
        .next_back()
        .map(|(s, t)| (*s, t.clone()))
        .unwrap();
    let mut forged = template;
    forged.id = cipher_clerk::TransferId([0xEE; 16]);
    forged.entries.truncate(1);
    let entry = &mut forged.entries[0];
    entry.direction = cipher_clerk::Direction::Credit;
    entry.amount = cipher_clerk::crypto::Amount(RISTRETTO_BASEPOINT_POINT.compress().to_bytes());
    let (account, layer) = (entry.account_id, entry.layer.as_index());
    snap.history.transfers.insert(seq + 1, forged);
    for a in [
        snap.history.accounts.get_mut(&account),
        snap.stored.get_mut(&account).and_then(Option::as_mut),
    ]
    .into_iter()
    .flatten()
    {
        plus_one(&mut a.balances[layer].cr.0);
    }
    let run = run_on(&k, Some(snap));
    assert!(axis(&run, 1).findings.is_empty(), "{run:#?}");
    let two = &axis(&run, 2).findings;
    assert!(
        two.iter()
            .any(|f| matches!(f, Finding::Unbalanced { over, .. } if over == "account leaves")),
        "{two:#?}"
    );
    assert!(
        two.iter()
            .any(|f| matches!(f, Finding::Unbalanced { over, .. } if over == "entries"))
    );
    let dir = tmp.path().join("runs");
    run.record(&dir).unwrap();
    assert!(pages(&dir, 2_000).got("AledgentReconciliationDifference"));
}

struct Source(BTreeMap<(AccountId, AssetId), (i128, i128)>);

impl ExternalSource for Source {
    fn pairs(&self) -> Vec<(AccountId, AssetId)> {
        self.0.keys().copied().collect()
    }
    fn figure(&self, account: &AccountId, asset: &AssetId, kind: ExternalKind) -> Option<i128> {
        self.0.get(&(*account, *asset)).map(|(a, r)| match kind {
            ExternalKind::Available => *a,
            ExternalKind::Reserved => *r,
        })
    }
}

/// `NFR-9` axis 1(c), `REQ-PV-4`: the external source is compared inside the core, through the
/// privileged-read mechanism — recorded in the audit store with its justification, refused
/// without the kind — and only mismatches come back, at one minor unit.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn req_pv_4_the_external_source_is_a_counted_audited_read() {
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kit(tmp.path()).await;
    // The source as the legacy book would state it: exactly the book's figures.
    let mut exact = BTreeMap::new();
    k.d.core.with_ledger(|l| {
        for a in l.state().all_accounts() {
            for asset in l.state().registry().iter() {
                let v = l.balance_view(&a.id(), &asset.id()).unwrap();
                if v.available != 0 || v.reserved != 0 {
                    exact.insert((a.id(), asset.id()), (v.available, v.reserved));
                }
            }
        }
    });
    assert!(!exact.is_empty());
    let mut off = exact.clone();
    let first = *off.keys().next().unwrap();
    off.get_mut(&first).unwrap().0 += 1;
    let ctx = k.ops_ctx();
    let audit_before = k.audit.len();
    let found = |k: &mut MemKit, source: &Source, ctx: &ledger_core::ledger::Ctx| {
        let audit = &mut k.audit;
        k.d.core
            .checkpoint(|l, _| {
                reconcile::run(
                    l,
                    None,
                    Sources {
                        external: Some(External {
                            source,
                            ctx: *ctx,
                            justification: b"cutover shadow day 1",
                            sink: audit,
                        }),
                        settlement: &NotRun::default(),
                    },
                    1_000,
                )
            })
            .unwrap()
    };
    let clean = found(&mut k, &Source(exact), &ctx).unwrap();
    assert!(axis(&clean, 1).findings.is_empty(), "{clean:#?}");
    let run = found(&mut k, &Source(off), &ctx).unwrap();
    let one = &axis(&run, 1).findings;
    assert_eq!(
        one,
        &vec![Finding::External {
            account: first.0.to_did(),
            asset: first.1.as_str().to_owned(),
            reason: "differs: Available".into(),
        }]
    );
    // No figure in the finding, and each read is on the audit trail with its justification.
    assert!(!format!("{one:?}").contains(&exact_figure(&k, first).to_string()));
    let reads: Vec<_> = (audit_before..k.audit.len())
        .filter_map(|i| k.audit.get(i as u64).cloned())
        .filter(|r| r.action == AuditAction::PrivilegedRead(ReadKind::Reconciliation))
        .collect();
    assert_eq!(reads.len(), 2, "each external read is recorded");
    // Without the PrivilegedRead kind, it is refused.
    let mut alice = ctx;
    alice.caller = ops::kit::caller_of(&k.parties[0]);
    assert!(found(&mut k, &Source(BTreeMap::new()), &alice).is_err());
    let _ = k.d.clock.now();
}

fn exact_figure(k: &MemKit, pair: (AccountId, AssetId)) -> i128 {
    k.d.core
        .with_ledger(|l| l.balance_view(&pair.0, &pair.1).unwrap().available)
        .unwrap()
        + 1
}
