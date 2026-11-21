// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The reopen times of a large replica (`T-031-26`, `NFR-4`): after a clean stop (T-031-25's
//! record), after an unclean one (the last manifest, its tail replayed), and the fallback (a
//! rebuild from genesis), on one book.
//!
//! The book: a chainspec genesis of `ALEDGENT_T031_26_ACCOUNTS` identities, each with a ledger
//! account and an alias, a third of them with an opening balance (`common::spec_of`), founded
//! natively, then `ALEDGENT_T031_26_INPUTS` inputs (identities the operator opens), a manifest
//! every 1 024 of them (the default). The kill is a copy of the replica's directory while the core
//! holds it, `ALEDGENT_T031_26_TAIL` inputs after the last manifest. Ignored: it is a measurement,
//! run on purpose, in release, and its numbers go to `features/F-031-core-throughput/NOTES.md`:
//!
//! ```text
//! ALEDGENT_T031_26_ACCOUNTS=175000 ALEDGENT_T031_26_INPUTS=5000 \
//!   cargo test --release -p core-binding --features ingress --test replica_measure -- --ignored --nocapture
//! ```

mod common;

use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use core_binding::core::{AgentCore, Founding, HostLog as _, MemLog};
use core_binding::host::{KEPT_FILE, NativePort, ReplicaStores};
use core_binding::hot::{Limits, MAX_BUDGET};
use ingress::boundary::{CallerBody, Core as _, CoreInput, InputBody, InputKey};
use ingress::testkit::{T0, genesis};
use ledger_core::chainspec::{ChainSpec, ChunkLimits};

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn founding(s: &ChainSpec) -> Founding {
    let mut g = genesis(T0);
    g.genesis.chainspec = Some(s.commitment());
    g.genesis.spec = Some(std::sync::Arc::new(s.clone()));
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

fn open_identity(founded: &ledger_core::ledger::Founded, i: u64) -> CoreInput {
    let mut k = [0xC7; 16];
    k[8..].copy_from_slice(&i.to_be_bytes());
    CoreInput {
        key: InputKey(k),
        body: InputBody::Command {
            caller: CallerBody {
                principal: founded.operator.to_did(),
                credential: founded.operator_credential.to_did(),
                on_behalf_of: None,
            },
            operation: ledger_core::id::OperationId::from_ref(
                ledger_kernel::posting::OperationRef(k),
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

fn disk(dir: &Path) -> ReplicaStores {
    ReplicaStores::Disk {
        dir: dir.to_path_buf(),
        cache_bytes: core_binding::host::REPLICA_CACHE_BYTES,
    }
}

fn copy_port(p: &NativePort) -> NativePort {
    let mut q = NativePort::new();
    q.rows = p.rows.clone();
    q.slot = p.slot;
    q
}

fn copy_log(log: &MemLog) -> MemLog {
    let mut out = MemLog::default();
    for e in log.entries().unwrap() {
        out.push_entry(&e).unwrap();
    }
    for (k, r) in log.replies().unwrap() {
        out.push_reply(&k, &r).unwrap();
    }
    out
}

fn dir_bytes(dir: &Path) -> u64 {
    let mut n = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        n += if entry.file_type().unwrap().is_dir() {
            dir_bytes(&entry.path())
        } else {
            entry.metadata().unwrap().len()
        };
    }
    n
}

/// One start over `dir`: whether it reopened, how many inputs it replayed, and how long the open
/// took, wall clock.
fn start(
    s: &ChainSpec,
    dir: &Path,
    port: NativePort,
    log: MemLog,
) -> (AgentCore<NativePort>, u64, u64, Duration) {
    let t = Instant::now();
    let core = AgentCore::open_in(port, founding(s), log, disk(dir)).unwrap();
    let took = t.elapsed();
    let st = core.stats();
    let r = (
        st.replica_reused.load(Ordering::Relaxed),
        st.replica_rolled.load(Ordering::Relaxed),
    );
    (core, r.0, r.1, took)
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "a measurement: run on purpose, in release (module documentation)"]
async fn t031_26_measure_reopen_on_a_large_book() {
    let accounts = env("ALEDGENT_T031_26_ACCOUNTS", 20_000);
    let inputs = env("ALEDGENT_T031_26_INPUTS", 3_000);
    let tail = env("ALEDGENT_T031_26_TAIL", 500);
    let base = std::env::temp_dir().join(format!("aledgent-measure-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let live = base.join("live");
    let g = genesis(T0);
    let t = Instant::now();
    let s = common::spec_of(
        g.op,
        g.at,
        g.genesis.assets.clone(),
        accounts,
        ChunkLimits::DEFAULT,
    );
    eprintln!(
        "chainspec of {accounts} accounts built in {:?}",
        t.elapsed()
    );
    let log = MemLog::default();
    let t = Instant::now();
    let core =
        AgentCore::open_in(NativePort::new(), founding(&s), log.clone(), disk(&live)).unwrap();
    eprintln!("founded natively in {:?}", t.elapsed());
    let founded = core.founded();
    let t = Instant::now();
    // All but the tail, then a manifest, then the tail: the kill lands `tail` inputs after it.
    let head = inputs.saturating_sub(tail);
    let mut i = 0;
    while i < inputs {
        if i == head {
            core.take_manifest().expect("a manifest");
        }
        let end = (i + 8).min(if i < head { head } else { inputs });
        let batch: Vec<CoreInput> = (i..end).map(|n| open_identity(&founded, n)).collect();
        for r in core.apply_batch(&batch).await {
            r.expect("decided");
        }
        i = end;
    }
    let ran = t.elapsed();
    let m = core.manifest().expect("a manifest").seq;
    let n = log.entries().unwrap().len() as u64;
    let manifests = core.stats().replica_manifests.load(Ordering::Relaxed);
    eprintln!(
        "{n} inputs in {ran:?} ({}/s), {manifests} manifests, the last at input {m}; replica {} MB",
        u128::from(n) * 1000 / ran.as_millis().max(1),
        dir_bytes(&live) / 1_000_000
    );

    // The kill: the directory as the process holds it.
    let want = core.replica_digest().expect("a replica");
    let leaves = want.0.len();
    let killed = base.join("killed");
    copy_dir(&live, &killed);
    let port = core.with_port(|p| copy_port(p)).expect("a port");
    let logged = copy_log(&log);

    // The clean stop, and its reopen.
    core.keep_replica(Duration::from_secs(1)).expect("kept");
    drop(core);
    let (c, reused, _, clean) = start(&s, &live, copy_port(&port), copy_log(&logged));
    assert_eq!(reused, 1);
    assert_eq!(c.replica_digest().as_ref(), Some(&want));
    drop(c);

    // The unclean stop, and its reopen from the manifest.
    let (c, reused, rolled, unclean) = start(&s, &killed, copy_port(&port), copy_log(&logged));
    assert_eq!(reused, 1);
    assert_eq!(rolled, n - m);
    assert_eq!(c.replica_digest().as_ref(), Some(&want));
    drop(c);

    // The fallback: no usable manifest, a rebuild from genesis.
    let fallback_dir = base.join("fallback");
    copy_dir(&killed, &fallback_dir);
    std::fs::remove_file(fallback_dir.join(KEPT_FILE)).unwrap();
    let (c, reused, _, fallback) = start(&s, &fallback_dir, copy_port(&port), copy_log(&logged));
    assert_eq!(reused, 0);
    assert_eq!(c.replica_digest().as_ref(), Some(&want));
    drop(c);

    eprintln!(
        "T-031-26 reopen, {accounts} accounts, {n} inputs, {leaves} service leaves: clean {clean:?}; \
         unclean {unclean:?} ({rolled} inputs replayed over the manifest at input {m}); fallback \
         (rebuild from genesis) {fallback:?}"
    );
    let _ = std::fs::remove_dir_all(&base);
}
