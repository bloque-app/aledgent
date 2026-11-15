// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A replica kept across a clean stop (`T-031-25`, aledgent#129; `REQ-RT-3`): the core host that
//! is told to stop makes its replica's stores durable and records where they stand, and the next
//! start reopens them, checked, instead of replaying the archive from genesis. A record that does
//! not match the Agent, or none, rebuilds the replica as before.

use std::sync::atomic::Ordering;
use std::time::Duration;

use core_binding::core::{AgentCore, Founding, MemLog};
use core_binding::host::{KEPT_FILE, NativePort, ReplicaStores};
use core_binding::hot::{Limits, MAX_BUDGET};
use ingress::boundary::Core as _;
use ingress::inprocess::MemJournal;
use ingress::store::mem::MemStore;
use ingress::testkit::{Deployment, Party, T0, USD, fast_config, genesis};
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

type Dep = Deployment<MemStore, AgentCore<NativePort>>;

async fn deploy(core: &AgentCore<NativePort>, parties: &[&Party]) -> Dep {
    let mut config = fast_config("ingress-0");
    // No tick: a ticker left running would apply to a core whose port was taken, over the same
    // log and stores.
    config.tick_interval = Duration::from_secs(365 * 86_400);
    let d = Deployment::assemble(
        MemStore::new(Duration::from_millis(50)),
        config,
        core.clone(),
        core.founded(),
        MemJournal::new(),
    )
    .await;
    for p in parties {
        d.adopt(p).await;
    }
    d
}

/// A party's balances: the reads every restart must answer alike.
async fn account(d: &Dep, p: &Party) -> Value {
    let (s, v) = d
        .call(
            "GET",
            &format!(
                "/v1/balances?account={}",
                p.account.as_deref().expect("a funded party has an account")
            ),
            Some(&p.token),
            &[],
            None,
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    v
}

fn take_port(core: AgentCore<NativePort>) -> NativePort {
    core.with_port(|p| std::mem::replace(p, NativePort::new()))
        .expect("a port")
}

fn reused(core: &AgentCore<NativePort>) -> u64 {
    core.stats().replica_reused.load(Ordering::Relaxed)
}

/// `T-031-25`: a clean stop keeps the replica, the next start reopens it without a rebuild, and
/// the reopened core answers and commits as the one that stopped; a record left stale by inputs
/// committed after it, or a start with no record, rebuilds the replica, and answers alike.
#[tokio::test(flavor = "multi_thread")]
async fn t031_25_a_replica_kept_at_a_clean_stop_is_reopened_not_rebuilt() {
    let dir = std::env::temp_dir().join(format!("aledgent-kept-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let stores = ReplicaStores::Disk {
        dir: dir.join("replica"),
        cache_bytes: 1024 * 1024,
    };
    let kept_file = dir.join("replica").join(KEPT_FILE);
    let log = MemLog::default();

    // A book with identities, credentials, accounts, an issuance and a reservation in it.
    let core =
        AgentCore::open_in(NativePort::new(), founding(), log.clone(), stores.clone()).unwrap();
    assert_eq!(reused(&core), 0, "a founding is no reuse");
    let d = deploy(&core, &[]).await;
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 0).await;
    let (s, r) = d
        .call_decided(
            "POST",
            "/v1/reservations",
            Some(&alice.token),
            &[("client-key", "r1")],
            Some(json!({ "payer": alice.account, "asset": USD, "amount": "7" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{r}");
    let before = (account(&d, &alice).await, account(&d, &bob).await);
    drop(d);

    // The clean stop: nothing is answered after it, and the record is written.
    let replica = core.replica_digest().expect("a replica");
    assert!(
        replica.2 > 0,
        "audit records wait unshipped: no audit store here"
    );
    let at = core.keep_replica(Duration::from_secs(2)).expect("kept");
    assert!(kept_file.exists());
    assert!(
        core.last_tick().await.is_err(),
        "a kept core answers nothing"
    );
    let port = take_port(core);

    // Reopened, it is the replica that stopped, leaf for leaf, with the same audit chain and the
    // same records unshipped.
    let reopened = AgentCore::open_in(port, founding(), log.clone(), stores.clone()).unwrap();
    assert_eq!(reused(&reopened), 1);
    assert_eq!(reopened.replica_digest().as_ref(), Some(&replica));
    // And a replay from genesis is that replica too.
    let port = take_port(reopened);
    std::fs::remove_file(&kept_file).unwrap();
    let replayed = AgentCore::open_in(port, founding(), log.clone(), stores.clone()).unwrap();
    assert_eq!(reused(&replayed), 0);
    assert_eq!(replayed.replica_digest().as_ref(), Some(&replica));
    replayed.keep_replica(Duration::from_secs(2)).expect("kept");
    let port = take_port(replayed);

    // The next start reopens it, and removes the record.
    let again = AgentCore::open_in(port, founding(), log.clone(), stores.clone()).unwrap();
    assert_eq!(reused(&again), 1, "reopened, not rebuilt");
    assert!(
        kept_file.exists(),
        "the record stays until the first decision"
    );
    let d = deploy(&again, &[&alice, &bob]).await;
    assert_eq!((account(&d, &alice).await, account(&d, &bob).await), before);
    // It commits on: a capture of the reservation, checked input by input against the Agent.
    let op = r["operation_reference"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let (s, c) = d
        .call(
            "POST",
            &format!("/v1/reservations/{op}/capture"),
            Some(&alice.token),
            &[],
            Some(json!({ "destination": bob.account, "cleared": "7" })),
        )
        .await;
    assert!(s.as_u16() == 200 && c["state"] == "Settled", "{s} {c}");
    assert!(!kept_file.exists(), "a decision removes the record");
    assert_eq!(again.stopped(), None);
    let after = (account(&d, &alice).await, account(&d, &bob).await);
    assert_ne!(after, before);
    drop(d);

    // A record left stale: kept, copied, then more inputs committed past it.
    again.keep_replica(Duration::from_secs(2)).expect("kept");
    let stale = std::fs::read(&kept_file).unwrap();
    let port = take_port(again);
    let third = AgentCore::open_in(port, founding(), log.clone(), stores.clone()).unwrap();
    assert_eq!(reused(&third), 1);
    let d = deploy(&third, &[&alice, &bob]).await;
    let (s, v) = d
        .call(
            "POST",
            "/v1/identities",
            Some(&d.operator.token),
            &[("client-key", "late")],
            Some(json!({ "kind": "person", "verifier": "0a" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    drop(d);
    let port = take_port(third);
    std::fs::write(&kept_file, stale).unwrap();
    let fourth = AgentCore::open_in(port, founding(), log.clone(), stores.clone()).unwrap();
    assert_eq!(reused(&fourth), 0, "a stale record rebuilds");
    assert!(!kept_file.exists());
    let d = deploy(&fourth, &[&alice, &bob]).await;
    assert_eq!((account(&d, &alice).await, account(&d, &bob).await), after);
    drop(d);

    // No record (the host was killed): rebuilt, alike.
    let port = take_port(fourth);
    let fifth = AgentCore::open_in(port, founding(), log, stores).unwrap();
    assert_eq!(reused(&fifth), 0);
    let d = deploy(&fifth, &[&alice, &bob]).await;
    assert_eq!((account(&d, &alice).await, account(&d, &bob).await), after);
    assert!(at > 0);
    drop(d);
    drop(fifth);
    let _ = std::fs::remove_dir_all(&dir);
}
