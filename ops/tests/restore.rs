// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The restore procedure and its drill, in process (`T-018-01`, `T-018-02`, `T-018-03`):
//! ingress's received-log and the adapter's store in memory, the core restored from an `F-014`
//! export. `tests/postgres.rs` runs the same drill with both stores restored from PostgreSQL base
//! backups and WAL archives.

use std::time::Duration;

use ingress::store::Store as _;
use ingress::testkit::USD;
use ledger_core::export::rederive::Acknowledgement;
use ops::backup;
use ops::drill::{Expected, Verdict};
use ops::kit::{DrillPlan, OpsKit, TEST_SEALING_KEY, run_drill};
use ops::seal::SealingKey;
use serde_json::json;

type MemKit = OpsKit<ingress::store::mem::MemStore, settle::store::mem::MemStore>;

async fn kit(dir: &std::path::Path) -> MemKit {
    OpsKit::start(
        ingress::store::mem::MemStore::new(Duration::from_millis(50)),
        settle::store::mem::MemStore::new(),
        &dir.join("audit"),
        &dir.join("book"),
    )
    .await
}

fn expected(k: &MemKit) -> Expected {
    let r = k.roots().expect("the core runs");
    Expected {
        kernel_root: r.kernel,
        book_root: r.book,
        inputs: r.inputs,
    }
}

/// `T-018-01`: a core backup is three artifacts with verified checksums; it holds no blinding
/// and no key root; its openings open only under the sealing key; one changed byte fails it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_018_01_a_core_backup_verifies_and_holds_no_secret_in_the_clear() {
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kit(tmp.path()).await;
    k.round().await;
    let root = tmp.path().join("backups");
    let (dir, m) = k.backup_core(&root, "b1").await.unwrap();
    assert_eq!(backup::verify(&dir).unwrap(), m);
    let point = m.point.clone().unwrap();
    assert!(point.inputs > 0 && point.sequence > 0);
    let bundle = backup::core::read(&dir, &SealingKey::from_bytes(TEST_SEALING_KEY)).unwrap();
    assert_eq!(bundle.sidecar.inputs, point.inputs);
    // The export and the sidecar hold no blinding and not the key root.
    let blindings: Vec<[u8; 32]> = bundle
        .openings
        .rows()
        .values()
        .flat_map(|s| [*s.dr.blinding.as_bytes(), *s.cr.blinding.as_bytes()])
        .filter(|b| *b != [0u8; 32])
        .collect();
    assert!(!blindings.is_empty());
    let root_key = ingress::testkit::genesis(ingress::testkit::T0).root;
    for name in [
        backup::core::EXPORT,
        backup::core::SIDECAR,
        backup::core::OPENINGS,
    ] {
        let bytes = std::fs::read(dir.join(name)).unwrap();
        for b in blindings.iter().chain([&root_key]) {
            assert!(!bytes.windows(32).any(|w| w == b), "{name} holds a secret");
        }
    }
    // Another sealing key does not open the openings; a changed byte fails verification.
    assert!(backup::core::read(&dir, &SealingKey::from_bytes([1; 32])).is_err());
    let mut export = std::fs::read(dir.join(backup::core::EXPORT)).unwrap();
    let last = export.len() - 1;
    export[last] ^= 1;
    std::fs::write(dir.join(backup::core::EXPORT), export).unwrap();
    assert!(matches!(
        backup::verify(&dir),
        Err(ops::Error::Checksum { .. })
    ));
    // The audit store's backup is a verified copy with the same head.
    let (adir, _, head) = backup::audit::take(&k.audit_dir, &root, "a1", 1).unwrap();
    assert_eq!(
        head,
        audit_store::FileAuditStore::read(&k.audit_dir)
            .unwrap()
            .verify()
            .unwrap()
    );
    let restored = tmp.path().join("audit-restored");
    assert_eq!(backup::audit::restore(&adir, &restored).unwrap(), head);
    assert!(
        backup::audit::restore(&adir, &restored).is_err(),
        "not over a store"
    );
}

