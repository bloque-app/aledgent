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

// ---------------------------------------------------------------------------------------------
// `opsctl reconcile` (`T-018-07`, `T-018-12`): the daily job against a running deployment.
// ---------------------------------------------------------------------------------------------

use ops::drill::{Check, Verdict};
use ops::reconcile::{ArchiveChecked, IngressLink};
use serde_json::{Value, json};

/// Ingress as canned answers, remembering every request made. Anything not canned answers 404.
struct Canned {
    answers: BTreeMap<String, (u16, Value)>,
    calls: std::sync::Mutex<Vec<String>>,
}

impl Canned {
    fn new(answers: &[(&str, u16, Value)]) -> Canned {
        Canned {
            answers: answers
                .iter()
                .map(|(k, s, v)| ((*k).to_owned(), (*s, v.clone())))
                .collect(),
            calls: Default::default(),
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    /// Whether any request was a write.
    fn wrote(&self) -> bool {
        self.calls().iter().any(|c| !c.starts_with("GET "))
    }
}

impl IngressLink for Canned {
    async fn call(
        &self,
        method: &str,
        path: &str,
        _body: Option<Value>,
    ) -> Result<(u16, Value), String> {
        let key = format!("{method} {path}");
        self.calls.lock().unwrap().push(key.clone());
        Ok(self
            .answers
            .get(&key)
            .cloned()
            .unwrap_or((404, json!({ "error": "not canned" }))))
    }
}

/// The operator's link: the trial balance is the operator's, and `read_signals` alone is refused.
fn operator_link(k: &MemKit) -> RouterLink {
    RouterLink {
        router: k.d.router.clone(),
        token: k.d.operator.token.clone(),
    }
}

fn balanced() -> (&'static str, u16, Value) {
    (
        "GET /v1/trial-balance",
        200,
        json!({ "view": 7, "accounts": 12, "unbalanced": [],
                "rows": [{ "asset": "COPM", "openings": "0" }] }),
    )
}

fn assets(binding: &str) -> (&'static str, u16, Value) {
    (
        "GET /v1/assets",
        200,
        json!([{ "id": "COPM", "binding": binding, "precision": 2, "kind": "fiat",
                 "status": "active" }]),
    )
}

fn check(name: &str, verdict: Verdict) -> Check {
    Check {
        name: name.into(),
        verdict,
        detail: format!("{name} detail"),
    }
}

fn archive(checks: Vec<Check>) -> ArchiveChecked {
    ArchiveChecked {
        archive: "fs:/archive".into(),
        segments: 3,
        inputs: 120,
        postings: 80,
        checks,
    }
}

fn verified() -> ArchiveChecked {
    archive(vec![check("a", Verdict::Passed)])
}

/// `NFR-9`, `INV-6`: axis 1 is the archive's verification. A failed check is a finding, whatever
/// else did not run; an undecided one is not a pass (as `archive verify` has it) and is not run;
/// an archive that could not be read is not run, with the reason; the inputs name what was read.
#[test]
fn nfr_9_axis_1_is_the_archives_verification() {
    let (r, inputs) = reconcile::axis_1_archive(Ok(&verified()));
    assert!(r.ran && r.findings.is_empty());
    assert_eq!(inputs["archive_inputs"], "120");
    assert_eq!(inputs["archive_segments"], "3");

    let failed = archive(vec![
        check("a", Verdict::Passed),
        check("b", Verdict::Failed),
        check("c", Verdict::Unknown),
    ]);
    let (r, _) = reconcile::axis_1_archive(Ok(&failed));
    assert!(
        r.ran && matches!(r.findings.as_slice(), [Finding::Archive { check, .. }] if check == "b"),
        "{r:?}"
    );

    let undecided = archive(vec![
        check("a", Verdict::Passed),
        check("c", Verdict::Unknown),
    ]);
    let (r, _) = reconcile::axis_1_archive(Ok(&undecided));
    assert!(
        !r.ran && r.note.as_deref().unwrap().contains("not a pass"),
        "{r:?}"
    );

    let (r, _) = reconcile::axis_1_archive(Ok(&archive(vec![check("a", Verdict::Deferred)])));
    assert!(!r.ran, "nothing passed is not a pass");

    let (r, inputs) = reconcile::axis_1_archive(Err("the archive is damaged"));
    assert!(!r.ran && r.note.as_deref().unwrap().contains("damaged"));
    assert!(inputs.is_empty());
}

/// `NFR-9`, `INV-5`, `AC-J3.2`: axis 2 reads the trial balance. A (currency, layer) that does not
/// sum to zero, and openings off zero by one minor unit, are findings; an unparsable figure is
/// not zero; a refusal is not run.
#[tokio::test]
async fn ac_j3_2_axis_2_through_ingress_finds_one_minor_unit() {
    let clean = Canned::new(&[balanced()]);
    let (r, inputs) = reconcile::axis_2_route(&clean).await;
    assert!(r.ran && r.findings.is_empty());
    assert_eq!(
        (
            inputs["trial_balance_view"].as_str(),
            inputs["trial_balance_accounts"].as_str()
        ),
        ("7", "12")
    );

    let off = Canned::new(&[(
        "GET /v1/trial-balance",
        200,
        json!({ "view": 8, "accounts": 12, "unbalanced": [{ "currency": 3, "layer": 1 }],
                "rows": [{ "asset": "COPM", "openings": "-1" },
                         { "asset": "DUSD", "openings": "0" },
                         { "asset": "USD", "openings": "oops" }] }),
    )]);
    let (r, _) = reconcile::axis_2_route(&off).await;
    assert!(r.ran);
    assert_eq!(
        r.findings,
        vec![
            Finding::Unbalanced {
                currency: 3,
                layer: 1,
                over: "the trial balance route".into()
            },
            Finding::TrialBalance {
                asset: "COPM".into()
            },
            Finding::TrialBalance {
                asset: "USD".into()
            },
        ]
    );

    let refused = Canned::new(&[("GET /v1/trial-balance", 403, json!({ "error": "no" }))]);
    let (r, _) = reconcile::axis_2_route(&refused).await;
    assert!(!r.ran && r.note.as_deref().unwrap().contains("403"));
}

/// `NFR-9`, owner's ruling: with nothing bound axis 3 has no baseline. It is reported as such,
/// counts as run with no difference, and writes nothing, flag or no flag. The run is clean and
/// pages no one.
#[tokio::test]
async fn nfr_9_axis_3_with_nothing_bound_is_no_baseline_and_writes_nothing() {
    for compare in [false, true] {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("runs");
        let link = Canned::new(&[balanced(), assets("book_only")]);
        let run = reconcile::job(&link, Ok(&verified()), compare, 5_000, &dir)
            .await
            .unwrap();
        let three = axis(&run, 3);
        assert!(three.ran && three.findings.is_empty(), "{three:?}");
        assert_eq!(three.note.as_deref(), Some("no baseline (nothing bound)"));
        assert!(run.clean() && run.audited.is_none());
        assert!(!link.wrote(), "{:?}", link.calls());
        assert!(pages(&dir, 5_000 + 3_600_000).delivered.is_empty());
    }
}

/// `NFR-9`: a bound asset needs the comparison, and the comparison writes an audit record. Without
/// `--compare-settlement` it is not made: axis 3 is not run (and pages as not run), the run is not
/// clean, and no write reaches ingress. An asset still `binding` is not bound.
#[tokio::test]
async fn nfr_9_axis_3_with_a_bound_asset_is_not_compared_without_the_opt_in() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("runs");
    let link = Canned::new(&[balanced(), assets("bound")]);
    let run = reconcile::job(&link, Ok(&verified()), false, 5_000, &dir)
        .await
        .unwrap();
    let three = axis(&run, 3);
    assert!(
        !three.ran
            && three
                .note
                .as_deref()
                .unwrap()
                .contains("--compare-settlement")
    );
    assert!(!run.clean() && run.audited.is_none());
    assert!(!link.wrote(), "{:?}", link.calls());
    assert_eq!(run.inputs["assets_bound"], "1");
    assert!(pages(&dir, 5_000 + 86_400_001).got("AledgentReconciliationNotRun"));

    // The opt-in on an asset that is only `binding` still compares nothing.
    let link = Canned::new(&[balanced(), assets("binding")]);
    let run = reconcile::job(&link, Ok(&verified()), true, 5_000, &dir)
        .await
        .unwrap();
    assert!(axis(&run, 3).ran && run.clean());
    assert!(!link.wrote(), "{:?}", link.calls());
}

/// `AC-J3.1`: with a bound asset and the opt-in, and only then, the comparison is posted — once —
/// and its operation reference is on the record. A difference in it is a finding.
#[tokio::test]
async fn ac_j3_1_the_opt_in_posts_the_comparison_once_for_a_bound_asset() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("runs");
    let comparison = |difference: &str| {
        json!({ "operation_reference": "did:op:1",
                "result": { "anchoring": { "comparison": {
                    "cut": 4, "height": 9, "at": 5_000, "anchors": [1], "differs": difference != "0",
                    "rows": [{ "asset": "COPM", "book": "10", "layer": "10", "difference": difference }]
                } } } })
    };
    for (difference, clean) in [("0", true), ("1", false)] {
        let link = Canned::new(&[
            balanced(),
            assets("bound"),
            (
                "GET /v1/ops/anchors",
                200,
                json!({ "latest_comparison": { "at": 4_000 } }),
            ),
            (
                "POST /v1/ops/settlement/comparisons",
                200,
                comparison(difference),
            ),
        ]);
        let run = reconcile::job(&link, Ok(&verified()), true, 5_000, &dir)
            .await
            .unwrap();
        let posts: Vec<_> = link
            .calls()
            .into_iter()
            .filter(|c| c.starts_with("POST"))
            .collect();
        assert_eq!(posts, ["POST /v1/ops/settlement/comparisons"]);
        assert_eq!(run.audited.as_deref(), Some("did:op:1"));
        assert_eq!(run.clean(), clean, "{run:#?}");
        if !clean {
            assert!(matches!(
                axis(&run, 3).findings.as_slice(),
                [Finding::Settlement { difference, .. }] if difference == "1"
            ));
        }
    }
}

