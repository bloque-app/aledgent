// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The core host's continuous backup, end to end over the native Agent (`T-005-15`, `T-005-16`,
//! `NFR-4`, `NFR-10`): a real deployment (ingress, the Agent core, its log) streaming to a
//! directory sink; the archive read back, checked, laid out on a fresh node and rolled forward.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use core_archive::archive::{Archive, ArchiveSource as _};
use core_archive::native::{STATE_FILE, SavingPort, decode_rows};
use core_archive::record::decode_segment;
use core_archive::store::{FsStore, ObjectStore, Put, StoreError};
use core_archive::{
    ArchiveStatus, Archiver, ArchiverConfig, ArchivingLog, STATUS_FILE, book_of, fetch, lay_out,
};
use core_binding::core::{AgentCore, FileLog, Founding, HostLog as _, roll_forward_agent};
use core_binding::host::{NativePort, ReplicaStores};
use core_binding::hot::{Limits, MAX_BUDGET};
use ingress::boundary::{Core as _, CoreInput, InputBody};
use ingress::inprocess::MemJournal;
use ingress::store::mem::MemStore;
use ingress::testkit::{Deployment, T0, USD, fast_config, genesis};
use serde_json::json;

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

fn config() -> ArchiverConfig {
    ArchiverConfig {
        interval: Duration::from_millis(20),
        checkpoint_every_entries: 8,
        max_backoff: Duration::from_millis(50),
        ..ArchiverConfig::default()
    }
}

type Node = (
    Deployment<MemStore, AgentCore<SavingPort>>,
    AgentCore<SavingPort>,
    Archiver,
);

/// A deployment whose Agent core's log streams to `sink`.
async fn node(dir: &Path, sink: Box<dyn ObjectStore>, config: ArchiverConfig) -> Node {
    let log = FileLog::open(dir).unwrap();
    let (log, tap) = ArchivingLog::new(log, dir, Some(SavingPort::reader(dir)));
    let core = AgentCore::open(SavingPort::restore(dir).unwrap(), founding(), log)
        .unwrap_or_else(|e| panic!("the Agent core opens: {e}"));
    let archiver = Archiver::start(
        tap,
        dir,
        Archive::new(sink),
        book_of(&founding().op),
        config,
    );
    let d = Deployment::assemble(
        MemStore::new(Duration::from_millis(50)),
        fast_config("ingress-0"),
        core.clone(),
        core.founded(),
        MemJournal::new(),
    )
    .await;
    (d, core, archiver)
}

async fn workload(d: &Deployment<MemStore, AgentCore<SavingPort>>, tag: &str, n: u32) {
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

fn lines(path: &Path) -> u64 {
    std::fs::read(path)
        .map(|b| b.iter().filter(|c| **c == b'\n').count() as u64)
        .unwrap_or(0)
}

/// `NFR-4`, `NFR-10`: every decision cut before the archiver drains is in the sink, the archive
/// reads back as a byte-exact prefix of the host's logs, with checkpoints; laid out on a fresh
/// node and rolled forward, the Agent stands at the archive's point and answers every archived
/// key with its archived reply.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_10_the_archive_restores_a_fresh_node_to_its_point() {
    let tmp = tempfile::tempdir().unwrap();
    let (dir, sink) = (tmp.path().join("node"), tmp.path().join("sink"));
    std::fs::create_dir_all(&dir).unwrap();
    let (d, _core, archiver) = node(&dir, Box::new(FsStore::open(&sink).unwrap()), config()).await;
    workload(&d, "a", 12).await;
    let answered = lines(&dir.join("replies.jsonl"));
    archiver.stop();
    let archive = Archive::new(Box::new(FsStore::open(&sink).unwrap()));
    let f = fetch(&archive, Some(&book_of(&founding().op))).unwrap();
    assert!(
        f.point.inputs >= answered,
        "every decision answered is archived"
    );
    assert!(
        std::fs::read(dir.join("archive.bin"))
            .unwrap()
            .starts_with(&f.archive)
    );
    assert!(
        std::fs::read(dir.join("replies.jsonl"))
            .unwrap()
            .starts_with(&f.replies)
    );
    let c = f.checkpoint.as_ref().expect("a checkpoint");
    assert!(c.header.point.entries > 0 && c.header.point.entries <= f.point.entries);
    let status = ArchiveStatus::read(&dir.join(STATUS_FILE)).unwrap();
    assert_eq!((status.conflict, status.failures), (None, 0));

    // From genesis, in memory.
    let (_, rolled) = roll_forward_agent(
        NativePort::new(),
        &founding(),
        &f.entries,
        &ReplicaStores::Memory,
    )
    .unwrap();
    assert_eq!(hex::encode(rolled.roots.kernel), f.point.kernel_root);
    assert_eq!(hex::encode(rolled.log_head), f.point.log_head);

    // A fresh node from the checkpoint: laid out, rolled forward, opened, serving.
    let fresh = tmp.path().join("fresh");
    let plan = lay_out(&fresh, &f, &archive.describe(), STATE_FILE, true, 0).unwrap();
    assert_eq!(plan.checkpoint.map(|c| c.1), Some(c.header.point.entries));
    assert!(
        lay_out(&fresh, &f, "again", STATE_FILE, true, 0).is_err(),
        "never over a restore"
    );
    let port = SavingPort::restore(&fresh).unwrap();
    assert_eq!(
        decode_rows(&std::fs::read(fresh.join(STATE_FILE)).unwrap()).unwrap(),
        port.inner.rows
    );
    let entries = FileLog::open(&fresh).unwrap().entries().unwrap();
    let (_, rolled) =
        roll_forward_agent(port, &founding(), &entries, &ReplicaStores::Memory).unwrap();
    assert!(!rolled.founded && rolled.from == c.header.point.entries);
    assert_eq!(rolled.to, f.point.entries);
    let restored = AgentCore::open(
        SavingPort::restore(&fresh).unwrap(),
        founding(),
        FileLog::open(&fresh).unwrap(),
    )
    .unwrap();
    for (key, reply) in f.replies_parsed.iter().step_by(7) {
        let again = restored
            .apply(&CoreInput {
                key: *key,
                body: InputBody::Tick { now: T0 },
            })
            .await
            .unwrap();
        assert_eq!(
            &again, reply,
            "INV-18: an archived key answers its archived reply"
        );
    }
    drop(d);
}