/// `REQ-PO-5`, `REQ-RC-3` (ruling `pq8-export` (a), `F-009` decision 31): a core backup carries the
/// pass-through data of its operations, read from the received-log, since the core never holds it;
/// an import restores it onto a log that lacks it, leaves it where the log holds it, and refuses a
/// log of another history.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn req_po_5_an_export_carries_pass_through_data_and_an_import_restores_it() {
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kit(tmp.path()).await;
    let (a, b) = (k.parties[0].clone(), k.parties[1].clone());
    let data = json!({ "order": "o-1", "graph": { "id": "g-7", "node": "transfer_single" } });
    let (s, v) =
        k.d.call(
            "POST",
            "/v1/transfers",
            Some(&a.token),
            &[],
            Some(json!({
                "legs": [{ "from": a.account, "to": b.account, "asset": USD, "amount": "2" }],
                "reference": data,
            })),
        )
        .await;
    assert_eq!(s.as_u16(), 202, "{v}");
    let op = v["operation_reference"].as_str().unwrap().to_owned();
    k.quiet().await;
    let root = tmp.path().join("backups");
    let (dir, m) = k.backup_core(&root, "b1").await.unwrap();
    assert!(m.artifact(backup::core::PASS_THROUGH).is_some());
    let bundle = backup::core::read(&dir, &SealingKey::from_bytes(TEST_SEALING_KEY)).unwrap();
    let key = ingress::pass_through::key_of(&op).unwrap();
    assert_eq!(bundle.pass_through.len(), 1);
    let carried = &bundle.pass_through[0];
    assert_eq!(
        (carried.op_ref, carried.principal.as_str()),
        (key, a.identity.as_str())
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(carried.pass_through.get()).unwrap(),
        data
    );
    // The log the backup was read from holds it already: nothing is written.
    let held = ops::restore::restore_pass_through(&k.d.store, &bundle.pass_through)
        .await
        .unwrap();
    assert_eq!((held.carried, held.written), (1, 0));
    // A log whose row lacks it gets it back, exactly as sent.
    let lacking = ingress::store::mem::MemStore::new(Duration::from_millis(50));
    let mut row = k.d.store.get(key).await.unwrap().unwrap().new;
    row.pass_through = None;
    lacking.append(row).await.unwrap();
    let restored = ops::restore::restore_pass_through(&lacking, &bundle.pass_through)
        .await
        .unwrap();
    assert_eq!((restored.carried, restored.written), (1, 1));
    let back = lacking
        .get(key)
        .await
        .unwrap()
        .unwrap()
        .new
        .pass_through
        .unwrap();
    assert_eq!(back.get(), carried.pass_through.get());
    // Another principal's row, or no row at all: the log is not of the backup's history.
    let mut other = bundle.pass_through.clone();
    other[0].principal = b.identity.clone();
    assert!(
        ops::restore::restore_pass_through(&lacking, &other)
            .await
            .is_err()
    );
    let empty = ingress::store::mem::MemStore::new(Duration::from_millis(50));
    assert!(
        ops::restore::restore_pass_through(&empty, &bundle.pass_through)
            .await
            .is_err()
    );
}

