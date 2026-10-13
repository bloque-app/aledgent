// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A restore of the core Agent from the host's archive (`T-005-16`, `NFR-4`, `NFR-10`, `REQ-RT-3`):
//! a fresh Agent, founded afresh or from a checkpoint of its runtime state, is brought forward
//! through the archived inputs it does not hold, each re-sent exactly as it was sent live and
//! checked against its archived entry, and ends where the lost Agent stood.

use std::time::Duration;

use core_binding::core::{AgentCore, Founding, HostLog as _, MemLog, roll_forward_agent};
use core_binding::host::{AgentPort as _, NativePort, ReplicaStores};
use core_binding::hot::{Hot, Limits, MAX_BUDGET};
use ingress::inprocess::MemJournal;
use ingress::store::mem::MemStore;
use ingress::testkit::{Deployment, T0, USD, fast_config, genesis};
use serde_json::json;

type AgentDeployment = Deployment<MemStore, AgentCore<NativePort>>;

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

async fn deployment() -> (AgentDeployment, AgentCore<NativePort>, MemLog) {
    let log = MemLog::default();
    let core = AgentCore::open(NativePort::new(), founding(), log.clone())
        .unwrap_or_else(|e| panic!("the Agent core opens: {e}"));
    let d = Deployment::assemble(
        MemStore::new(Duration::from_millis(50)),
        fast_config("ingress-0"),
        core.clone(),
        core.founded(),
        MemJournal::new(),
    )
    .await;
    (d, core, log)
}

fn hot(core: &AgentCore<NativePort>) -> Hot {
    core.with_port(|p| p.hot())
        .and_then(Result::ok)
        .flatten()
        .unwrap_or_else(|| panic!("the Agent is founded"))
}

/// Reservations and transfers between two parties: every kind of input the core takes in a day.
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

/// `NFR-10`, `REQ-RT-3`: from genesis, a fresh Agent re-sent every archived input reaches the
/// lost Agent's input number, log head and roots; from a checkpoint taken half way, it re-sends
/// only the inputs after it, and ends at the same place.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_10_a_fresh_agent_rolls_forward_to_where_the_lost_one_stood() {
    let (d, core, log) = deployment().await;
    workload(&d, "a", 3).await;
    // A checkpoint: the Agent's runtime state as the archive keeps it. Ticks and deliveries keep
    // committing in the background, so where it stands is read from the checkpoint itself.
    let checkpoint = core.with_port(|p| p.rows.clone()).expect("the port");
    let mut half = NativePort::new();
    half.rows = checkpoint.clone();
    let half = half.hot().unwrap().expect("founded");
    workload(&d, "b", 3).await;
    // The archive at the failure: the lost Agent stood at its last entry.
    let archive = log.entries().unwrap();
    let last = archive.last().expect("inputs").clone();
    assert!(last.seq > half.inputs);

    // From genesis.
    let (_, rolled) = roll_forward_agent(
        NativePort::new(),
        &founding(),
        &archive,
        &ReplicaStores::Memory,
    )
    .unwrap_or_else(|e| panic!("roll forward from genesis: {e}"));
    assert!(rolled.founded);
    assert_eq!(
        (rolled.from, rolled.to, rolled.roots, rolled.log_head),
        (0, last.seq, last.roots, last.log_hash())
    );

    // From the checkpoint.
    let mut port = NativePort::new();
    port.rows = checkpoint;
    let (mut port, rolled) =
        roll_forward_agent(port, &founding(), &archive, &ReplicaStores::Memory)
            .unwrap_or_else(|e| panic!("roll forward from the checkpoint: {e}"));
    assert!(!rolled.founded);
    assert_eq!(
        (rolled.from, rolled.to, rolled.roots),
        (half.inputs, last.seq, last.roots)
    );
    // The restored Agent serves: a core opened on it and the archive answers the old replies.
    let restored_log = MemLog::default();
    for e in &archive {
        let mut l = restored_log.clone();
        core_binding::core::HostLog::push_entry(&mut l, e).unwrap();
    }
    let restored = AgentCore::open(std::mem::take(&mut port), founding(), restored_log.clone())
        .unwrap_or_else(|e| panic!("the restored Agent opens: {e}"));
    assert_eq!(hot(&restored).roots(), last.roots);
    drop(d);
}

