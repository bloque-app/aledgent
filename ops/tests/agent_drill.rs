// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The restore drill of the Agent core (`NFR-10`, `NFR-4`, `INV-5`, `INV-6`, `INV-19`, `T-005-17`),
//! automated: a deployment whose core Agent streams every decision to its off-node archive is
//! backed up, loses its node, and is restored on a fresh one.
//!
//! 1. **Back up.** Ingress over the core Agent (the native transition; `aledgent-core-host`
//!    runs the same over VOS, and the stress workflow's drill scenario runs it there), its log
//!    streaming to a directory sink. The archive is stopped part way, so the decisions after it
//!    are the loss window a failure would open.
//! 2. **Destroy.** The node's state directory is deleted.
//! 3. **Restore.** The archive is read back and checked, its book re-derived natively and the
//!    drill's checks run (`ops::agent::verify`); it is laid out on a fresh node and the Agent is
//!    brought forward from its checkpoint (`roll_forward_agent`, what `aledgent-core-host
//!    restore` runs).
//! 4. **Verify.** Up to the archive's point the restored logs are the lost node's, byte for byte,
//!    and every reply the received-log recorded is the restored one: no acknowledged operation is
//!    lost up to the archive point. Then the received-log's tail is replayed through the
//!    restored core (`ops::agent::replay_tail`), and the restored node ends exactly where the lost
//!    one stood: the same logs, byte for byte, and the same roots. Zero divergence.

use std::collections::BTreeMap;
use std::time::Duration;

use core_archive::archive::Archive;
use core_archive::native::{STATE_FILE, SavingPort};
use core_archive::store::FsStore;
use core_archive::{Archiver, ArchiverConfig, ArchivingLog, book_of, fetch, lay_out};
use core_binding::core::{AgentCore, FileLog, Founding, HostLog as _, roll_forward_agent};
use core_binding::host::ReplicaStores;
use core_binding::hot::{Hot, Limits, MAX_BUDGET};
use ingress::auth::{CredentialKind, Token};
use ingress::boundary::{CoreReply, Outcome};
use ingress::inprocess::MemJournal;
use ingress::store::Store as _;
use ingress::store::mem::MemStore;
use ingress::testkit::{ANCHOR_INTERVAL_MS, Deployment, T0, USD, fast_config, genesis};
use ops::drill::Verdict;
use serde_json::json;

type AgentDeployment = Deployment<MemStore, AgentCore<SavingPort>>;

fn founding() -> Founding {
    let g = genesis(T0);
    Founding {
        genesis: g.genesis,
        op: g.op,
        at: g.at,
        keys: core_binding::host::HostKeys::local(g.root, None),
        limits: Limits {
            budget: MAX_BUDGET,
            ..Limits::DEFAULT
        },
        genesis_roots: None,
    }
}

fn hot<P: core_binding::host::AgentPort + Send + 'static>(core: &AgentCore<P>) -> Hot {
    core.with_port(|p| p.hot())
        .and_then(Result::ok)
        .flatten()
        .unwrap_or_else(|| panic!("the Agent is founded"))
}

async fn workload(d: &AgentDeployment, tag: &str, n: u32) {
    let alice = d.party(&format!("alice-{tag}"), 10_000).await;
    let bob = d.party(&format!("bob-{tag}"), 0).await;
    let a = Some(alice.token.as_str());
    for i in 0..n {
        let (s, r) = d
            .call_decided(
                "POST",
                "/v1/reservations",
                a,
                &[("client-key", &format!("{tag}-r{i}"))],
                Some(
                    json!({ "payer": alice.account, "asset": USD, "amount": (i + 1).to_string() }),
                ),
            )
            .await;
        assert_eq!(s.as_u16(), 200, "reserve: {r}");
        let (s, t) = d
            .call_decided(
                "POST",
                "/v1/transfers",
                a,
                &[("client-key", &format!("{tag}-t{i}"))],
                Some(json!({ "legs": [{
                    "from": alice.account, "to": bob.account, "asset": USD,
                    "amount": (i + 2).to_string(),
                }] })),
            )
            .await;
        assert_eq!(s.as_u16(), 200, "transfer: {t}");
    }
}

/// The adapter principal's bearer token: a constant, public, worth nothing.
const ADAPTER_TOKEN: &str = "adapter-token";