/// `T-018-02` and `T-018-03`: a restore to an older backup plus the ordered replay of the
/// received-log yields the pre-failure composite root and book root (`RISK-5`); the drill's
/// checks pass (`INV-6`, `INV-5`, `INV-19` against the layer, `NFR-4`, `REQ-IN-5`), the export
/// of format 5 carries `F-017`'s anchoring sections across, a command acknowledged at `Received`
/// while the core was down is decided by the restored core, a deposit the layer recorded during
/// the outage is read by the restored adapter, and the result is recorded (`NFR-10`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_k5_1_a_restore_to_an_older_backup_and_replay_yield_the_pre_failure_root() {
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kit(tmp.path()).await;
    for _ in 0..2 {
        k.round().await;
        k.anchor_round(12).await;
    }
    let root = tmp.path().join("backups");
    let (core_backup, m) = k.backup_core(&root, "b1").await.unwrap();
    let (audit_backup, _, _) = backup::audit::take(&k.audit_dir, &root, "a1", 1).unwrap();
    // Export format 12 (F-017's six anchoring sections, the signing records of the owner's ruling
    // of 2026-09-24, F-027's anchor venues, amendment 0006's resolution cases, F-028's
    // credential schemes, amendment 0011's integrating parties and notification anchor, F-029's
    // asset bindings and attestations, F-031's kernel accounts placed in partitions, and
    // T-037-03's checkpoint construction and code binding): the anchoring sections, filled, cross
    // the backup.
    let bundle =
        backup::core::read(&core_backup, &SealingKey::from_bytes(TEST_SEALING_KEY)).unwrap();
    assert_eq!(
        bundle.export.manifest.version,
        ledger_core::export::FORMAT_VERSION
    );
    assert_eq!(bundle.export.manifest.version, 12);
    let anchoring: Vec<_> = bundle
        .export
        .manifest
        .sections
        .iter()
        .filter(|e| e.name.starts_with("service.anchor-"))
        .map(|e| (e.name.clone(), e.count))
        .collect();
    assert_eq!(anchoring.len(), 6, "{anchoring:?}");
    for want in [
        "service.anchor-checkpoints",
        "service.anchor-comparisons",
        "service.anchor-latest",
    ] {
        assert!(
            anchoring.iter().any(|(n, c)| n == want && *c > 0),
            "{anchoring:?}"
        );
    }
    let finals_at_backup = final_count(&k);
    assert!(finals_at_backup >= 2);
    // The book moves on after the backup: this is what the replay must reproduce.
    for _ in 0..3 {
        k.round().await;
        k.anchor_round(12).await;
    }
    k.quiet().await;
    assert!(
        final_count(&k) > finals_at_backup,
        "anchors after the backup point"
    );
    let source_anchoring = anchoring_of(&k.d.core);
    let want = expected(&k);
    assert!(want.inputs > m.point.as_ref().unwrap().inputs);
    let carol_before = available(&k.d.router, &k.parties[2]).await;
    // A transfer acknowledged at Received while the core is down, and a deposit on the layer.
    k.fail();
    let (a, b) = (k.parties[0].clone(), k.parties[1].clone());
    let (s, v) = k
        .d
        .call(
            "POST",
            "/v1/transfers",
            Some(&a.token),
            &[],
            Some(json!({ "legs": [{ "from": a.account, "to": b.account, "asset": USD, "amount": "3" }] })),
        )
        .await;
    assert_eq!(s.as_u16(), 202, "{v}");
    let received = v["operation_reference"].as_str().unwrap().to_owned();
    k.deposit(&k.parties[2].clone(), 777);
    k.chain.advance(2);
    // The drill, on a host of its own: only the backups, the restored stores and the layer.
    let host = tmp.path().join("isolated");
    let outside = k.outside();
    let drilled = run_drill(
        &DrillPlan {
            id: "d1",
            host: "in-process",
            workdir: &host,
            core_backup: &core_backup,
            audit_backup: &audit_backup,
            expected: Some(want),
            wal_window_s: None,
        },
        &outside,
        k.d.store.clone(),
        k.adapter_store.clone(),
    )
    .await
    .unwrap();
    let r = &drilled.report;
    for c in &r.checks {
        assert_eq!(c.verdict, Verdict::Passed, "{}: {}", c.name, c.detail);
    }
    assert!(r.passed(), "{r:#?}");
    // INV-19: the anchors after the backup point recomputed from rebuilt state; every one on the
    // layer.
    let inv19 = r.checks.iter().find(|c| c.name == "INV-19").unwrap();
    assert!(!inv19.detail.contains(" 0 recomputed"), "{}", inv19.detail);
    // The restored anchor store is the source's (format 5 round trip, then the replay).
    assert_eq!(anchoring_of(&drilled.core), source_anchoring);
    assert_eq!(r.replay.backup_point, m.point.unwrap().inputs);
    assert_eq!(r.replay.last_input, want.inputs);
    assert!(r.replay.replayed > 0 && r.replay.undecided > 0);
    assert_eq!(r.data_loss.inputs_lost, 0);
    assert!(r.rebuild.is_clean() && r.rebuild.records > 0);
    assert!(r.rescan.as_ref().unwrap().is_clean());
    // The transfer acknowledged at Received was decided by the restored core.
    let op = ledger_core::id::OperationId::parse(&received).unwrap();
    let state = drilled
        .core
        .with_ledger(|l| l.operation(&op).map(|o| o.state()))
        .flatten();
    assert!(state.is_some(), "the Received transfer exists");
    let acks = [Acknowledgement { op, state: None }];
    assert_eq!(
        drilled.core.with_ledger(|l| l.rederive(&acks)),
        Some(Ok(()))
    );
    // The deposit of the outage was read and issued by the restored adapter.
    let after = available(&drilled.router, &k.parties[2]).await;
    assert_eq!(after, carol_before + 777, "the outage's deposit was issued");
    // The restored deployment takes new commands, and restarts from its rebuilt journal.
    let (s, v) = ingress::testkit::call(
        &drilled.router,
        "POST",
        "/v1/transfers",
        Some(&a.token),
        &[],
        Some(json!({ "legs": [{ "from": a.account, "to": b.account, "asset": USD, "amount": "1" }] })),
    )
    .await;
    assert_eq!(s.as_u16(), 202, "{v}");
    let quiet = tokio::time::Instant::now() + ops::kit::PATIENCE;
    while !k.d.store.oldest_undecided().await.unwrap().is_none() {
        assert!(tokio::time::Instant::now() < quiet, "undecided");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    drop(drilled.replica);
    let before = drilled
        .core
        .with_ledger(|l| l.book().root().unwrap())
        .unwrap();
    drilled.core.stop();
    drilled.core.start().unwrap();
    let after = drilled
        .core
        .with_ledger(|l| l.book().root().unwrap())
        .unwrap();
    assert_eq!(before, after, "a genesis replay of the rebuilt journal");
    // The record.
    let path = r.record(&tmp.path().join("results")).unwrap();
    let back = ops::drill::DrillReport::latest(path.parent().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(&back, r);
    // The roots check bites: another last committed root fails it.
    let bad = Expected {
        kernel_root: [0; 32],
        ..want
    };
    let failed = drilled
        .core
        .with_ledger(|l| ops::drill::checks(l, &[], Some(&bad), &r.rebuild, None))
        .unwrap();
    assert!(
        failed
            .iter()
            .any(|c| c.name == "composite-root" && c.verdict == Verdict::Failed)
    );
    // INV-19 bites: a layer that holds none of the anchors fails it.
    let empty = verify::MemAnchors::new();
    let failed = drilled
        .core
        .with_ledger(|l| {
            ops::drill::anchor_check(
                l,
                r.replay.import_commits,
                &empty as &dyn verify::AnchorSource,
            )
        })
        .unwrap();
    assert_eq!(failed.verdict, Verdict::Failed, "{}", failed.detail);
}

/// How many checkpoints are final in the running core.
fn final_count(k: &MemKit) -> usize {
    k.d.core
        .with_ledger(|l| {
            l.state()
                .anchoring()
                .checkpoints()
                .filter(|c| c.status == ledger_core::anchor::CheckpointStatus::Final)
                .count()
        })
        .unwrap()
}

/// The anchor store's checkpoints and incidents, as comparable values.
fn anchoring_of(
    core: &ingress::testkit::TestCore,
) -> (Vec<ledger_core::anchor::Checkpoint>, usize) {
    core.with_ledger(|l| {
        let s = l.state().anchoring();
        (s.checkpoints().copied().collect(), s.incidents().len())
    })
    .unwrap()
}

/// `party`'s available balance of `USD`, through `router`.
async fn available(router: &axum::Router, party: &ingress::testkit::Party) -> u64 {
    let account = party.account.clone().unwrap();
    let (_, v) = ingress::testkit::call(
        router,
        "GET",
        &format!("/v1/balances?account={account}&asset={USD}"),
        Some(&party.token),
        &[],
        None,
    )
    .await;
    fn find(v: &serde_json::Value) -> Option<u64> {
        match v {
            serde_json::Value::Object(m) => m
                .get("available")
                .and_then(|x| x.as_str().and_then(|s| s.parse().ok()).or(x.as_u64()))
                .or_else(|| m.values().find_map(find)),
            serde_json::Value::Array(a) => a.iter().find_map(find),
            _ => None,
        }
    }
    find(&v).unwrap_or_else(|| panic!("no available balance in {v}"))
}

/// `T-018-02`: a received-log that ends before the core backup's point, or that lost an input,
/// is refused; nothing is imported from it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn t_018_02_a_log_behind_the_core_backup_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let mut k = kit(tmp.path()).await;
    k.round().await;
    k.quiet().await;
    let root = tmp.path().join("backups");
    let (core_backup, _) = k.backup_core(&root, "b1").await.unwrap();
    let rows = k.d.store.rows().await.unwrap();
    let key = SealingKey::from_bytes(TEST_SEALING_KEY);
    let mut audit = ops::kit::open_audit(&k.audit_dir, &tmp.path().join("book"));
    let by = ops::restore::ImportBy {
        ctx: k.ops_ctx(),
        justification: b"drill",
    };
    for cut in [rows.len() / 2, rows.len() - 1] {
        let bundle = backup::core::read(&core_backup, &key).unwrap();
        let mut short: Vec<_> = rows.clone();
        // Drop the decided row of one input: either the tail or one in the middle.
        let victim = if cut == rows.len() - 1 {
            short
                .iter()
                .enumerate()
                .filter(|(_, r)| r.core_input_seq.is_some())
                .max_by_key(|(_, r)| r.core_input_seq)
                .map(|(i, _)| i)
                .unwrap()
        } else {
            cut
        };
        short.remove(victim);
        let r = ops::restore::restore_core(
            bundle,
            ingress::testkit::genesis(ingress::testkit::T0),
            ledger_kernel::store::MemStore::new(),
            ledger_kernel::vault::MemVault::new(),
            ops::kit::mem_factory(),
            ingress::inprocess::MemJournal::new(),
            Some(ingress::testkit::test_clerk()),
            &short,
            &by,
            &mut audit,
        )
        .await;
        assert!(matches!(r, Err(ops::Error::Restore(_))), "{:?}", r.err());
    }
    // With no core backup at all, the journal rebuilt from the whole log replays from genesis to
    // the same book (the key-loss runbook's way back).
    let want = expected(&k);
    let mut journal = ingress::inprocess::MemJournal::new();
    let n = ops::restore::rebuild_journal(&rows, &mut journal).unwrap();
    assert_eq!(n, want.inputs);
    let core = ingress::inprocess::InProcessCore::in_memory(
        ingress::testkit::genesis(ingress::testkit::T0),
        journal,
    )
    .with_clerk(ingress::testkit::test_clerk());
    core.start().unwrap();
    let root = core.with_ledger(|l| l.book().root().unwrap()).unwrap();
    assert_eq!(root, want.kernel_root);
    assert_eq!(core.inputs(), Some(want.inputs));
}