/// Fail closed: a tampered archive, or a checkpoint that is not of this archive, restores
/// nothing.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_10_a_tampered_archive_or_a_foreign_checkpoint_is_refused() {
    let (d, core, log) = deployment().await;
    workload(&d, "a", 2).await;
    let archive = log.entries().unwrap();
    let n = archive.len();
    assert!(n > 3);

    // One entry's roots changed: the chain still links to it, but the Agent cannot reach them.
    let mut tampered = archive.clone();
    tampered[n - 2].roots.kernel[0] ^= 1;
    let e = roll_forward_agent(
        NativePort::new(),
        &founding(),
        &tampered,
        &ReplicaStores::Memory,
    )
    .err()
    .expect("refused");
    assert!(e.contains("does not reproduce"), "{e}");

    // One entry dropped: the chain breaks.
    let mut gapped = archive.clone();
    gapped.remove(n / 2);
    let e = roll_forward_agent(
        NativePort::new(),
        &founding(),
        &gapped,
        &ReplicaStores::Memory,
    )
    .err()
    .expect("refused");
    assert!(e.contains("does not continue"), "{e}");

    // A checkpoint ahead of the archive is not of it.
    let mut ahead = NativePort::new();
    ahead.rows = core.with_port(|p| p.rows.clone()).expect("the port");
    let e = roll_forward_agent(
        ahead,
        &founding(),
        &archive[..n - 1],
        &ReplicaStores::Memory,
    )
    .err()
    .expect("refused");
    assert!(e.contains("past the archive"), "{e}");

    // A checkpoint of another book (another deployment's Agent) is not of it either.
    let (d2, other, _) = deployment().await;
    workload(&d2, "z", 1).await;
    let mut foreign = NativePort::new();
    foreign.rows = other.with_port(|p| p.rows.clone()).expect("the port");
    let e = roll_forward_agent(foreign, &founding(), &archive, &ReplicaStores::Memory)
        .err()
        .expect("refused");
    assert!(
        e.contains("not of this archive") || e.contains("checkpoint"),
        "{e}"
    );
    drop(d);
}

/// `T-005-16`: an Agent that holds nothing over a log that holds committed inputs (its runtime
/// state lost, or a restore laid out and not finished) is never founded afresh: that would serve
/// an empty book beside the old replies.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_4_an_empty_agent_over_a_logged_archive_is_never_founded_afresh() {
    let (d, _core, log) = deployment().await;
    workload(&d, "a", 1).await;
    let e = AgentCore::open(NativePort::new(), founding(), log)
        .err()
        .expect("refused");
    assert!(e.contains("restore it from the archive"), "{e}");
}

/// `T-005-15`: a torn last record in either log file (the host died inside an append) is cut off
/// when the log opens, so the next append does not follow it and every reader sees what comes
/// after.
#[test]
fn t_005_15_a_torn_last_record_is_cut_off_when_the_log_opens() {
    use core_binding::core::{ARCHIVE_FILE, FileLog, REPLIES_FILE};
    let tmp = std::env::temp_dir().join(format!("torn-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let mut archive = Vec::new();
    archive.extend_from_slice(&3u32.to_be_bytes());
    archive.extend_from_slice(b"abc");
    let whole = archive.len();
    archive.extend_from_slice(&9u32.to_be_bytes());
    archive.extend_from_slice(b"defg");
    std::fs::write(tmp.join(ARCHIVE_FILE), &archive).unwrap();
    std::fs::write(tmp.join(REPLIES_FILE), b"{\"a\":1}\n{\"b\":").unwrap();
    FileLog::open(&tmp).unwrap();
    assert_eq!(
        std::fs::read(tmp.join(ARCHIVE_FILE)).unwrap(),
        archive[..whole]
    );
    assert_eq!(
        std::fs::read(tmp.join(REPLIES_FILE)).unwrap(),
        b"{\"a\":1}\n"
    );
    std::fs::remove_dir_all(&tmp).unwrap();
}

/// `T-005-17`: an archived entry decodes in every build. A production call's postcard tag must not
/// depend on `conformance-ports`, which a workspace build unifies into every binary: the stress
/// drill's `opsctl` could not read an archive the core host wrote while the ports' variants sat
/// before `Tick`. This runs both with the feature (the workspace's `cargo test`) and without it.
#[test]
fn t_005_17_a_calls_tag_does_not_depend_on_the_conformance_ports() {
    use core_binding::envelope::encode;
    use ledger_core::call::Call;
    let tick = encode(&Call::Tick {
        now: 1,
        due: Vec::new(),
    });
    assert_eq!(tick[0], 20, "Tick is the 21st variant in every build");
    let last = encode(&Call::Unsupported {
        method: String::new(),
    });
    assert!(last[0] > tick[0]);
}