/// `NFR-9`: the record is written even when an axis could not run, in the exporter's format; a
/// run that did not look at `Suspense` says so, and the exporter publishes no `Suspense` signal
/// from it rather than a zero. A record from before the new fields still reads.
#[tokio::test]
async fn nfr_9_the_record_says_what_it_covered_and_what_it_did_not() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("runs");
    let link = Canned::new(&[("GET /v1/assets", 500, json!({ "error": "down" }))]);
    let run = reconcile::job(&link, Err("no route to the archive"), true, 9_000, &dir)
        .await
        .unwrap();
    assert!(!axis(&run, 1).ran && !axis(&run, 2).ran && !axis(&run, 3).ran);
    assert!(!link.wrote());
    let back = reconcile::latest_run(&dir).unwrap().unwrap();
    assert_eq!(back, run);
    assert!(!back.suspense_checked && back.suspense.is_empty());
    assert!(axis(&back, 1).note.as_deref().unwrap().contains("no route"));
    assert!(reconcile::latest_axes(&dir).unwrap().iter().all(|a| !a.ran));

    // An old record: no `note`, `inputs` or `suspense_checked`.
    let old = json!({ "at": 1, "axes": [{ "axis": 1, "ran": true, "findings": [] }] });
    let old: Run = serde_json::from_value(old).unwrap();
    assert!(old.suspense_checked && old.inputs.is_empty() && old.axes[0].note.is_none());
}

