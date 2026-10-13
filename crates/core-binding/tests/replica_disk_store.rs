// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `T-036-17`, step 1: the replica's service store on disk (`F-036`, ruling `a0022-q5`).
//!
//! The replica's service tree, under [`ReplicaStores::Disk`], already holds every leaf of the
//! service store; step 1 makes it the store the replica reads back from
//! ([`ServiceStore::leaf`], [`ServiceStore::partial_state`]). These tests pin what the later
//! steps rely on, on the same chainspec:
//!
//! - the in-memory and the disk stores build the same genesis, roots and hot state;
//! - the disk tree holds exactly the in-memory service store's leaves, and a store rebuilt from
//!   it alone, as the Agent rebuilds one from a witness, holds them again;
//! - commands decided over either store reach the same roots, input by input.

mod common;

use std::sync::Arc;

use core_binding::core::{AgentCore, Founding, MemLog};
use core_binding::host::{AgentPort as _, HostKeys, NativePort, ReplicaStores, build_genesis};
use core_binding::hot::{Limits, MAX_BUDGET};
use ingress::boundary::{CallerBody, Core, CoreInput, InputBody, InputKey, Outcome};
use ingress::testkit::{T0, genesis};
use ledger_core::chainspec::{ChainSpec, ChunkLimits};
use ledger_core::export::AuditHead;
use ledger_core::state::Touched;

fn spec() -> ChainSpec {
    let g = genesis(T0);
    common::spec_of(
        g.op,
        g.at,
        g.genesis.assets.clone(),
        120,
        ChunkLimits {
            max_records: 40,
            ..ChunkLimits::DEFAULT
        },
    )
}

fn founding(s: &ChainSpec) -> Founding {
    let mut g = genesis(T0);
    g.genesis.chainspec = Some(s.commitment());
    g.genesis.spec = Some(Arc::new(s.clone()));
    Founding {
        genesis: g.genesis,
        op: g.op,
        at: g.at,
        keys: HostKeys::local(g.root, None),
        limits: Limits {
            budget: MAX_BUDGET,
            ..Limits::DEFAULT
        },
        genesis_roots: None,
    }
}

fn disk(dir: &tempfile::TempDir) -> ReplicaStores {
    ReplicaStores::Disk {
        dir: dir.path().join("replica"),
        cache_bytes: 1 << 20,
    }
}

#[test]
fn t_036_17_the_disk_store_builds_the_in_memory_genesis_leaf_for_leaf() {
    let s = spec();
    let f = founding(&s);
    let dir = tempfile::tempdir().unwrap();
    let build = |stores: &ReplicaStores| {
        build_genesis(stores, &f.keys, f.genesis.clone(), f.op, f.at)
            .unwrap_or_else(|e| panic!("the genesis builds: {e:?}"))
    };
    let mem = build(&ReplicaStores::Memory);
    let on_disk = build(&disk(&dir));
    assert_eq!(mem.roots, on_disk.roots, "the same roots");
    assert_eq!(mem.state, on_disk.state, "the same hot state");

    // The disk tree holds exactly the in-memory service store's leaves, kind by kind.
    let whole = mem.replica.state();
    let mut leaves = Vec::new();
    whole.for_each_leaf(&mut |kind, key, value| leaves.push((kind, key, value)));
    assert!(leaves.len() > 400, "a real book: {} leaves", leaves.len());
    for (kind, key, value) in &leaves {
        assert_eq!(
            on_disk.service.leaf(*kind, key).unwrap().as_ref(),
            Some(value),
            "kind {kind}"
        );
    }
    let kinds = whole.leaf_kinds();
    for kind in &kinds {
        let n = leaves.iter().filter(|(k, _, _)| k == kind).count();
        assert_eq!(on_disk.service.count(*kind).unwrap(), n, "kind {kind}");
    }

    // Rebuilt from the disk tree alone, as the Agent rebuilds its store from a witness: every kind
    // given whole, the store holds the same leaves and misses nothing.
    let audit = whole.audit();
    let head = AuditHead {
        emitted: audit.emitted(),
        head: audit.head_hash(),
        shipped: audit.shipped(),
    };
    let all: Vec<Touched> = kinds
        .iter()
        .map(|k| Touched {
            kind: *k,
            keys: Vec::new(),
            whole: true,
        })
        .collect();
    let rebuilt = on_disk.service.partial_state(&head, &all).unwrap();
    let mut back: Vec<_> = rebuilt
        .witnessed_leaves()
        .into_iter()
        .filter_map(|(k, key, v)| v.map(|v| (k, key, v)))
        .collect();
    back.sort();
    let mut want = leaves.clone();
    want.sort();
    assert_eq!(back, want);
    assert!(!rebuilt.missed());

    // One key: the store holds that leaf and nothing else.
    let (kind, key, _) = leaves[leaves.len() / 2].clone();
    let one = on_disk
        .service
        .partial_state(
            &head,
            &[Touched {
                kind,
                keys: vec![key.clone()],
                whole: false,
            }],
        )
        .unwrap();
    assert_eq!(one.witnessed_leaves().len(), 1);
    assert!(!one.missed());
}

fn key(n: u8, i: u32) -> InputKey {
    let mut k = [n; 16];
    k[12..].copy_from_slice(&i.to_be_bytes());
    InputKey(k)
}

fn open_identity(founded: &ledger_core::ledger::Founded, n: u8) -> CoreInput {
    CoreInput {
        key: key(0xC7, u32::from(n)),
        body: InputBody::Command {
            caller: CallerBody {
                principal: founded.operator.to_did(),
                credential: founded.operator_credential.to_did(),
                on_behalf_of: None,
            },
            operation: ledger_core::id::OperationId::from_ref(
                ledger_kernel::posting::OperationRef([n; 16]),
            )
            .to_did(),
            received_at: T0,
            now: T0,
            command: Box::new(
                serde_json::from_value(serde_json::json!({
                    "command": "open_identity",
                    "kind": "person",
                    "verifier": "0101"
                }))
                .unwrap(),
            ),
        },
    }
}

fn roots_of(core: &AgentCore<NativePort>) -> core_binding::envelope::Roots {
    core.with_port(|p| p.hot())
        .and_then(Result::ok)
        .flatten()
        .unwrap()
        .roots()
}

#[tokio::test(flavor = "multi_thread")]
async fn t_036_17_commands_reach_the_same_roots_over_the_memory_and_disk_stores() {
    let s = spec();
    let dir = tempfile::tempdir().unwrap();
    let mem = AgentCore::open_in(
        NativePort::new(),
        founding(&s),
        MemLog::default(),
        ReplicaStores::Memory,
    )
    .unwrap_or_else(|e| panic!("in memory: {e}"));
    let on_disk = AgentCore::open_in(
        NativePort::new(),
        founding(&s),
        MemLog::default(),
        disk(&dir),
    )
    .unwrap_or_else(|e| panic!("on disk: {e}"));
    assert_eq!(roots_of(&mem), roots_of(&on_disk), "the genesis");
    let founded = mem.founded();
    for n in 1..=6u8 {
        let input = open_identity(&founded, n);
        for core in [&mem, &on_disk] {
            match core.apply(&input).await.unwrap().outcome {
                Outcome::Decided { .. } => {}
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(roots_of(&mem), roots_of(&on_disk), "after input {n}");
    }
}
