// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The first start of this build over a replica the current release kept at a clean stop
//! (`T-031-26`; `REQ-RT-3`, `NFR-4`): shipping the manifest takes one clean restart of the core
//! host, and that start must reopen the replica the release kept, not rebuild it.
//!
//! **The fixture.** `tests/fixtures/t031-26-release-e935c21/` is what the release's build
//! (`e935c21`, `v0.5.0-dev+e935c21`, the build prod runs since 17 Nov) left after a clean stop of a
//! small book: the replica's directory as it was (`replica/`: the book's leaves, its openings, its
//! archive, the service tree and T-031-25's `kept.json`), the host's log in its own files (`log/`:
//! `archive.bin` and `replies.jsonl`), and the native Agent's rows (`agent.json`). It was recorded
//! by [`record_fixture`] run on that commit (this file up to and including that test, copied
//! there: it uses no API the release does not have), and is never rewritten.
//!
//! **The proof.** This build opens the fixture's replica with the fixture's Agent and log, and must
//! reopen it without a rebuild, leaf for leaf what a rebuild from genesis over the same Agent and
//! log gives; replace the record with a manifest at once; commit on; and, killed after that, come
//! back from the manifest without a rebuild.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use core_binding::core::{AgentCore, FileLog, Founding};
use core_binding::host::{KEPT_FILE, NativePort, ReplicaStores};
use core_binding::hot::{Limits, MAX_BUDGET};
use ingress::inprocess::MemJournal;
use ingress::store::mem::MemStore;
use ingress::testkit::{Deployment, T0, USD, fast_config, genesis};
use serde_json::{Value, json};

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

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/t031-26-release-e935c21")
}

fn disk(dir: &Path) -> ReplicaStores {
    ReplicaStores::Disk {
        dir: dir.to_path_buf(),
        cache_bytes: 1024 * 1024,
    }
}

