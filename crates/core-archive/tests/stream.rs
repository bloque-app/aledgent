// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The core host's continuous backup, end to end over the native Agent (`T-005-15`, `T-005-16`,
//! `NFR-4`, `NFR-10`): a real deployment (ingress, the Agent core, its log) streaming to a
//! directory sink; the archive read back, checked, laid out on a fresh node and rolled forward.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use core_archive::age::Identity;
use core_archive::archive::{Archive, ArchiveSource};
use core_archive::native::{STATE_FILE, SavingPort, decode_rows};
use core_archive::record::decode_segment;
use core_archive::seal::{Keys, MAGIC_LINE, Sealed, Unsealed};
use core_archive::store::{FsStore, ObjectStore, Put, StoreError};
use core_archive::{
    ArchiveStatus, Archiver, ArchiverConfig, ArchivingLog, Fetched, STATUS_FILE, book_of, fetch,
    fetch_with, lay_out,
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

/// `F-044`: the core host archives to a sealed store and holds no key that opens it. Restarted on
/// its disk with a new writer (another file key, nothing remembered), it resumes from what the seal
/// keeps in clear, one chain. The archive restores with the offline recovery key alone, and with
/// Transit alone; with neither, nothing of it is read.
#[tokio::test(flavor = "multi_thread")]
async fn f_044_a_sealed_archive_resumes_after_a_restart_and_restores_with_either_key() {
    let tmp = tempfile::tempdir().unwrap();
    let (dir, sink) = (tmp.path().join("node"), tmp.path().join("sink"));
    std::fs::create_dir_all(&dir).unwrap();
    let recovery = Identity::generate();
    let writer = || -> Box<dyn ObjectStore> {
        Box::new(Sealed::new(
            Box::new(FsStore::open(&sink).unwrap()),
            Some(common::recipients(1, &recovery)),
            Keys::none(),
            Unsealed::Refuse,
        ))
    };
    let reader = |keys: Keys| {
        Archive::new(Box::new(Sealed::new(
            Box::new(FsStore::open(&sink).unwrap()),
            None,
            keys,
            Unsealed::Refuse,
        )))
    };
    let small = ArchiverConfig {
        max_segment_bytes: 700,
        ..config()
    };
    let (mut d, core, archiver) = node(&dir, writer(), small).await;
    workload(&d, "a", 4).await;
    assert!(d.quiesce(Duration::from_secs(60)).await);
    d.replica.kill();
    tokio::time::sleep(Duration::from_millis(300)).await;
    archiver.stop();
    drop(d);
    drop(core);
    let before = reader(Keys::none()).segments().unwrap().len() as u64;
    assert!(before > 0);
    let (d, _core, archiver) = node(&dir, writer(), small).await;
    workload(&d, "b", 4).await;
    let answered = lines(&dir.join("replies.jsonl"));
    archiver.stop();
    let status = ArchiveStatus::read(&dir.join(STATUS_FILE)).unwrap();
    assert_eq!(
        (status.conflict, status.failures),
        (None, 0),
        "the restarted writer resumed with no key"
    );
    let plain = FsStore::open(&sink).unwrap();
    for prefix in ["segments/", "checkpoints/"] {
        for name in plain.list(prefix).unwrap() {
            assert!(
                std::fs::read(sink.join(&name))
                    .unwrap()
                    .starts_with(MAGIC_LINE),
                "{name} is sealed"
            );
        }
    }

    let by_recovery = reader(Keys {
        transit: None,
        recovery: vec![Identity::parse(&recovery.to_bech32()).unwrap()],
    });
    let f = fetch(&by_recovery, Some(&book_of(&founding().op))).unwrap();
    assert!(!f.damaged, "{:?}", f.notes);
    assert!(f.segments > before && f.point.inputs >= answered);
    assert!(f.checkpoint.is_some());
    let (_, rolled) = roll_forward_agent(
        NativePort::new(),
        &founding(),
        &f.entries,
        &ReplicaStores::Memory,
    )
    .unwrap();
    assert_eq!(hex::encode(rolled.roots.kernel), f.point.kernel_root);

    let by_transit = reader(Keys {
        transit: Some(Box::new(common::LocalTransit::default())),
        recovery: Vec::new(),
    });
    let g = fetch(&by_transit, None).unwrap();
    assert_eq!((g.point, g.segments), (f.point.clone(), f.segments));

    match fetch(&reader(Keys::none()), None) {
        Err(e) => assert!(e.contains("sealed"), "{e}"),
        Ok(f) => assert!(f.damaged && f.segments == 0, "nothing read with no key"),
    }
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

/// An archive whose segments arrive out of order: each read waits a time that does not grow with
/// its number, so later segments often come back before earlier ones. It can also lose, garble or
/// fail to read one segment.
struct OutOfOrder {
    inner: Archive,
    vanish: Option<u64>,
    garble: Option<u64>,
    fail: Option<u64>,
    arrived: std::sync::Mutex<Vec<u64>>,
}

impl OutOfOrder {
    fn new(sink: &Path) -> OutOfOrder {
        OutOfOrder {
            inner: Archive::new(Box::new(FsStore::open(sink).unwrap())),
            vanish: None,
            garble: None,
            fail: None,
            arrived: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl ArchiveSource for OutOfOrder {
    fn segments(&self) -> Result<Vec<u64>, StoreError> {
        self.inner.segments()
    }
    fn checkpoints(&self) -> Result<Vec<u64>, StoreError> {
        self.inner.checkpoints()
    }
    fn get_segment(&self, n: u64) -> Result<Option<Vec<u8>>, StoreError> {
        std::thread::sleep(Duration::from_millis((n * 7919) % 13));
        self.arrived.lock().unwrap().push(n);
        if self.fail == Some(n) {
            return Err(StoreError::Unavailable("injected".into()));
        }
        if self.vanish == Some(n) {
            return Ok(None);
        }
        let mut rec = self.inner.get_segment(n)?;
        if self.garble == Some(n)
            && let Some(r) = rec.as_mut()
        {
            let last = r.len() - 1;
            r[last] ^= 0xff;
        }
        Ok(rec)
    }
    fn get_checkpoint(&self, n: u64) -> Result<Option<Vec<u8>>, StoreError> {
        self.inner.get_checkpoint(n)
    }
}

/// What a read checked and kept, to compare two reads of one archive.
fn outcome(f: Result<Fetched, String>) -> Result<String, String> {
    use sha2::{Digest as _, Sha256};
    f.map(|f| {
        format!(
            "{} {} {:?} {:?} {} {} {} {} {:?}",
            f.book,
            f.segments,
            f.point,
            f.notes,
            f.damaged,
            hex::encode(Sha256::digest(&f.archive)),
            hex::encode(Sha256::digest(&f.replies)),
            f.entries.len(),
            f.checkpoint.as_ref().map(|c| c.header.n),
        )
    })
}

/// `T-005-16`, `NFR-4` (the 2026-10-15 restore drill, item 8): segments read many at once, and
/// arriving out of order, are checked exactly as when read one at a time: the same segments in
/// the same order, the same point, the same notes. A missing, garbled or unreadable segment ends
/// the read, or fails it, exactly as sequentially, whatever was read past it.
#[tokio::test(flavor = "multi_thread")]
async fn t_005_16_a_parallel_read_checks_in_order_whatever_order_segments_arrive() {
    let tmp = tempfile::tempdir().unwrap();
    let (dir, sink) = (tmp.path().join("node"), tmp.path().join("sink"));
    std::fs::create_dir_all(&dir).unwrap();
    let small = ArchiverConfig {
        max_segment_bytes: 700,
        ..config()
    };
    let (d, _core, archiver) = node(&dir, Box::new(FsStore::open(&sink).unwrap()), small).await;
    workload(&d, "a", 6).await;
    archiver.stop();
    drop(d);
    let book = book_of(&founding().op);
    let reads = |src: &OutOfOrder| {
        (
            outcome(fetch_with(src, Some(&book), 1)),
            outcome(fetch_with(src, Some(&book), 8)),
            outcome(fetch_with(src, Some(&book), 4096)),
        )
    };

    // Healthy: one read, whatever the concurrency; and the segments did arrive out of order.
    let src = OutOfOrder::new(&sink);
    let segments = src.segments().unwrap().len();
    assert!(segments >= 8, "enough segments to overlap: {segments}");
    let (one, many, more) = reads(&src);
    let one = one.expect("a healthy archive reads");
    assert_eq!(Ok(&one), many.as_ref());
    assert_eq!(Ok(&one), more.as_ref());
    let arrived = src.arrived.lock().unwrap().clone();
    let parallel = &arrived[segments..];
    assert!(
        parallel.windows(2).any(|w| w[1] < w[0]),
        "the segments arrived out of order: {parallel:?}"
    );
    let full = fetch_with(&src, Some(&book), 8).unwrap();
    assert!(!full.damaged, "{:?}", full.notes);

    let mid = segments as u64 / 2;
    // A garbled segment, a vanished one: the archive ends before it, the same way.
    for (garble, vanish) in [(Some(mid), None), (None, Some(mid))] {
        let src = OutOfOrder {
            garble,
            vanish,
            ..OutOfOrder::new(&sink)
        };
        let (one, many, more) = reads(&src);
        let one = one.expect("a damaged archive reads to before the damage");
        assert_eq!(Ok(&one), many.as_ref());
        assert_eq!(Ok(&one), more.as_ref());
        let f = fetch_with(&src, Some(&book), 8).unwrap();
        assert!(f.damaged && f.segments < mid, "{:?}", f.notes);
        assert!(
            f.notes
                .iter()
                .any(|n| n.starts_with(&format!("segment {mid} "))),
            "{:?}",
            f.notes
        );
    }

    // An unreadable segment fails the read, with the same error.
    let src = OutOfOrder {
        fail: Some(mid),
        ..OutOfOrder::new(&sink)
    };
    let (one, many, more) = reads(&src);
    let err = one.expect_err("a store error fails the read");
    assert!(err.starts_with(&format!("segment {mid}: ")), "{err}");
    assert_eq!(Err(&err), many.as_ref());
    assert_eq!(Err(&err), more.as_ref());

    // A garbled segment before an unreadable one: the read ends at the first, as it does
    // sequentially; the error read past it, out of order, changes nothing.
    let src = OutOfOrder {
        garble: Some(mid),
        fail: Some(mid + 1),
        ..OutOfOrder::new(&sink)
    };
    let (one, many, more) = reads(&src);
    let one = one.expect("the damage comes first");
    assert_eq!(Ok(&one), many.as_ref());
    assert_eq!(Ok(&one), more.as_ref());
}