/// The settlement configuration names the deployment's instance, which every checkpoint's
/// commitment binds (`F-017`): with it, the book takes checkpoints on its ticks. The adapter it
/// names is a principal with a token, so [`adapter_inputs`] can speak as it.
async fn configure_settlement(d: &AgentDeployment) {
    let op = d.operator.token.clone();
    let (s, v) = d
        .call_decided(
            "POST",
            "/v1/identities",
            Some(&op),
            &[],
            Some(json!({ "kind": "organisation", "verifier": "0b" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    d.auth
        .add_token(
            &Token::new(ADAPTER_TOKEN.as_bytes()),
            v["result"]["identity"].as_str().unwrap(),
            v["result"]["credential"].as_str().unwrap(),
            CredentialKind::Service,
        )
        .expect("the adapter's token registers");
    let (s, v) = d
        .call_decided(
            "POST",
            "/v1/ops/settlement",
            Some(&op),
            &[],
            Some(json!({
                "adapter": v["result"]["identity"],
                "instance": 1,
                "inbound_deadline": 600_000u64,
                "expiry_window": 40,
                "block_time": 1_000,
                "finality_margin": 60_000u64,
            })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
}

/// What the settlement adapter sends the core, `n` times: its status, the finalised heads it
/// observed, and the ranges it skipped. Each is decided under a reference ingress minted for
/// it, and none of them is an operation (`REQ-IN-5`): most of a real book's replies are of this
/// kind.
async fn adapter_inputs(d: &AgentDeployment, n: u64) {
    for i in 0..n {
        let (s, v) = d
            .call_decided(
                "POST",
                "/v1/settlement/declarations",
                Some(ADAPTER_TOKEN),
                &[("client-key", &format!("declare-{i}"))],
                Some(json!({ "serving": true, "lease": 60_000u64 })),
            )
            .await;
        assert_eq!(s.as_u16(), 200, "declare: {v}");
        let (s, v) = d
            .call_decided(
                "POST",
                "/v1/settlement/heads",
                Some(ADAPTER_TOKEN),
                &[("client-key", &format!("head-{i}"))],
                Some(json!({ "height": 100 + i, "next_seq": 0 })),
            )
            .await;
        assert_eq!(s.as_u16(), 200, "head: {v}");
        let (s, v) = d
            .call_decided(
                "POST",
                "/v1/settlement/gaps",
                Some(ADAPTER_TOKEN),
                &[("client-key", &format!("gap-{i}"))],
                Some(json!({ "from": i, "to": i + 1, "reason": "pruned" })),
            )
            .await;
        assert_eq!(s.as_u16(), 200, "gap: {v}");
    }
}

fn replies_by_input(bytes: &[u8]) -> BTreeMap<u64, CoreReply> {
    #[derive(serde::Deserialize)]
    struct Line {
        reply: CoreReply,
    }
    bytes
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| {
            let l: Line = serde_json::from_slice(l).unwrap();
            (l.reply.input_seq, l.reply)
        })
        .collect()
}

/// `NFR-10`, `NFR-4`: back up, destroy, restore, then verify zero divergence and that no
/// acknowledged operation was lost up to the archive point (module documentation).
#[tokio::test(flavor = "multi_thread")]
async fn nfr_10_the_agent_core_restore_drill() {
    let tmp = tempfile::tempdir().unwrap();
    let (node, sink, fresh) = (
        tmp.path().join("node"),
        tmp.path().join("sink"),
        tmp.path().join("fresh"),
    );
    std::fs::create_dir_all(&node).unwrap();

    // 1. Back up: every decision streams to the sink, with checkpoints.
    let log = FileLog::open(&node).unwrap();
    let (log, tap) = ArchivingLog::new(log, &node, Some(SavingPort::reader(&node)));
    let core = AgentCore::open(SavingPort::new(&node), founding(), log).unwrap();
    let archiver = Archiver::start(
        tap,
        &node,
        Archive::new(Box::new(FsStore::open(&sink).unwrap())),
        book_of(&founding().op),
        ArchiverConfig {
            interval: Duration::from_millis(20),
            checkpoint_every_entries: 10,
            ..ArchiverConfig::default()
        },
    );
    let mut d = Deployment::assemble(
        MemStore::new(Duration::from_millis(50)),
        fast_config("ingress-0"),
        core.clone(),
        core.founded(),
        MemJournal::new(),
    )
    .await;
    configure_settlement(&d).await;
    adapter_inputs(&d, 6).await;
    workload(&d, "a", 10).await;
    // An anchoring interval passes: the book takes checkpoints (F-017), which the restore's
    // anchor checks read.
    d.clock.advance(ANCHOR_INTERVAL_MS + 1);
    workload(&d, "b", 4).await;
    // The archive stops here: what follows is the window a failure now would open.
    archiver.stop();
    workload(&d, "c", 5).await;
    assert!(d.quiesce(ops::kit::PATIENCE).await, "every input decided");
    d.replica.kill();
    let lost = hot(&core);
    let lost_archive = std::fs::read(node.join("archive.bin")).unwrap();
    let lost_replies = std::fs::read(node.join("replies.jsonl")).unwrap();
    let rows = d.store.rows().await.unwrap();

    // 2. Destroy the node.
    drop(core);
    std::fs::remove_dir_all(&node).unwrap();

    // 3. Restore: read and check the archive, re-derive its book, run the drill's checks.
    let started = std::time::Instant::now();
    let archive = Archive::new(Box::new(FsStore::open(&sink).unwrap()));
    let f = fetch(&archive, Some(&book_of(&founding().op))).unwrap();
    assert!(!f.damaged, "{:?}", f.notes);
    let layer = verify::MemAnchors::new();
    let verified = ops::agent::verify(&f, &founding(), Some(&layer)).unwrap();
    for c in &verified.checks {
        assert_ne!(c.verdict, Verdict::Failed, "{}: {}", c.name, c.detail);
    }
    assert!(verified.passed());
    assert!(verified.rebuild.is_clean());
    // The archive holds the adapter's replies, which name no operation (the 2026-10-15 drill's
    // g8 archive was mostly these), and NFR-4 holds the other replies to the book without them.
    let observed = f
        .replies_parsed
        .iter()
        .filter(|(_, r)| {
            matches!(&r.outcome, Outcome::Decided { result, .. } if result.settlement.is_some())
        })
        .count();
    assert!(observed >= 18, "{observed} adapter replies archived");
    let held = ops::agent::acknowledgements(&f.replies_parsed, &f.entries);
    assert!(
        ops::agent::acknowledgements(&f.replies_parsed, &[]).len() >= held.len() + 18,
        "the adapter's declarations and heads and gaps are not held to the book as operations"
    );
    for name in [
        "INV-6",
        "INV-5",
        "composite-root",
        "INV-19",
        "NFR-4",
        "archive-point",
    ] {
        assert!(
            verified
                .checks
                .iter()
                .any(|c| c.name == name && c.verdict == Verdict::Passed),
            "{name} ran and passed"
        );
    }
    let plan = lay_out(&fresh, &f, &archive.describe(), STATE_FILE, true, 0).unwrap();
    let (_, start) = plan
        .checkpoint
        .expect("the restore starts from a checkpoint");
    let entries = FileLog::open(&fresh).unwrap().entries().unwrap();
    let (_, rolled) = roll_forward_agent(
        SavingPort::restore(&fresh).unwrap(),
        &founding(),
        &entries,
        &ReplicaStores::Memory,
    )
    .unwrap();
    assert_eq!((rolled.from, rolled.to), (start, f.point.entries));
    assert_eq!(hex::encode(rolled.roots.kernel), f.point.kernel_root);
    let restored = AgentCore::open(
        SavingPort::restore(&fresh).unwrap(),
        founding(),
        FileLog::open(&fresh).unwrap(),
    )
    .unwrap();
    let recovery_ms = started.elapsed().as_millis();

    // 4a. Up to the archive's point: the lost node's logs byte for byte, and every reply the
    // received-log recorded.
    assert!(f.point.inputs > 0 && f.point.inputs < replies_by_input(&lost_replies).len() as u64);
    assert_eq!(f.archive[..], lost_archive[..f.archive.len()]);
    assert_eq!(f.replies[..], lost_replies[..f.replies.len()]);
    let restored_replies = replies_by_input(&std::fs::read(fresh.join("replies.jsonl")).unwrap());
    let mut acknowledged = 0;
    for r in rows
        .iter()
        .filter(|r| r.status == ingress::store::Status::Decided)
    {
        let seq = r.core_input_seq.expect("a decided row has its input");
        if seq > f.point.inputs {
            continue;
        }
        let stored: CoreReply = serde_json::from_slice(r.core_reply.as_deref().unwrap()).unwrap();
        assert_eq!(
            restored_replies.get(&seq),
            Some(&stored),
            "NFR-4: input {seq}, acknowledged before the archive's point, is restored"
        );
        acknowledged += 1;
    }
    assert_eq!(acknowledged, f.point.inputs);

    // 4b. The window after the point, from the received-log: the restored node ends where the
    // lost one stood. Zero divergence.
    let tail = ops::agent::replay_tail(&restored, &rows, f.point.inputs)
        .await
        .unwrap();
    assert!(tail.replayed > 0, "the loss window was replayed");
    assert_eq!(tail.after + tail.replayed, tail.last);
    let now = hot(&restored);
    assert_eq!(
        (now.inputs, now.log_head, now.roots()),
        (lost.inputs, lost.log_head, lost.roots()),
        "zero divergence: the restored Agent stands where the lost one did"
    );
    assert_eq!(
        std::fs::read(fresh.join("archive.bin")).unwrap(),
        lost_archive
    );
    assert_eq!(
        std::fs::read(fresh.join("replies.jsonl")).unwrap(),
        lost_replies
    );
    eprintln!(
        "drill: archive point at input {} of {} ({} replayed from the received-log), checkpoint at Agent input {start}, restored in {recovery_ms} ms",
        f.point.inputs, tail.last, tail.replayed
    );
}

/// The restore refuses, before laying anything out, an archive the settlement layer contradicts:
/// an anchor after the archived book's last checkpoint (the archive is behind the book the layer
/// anchored), or a layer it cannot read. And an archived entry tampered with, chain and digests
/// recomputed, does not re-derive.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_10_the_restore_refuses_an_archive_the_anchors_or_its_roots_contradict() {
    let tmp = tempfile::tempdir().unwrap();
    let (node, sink) = (tmp.path().join("node"), tmp.path().join("sink"));
    std::fs::create_dir_all(&node).unwrap();
    let log = FileLog::open(&node).unwrap();
    let (log, tap) = ArchivingLog::new(log, &node, None);
    let core = AgentCore::open(SavingPort::new(&node), founding(), log).unwrap();
    let archiver = Archiver::start(
        tap,
        &node,
        Archive::new(Box::new(FsStore::open(&sink).unwrap())),
        book_of(&founding().op),
        ArchiverConfig {
            interval: Duration::from_millis(20),
            ..ArchiverConfig::default()
        },
    );
    let d = Deployment::assemble(
        MemStore::new(Duration::from_millis(50)),
        fast_config("ingress-0"),
        core.clone(),
        core.founded(),
        MemJournal::new(),
    )
    .await;
    configure_settlement(&d).await;
    workload(&d, "a", 2).await;
    d.clock.advance(ANCHOR_INTERVAL_MS + 1);
    workload(&d, "b", 2).await;
    archiver.stop();
    let f = fetch(&Archive::new(Box::new(FsStore::open(&sink).unwrap())), None).unwrap();
    // The archived book's checkpoints, and where they are anchored.
    let (replica, _) = core_binding::host::replay(
        &founding().keys,
        founding().genesis,
        founding().op,
        founding().at,
        &f.entries,
    )
    .unwrap();
    let last = replica
        .state()
        .anchoring()
        .checkpoints()
        .max_by_key(|c| c.number)
        .copied()
        .expect("an anchoring interval passed: a checkpoint was taken");
    let refused = |layer: &verify::MemAnchors| {
        let v = ops::agent::verify(&f, &founding(), Some(layer)).unwrap();
        assert!(!v.passed());
        v.checks
            .into_iter()
            .find(|c| c.verdict == Verdict::Failed)
            .unwrap()
    };
    let mut ahead = verify::MemAnchors::new();
    ahead.put_in(
        last.anchored_in(),
        last.number + 1,
        verify::PublicAnchor {
            commitment: [7; 32],
            stored_at: 9,
            read_at: 10,
        },
    );
    let c = refused(&ahead);
    assert_eq!(c.name, "archive-behind-anchor", "{}", c.detail);
    let mut down = verify::MemAnchors::new();
    down.fail("the node does not answer");
    refused(&down);

    // A tampered entry, its chain recomputed: the roots it claims are not the book's.
    let mut bad = f.entries.clone();
    let k = bad.len() / 2;
    bad[k].roots.kernel[0] ^= 1;
    let mut prev = bad[k].log_hash();
    for e in bad.iter_mut().skip(k + 1) {
        e.prev = prev;
        prev = e.log_hash();
    }
    let tampered = core_archive::Fetched { entries: bad, ..f };
    let e = ops::agent::verify(&tampered, &founding(), None).unwrap_err();
    assert!(e.to_string().contains("does not re-derive"), "{e}");
    drop((d, core));
}