async fn deploy(core: &AgentCore<NativePort>) -> Deployment<MemStore, AgentCore<NativePort>> {
    let mut config = fast_config("ingress-0");
    config.tick_interval = Duration::from_secs(365 * 86_400);
    Deployment::assemble(
        MemStore::new(Duration::from_millis(50)),
        config,
        core.clone(),
        core.founded(),
        MemJournal::new(),
    )
    .await
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn agent_json(port: &NativePort) -> Value {
    json!({
        "slot": port.slot,
        "rows": port
            .rows
            .0
            .iter()
            .map(|(k, v)| [hex::encode(k), hex::encode(v)])
            .collect::<Vec<_>>(),
    })
}

fn agent_of(v: &Value) -> NativePort {
    let mut p = NativePort::new();
    p.slot = v["slot"].as_u64().unwrap();
    for kv in v["rows"].as_array().unwrap() {
        let k = hex::decode(kv[0].as_str().unwrap()).unwrap();
        let v = hex::decode(kv[1].as_str().unwrap()).unwrap();
        p.rows.0.insert(k, v);
    }
    p
}

/// Record the fixture (module documentation), on the release's build only. Refuses to run unless
/// `ALEDGENT_T031_26_RECORD` names an empty directory to write it to.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "records the fixture on the release's build: ALEDGENT_T031_26_RECORD=<dir>"]
async fn record_fixture() {
    let out = PathBuf::from(std::env::var("ALEDGENT_T031_26_RECORD").expect("a directory"));
    assert!(
        !out.exists() || std::fs::read_dir(&out).unwrap().next().is_none(),
        "the fixture is recorded already: it is never rewritten"
    );
    let work = std::env::temp_dir().join(format!("aledgent-release-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    let core = AgentCore::open_in(
        NativePort::new(),
        founding(),
        FileLog::open(work.join("log")).unwrap(),
        disk(&work.join("replica")),
    )
    .unwrap();
    let d = deploy(&core).await;
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 0).await;
    for i in 0..4 {
        let (s, v) = d
            .call_decided(
                "POST",
                "/v1/reservations",
                Some(&alice.token),
                &[("client-key", &format!("r{i}"))],
                Some(json!({ "payer": alice.account, "asset": USD, "amount": "3" })),
            )
            .await;
        assert_eq!(s.as_u16(), 200, "{v}");
    }
    let (s, v) = d
        .call(
            "POST",
            "/v1/identities",
            Some(&d.operator.token),
            &[("client-key", "carol")],
            Some(json!({ "kind": "person", "verifier": "0a" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    assert!(bob.account.is_some());
    drop(d);
    let kept = core.keep_replica(Duration::from_secs(1)).expect("kept");
    let agent = core.with_port(|p| agent_json(p)).expect("the Agent's rows");
    drop(core);
    copy_dir(&work.join("replica"), &out.join("replica"));
    copy_dir(&work.join("log"), &out.join("log"));
    std::fs::write(
        out.join("agent.json"),
        serde_json::to_vec_pretty(&agent).unwrap(),
    )
    .unwrap();
    eprintln!("recorded: a replica kept at input {kept}");
    let _ = std::fs::remove_dir_all(&work);
}

fn layout(dir: &Path) -> u64 {
    let v: Value = serde_json::from_slice(&std::fs::read(dir.join(KEPT_FILE)).unwrap()).unwrap();
    v["version"].as_u64().unwrap()
}

/// `T-031-26`: this build's first start over the release's clean stop reopens the replica without
/// a rebuild, and it is what a rebuild gives; it replaces the record with a manifest at once,
/// commits on, and a kill after that comes back from the manifest.
#[tokio::test(flavor = "multi_thread")]
async fn t031_26_the_first_start_over_the_release_s_clean_stop_reopens_without_a_rebuild() {
    // The fixture is a book in roots v2, the release's; a roots-v1 build (smt-v1) founds another
    // book from the same genesis, as for the code-history gate's.
    if cfg!(feature = "smt-v1") {
        return;
    }
    let fx = fixture();
    assert_eq!(layout(&fx.join("replica")), 1, "the release's record");
    let agent: Value =
        serde_json::from_slice(&std::fs::read(fx.join("agent.json")).unwrap()).unwrap();
    let work = std::env::temp_dir().join(format!("aledgent-upgrade-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    copy_dir(&fx, &work);

    // The arbiter: a rebuild from genesis over the same Agent and log.
    let scratch = work.join("rebuilt");
    copy_dir(&fx.join("log"), &work.join("log-rebuilt"));
    let rebuilt = AgentCore::open_in(
        agent_of(&agent),
        founding(),
        FileLog::open(work.join("log-rebuilt")).unwrap(),
        disk(&scratch),
    )
    .unwrap();
    assert_eq!(rebuilt.stats().replica_reused.load(Ordering::Relaxed), 0);
    let want = rebuilt.replica_digest().expect("a replica");
    drop(rebuilt);

    // The upgrade: the first start of this build over the release's replica.
    let core = AgentCore::open_in(
        agent_of(&agent),
        founding(),
        FileLog::open(work.join("log")).unwrap(),
        disk(&work.join("replica")),
    )
    .unwrap();
    let stats = core.stats();
    assert_eq!(
        stats.replica_reused.load(Ordering::Relaxed),
        1,
        "reopened, not rebuilt"
    );
    assert_eq!(stats.replica_rolled.load(Ordering::Relaxed), 0);
    assert_eq!(core.replica_digest().as_ref(), Some(&want));
    assert_eq!(layout(&work.join("replica")), 2, "a manifest at once");
    let at = core.manifest().expect("a manifest").seq;

    // It commits on.
    let d = deploy(&core).await;
    let (s, v) = d
        .call(
            "POST",
            "/v1/identities",
            Some(&d.operator.token),
            &[("client-key", "dave")],
            Some(json!({ "kind": "person", "verifier": "0b" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    drop(d);
    assert_eq!(core.stopped(), None);

    // Killed: the directory as the process holds it, and the Agent as it stands.
    let killed = work.join("killed");
    copy_dir(&work.join("replica"), &killed);
    let agent = core.with_port(|p| agent_json(p)).expect("the Agent's rows");
    let want = core.replica_digest().expect("a replica");
    drop(core);
    let again = AgentCore::open_in(
        agent_of(&agent),
        founding(),
        FileLog::open(work.join("log")).unwrap(),
        disk(&killed),
    )
    .unwrap();
    let stats = again.stats();
    assert_eq!(stats.replica_reused.load(Ordering::Relaxed), 1);
    assert!(stats.replica_rolled.load(Ordering::Relaxed) >= 1);
    assert_eq!(again.manifest().map(|k| k.seq > at), Some(true));
    assert_eq!(again.replica_digest().as_ref(), Some(&want));
    drop(again);
    let _ = std::fs::remove_dir_all(&work);
}