/// `NFR-9`, `AC-J3.1`, `AC-J3.2`: against a real deployment, through its HTTP surface and the
/// operator's token. A clean book is clean on all three axes with the opt-in, the comparison is
/// on the audit trail, and nothing pages; without the opt-in no comparison is requested of the
/// core and axis 3 is not run, and a difference in the settlement layer is found with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_j3_1_the_job_against_a_deployment_writes_only_when_asked() {
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kit(tmp.path()).await;
    let dir = tmp.path().join("runs");
    let now = k.d.clock.now();
    let comparisons = |k: &MemKit| {
        k.d.core
            .with_ledger(|l| l.state().anchoring().comparisons().len())
            .unwrap()
    };
    // The job's principal is the operator (the trial balance is the operator's) and holds the
    // `compare_settlement` kind for the opt-in.
    let op = k.d.operator.token.clone();
    let (s, v) =
        k.d.call(
            "POST",
            "/v1/ops/operator-grants",
            Some(&op),
            &[],
            Some(json!({
                "principal": k.d.operator.identity,
                "kinds": ["compare_settlement"],
                "justification": "T-018-12: the daily job's principal",
            })),
        )
        .await;
    assert!(s.is_success(), "{v}");
    let before = comparisons(&k);

    // No opt-in: the trial balance is read, and no comparison is made.
    let run = reconcile::job(&operator_link(&k), Ok(&verified()), false, now, &dir)
        .await
        .unwrap();
    assert!(
        axis(&run, 2).ran && axis(&run, 2).findings.is_empty(),
        "{run:#?}"
    );
    assert_eq!(comparisons(&k), before);
    assert!(run.audited.is_none());

    // The opt-in.
    let run = reconcile::job(&operator_link(&k), Ok(&verified()), true, now, &dir)
        .await
        .unwrap();
    assert!(run.clean(), "{run:#?}");
    assert!(run.audited.is_some());
    assert!(comparisons(&k) > before);
    assert!(pages(&dir, now).delivered.is_empty());

    // One minor unit at the settlement layer, with the opt-in.
    k.chain
        .top_up_position(settle::testkit::INSTANCE, &settle::testkit::chain_usd(), 1);
    k.settle(3).await;
    let run = reconcile::job(&operator_link(&k), Ok(&verified()), true, now + 1, &dir)
        .await
        .unwrap();
    assert!(!run.clean());
    assert!(matches!(
        axis(&run, 3).findings.as_slice(),
        [Finding::Settlement { asset, difference, .. }] if asset == "USD" && difference == "1"
    ));
    assert!(pages(&dir, now + 1).got("AledgentReconciliationDifference"));
}

