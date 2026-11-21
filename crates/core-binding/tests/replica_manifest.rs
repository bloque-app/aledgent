// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The replica's manifest (`T-031-26`, T-031-25's v2; `REQ-RT-3`, `INV-6`, `NFR-4`): a running
//! core host takes a manifest of its replica every few inputs, and the start after an unclean stop
//! reopens the last one, checks it, and replays only the archive's tail since it. Whatever it
//! reopens is, leaf for leaf, what a rebuild from genesis gives; any check that fails rebuilds.
//!
//! **A kill, simulated.** A kill leaves on disk what the process had written, synced or not; what
//! it held in its own memory is lost. A copy of the replica's files taken while the process holds
//! them open is exactly that, so each "kill" here copies the directory at the point under test,
//! with the host's log and the Agent as they stood there, and a new core is started over the copy.
//! A power loss, which can also lose what was written and not synced, is not simulated: the order
//! of syncs is what covers it.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use core_binding::core::{AgentCore, Founding, HostLog, ManifestEvery, MemLog, ReplicaDigest};
use core_binding::host::manifest::{WritePoint, set_write_hook};
use core_binding::host::{KEPT_FILE, Kept, NativePort, ReplicaStores};
use core_binding::hot::{Limits, MAX_BUDGET, RingEntry};
use ingress::boundary::{CoreReply, InputKey};
use ingress::inprocess::MemJournal;
use ingress::store::mem::MemStore;
use ingress::testkit::{Deployment, Party, T0, USD, fast_config, genesis};
use leaf_store::engine::RedbEngine;
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

/// The write hook is one per process: the tests that arm it take turns.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

type Core = AgentCore<NativePort>;
type Dep = Deployment<MemStore, Core>;

