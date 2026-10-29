// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The chainspec genesis through the core Agent (`F-036`, owner ruling `genesis-chainspec`,
//! `REQ-RC-6`, `REQ-RT-3`, `INV-18`).
//!
//! - **The chainspec book**: the host builds the genesis state natively and founds the Agent on its
//!   roots in one invocation (`StateGenesis`); the core refuses a genesis whose roots are not the
//!   chainspec's (the manifest's, or an Agent founded on others); a restore rebuilds the same state.
//! - **The chunk-by-chunk path** (`verify-genesis`): a book founded on the commitment alone takes the
//!   chunks as inputs, refuses everything else until the seal, and ends at the same roots.

mod common;

use core_binding::core::{AgentCore, Founding, HostLog as _, MemLog, roll_forward_agent};
use core_binding::host::{AgentPort as _, NativePort, ReplicaStores};
use core_binding::hot::{Limits, MAX_BUDGET};
use ingress::boundary::{CallerBody, Core, CoreInput, InputBody, InputKey, Outcome};
use ingress::testkit::{T0, genesis};
use ledger_core::chainspec::{ChainSpec, ChunkLimits};

fn seed_for(g: &ingress::inprocess::CoreGenesis, n: u64) -> ChainSpec {
    common::spec_of(
        g.op,
        g.at,
        g.genesis.assets.clone(),
        n,
        ChunkLimits {
            max_records: 40,
            ..ChunkLimits::DEFAULT
        },
    )
}

fn founding(s: &ChainSpec) -> Founding {
    founding_with(s, false)
}