// ---------------------------------------------------------------------------------------------
// Review of #122: the gaps that made a pass out of nothing.
// ---------------------------------------------------------------------------------------------

/// `INV-5`: a trial balance over no accounts, or no assets, is zero by construction and reconciles
/// nothing: axis 2 is not run, and the run is not clean.
#[tokio::test]
async fn inv_5_an_empty_trial_balance_is_not_a_pass() {
    for body in [
        json!({ "view": 7, "accounts": 12, "unbalanced": [], "rows": [] }),
        json!({ "view": 7, "accounts": 0, "unbalanced": [],
                "rows": [{ "asset": "COPM", "openings": "0" }] }),
    ] {
        let link = Canned::new(&[("GET /v1/trial-balance", 200, body)]);
        let (r, inputs) = reconcile::axis_2_route(&link).await;
        assert!(
            !r.ran && r.note.as_deref().unwrap().contains("covered nothing"),
            "{r:?}"
        );
        assert_eq!(inputs["trial_balance_view"], "7");
    }
}

/// `INV-19`, `NFR-9`: a deferred check (the anchor checks under `--no-anchors`) is not a pass, even
/// beside passed ones: axis 1 is not run, and the note names the deferred checks.
#[test]
fn nfr_9_axis_1_with_a_deferred_check_is_not_run() {
    let a = archive(vec![
        check("book-rederives", Verdict::Passed),
        check("inv-19-anchors", Verdict::Deferred),
    ]);
    let (r, _) = reconcile::axis_1_archive(Ok(&a));
    assert!(!r.ran, "{r:?}");
    let note = r.note.unwrap();
    assert!(
        note.contains("inv-19-anchors") && note.contains("deferred"),
        "{note}"
    );
}

/// `NFR-9`: an unknown binding value, or an asset without a text id, is not known to have no
/// baseline: axis 3 is not run and writes nothing.
#[tokio::test]
async fn nfr_9_axis_3_with_an_unknown_binding_is_not_run() {
    for list in [
        json!([{ "id": "COPM", "binding": "unbinding" }]),
        json!([{ "id": "COPM", "binding": "book_only" }, { "id": 7, "binding": "book_only" }]),
        json!([{ "id": "COPM" }]),
    ] {
        let link = Canned::new(&[("GET /v1/assets", 200, list)]);
        let (r, audited, _) = reconcile::axis_3_gated(&link, true, "s", 1).await;
        assert!(!r.ran && audited.is_none(), "{r:?}");
        assert!(!link.wrote());
    }
}