/// A store that is down for its first `n` publications.
struct Flaky {
    inner: FsStore,
    down: Arc<AtomicU32>,
}

impl ObjectStore for Flaky {
    fn put_once(&self, name: &str, bytes: &[u8]) -> Result<Put, StoreError> {
        let mut n = self.down.load(Ordering::Acquire);
        let mut failing = false;
        while n > 0 {
            match self
                .down
                .compare_exchange(n, n - 1, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => {
                    failing = true;
                    break;
                }
                Err(now) => n = now,
            }
        }
        if failing {
            return Err(StoreError::Unavailable("injected".into()));
        }
        self.inner.put_once(name, bytes)
    }
    fn get(&self, name: &str, max: u64) -> Result<Option<Vec<u8>>, StoreError> {
        self.inner.get(name, max)
    }
    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        self.inner.list(prefix)
    }
    fn describe(&self) -> String {
        self.inner.describe()
    }
}

/// The sink down, then back; the host restarted on the same disk; a backlog split into parts:
/// the archive stays one chain, every decision reaches it, and nothing on the host waits.
#[tokio::test(flavor = "multi_thread")]
async fn t_005_15_outages_restarts_and_backlogs_keep_one_chain() {
    let tmp = tempfile::tempdir().unwrap();
    let (dir, sink) = (tmp.path().join("node"), tmp.path().join("sink"));
    std::fs::create_dir_all(&dir).unwrap();
    let down = Arc::new(AtomicU32::new(4));
    let flaky = Flaky {
        inner: FsStore::open(&sink).unwrap(),
        down: Arc::clone(&down),
    };
    let small = ArchiverConfig {
        max_segment_bytes: 700,
        ..config()
    };
    let (mut d, core, archiver) = node(&dir, Box::new(flaky), small).await;
    workload(&d, "a", 4).await;
    // The host stops: nothing decides on this core any more before another opens its disk (one
    // core identity on one machine, INV-25).
    assert!(d.quiesce(Duration::from_secs(60)).await);
    d.replica.kill();
    tokio::time::sleep(Duration::from_millis(300)).await;
    archiver.stop();
    assert_eq!(down.load(Ordering::Acquire), 0, "the outage happened");
    drop(d);
    drop(core);
    // The host restarts on its disk: the archiver resumes where the sink stands.
    let (d, _core, archiver) = node(&dir, Box::new(FsStore::open(&sink).unwrap()), small).await;
    workload(&d, "b", 4).await;
    let answered = lines(&dir.join("replies.jsonl"));
    archiver.stop();
    let archive = Archive::new(Box::new(FsStore::open(&sink).unwrap()));
    let f = fetch(&archive, None).unwrap();
    assert!(f.point.inputs >= answered);
    let parts = (1..=f.segments)
        .filter(|n| {
            let rec = archive.get_segment(*n).unwrap().unwrap();
            !decode_segment(&rec).unwrap().header.cut
        })
        .count();
    assert!(parts > 0, "a backlog over 700 bytes was sent in parts");
    let (_, rolled) = roll_forward_agent(
        NativePort::new(),
        &founding(),
        &f.entries,
        &ReplicaStores::Memory,
    )
    .unwrap();
    assert_eq!(hex::encode(rolled.roots.kernel), f.point.kernel_root);
    drop(d);
}

/// One history (`INV-25`): a node whose logs are not the archive's never writes to it, and says
/// so in its status.
#[tokio::test(flavor = "multi_thread")]
async fn inv_25_another_history_never_writes_to_the_archive() {
    let tmp = tempfile::tempdir().unwrap();
    let sink = tmp.path().join("sink");
    let one = tmp.path().join("one");
    let two = tmp.path().join("two");
    for d in [&one, &two] {
        std::fs::create_dir_all(d).unwrap();
    }
    let (d1, _c1, a1) = node(&one, Box::new(FsStore::open(&sink).unwrap()), config()).await;
    workload(&d1, "a", 2).await;
    a1.stop();
    let archive = Archive::new(Box::new(FsStore::open(&sink).unwrap()));
    let before = archive.segments().unwrap();
    // Another node of the same book, another history (different requests).
    let (d2, _c2, a2) = node(&two, Box::new(FsStore::open(&sink).unwrap()), config()).await;
    workload(&d2, "zz", 3).await;
    a2.stop();
    assert_eq!(archive.segments().unwrap(), before, "nothing written");
    let status = ArchiveStatus::read(&two.join(STATUS_FILE)).unwrap();
    assert!(status.conflict.is_some(), "{status:?}");
    assert!(status.lag_inputs > 0 || status.pending_bytes > 0);
    drop((d1, d2));
}