/// The founding of a chainspec book: with the chainspec itself (`built`), its genesis state is
/// built natively; without it, the book is founded unsealed, for the chunks.
fn founding_with(s: &ChainSpec, built: bool) -> Founding {
    let mut g = genesis(T0);
    g.genesis.chainspec = Some(s.commitment());
    g.genesis.spec = built.then(|| std::sync::Arc::new(s.clone()));
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

fn key(n: u8, i: u32) -> InputKey {
    let mut k = [n; 16];
    k[12..].copy_from_slice(&i.to_be_bytes());
    InputKey(k)
}

fn chunk_input(s: &ChainSpec, i: u32) -> CoreInput {
    let bytes = s.chunk_bytes();
    let paths = s.paths();
    CoreInput {
        key: key(0x5E, i),
        body: InputBody::SpecChunk {
            chunk: hex::encode(&bytes[i as usize]),
            path: paths[i as usize].iter().map(hex::encode).collect(),
        },
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn req_rc_6_the_agent_applies_the_seed_and_a_restore_replays_it() {
    let s = seed_for(&genesis(T0), 90);
    assert!(s.chunk_count() > 4);
    let log = MemLog::default();
    let core = AgentCore::open(NativePort::new(), founding(&s), log.clone())
        .unwrap_or_else(|e| panic!("the Agent core opens: {e}"));
    let founded = core.founded();

    // Before the seal, a command is refused with the named condition, and a tick moves nothing.
    let command = CoreInput {
        key: key(0xC0, 0),
        body: InputBody::Command {
            caller: CallerBody {
                principal: founded.operator.to_did(),
                credential: founded.operator_credential.to_did(),
                on_behalf_of: None,
            },
            operation: ledger_core::id::OperationId::from_ref(
                ledger_kernel::posting::OperationRef([0xC1; 16]),
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
    };
    match core.apply(&command).await.unwrap().outcome {
        Outcome::Refused { error, .. } => {
            assert_eq!(error.code(), "ERR-InitialStateNotSealed")
        }
        other => panic!("{other:?}"),
    }

    // The chunks, in batches the forwarder would make, and one again: answered, not re-applied.
    let n = s.chunk_count();
    let inputs: Vec<CoreInput> = (0..n).map(|i| chunk_input(&s, i)).collect();
    for batch in inputs.chunks(3) {
        for r in core.apply_batch(batch).await {
            match r.unwrap().outcome {
                Outcome::SpecApplied { .. } => {}
                other => panic!("{other:?}"),
            }
        }
    }
    // Under its own key, the core answers from its replies (`INV-18`); under another key, the
    // book answers that the chunk was applied, and applies nothing.
    match core.apply(&chunk_input(&s, 1)).await.unwrap().outcome {
        Outcome::SpecApplied {
            index: 1,
            applied: 2,
            sealed: false,
            ..
        } => {}
        other => panic!("{other:?}"),
    }
    let mut again = chunk_input(&s, 1);
    again.key = key(0x5F, 1);
    match core.apply(&again).await.unwrap().outcome {
        Outcome::SpecApplied {
            index: 1,
            applied,
            sealed: true,
            ..
        } => assert_eq!(applied, n),
        other => panic!("{other:?}"),
    }

    // After the seal, the same command is decided.
    let mut after = command.clone();
    after.key = key(0xC0, 1);
    match core.apply(&after).await.unwrap().outcome {
        Outcome::Decided { .. } => {}
        other => panic!("{other:?}"),
    }

    // A fresh Agent, re-sent the archive (the seed's chunks among its entries), stands where
    // this one does.
    let archive = log.entries().unwrap();
    let last = archive.last().unwrap().clone();
    let hot = core
        .with_port(|p| p.hot())
        .and_then(Result::ok)
        .flatten()
        .unwrap();
    assert_eq!(hot.roots(), last.roots);
    let (_, rolled) = roll_forward_agent(
        NativePort::new(),
        &founding(&s),
        &archive,
        &ReplicaStores::Memory,
    )
    .unwrap_or_else(|e| panic!("roll forward: {e}"));
    assert_eq!(
        (rolled.to, rolled.roots, rolled.log_head),
        (last.seq, last.roots, last.log_hash())
    );
}

/// `F-036` T-036-10: the host's side of a seeding, measured: the seed through the core with the
/// Agent run natively (no VOS), `ALEDGENT_SEED_RATE_N` identities (default 2 000) in chunks of
/// `ALEDGENT_SEED_RATE_RECORDS` (default 30). What it prints is the host's work per record (the
/// native run, the witnesses, the replica's service tree) plus the Agent's transition run natively.
#[tokio::test(flavor = "multi_thread")]
async fn f_036_the_hosts_side_of_a_seeding() {
    let n: u64 = std::env::var("ALEDGENT_SEED_RATE_N")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000);
    let records: u32 = std::env::var("ALEDGENT_SEED_RATE_RECORDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let g = genesis(T0);
    let s = common::spec_of(
        g.op,
        g.at,
        g.genesis.assets.clone(),
        n,
        ChunkLimits {
            max_records: records,
            ..ChunkLimits::DEFAULT
        },
    );
    let core = AgentCore::open(NativePort::new(), founding(&s), MemLog::default())
        .unwrap_or_else(|e| panic!("the Agent core opens: {e}"));
    let inputs: Vec<CoreInput> = (0..s.chunk_count()).map(|i| chunk_input(&s, i)).collect();
    let t = std::time::Instant::now();
    for batch in inputs.chunks(24) {
        for r in core.apply_batch(batch).await {
            assert!(matches!(r.unwrap().outcome, Outcome::SpecApplied { .. }));
        }
    }
    let us = t.elapsed().as_micros().max(1);
    println!(
        "F-036 host side: {} records in {} chunks of {records}: {} ms, {} records/s; {}",
        s.records().len(),
        s.chunk_count(),
        us / 1000,
        s.records().len() as u128 * 1_000_000 / us,
        core.stats().report()
    );
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

fn hot_of(core: &AgentCore<NativePort>) -> core_binding::hot::Hot {
    core.with_port(|p| p.hot())
        .and_then(Result::ok)
        .flatten()
        .unwrap()
}

/// Owner ruling `genesis-chainspec`: the Agent is founded on the genesis state in one invocation,
/// born sealed; its roots are those the chunk-by-chunk path reaches inside the Agent
/// (`verify-genesis`'s check); and a fresh Agent restored from the archive stands where it does.
#[tokio::test(flavor = "multi_thread")]
async fn genesis_chainspec_the_agent_starts_from_the_built_state() {
    let s = seed_for(&genesis(T0), 90);
    let log = MemLog::default();
    let core = AgentCore::open(NativePort::new(), founding_with(&s, true), log.clone())
        .unwrap_or_else(|e| panic!("the Agent core opens: {e}"));
    let founded = core.founded();
    let hot = hot_of(&core);
    assert_eq!(hot.inputs, 0, "one invocation, no input");
    let built = hot.roots();

    // The chunk-by-chunk path, inside the Agent, reaches the same roots.
    let slow = AgentCore::open(NativePort::new(), founding(&s), MemLog::default()).unwrap();
    let inputs: Vec<CoreInput> = (0..s.chunk_count()).map(|i| chunk_input(&s, i)).collect();
    for batch in inputs.chunks(8) {
        for r in slow.apply_batch(batch).await {
            assert!(matches!(r.unwrap().outcome, Outcome::SpecApplied { .. }));
        }
    }
    assert_eq!(hot_of(&slow).roots(), built, "verify-genesis's check");

    // Born sealed: a command is decided at once, and the archive holds it.
    match core
        .apply(&open_identity(&founded, 1))
        .await
        .unwrap()
        .outcome
    {
        Outcome::Decided { .. } => {}
        other => panic!("{other:?}"),
    }
    let archive = log.entries().unwrap();
    assert_eq!(archive.len(), 1);

    // A restore: a fresh Agent, founded from the chainspec again, rolls forward to the same place.
    let (_, rolled) = roll_forward_agent(
        NativePort::new(),
        &founding_with(&s, true),
        &archive,
        &ReplicaStores::Memory,
    )
    .unwrap_or_else(|e| panic!("roll forward: {e}"));
    let last = archive.last().unwrap();
    assert_eq!((rolled.to, rolled.roots), (last.seq, last.roots));
    assert!(rolled.founded);

    // The same core restarted over its Agent and log: the rebuilt genesis and the replay meet.
    let port = core.with_port(std::mem::take).unwrap();
    drop(core);
    let again = AgentCore::open(port, founding_with(&s, true), log.clone())
        .unwrap_or_else(|e| panic!("reopened: {e}"));
    assert_eq!(hot_of(&again).roots(), last.roots);
}

/// Owner ruling `genesis-chainspec`: the core refuses a genesis whose roots are not the
/// chainspec's: a manifest that states other roots, or an Agent founded on other roots.
#[tokio::test(flavor = "multi_thread")]
async fn genesis_chainspec_a_genesis_whose_roots_are_not_the_chainspecs_is_refused() {
    let s = seed_for(&genesis(T0), 30);
    // The manifest's roots, right and wrong.
    let right = {
        let core = AgentCore::open(
            NativePort::new(),
            founding_with(&s, true),
            MemLog::default(),
        )
        .unwrap();
        hot_of(&core).roots()
    };
    let mut f = founding_with(&s, true);
    f.genesis_roots = Some(right);
    assert!(AgentCore::open(NativePort::new(), f, MemLog::default()).is_ok());
    let mut f = founding_with(&s, true);
    let mut wrong = right;
    wrong.service[0] ^= 1;
    f.genesis_roots = Some(wrong);
    let e = AgentCore::open(NativePort::new(), f, MemLog::default())
        .err()
        .expect("refused");
    assert!(e.contains("manifest"), "{e}");

    // An Agent founded on another state: the host's rebuild does not meet it.
    let core = AgentCore::open(
        NativePort::new(),
        founding_with(&s, true),
        MemLog::default(),
    )
    .unwrap();
    let mut port = core.with_port(std::mem::take).unwrap();
    drop(core);
    let mut hot = port.hot().unwrap().unwrap();
    hot.service_root[0] ^= 1;
    port.rows.0.insert(
        core_binding::hot::HOT.to_vec(),
        core_binding::envelope::encode(&hot),
    );
    assert!(
        AgentCore::open(port, founding_with(&s, true), MemLog::default()).is_err(),
        "an Agent that does not stand at the chainspec's genesis"
    );
    // And a chainspec that is not the one genesis commits founds nothing.
    let other = seed_for(&genesis(T0), 31);
    let mut f = founding_with(&s, true);
    f.genesis.spec = Some(std::sync::Arc::new(other));
    assert!(AgentCore::open(NativePort::new(), f, MemLog::default()).is_err());
}

async fn apply_chunks(core: &AgentCore<NativePort>, s: &ChainSpec, range: std::ops::Range<u32>) {
    let inputs: Vec<CoreInput> = range.map(|i| chunk_input(s, i)).collect();
    for batch in inputs.chunks(8) {
        for r in core.apply_batch(batch).await {
            assert!(matches!(r.unwrap().outcome, Outcome::SpecApplied { .. }));
        }
    }
}

/// `verify-genesis --segments` (`F-036`, `REQ-RC-6`): scratch Agents founded at chunk boundaries
/// on the natively applied prefix (`AgentCore::open_segment`) chain to the single run. Each
/// segment's founding state is the state the segment before it reached inside the Agent, and the
/// last one ends where the single run does, sealed; a segment founded one chunk off does not chain.
#[tokio::test(flavor = "multi_thread")]
async fn req_rc_6_segments_chain_to_the_single_run() {
    let g = genesis(T0);
    let s = common::spec_of(
        g.op,
        g.at,
        g.genesis.assets.clone(),
        40,
        ChunkLimits {
            max_records: 6,
            ..ChunkLimits::DEFAULT
        },
    );
    let n = s.chunk_count();
    assert!(n > 12, "enough chunks to cut: {n}");
    let shared = std::sync::Arc::new(s.clone());

    let single = AgentCore::open(NativePort::new(), founding(&s), MemLog::default()).unwrap();
    apply_chunks(&single, &s, 0..n).await;
    let whole = hot_of(&single).genesis_state();

    let cuts = [0, n / 4, n / 2, n - 1, n];
    let mut reached = None;
    for w in cuts.windows(2) {
        let core = AgentCore::open_segment(
            NativePort::new(),
            founding(&s),
            MemLog::default(),
            ReplicaStores::Memory,
            &shared,
            w[0],
        )
        .unwrap_or_else(|e| panic!("segment at {}: {e}", w[0]));
        let start = hot_of(&core).genesis_state();
        if let Some(prev) = reached {
            assert_eq!(
                prev, start,
                "segment at {} starts where the one before ended",
                w[0]
            );
        }
        apply_chunks(&core, &s, w[0]..w[1]).await;
        reached = Some(hot_of(&core).genesis_state());
    }
    assert_eq!(
        reached,
        Some(whole),
        "the chain ends at the single run's state"
    );

    // One chunk off: the founding state is not the state the previous segment reached.
    let off = AgentCore::open_segment(
        NativePort::new(),
        founding(&s),
        MemLog::default(),
        ReplicaStores::Memory,
        &shared,
        n / 4 + 1,
    )
    .unwrap();
    let first = AgentCore::open(NativePort::new(), founding(&s), MemLog::default()).unwrap();
    apply_chunks(&first, &s, 0..n / 4).await;
    assert_ne!(hot_of(&first).genesis_state(), hot_of(&off).genesis_state());
    // A segment is founded on the commitment alone, and never past the last chunk.
    assert!(
        AgentCore::open_segment(
            NativePort::new(),
            founding_with(&s, true),
            MemLog::default(),
            ReplicaStores::Memory,
            &shared,
            1,
        )
        .is_err()
    );
    assert!(
        AgentCore::open_segment(
            NativePort::new(),
            founding(&s),
            MemLog::default(),
            ReplicaStores::Memory,
            &shared,
            n,
        )
        .is_err()
    );
}

/// `verify-genesis --segments` (`F-036`, `REQ-RC-6`): a segment's core takes back an invocation the
/// Agent did not commit, as a core founded at chunk 0 does: it rebuilds its replica as the segment
/// was founded (chunks `0..start` natively, then the segment's archive) and runs the inputs again,
/// one at a time, and the segment still ends at the single run's state. Before, a segment's core
/// rebuilt from genesis alone, could not reach the Agent's roots and stopped: every later chunk
/// was unavailable, and `verify-genesis` waited on it for good (the full-size run of
/// 2026-10-11, after the guest's heap aborted one batch).
#[tokio::test(flavor = "multi_thread")]
async fn req_rc_6_a_segment_recovers_from_an_invocation_the_agent_did_not_commit() {
    let g = genesis(T0);
    let s = common::spec_of(
        g.op,
        g.at,
        g.genesis.assets.clone(),
        40,
        ChunkLimits {
            max_records: 6,
            ..ChunkLimits::DEFAULT
        },
    );
    let n = s.chunk_count();
    let single = AgentCore::open(NativePort::new(), founding(&s), MemLog::default()).unwrap();
    apply_chunks(&single, &s, 0..n).await;
    let whole = hot_of(&single).genesis_state();

    let start = n / 2;
    let core = AgentCore::open_segment(
        NativePort::new(),
        founding(&s),
        MemLog::default(),
        ReplicaStores::Memory,
        &std::sync::Arc::new(s.clone()),
        start,
    )
    .unwrap();
    apply_chunks(&core, &s, start..start + 4).await;
    // The next invocation fails before the Agent runs it: nothing of it commits.
    core.with_port(|p| p.fail_next = Some(false)).unwrap();
    apply_chunks(&core, &s, start + 4..n).await;
    assert_eq!(core.stopped(), None);
    assert_eq!(
        hot_of(&core).genesis_state(),
        whole,
        "the segment ends at the single run's state"
    );
}

/// `T-037-09`: a book whose chainspec names its genesis code *H₀*, as test's and prod's do, restores
/// under its code map: the map the entries claim starts at *H₀*, generation 0, and the roll
/// forward, which holds every entry to the map it derives from the founding's code, reaches the
/// same map and the archive's end.
#[test]
fn t037_09_a_book_on_a_named_genesis_code_restores_under_its_code_map() {
    use core_binding::code_map::{CodeMap, CodeRange};
    use core_binding::core::claimed_code_map;
    use ledger_core::chainspec::GenesisCode;
    use ledger_core::code::{
        Artefact, BuildProfile, CodeManifest, KernelIdentity, Platform, Role, WriterKind,
    };
    let manifest = CodeManifest {
        writer: WriterKind::Core,
        platform: Platform {
            name: "vos-saga".into(),
            revision: "bd8d1bb7".into(),
            identities: Vec::new(),
        },
        generation: 0,
        artefacts: vec![Artefact {
            role: Role::Package,
            digest: [0x11; 32],
            len: 1,
            identities: Vec::new(),
            profile: BuildProfile {
                target: "riscv64em-vos".into(),
                toolchain: "nightly".into(),
                opt_level: "s".into(),
                features: Vec::new(),
                source_revision: "0123abcd".into(),
                cargo_lock: [0; 32],
            },
        }],
        kernel: KernelIdentity {
            revision: "e9a7b08".into(),
            checksum: None,
        },
        constructions: vec![ledger_core::anchor::construction()],
        messages: Vec::new(),
    };
    let h0 = manifest.version();
    assert_ne!(h0, [0; 32]);
    let spec = common::spec_of(
        common::op(0),
        1,
        common::genesis().assets,
        4,
        ChunkLimits::DEFAULT,
    )
    .with_code(GenesisCode {
        manifest,
        construction: ledger_core::anchor::construction(),
    })
    .unwrap();
    let mut g = common::genesis();
    g.chainspec = Some(spec.commitment());
    g.spec = Some(std::sync::Arc::new(spec));
    let limits = Limits {
        budget: MAX_BUDGET,
        ..Limits::DEFAULT
    };
    let mut fx = common::Fx::with_genesis(NativePort::new(), limits, g.clone());
    let (p, _) = fx.funded(1_000);
    let _ = fx.account(&p);
    let last = fx.archive.last().unwrap().clone();
    let founding = Founding {
        genesis: g,
        op: common::op(0),
        at: 1,
        keys: common::keys(),
        limits,
        genesis_roots: None,
    };
    // The port tracks the code it runs, as the core host's does for a named H₀ (`T-037-14`): it
    // holds H₀ and nothing else, so the roll forward's check that the port holds every version is
    // a real one (a port with no map holds every version).
    let mut port = NativePort::new();
    port.code_map = [(h0, 0)].into_iter().collect();
    assert!(!port.holds_code(&[0xEE; 32]));
    let claimed = claimed_code_map(&mut port, &founding, &fx.archive).unwrap();
    let want = CodeMap {
        ranges: vec![CodeRange {
            from: 1,
            to: last.seq,
            code: h0,
            generation: 0,
        }],
    };
    assert_eq!(claimed, want);
    let (_, rolled) =
        roll_forward_agent(port, &founding, &fx.archive, &ReplicaStores::Memory).unwrap();
    assert_eq!(
        (rolled.to, rolled.log_head, rolled.code_map),
        (last.seq, last.log_hash(), want)
    );
}