fn comparison_reply(differs: bool, rows: Value) -> Value {
    json!({ "operation_reference": "did:op:1",
            "result": { "anchoring": { "comparison": {
                "cut": 4, "height": 9, "at": 5_000, "anchors": [1], "differs": differs,
                "rows": rows } } } })
}

fn zero_row(asset: &str) -> Value {
    json!({ "asset": asset, "book": "10", "layer": "10", "difference": "0" })
}

/// `AC-J3.2`, `REQ-PR-5`: a comparison that says it differs is a finding even with no row to show
/// it; a bound asset with no row is held by neither side (the rows are every asset either side
/// holds), and is said in the note.
#[tokio::test]
async fn ac_j3_2_the_comparison_must_cover_every_bound_asset_and_agree() {
    let anchors = (
        "GET /v1/ops/anchors",
        200,
        json!({ "latest_comparison": { "at": 4_000 } }),
    );
    let two_bound = (
        "GET /v1/assets",
        200,
        json!([{ "id": "COPM", "binding": "bound" }, { "id": "USD", "binding": "bound" }]),
    );
    let post = |differs, rows| {
        (
            "POST /v1/ops/settlement/comparisons",
            200,
            comparison_reply(differs, rows),
        )
    };

    let link = Canned::new(&[
        two_bound.clone(),
        anchors.clone(),
        post(true, json!([zero_row("COPM"), zero_row("USD")])),
    ]);
    let (r, _, _) = reconcile::axis_3_gated(&link, true, "s", 5_000).await;
    assert!(
        r.ran
            && matches!(
                r.findings.as_slice(),
                [Finding::SettlementDiffers { cut: 4 }]
            ),
        "{r:?}"
    );

    let link = Canned::new(&[
        two_bound.clone(),
        anchors.clone(),
        post(false, json!([zero_row("COPM")])),
    ]);
    let (r, _, _) = reconcile::axis_3_gated(&link, true, "s", 5_000).await;
    // `ComparisonBody.rows` holds every asset either side holds: no row is zero on both sides, and
    // the note says so rather than leaving it unsaid.
    assert!(r.ran && r.findings.is_empty(), "{r:?}");
    assert!(
        r.note
            .as_deref()
            .unwrap()
            .contains("no row for bound asset(s) USD"),
        "{r:?}"
    );

    // Covered and agreeing: clean. The anchors unreadable: said in a note, not skipped silently.
    let link = Canned::new(&[
        two_bound.clone(),
        ("GET /v1/ops/anchors", 500, json!({ "error": "down" })),
        post(false, json!([zero_row("COPM"), zero_row("USD")])),
    ]);
    let (r, _, _) = reconcile::axis_3_gated(&link, true, "s", 5_000).await;
    assert!(r.ran && r.findings.is_empty(), "{r:?}");
    assert!(
        r.note
            .as_deref()
            .unwrap()
            .contains("stale-cut check was skipped"),
        "{r:?}"
    );
}

/// `REQ-LA-2`: a later run that did not look at `Suspense` neither hides the last report that did
/// nor stands for a zero: the exporter keeps publishing that report, and its age keeps growing, so
/// `AledgentSuspenseNotReconciled` still fires. With no run that looked, there is no signal.
#[test]
fn req_la_2_the_suspense_report_outlives_runs_that_did_not_make_it() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let mk = |at, suspense: Vec<reconcile::SuspenseRow>, checked| Run {
        at,
        axes: Vec::new(),
        suspense,
        audited: None,
        inputs: Default::default(),
        suspense_checked: checked,
    };
    mk(100, Vec::new(), false).record(dir).unwrap();
    assert!(reconcile::latest_suspense_run(dir).unwrap().is_none());
    mk(
        200,
        vec![reconcile::SuspenseRow {
            account: "did:a".into(),
            asset: "USD".into(),
            since: 150,
        }],
        true,
    )
    .record(dir)
    .unwrap();
    mk(300, Vec::new(), false).record(dir).unwrap();
    let r = reconcile::latest_suspense_run(dir).unwrap().unwrap();
    assert_eq!(r.at, 200);
    let s = r.suspense_signal(150 + 86_400_001);
    assert_eq!((s.nonzero, s.oldest_age), (1, 86_400_001));
}