async fn deploy(core: &Core, parties: &[&Party]) -> Dep {
    let mut config = fast_config("ingress-0");
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

/// `n` inputs: identities the operator registers, each one input that moves the service tree.
async fn traffic(d: &Dep, tag: &str, n: usize) {
    for i in 0..n {
        let (s, v) = d
            .call(
                "POST",
                "/v1/identities",
                Some(&d.operator.token),
                &[("client-key", &format!("{tag}-{i}"))],
                Some(json!({ "kind": "person", "verifier": "0a" })),
            )
            .await;
        assert_eq!(s.as_u16(), 200, "{v}");
    }
}

/// `n` inputs that move the kernel, the openings and the service tree: reservations of 1 by `p`,
/// alternating with identities.
async fn mixed(d: &Dep, p: &Party, tag: &str, n: usize) {
    for i in 0..n {
        if i % 2 == 0 {
            let (s, v) = d
                .call_decided(
                    "POST",
                    "/v1/reservations",
                    Some(&p.token),
                    &[("client-key", &format!("{tag}-r{i}"))],
                    Some(json!({ "payer": p.account, "asset": USD, "amount": "1" })),
                )
                .await;
            assert_eq!(s.as_u16(), 200, "{v}");
        } else {
            traffic(d, &format!("{tag}-{i}"), 1).await;
        }
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
        cache_bytes: 1024 * 1024,
    }
}

fn log_of(entries: Vec<RingEntry>, replies: Vec<(InputKey, CoreReply)>) -> MemLog {
    let mut log = MemLog::default();
    for e in &entries {
        log.push_entry(e).unwrap();
    }
    for (k, r) in &replies {
        log.push_reply(k, r).unwrap();
    }
    log
}

fn copy_log(log: &impl HostLog) -> MemLog {
    log_of(log.entries().unwrap(), log.replies().unwrap())
}

fn copy_port(core: &Core) -> NativePort {
    core.with_port(|p| {
        let mut q = NativePort::new();
        q.rows = p.rows.clone();
        q.origin = p.origin;
        q.slot = p.slot;
        q.generation = p.generation;
        q.code_map = p.code_map.clone();
        q
    })
    .expect("a port")
}

fn stat(core: &Core, f: impl Fn(&core_binding::core::CoreStats) -> u64) -> u64 {
    f(&core.stats())
}

fn reused(core: &Core) -> u64 {
    stat(core, |s| s.replica_reused.load(Ordering::Relaxed))
}

fn rolled(core: &Core) -> u64 {
    stat(core, |s| s.replica_rolled.load(Ordering::Relaxed))
}

/// What a start over `dir` gives with the Agent `port` and the host's `log`: the core, and whether
/// it reopened (1) or rebuilt (0) the replica.
fn start(dir: &Path, port: NativePort, log: MemLog) -> (Core, u64) {
    let core = AgentCore::open_in(port, founding(), log, disk(dir)).unwrap();
    let r = reused(&core);
    (core, r)
}

/// What a rebuild from genesis gives over the same Agent and log: the arbiter.
fn rebuilt(scratch: &Path, port: NativePort, log: MemLog) -> ReplicaDigest {
    let _ = std::fs::remove_dir_all(scratch);
    let (core, r) = start(scratch, port, log);
    assert_eq!(r, 0, "a fresh directory rebuilds");
    let digest = core.replica_digest().expect("a replica");
    drop(core);
    let _ = std::fs::remove_dir_all(scratch);
    digest
}

fn root(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aledgent-manifest-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn read_kept(dir: &Path) -> Kept {
    serde_json::from_slice(&std::fs::read(dir.join(KEPT_FILE)).unwrap()).unwrap()
}

fn write_kept(dir: &Path, kept: &Kept) {
    std::fs::write(dir.join(KEPT_FILE), serde_json::to_vec(kept).unwrap()).unwrap();
}

/// `T-031-26`: a kill between two manifests. The next start reopens the last manifest, replays
/// the inputs after it (the manifest is behind the archive), and stands where the Agent does,
/// leaf for leaf what a rebuild from genesis gives, with the same audit chain; it commits on.
#[tokio::test(flavor = "multi_thread")]
async fn t031_26_a_kill_reopens_the_last_manifest_and_replays_the_tail() {
    let _serial = SERIAL.lock().await;
    let base = root("tail");
    let live = base.join("live");
    let log = MemLog::default();
    let core = AgentCore::open_in(NativePort::new(), founding(), log.clone(), disk(&live)).unwrap();
    assert_eq!(
        core.manifest().map(|k| k.seq),
        Some(0),
        "a founding takes a manifest at once"
    );
    core.set_manifest_every(ManifestEvery {
        inputs: 5,
        interval: Duration::from_secs(3600),
    });
    let d = deploy(&core, &[]).await;
    let alice = d.party("alice", 1_000).await;
    mixed(&d, &alice, "a", 12).await;
    assert!(stat(&core, |s| s.replica_manifests.load(Ordering::Relaxed)) >= 2);
    // No manifest from here: the archive goes past the last one.
    core.set_manifest_every(ManifestEvery::NEVER);
    traffic(&d, "b", 3).await;
    drop(d);
    let m = core.manifest().expect("a manifest").seq;
    assert!(m >= 5);
    let n = log.entries().unwrap().len() as u64;
    assert!(n > m, "the archive is past the manifest");

    // The kill.
    let want = core.replica_digest().expect("a replica");
    let killed = base.join("killed");
    copy_dir(&live, &killed);
    let (port, logged) = (copy_port(&core), copy_log(&log));
    drop(core);

    let (again, r) = start(&killed, copy_port_of(&port), copy_log(&logged));
    assert_eq!(r, 1, "reopened from the manifest, not rebuilt");
    assert_eq!(rolled(&again), n - m, "only the archive's tail is replayed");
    assert_eq!(again.replica_digest().as_ref(), Some(&want));
    assert_eq!(
        rebuilt(&base.join("replay"), copy_port_of(&port), copy_log(&logged)),
        want,
        "and a rebuild from genesis is that replica"
    );
    // It commits on, checked input by input against the Agent, and takes manifests again.
    let d = deploy(&again, &[&alice]).await;
    traffic(&d, "c", 2).await;
    drop(d);
    assert_eq!(again.stopped(), None);
    assert_eq!(
        again.manifest().map(|k| k.seq),
        Some(n),
        "the reopened replica's manifest, taken at once"
    );
    let _ = std::fs::remove_dir_all(&base);
}

fn copy_port_of(p: &NativePort) -> NativePort {
    let mut q = NativePort::new();
    q.rows = p.rows.clone();
    q.origin = p.origin;
    q.slot = p.slot;
    q.generation = p.generation;
    q.code_map = p.code_map.clone();
    q
}

/// Each point a manifest and a clean stop's record are written at, with the directory copied there
/// (the kill), keyed by the point and the order it was reached in.
#[derive(Default, Clone)]
struct Images(Arc<Mutex<Vec<(WritePoint, PathBuf)>>>);

impl Images {
    fn arm(&self, live: &Path, into: &Path) {
        let images = self.clone();
        let live = live.to_path_buf();
        let into = into.to_path_buf();
        set_write_hook(Some(Box::new(move |point, dir| {
            if dir != live {
                return;
            }
            let mut all = images.0.lock().unwrap();
            // One manifest's points, or one clean stop's.
            if all.iter().any(|(p, _)| *p == WritePoint::RecordDone)
                && point != WritePoint::SavepointsCleared
            {
                return;
            }
            let to = into.join(format!("{:02}-{point:?}", all.len()));
            copy_dir(dir, &to);
            all.push((point, to));
        })));
    }

    fn disarm(&self) -> Vec<(WritePoint, PathBuf)> {
        set_write_hook(None);
        core::mem::take(&mut *self.0.lock().unwrap())
    }
}

/// `T-031-26`: a kill at each write point of a manifest (the replica's archive synced, each of the
/// three savepoints taken, the record written, synced, renamed, its directory synced), and of a
/// clean stop's record. Every one reopens without a rebuild, from the previous manifest or the new
/// one, and is the replica the host had.
#[tokio::test(flavor = "multi_thread")]
async fn t031_26_a_kill_at_each_write_point_reopens_without_a_rebuild() {
    let _serial = SERIAL.lock().await;
    let base = root("points");
    let live = base.join("live");
    let log = MemLog::default();
    let core = AgentCore::open_in(NativePort::new(), founding(), log.clone(), disk(&live)).unwrap();
    core.set_manifest_every(ManifestEvery {
        inputs: 4,
        interval: Duration::from_secs(3600),
    });
    let d = deploy(&core, &[]).await;
    let alice = d.party("alice", 1_000).await;
    mixed(&d, &alice, "a", 6).await;
    let before = core.manifest().expect("a manifest").seq;

    // One input at a time until a manifest is taken, every write point copied.
    let images = Images::default();
    images.arm(&live, &base.join("images"));
    let mut i = 0;
    while core.manifest().expect("a manifest").seq == before {
        mixed(&d, &alice, &format!("p{i}"), 1).await;
        i += 1;
    }
    let shots = images.disarm();
    drop(d);
    let points: Vec<WritePoint> = shots.iter().map(|(p, _)| *p).collect();
    assert_eq!(
        points,
        vec![
            WritePoint::Begin,
            WritePoint::ArchiveSynced,
            WritePoint::BookSaved,
            WritePoint::OpeningsSaved,
            WritePoint::ServiceSaved,
            WritePoint::RecordWritten,
            WritePoint::RecordSynced,
            WritePoint::RecordRenamed,
            WritePoint::RecordDone,
        ]
    );
    let want = core.replica_digest().expect("a replica");
    let (port, logged) = (copy_port(&core), copy_log(&log));
    let n = logged.entries().unwrap().len() as u64;
    let new = read_kept(&shots[shots.len() - 1].1).seq;
    assert!(new > before);
    for (point, dir) in &shots {
        // Before the rename the previous manifest is in force; from it, the new one.
        let from = read_kept(dir).seq;
        let renamed = matches!(point, WritePoint::RecordRenamed | WritePoint::RecordDone);
        assert_eq!(from, if renamed { new } else { before }, "{point:?}");
        let (c, r) = start(dir, copy_port_of(&port), copy_log(&logged));
        assert_eq!(r, 1, "{point:?}: reopened, not rebuilt");
        assert_eq!(rolled(&c), n - from, "{point:?}");
        assert_eq!(c.replica_digest().as_ref(), Some(&want), "{point:?}");
        assert_eq!(
            c.manifest().map(|k| k.seq),
            Some(n),
            "{point:?}: a manifest at once"
        );
        drop(c);
    }

    // A clean stop's record, killed at each of its points.
    images.arm(&live, &base.join("stop"));
    core.keep_replica(Duration::from_secs(1)).expect("kept");
    let shots = images.disarm();
    let points: Vec<WritePoint> = shots.iter().map(|(p, _)| *p).collect();
    assert_eq!(
        points,
        vec![
            WritePoint::RecordWritten,
            WritePoint::RecordSynced,
            WritePoint::RecordRenamed,
            WritePoint::RecordDone,
            WritePoint::SavepointsCleared,
        ]
    );
    for (point, dir) in &shots {
        // Before the rename, the manifest in force, and the inputs after it; from it, the clean
        // stop's record at the last input.
        let from = read_kept(dir).seq;
        if matches!(
            point,
            WritePoint::RecordRenamed | WritePoint::RecordDone | WritePoint::SavepointsCleared
        ) {
            assert_eq!(from, n, "stop, {point:?}");
        }
        let (c, r) = start(dir, copy_port_of(&port), copy_log(&logged));
        assert_eq!(r, 1, "stop, {point:?}: reopened, not rebuilt");
        assert_eq!(rolled(&c), n - from, "stop, {point:?}");
        assert_eq!(c.replica_digest().as_ref(), Some(&want), "stop, {point:?}");
        drop(c);
    }
    // The clean stop left no savepoint for a build that does not know them.
    drop(core);
    let leaves = RedbEngine::open(live.join("service").join("leaves.redb")).unwrap();
    assert!(leaves.savepoints().unwrap().is_empty());
    drop(leaves);
    let _ = std::fs::remove_dir_all(&base);
}

/// A host log that copies the replica's directory, and keeps the log as it stands, just before it
/// archives a decision's entries: the Agent has committed, the host has not archived.
#[derive(Clone)]
struct CrashLog {
    log: MemLog,
    live: PathBuf,
    shot: Arc<Mutex<Option<(PathBuf, MemLog)>>>,
    armed: Arc<Mutex<Option<PathBuf>>>,
}

impl HostLog for CrashLog {
    fn entries(&self) -> Result<Vec<RingEntry>, String> {
        self.log.entries()
    }
    fn push_entry(&mut self, e: &RingEntry) -> Result<(), String> {
        self.push_entries(core::slice::from_ref(e))
    }
    fn push_entries(&mut self, entries: &[RingEntry]) -> Result<(), String> {
        if let Some(to) = self.armed.lock().unwrap().take() {
            copy_dir(&self.live, &to);
            *self.shot.lock().unwrap() = Some((to, copy_log(&self.log)));
        }
        self.log.push_entries(entries)
    }
    fn replies(&self) -> Result<Vec<(InputKey, CoreReply)>, String> {
        self.log.replies()
    }
    fn push_reply(&mut self, key: &InputKey, reply: &CoreReply) -> Result<(), String> {
        self.log.push_reply(key, reply)
    }
}

/// `T-031-26`: a kill inside an input. After the replica ran it and the Agent committed it, before
/// the host archived it: the next start takes it from the Agent's ring and replays it too. After
/// the replica ran it, before the Agent committed it (the replica's files ahead of the Agent): the
/// files are put back to the manifest and the start stands where the Agent does.
#[tokio::test(flavor = "multi_thread")]
async fn t031_26_a_kill_inside_an_input_reopens_where_the_agent_stands() {
    let _serial = SERIAL.lock().await;
    let base = root("inside");
    let live = base.join("live");
    let log = CrashLog {
        log: MemLog::default(),
        live: live.clone(),
        shot: Arc::default(),
        armed: Arc::default(),
    };
    let core = AgentCore::open_in(NativePort::new(), founding(), log.clone(), disk(&live)).unwrap();
    core.set_manifest_every(ManifestEvery {
        inputs: 50,
        interval: Duration::from_secs(3600),
    });
    let d = deploy(&core, &[]).await;
    // Identities alone: no notification, so no delivery decided on its own between the kill and
    // the copies of the Agent below.
    traffic(&d, "a", 5).await;
    let m = core.manifest().expect("a manifest").seq;
    let at_n = core.replica_digest().expect("a replica");
    let port_n = copy_port(&core);
    let n = log.entries().unwrap().len() as u64;
    // One input, one decision: an identity.
    *log.armed.lock().unwrap() = Some(base.join("shot"));
    traffic(&d, "b", 1).await;
    drop(d);
    let at_n1 = core.replica_digest().expect("a replica");
    let port_n1 = copy_port(&core);
    let (shot, logged) = log.shot.lock().unwrap().take().expect("a kill");
    assert_eq!(
        logged.entries().unwrap().len() as u64,
        n,
        "not archived yet"
    );

    // The Agent committed it: taken from its ring, replayed, and archived.
    let first = base.join("committed");
    copy_dir(&shot, &first);
    let (c, r) = start(&first, port_n1, copy_log(&logged));
    assert_eq!(r, 1);
    assert_eq!(rolled(&c), n + 1 - m);
    assert_eq!(c.replica_digest().as_ref(), Some(&at_n1));
    drop(c);

    // The Agent did not commit it: the files ran ahead of the Agent, and are put back.
    let second = base.join("ahead");
    copy_dir(&shot, &second);
    let (c, r) = start(&second, port_n, copy_log(&logged));
    assert_eq!(r, 1);
    assert_eq!(rolled(&c), n - m);
    assert_eq!(c.replica_digest().as_ref(), Some(&at_n));
    drop(c);
    drop(core);
    let _ = std::fs::remove_dir_all(&base);
}

/// A running core's directory, Agent and log, with a manifest behind the archive: what the cases of
/// a manifest that must not be used start from.
struct Killed {
    base: PathBuf,
    dir: PathBuf,
    port: NativePort,
    log: MemLog,
    want: ReplicaDigest,
}

impl Killed {
    /// A fresh copy of the killed directory, to change and start over.
    fn copy(&self, name: &str) -> PathBuf {
        let to = self.base.join(name);
        let _ = std::fs::remove_dir_all(&to);
        copy_dir(&self.dir, &to);
        to
    }

    fn start(&self, dir: &Path) -> (Core, u64) {
        start(dir, copy_port_of(&self.port), copy_log(&self.log))
    }

    /// Start over `dir`: it must rebuild, and stand where the Agent does.
    fn rebuilds(&self, dir: &Path, why: &str) {
        let (c, r) = self.start(dir);
        assert_eq!(r, 0, "{why}: rebuilt");
        assert_eq!(c.replica_digest().as_ref(), Some(&self.want), "{why}");
        drop(c);
    }
}

async fn killed(name: &str) -> Killed {
    let base = root(name);
    let live = base.join("live");
    let log = MemLog::default();
    let core = AgentCore::open_in(NativePort::new(), founding(), log.clone(), disk(&live)).unwrap();
    core.set_manifest_every(ManifestEvery::NEVER);
    let d = deploy(&core, &[]).await;
    let alice = d.party("alice", 1_000).await;
    // Two manifests with the kernel, the openings and the service tree moved between them, and
    // inputs after the second.
    mixed(&d, &alice, "a", 6).await;
    core.take_manifest().expect("a manifest");
    mixed(&d, &alice, "b", 2).await;
    core.take_manifest().expect("a manifest");
    mixed(&d, &alice, "c", 2).await;
    drop(d);
    let want = core.replica_digest().expect("a replica");
    let dir = base.join("killed");
    copy_dir(&live, &dir);
    let killed = Killed {
        dir,
        port: copy_port(&core),
        log: copy_log(&log),
        want,
        base,
    };
    drop(core);
    killed
}

/// `T-031-26`: a torn manifest, or one altered after it was written, does not match its digest and
/// is not used; nor is one sealed again over another history.
#[tokio::test(flavor = "multi_thread")]
async fn t031_26_a_torn_or_altered_manifest_rebuilds() {
    let _serial = SERIAL.lock().await;
    let k = killed("torn").await;
    let torn = k.copy("torn");
    let bytes = std::fs::read(torn.join(KEPT_FILE)).unwrap();
    std::fs::write(torn.join(KEPT_FILE), &bytes[..bytes.len() / 2]).unwrap();
    k.rebuilds(&torn, "a torn manifest");

    let altered = k.copy("altered");
    let mut kept = read_kept(&altered);
    assert_eq!(kept.version, Kept::MANIFEST);
    kept.seq -= 1;
    write_kept(&altered, &kept);
    k.rebuilds(&altered, "an altered manifest");

    let other = k.copy("other");
    let mut kept = read_kept(&other);
    kept.log_head = [7; 32];
    write_kept(&other, &kept.sealed().unwrap());
    k.rebuilds(&other, "a manifest of another history");

    // And unaltered, it reopens.
    let (c, r) = k.start(&k.copy("whole"));
    assert_eq!(r, 1);
    assert_eq!(c.replica_digest().as_ref(), Some(&k.want));
    drop(c);
    let _ = std::fs::remove_dir_all(&k.base);
}

/// `T-031-26`: a manifest ahead of the archive and the Agent (the replica's directory from later
/// than the Agent and the log it starts with, as a restore from older media gives) is not used; a
/// manifest behind them is reopened and the archive's tail replayed (above).
#[tokio::test(flavor = "multi_thread")]
async fn t031_26_a_manifest_ahead_of_the_archive_rebuilds() {
    let _serial = SERIAL.lock().await;
    let base = root("ahead");
    let live = base.join("live");
    let log = MemLog::default();
    let core = AgentCore::open_in(NativePort::new(), founding(), log.clone(), disk(&live)).unwrap();
    core.set_manifest_every(ManifestEvery {
        inputs: 3,
        interval: Duration::from_secs(3600),
    });
    let d = deploy(&core, &[]).await;
    traffic(&d, "a", 4).await;
    let earlier = (
        copy_port(&core),
        copy_log(&log),
        core.replica_digest().expect("a replica"),
    );
    traffic(&d, "b", 6).await;
    drop(d);
    let m = core.manifest().expect("a manifest").seq;
    assert!(m > earlier.1.entries().unwrap().len() as u64);
    let dir = base.join("killed");
    copy_dir(&live, &dir);
    drop(core);
    let (c, r) = start(&dir, earlier.0, earlier.1);
    assert_eq!(r, 0, "a manifest ahead of the Agent rebuilds");
    assert_eq!(c.replica_digest().as_ref(), Some(&earlier.2));
    drop(c);
    let _ = std::fs::remove_dir_all(&base);
}

/// The savepoint of `file` other than the one the manifest names: the previous manifest's.
fn other_savepoint(file: &Path, named: u64) -> u64 {
    let e = RedbEngine::open(file).unwrap();
    let other = e
        .savepoints()
        .unwrap()
        .into_iter()
        .find(|s| *s != named)
        .expect("the previous manifest's savepoint");
    drop(e);
    other
}

/// `T-031-26`: a replica whose files do not reach the manifest's roots (here, a file put back to
/// the previous manifest's savepoint) is not served; it rebuilds.
#[tokio::test(flavor = "multi_thread")]
async fn t031_26_a_replica_whose_roots_do_not_match_rebuilds() {
    let _serial = SERIAL.lock().await;
    let k = killed("roots").await;

    let service = k.copy("service");
    let mut kept = read_kept(&service);
    let m = kept.manifest.as_mut().expect("a manifest");
    m.service_savepoint = other_savepoint(
        &service.join("service").join("leaves.redb"),
        m.service_savepoint,
    );
    write_kept(&service, &kept.sealed().unwrap());
    k.rebuilds(&service, "the service tree at another input");

    let book = k.copy("book");
    let mut kept = read_kept(&book);
    let m = kept.manifest.as_mut().expect("a manifest");
    m.book_savepoint = other_savepoint(&book.join("book").join("leaves.redb"), m.book_savepoint);
    write_kept(&book, &kept.sealed().unwrap());
    k.rebuilds(&book, "the book's leaves at another input");

    let missing = k.copy("missing");
    let mut kept = read_kept(&missing);
    kept.manifest
        .as_mut()
        .expect("a manifest")
        .openings_savepoint = 999_999;
    write_kept(&missing, &kept.sealed().unwrap());
    k.rebuilds(&missing, "a savepoint the file does not hold");
    let _ = std::fs::remove_dir_all(&k.base);
}

/// `T-031-26`: a decision the Agent did not commit rebuilds the replica in the process; it reopens
/// the manifest in force and replays the tail, instead of replaying from genesis.
#[tokio::test(flavor = "multi_thread")]
async fn t031_26_a_rebuild_after_a_failed_decision_reopens_the_manifest() {
    let _serial = SERIAL.lock().await;
    let base = root("refused");
    let live = base.join("live");
    let log = MemLog::default();
    let core = AgentCore::open_in(NativePort::new(), founding(), log.clone(), disk(&live)).unwrap();
    core.set_manifest_every(ManifestEvery {
        inputs: 4,
        interval: Duration::from_secs(3600),
    });
    let d = deploy(&core, &[]).await;
    let alice = d.party("alice", 1_000).await;
    traffic(&d, "a", 6).await;
    // The next decision, a reservation, runs on the replica and is not committed by the Agent.
    core.with_port(|p| p.fail_next = Some(false));
    mixed(&d, &alice, "b", 2).await;
    drop(d);
    let stats = core.stats();
    assert!(stats.replica_rebuilds.load(Ordering::Relaxed) >= 1);
    assert_eq!(
        stats.replica_rebuilds.load(Ordering::Relaxed),
        stats.replica_rebuilds_reopened.load(Ordering::Relaxed),
        "every rebuild reopened the manifest"
    );
    assert_eq!(core.stopped(), None);
    let want = core.replica_digest().expect("a replica");
    let (port, logged) = (copy_port(&core), copy_log(&log));
    drop(core);
    assert_eq!(rebuilt(&base.join("replay"), port, logged), want);
    let _ = std::fs::remove_dir_all(&base);
}
