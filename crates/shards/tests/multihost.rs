// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The cross-host relay (feature `net`; `NFR-21` (a), an experiment):
//! the book's shard Agents on several hosts, every
//! stream carried between them over Noise sessions on the loopback interface, through the Agents'
//! own transition (`NativePort`).
//!
//! Hosts run in one process (threads) or as separate processes (this test binary run again as a
//! child, `multihost_child`); one child is killed (SIGKILL) and restarted from its Agents' rows and
//! its round logs.
#![cfg(feature = "net")]

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command as Proc};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use core_binding::transition::MemRows;
use shards::decide::Round;
use shards::host::{NativePort, Port, PortStats, ShardAgent, accounts};
use shards::net::noise::{Keypair, hex, unhex32};
use shards::net::wire::{Member, Report};
use shards::net::{Answered, Attached, Config, Layout, Node, RoundLog};
use shards::pool::Pool;
use shards::principals::PrincipalsCommand;
use shards::{Account, Command, Directory, Outcome, Refusal, ShardId};

const USD: u32 = 840;
const ROOT: [u8; 32] = [0x5D; 32];
const DRIVER: [u8; 32] = [0xD5; 32];
const SEED: [u8; 32] = [0x77; 32];
const FUNDS: u64 = 1_000_000;

fn cfg() -> Config {
    Config {
        batch: 16,
        ..Config::default()
    }
}

fn op(n: u64) -> [u8; 16] {
    let mut o = [0u8; 16];
    o[..8].copy_from_slice(&n.to_be_bytes());
    o[8] = 0x5A;
    o
}

fn agents(n: u8) -> Vec<[u8; 16]> {
    (0..n)
        .map(|i| {
            let mut p = [0xA0u8; 16];
            p[15] = i;
            p
        })
        .collect()
}

/// A deterministic stream: splitmix64.
struct Mix(u64);
impl Mix {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// Hosts in this process, connected over loopback.
fn mesh(layout: Layout) -> Vec<Arc<Node<NativePort>>> {
    mesh_with(layout, cfg())
}

fn mesh_with(layout: Layout, cfg: Config) -> Vec<Arc<Node<NativePort>>> {
    let mut nodes = Vec::new();
    let mut members = Vec::new();
    for h in 0..layout.nodes {
        let ports: Vec<NativePort> = layout
            .shards_of(h)
            .map(|_| NativePort::new(DRIVER))
            .collect();
        let logs = ports.iter().map(|_| None).collect();
        let principals = (h == 0).then(|| NativePort::new(DRIVER));
        let n = Node::found(h, layout, cfg, ports, logs, principals, ROOT, SEED).expect("found");
        let key = Keypair::generate();
        let public = key.public;
        let addr = n
            .listen(key, "127.0.0.1:0".parse().unwrap(), b"test-book")
            .expect("listen");
        members.push(Member {
            index: h,
            addr: addr.to_string(),
            key: public,
        });
        nodes.push(Arc::new(n));
    }
    nodes[0].set_roster(members.clone());
    for n in &nodes[1..] {
        n.add_peer(0, Some(members[0].addr.clone()), members[0].key);
    }
    connected(&nodes[0], layout.nodes);
    nodes
}

/// Wait until every host has a session with every other.
fn connected<P: Port + Send + 'static>(n0: &Node<P>, nodes: u16) {
    let t = Instant::now();
    loop {
        if let Ok(all) = n0.statuses(Duration::from_secs(2)) {
            if all.values().all(|s| s.connected + 1 == nodes) {
                return;
            }
        }
        assert!(
            t.elapsed() < Duration::from_secs(60),
            "the mesh did not form"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct Book {
    accts: Vec<Account>,
    fees: Pool,
    principals: Vec<[u8; 16]>,
}

impl Book {
    fn all(&self) -> Vec<(Account, u32)> {
        let mut v: Vec<(Account, u32)> = self.accts.iter().map(|a| (*a, USD)).collect();
        v.extend(self.fees.members.iter().map(|m| (*m, USD)));
        v
    }
}

/// Accounts and the fee pool opened through the principals Agent, grants, and funds.
fn setup<P: Port + Send + 'static>(n0: &Node<P>, accounts_n: u32) -> Book {
    let dir: Directory = n0.directory();
    let b = Book {
        accts: accounts(accounts_n),
        fees: Pool::new([0xFE; 16], &dir),
        principals: agents(4),
    };
    let mut cmds: Vec<PrincipalsCommand> = b
        .all()
        .into_iter()
        .map(|(account, asset)| PrincipalsCommand::Open { account, asset })
        .collect();
    for (i, a) in b.accts.iter().enumerate() {
        cmds.push(PrincipalsCommand::Grant {
            principal: b.principals[i % b.principals.len()],
            payer: *a,
            limit: 1_000,
        });
    }
    n0.principals(&cmds).expect("principals");
    n0.add_pool(b.fees.clone());
    wait(
        || n0.feed_everywhere() >= n0.feed_version(),
        "every shard learns the accounts",
    );
    for (i, a) in b.accts.iter().enumerate() {
        n0.operator(Command::Issue {
            op: op(1 << 40 | i as u64),
            account: *a,
            asset: USD,
            amount: FUNDS,
        });
    }
    let got = collect(n0, b.accts.len());
    assert!(got.iter().all(|a| a.outcome == Outcome::Issued), "{got:?}");
    b
}

fn wait(mut f: impl FnMut() -> bool, what: &str) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed() < Duration::from_secs(120), "timed out: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn collect<P: Port + Send + 'static>(n0: &Node<P>, n: usize) -> Vec<Answered> {
    let mut got = Vec::new();
    let t = Instant::now();
    while got.len() < n {
        got.extend(n0.answers(Duration::from_millis(200)));
        assert!(
            t.elapsed() < Duration::from_secs(180),
            "{} of {n} answered",
            got.len()
        );
    }
    got
}

/// `ops` transfers in chunks, a tenth to the fee pool, half by a principal, principal 0 revoked
/// and granted again half way, an anchor after every chunk (each must be a consistent cut).
/// `during(i)` runs before chunk `i` (faults).
fn load<P: Port + Send + 'static>(
    n0: &Node<P>,
    b: &Book,
    ops: usize,
    seed: u64,
    mut during: impl FnMut(usize),
) -> (Vec<Answered>, u64) {
    let mut mix = Mix(seed);
    let mut answered = Vec::new();
    let mut sent = 0;
    let mut anchors = 0;
    let mut revoked = false;
    let mut chunk_i = 0;
    while sent < ops {
        during(chunk_i);
        chunk_i += 1;
        let k = 24.min(ops - sent);
        let batch: Vec<_> = (0..k)
            .map(|_| {
                let i = (mix.next() % b.accts.len() as u64) as usize;
                let payee = if mix.next() % 10 == 0 {
                    b.fees.id
                } else {
                    b.accts[(mix.next() % b.accts.len() as u64) as usize]
                };
                let by = (mix.next() % 2 == 0).then(|| b.principals[i % b.principals.len()]);
                (by, b.accts[i], payee, USD, 1 + mix.next() % 500)
            })
            .collect();
        let tickets = n0.transfers(&batch);
        let accepted = tickets.iter().filter(|t| t.is_ok()).count();
        sent += k;
        answered.extend(collect(n0, accepted));
        n0.anchor().expect("every anchor is at a consistent cut");
        anchors += 1;
        if !revoked && sent >= ops / 2 {
            let mut cmds = vec![PrincipalsCommand::Revoke {
                principal: b.principals[0],
            }];
            for a in b.accts.iter().step_by(b.principals.len()) {
                cmds.push(PrincipalsCommand::Grant {
                    principal: b.principals[0],
                    payer: *a,
                    limit: 1_000,
                });
            }
            n0.principals(&cmds).expect("revoke");
            revoked = true;
        }
    }
    (answered, anchors)
}

/// Quiescent, then: exactly once read from the Agents, the trial balance with in-transit
/// positions, every balance equal to an independent ledger of the accepted transfers, every
/// replica at its Agent's root, and a final anchor.
fn verify<P: Port + Send + 'static>(
    n0: &Node<P>,
    b: &Book,
    answered: &[Answered],
) -> BTreeMap<u16, Report> {
    n0.quiesce(Duration::from_secs(120)).expect("quiescent");
    let (_, cut) = n0.anchor().expect("the final anchor");
    cut.check().expect("consistent");
    let reports = n0
        .reports(&[USD], &b.all(), false, Duration::from_secs(60))
        .expect("reports");
    let shards: Vec<_> = reports.values().flat_map(|r| r.shards.iter()).collect();
    assert_eq!(shards.len(), usize::from(n0.layout().total()));
    // Exactly once (INV-18, REQ-RT-11), from the Agents' committed heads.
    let applied: BTreeMap<(ShardId, ShardId), u64> = shards
        .iter()
        .flat_map(|s| s.applied.iter().map(move |(src, v)| ((*src, s.shard), *v)))
        .collect();
    let mut crossing = 0;
    for s in &shards {
        assert_ne!(
            s.root, [0xFF; 32],
            "shard {} replica and Agent disagree",
            s.shard.0
        );
        for (d, e) in &s.enqueued {
            assert_eq!(applied[&(s.shard, *d)], *e, "pair {}→{}", s.shard.0, d.0);
            crossing += e;
        }
        assert!(
            s.outbound_left.iter().all(|(_, left)| !left),
            "an outbox not trimmed"
        );
    }
    assert!(crossing > 0, "transfers crossed shards");
    // The trial balance (INV-5) with in-transit positions.
    let (mut sum, mut issued, mut transit, mut outstanding) = (0i128, 0i128, 0i128, 0i128);
    for s in &shards {
        let (_, total, iss, tr) = s.sums[0];
        assert_eq!(total, 0, "shard {} sums to zero", s.shard.0);
        sum += total - iss - tr;
        issued += iss;
        transit += tr;
        for (d, seq, asset, amount) in &s.untrimmed {
            if *asset == USD && *seq > applied[&(s.shard, *d)] {
                outstanding += i128::from(*amount);
            }
        }
    }
    assert_eq!(sum + issued + transit, 0);
    assert_eq!(transit, outstanding);
    // Zero divergence against an independent ledger of what was accepted (NFR-9).
    let mut want: BTreeMap<Account, i128> = b.all().iter().map(|(a, _)| (*a, 0)).collect();
    for a in &b.accts {
        *want.get_mut(a).unwrap() += i128::from(FUNDS);
    }
    for a in answered {
        if let Command::Transfer {
            payer,
            payee,
            amount,
            ..
        } = a.order.command
        {
            if a.outcome.accepted() {
                *want.get_mut(&payer).unwrap() -= i128::from(amount);
                *want.get_mut(&payee).unwrap() += i128::from(amount);
            }
        }
    }
    let got: BTreeMap<Account, i128> = shards
        .iter()
        .flat_map(|s| s.balances.iter().map(|(a, _, v)| (*a, *v)))
        .collect();
    assert_eq!(got, want, "every balance equals the independent ledger");
    reports
}

fn tally(answered: &[Answered]) -> BTreeMap<String, usize> {
    let mut m = BTreeMap::new();
    for a in answered {
        let k = match a.outcome {
            Outcome::Refused(r) => format!("{r:?}"),
            o => format!("{o:?}")
                .split([' ', '{'])
                .next()
                .unwrap()
                .to_string(),
        };
        *m.entry(k).or_default() += 1;
    }
    m
}

#[test]
fn nfr_21_every_stream_crosses_hosts_exactly_once() {
    // Host 0: the principals Agent and one shard; hosts 1 and 2: two shards each.
    let layout = Layout {
        nodes: 3,
        first: 1,
        per_node: 2,
    };
    let nodes = mesh(layout);
    let n0 = &nodes[0];
    let b = setup(n0.as_ref(), 24);
    let (answered, anchors) = load(n0.as_ref(), &b, 240, 11, |_| {});
    assert!(anchors > 5);
    let t = tally(&answered);
    assert!(t.get("Debited").copied().unwrap_or(0) > 100, "{t:?}");
    assert!(!t.contains_key("Insufficient"), "{t:?}");
    verify(n0.as_ref(), &b, &answered);
    // The revocation (REQ-AU-3) reached every shard on every host, and ingress refuses its
    // principal's old authority now.
    assert_eq!(n0.feed_everywhere(), n0.feed_version());
    assert_eq!(n0.take_revocation_lags().len(), 1);
    for n in nodes.iter().rev() {
        n.stop();
    }
}

#[test]
fn req_rt_11_exactly_once_while_sessions_drop_under_load() {
    let layout = Layout {
        nodes: 3,
        first: 0,
        per_node: 2,
    };
    let nodes = mesh(layout);
    let n0 = Arc::clone(&nodes[0]);
    let b = setup(n0.as_ref(), 24);
    // Every host drops all its sessions every 40 ms while the load runs.
    let stop = Arc::new(AtomicBool::new(false));
    let chaos: Vec<_> = nodes
        .iter()
        .map(|n| {
            let n = Arc::clone(n);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(Ordering::SeqCst) {
                    std::thread::sleep(Duration::from_millis(40));
                    n.net().unwrap().drop_sessions();
                }
            })
        })
        .collect();
    let (answered, _) = load(n0.as_ref(), &b, 240, 12, |_| {});
    stop.store(true, Ordering::SeqCst);
    for c in chaos {
        c.join().unwrap();
    }
    let reports = verify(n0.as_ref(), &b, &answered);
    let sessions: u64 = reports.values().map(|r| r.net.sessions).sum();
    assert!(
        sessions > 20,
        "sessions were dropped and re-established ({sessions})"
    );
    for n in nodes.iter().rev() {
        n.stop();
    }
}

// ---- Separate processes -----------------------------------------------------------------------

/// A native port whose rows are written to a file after every invocation: the Agent's durable
/// copy, as VOS would hold it, for a host that is killed and restarted.
struct DurablePort {
    inner: NativePort,
    path: PathBuf,
}

impl DurablePort {
    fn open(path: PathBuf) -> Self {
        let mut inner = NativePort::new(DRIVER);
        if let Ok(b) = std::fs::read(&path) {
            let rows: Vec<(Vec<u8>, Vec<u8>)> = postcard::from_bytes(&b).expect("rows");
            *inner.rows_mut() = MemRows(rows.into_iter().collect());
        }
        DurablePort { inner, path }
    }
    fn save(&self) {
        let rows: Vec<(&Vec<u8>, &Vec<u8>)> = self.inner.rows().0.iter().collect();
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, postcard::to_allocvec(&rows).unwrap()).unwrap();
        std::fs::rename(&tmp, &self.path).unwrap();
    }
}

impl Port for DurablePort {
    fn invoke(&mut self, message: Vec<u8>, blobs: Vec<Vec<u8>>) -> Result<Vec<u8>, String> {
        let r = self.inner.invoke(message, blobs);
        self.save();
        r
    }
    fn read(&mut self, query: Vec<u8>) -> Result<Vec<u8>, String> {
        self.inner.read(query)
    }
    fn blob_key(&self, bytes: &[u8]) -> core_binding::envelope::BlobKey {
        self.inner.blob_key(bytes)
    }
    fn stats(&self) -> PortStats {
        self.inner.stats()
    }
}

/// A round log in a file: length-prefixed postcard, appended before each invocation.
struct FileLog(std::fs::File);

impl RoundLog for FileLog {
    fn append(&mut self, round: &Round) -> std::io::Result<()> {
        let b = postcard::to_allocvec(round).expect("a round encodes");
        self.0.write_all(&(b.len() as u32).to_be_bytes())?;
        self.0.write_all(&b)?;
        self.0.flush()
    }
}

fn read_log(path: &Path) -> Vec<Round> {
    let mut b = Vec::new();
    if let Ok(mut f) = std::fs::File::open(path) {
        f.read_to_end(&mut b).unwrap();
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i + 4 <= b.len() {
        let n = u32::from_be_bytes(b[i..i + 4].try_into().unwrap()) as usize;
        if i + 4 + n > b.len() {
            break;
        }
        out.push(postcard::from_bytes(&b[i + 4..i + 4 + n]).expect("a round"));
        i += 4 + n;
    }
    out
}

/// Keep the first `n` rounds of a log (the rest were never committed by the Agent).
fn truncate_log(path: &Path, rounds: &[Round]) {
    let mut f = std::fs::File::create(path).unwrap();
    for r in rounds {
        let b = postcard::to_allocvec(r).unwrap();
        f.write_all(&(b.len() as u32).to_be_bytes()).unwrap();
        f.write_all(&b).unwrap();
    }
}

const MP_LAYOUT: Layout = Layout {
    nodes: 3,
    first: 0,
    per_node: 2,
};

/// The child's side: host `MULTIHOST_INDEX` of `MP_LAYOUT`, its Agents durable in
/// `MULTIHOST_DIR`; it founds them, or attaches to them if they exist (a restart).
#[test]
fn multihost_child() {
    let Ok(index) = std::env::var("MULTIHOST_INDEX") else {
        return;
    };
    let index: u16 = index.parse().unwrap();
    let dir = PathBuf::from(std::env::var("MULTIHOST_DIR").unwrap());
    let key = Keypair::from_secret(unhex32(&std::env::var("MULTIHOST_KEY").unwrap()).unwrap());
    let coord_addr = std::env::var("MULTIHOST_COORD").unwrap();
    let coord_key = unhex32(&std::env::var("MULTIHOST_COORD_KEY").unwrap()).unwrap();
    let book_dir = Directory::even(MP_LAYOUT.total());
    let ids: Vec<ShardId> = MP_LAYOUT.shards_of(index).map(ShardId).collect();
    let rows = |s: ShardId| dir.join(format!("shard-{}.rows", s.0));
    let logp = |s: ShardId| dir.join(format!("shard-{}.log", s.0));
    let restarted = rows(ids[0]).exists();
    let node = if restarted {
        let mut attached = Vec::new();
        for s in &ids {
            let log = read_log(&logp(*s));
            let (agent, replies) =
                ShardAgent::attach(DurablePort::open(rows(*s)), *s, &book_dir, ROOT, &log)
                    .expect("attach");
            let committed: Vec<Round> = log[..replies.len()].to_vec();
            truncate_log(&logp(*s), &committed);
            let f = std::fs::OpenOptions::new()
                .append(true)
                .open(logp(*s))
                .unwrap();
            attached.push(Attached {
                agent,
                rounds: committed.into_iter().zip(replies).collect(),
                log: Some(Box::new(FileLog(f)) as Box<dyn RoundLog>),
            });
        }
        let replayed: usize = attached.iter().map(|a| a.rounds.len()).sum();
        std::fs::write(dir.join(format!("resumed-{index}")), replayed.to_string()).unwrap();
        Node::resume(index, MP_LAYOUT, cfg(), attached, SEED).expect("resume")
    } else {
        let ports = ids.iter().map(|s| DurablePort::open(rows(*s))).collect();
        let logs = ids
            .iter()
            .map(|s| {
                Some(Box::new(FileLog(std::fs::File::create(logp(*s)).unwrap()))
                    as Box<dyn RoundLog>)
            })
            .collect();
        Node::found(index, MP_LAYOUT, cfg(), ports, logs, None, ROOT, SEED).expect("found")
    };
    let addr = node
        .listen(key, "127.0.0.1:0".parse().unwrap(), b"test-book-mp")
        .expect("listen");
    // Written whole, then renamed: the parent waits for the file and must never read half of it.
    let tmp = dir.join(format!("addr-{index}.tmp"));
    std::fs::write(&tmp, addr.to_string()).unwrap();
    std::fs::rename(&tmp, dir.join(format!("addr-{index}"))).unwrap();
    node.add_peer(0, Some(coord_addr), coord_key);
    assert!(
        node.wait_stopped(Duration::from_secs(600)),
        "never told to stop"
    );
    node.stop();
}

/// The children, killed if the test fails before it stops them.
struct Children(Vec<Option<Child>>);

impl Drop for Children {
    fn drop(&mut self) {
        for c in self.0.iter_mut().flatten() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn spawn_child(
    index: u16,
    dir: &Path,
    key: &Keypair,
    coord: SocketAddr,
    coord_key: &[u8; 32],
) -> Child {
    Proc::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "multihost_child",
            "--nocapture",
            "--test-threads",
            "1",
        ])
        .env("MULTIHOST_INDEX", index.to_string())
        .env("MULTIHOST_DIR", dir)
        .env("MULTIHOST_KEY", key.secret_hex())
        .env("MULTIHOST_COORD", coord.to_string())
        .env("MULTIHOST_COORD_KEY", hex(coord_key))
        .spawn()
        .expect("a child process")
}

fn child_addr(dir: &Path, index: u16) -> String {
    let p = dir.join(format!("addr-{index}"));
    wait(|| p.exists(), "a child listens");
    std::fs::read_to_string(p).unwrap()
}

#[test]
fn nfr_21_hosts_as_processes_one_killed_and_restarted_under_load() {
    let base = std::env::temp_dir().join(format!("aledgent-multihost-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let dirs: Vec<PathBuf> = (0..MP_LAYOUT.nodes)
        .map(|h| base.join(format!("host-{h}")))
        .collect();
    for d in &dirs {
        std::fs::create_dir_all(d).unwrap();
    }
    let keys: Vec<Keypair> = (0..MP_LAYOUT.nodes).map(|_| Keypair::generate()).collect();
    // This process is host 0: the principals Agent and the router.
    let n0 = Node::found(
        0,
        MP_LAYOUT,
        cfg(),
        Vec::<NativePort>::new(),
        Vec::new(),
        Some(NativePort::new(DRIVER)),
        ROOT,
        SEED,
    )
    .unwrap();
    let coord = n0
        .listen(
            keys[0].clone(),
            "127.0.0.1:0".parse().unwrap(),
            b"test-book-mp",
        )
        .unwrap();
    let mut children = Children(vec![None]);
    for h in 1..MP_LAYOUT.nodes {
        children.0.push(Some(spawn_child(
            h,
            &dirs[usize::from(h)],
            &keys[usize::from(h)],
            coord,
            &keys[0].public,
        )));
    }
    let mut members = vec![Member {
        index: 0,
        addr: coord.to_string(),
        key: keys[0].public,
    }];
    for h in 1..MP_LAYOUT.nodes {
        members.push(Member {
            index: h,
            addr: child_addr(&dirs[usize::from(h)], h),
            key: keys[usize::from(h)].public,
        });
    }
    n0.set_roster(members.clone());
    connected(&n0, MP_LAYOUT.nodes);
    let b = setup(&n0, 24);
    // Kill host 2 (SIGKILL) a third of the way, and start it again from its durable Agents.
    let victim = 2u16;
    let (answered, _) = load(&n0, &b, 300, 13, |i| {
        if i == 4 {
            let mut c = children.0[usize::from(victim)].take().unwrap();
            c.kill().unwrap();
            c.wait().unwrap();
            let _ = std::fs::remove_file(dirs[usize::from(victim)].join(format!("addr-{victim}")));
            children.0[usize::from(victim)] = Some(spawn_child(
                victim,
                &dirs[usize::from(victim)],
                &keys[usize::from(victim)],
                coord,
                &keys[0].public,
            ));
            let addr = child_addr(&dirs[usize::from(victim)], victim);
            let mut m = members.clone();
            m[usize::from(victim)].addr = addr;
            n0.set_roster(m);
        }
    });
    let t = tally(&answered);
    assert!(t.get("Debited").copied().unwrap_or(0) > 100, "{t:?}");
    let reports = verify(&n0, &b, &answered);
    let replayed: usize =
        std::fs::read_to_string(dirs[usize::from(victim)].join(format!("resumed-{victim}")))
            .expect("the restarted host resumed from its Agents")
            .parse()
            .unwrap();
    assert!(replayed > 0, "it replayed its committed rounds");
    assert!(
        reports[&victim].net.sessions >= 2,
        "the restarted host joined its peers"
    );
    n0.shutdown_all();
    for c in children.0.iter_mut().flatten() {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            if c.try_wait().unwrap().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = c.kill();
    }
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn ingress_refuses_a_revoked_principal_on_every_host() {
    let layout = Layout {
        nodes: 2,
        first: 0,
        per_node: 2,
    };
    let nodes = mesh(layout);
    let n0 = &nodes[0];
    let b = setup(n0.as_ref(), 8);
    n0.principals(&[PrincipalsCommand::Revoke {
        principal: b.principals[0],
    }])
    .unwrap();
    wait(
        || n0.feed_everywhere() >= n0.feed_version(),
        "the revocation lands",
    );
    let r = n0.transfers(&[(Some(b.principals[0]), b.accts[0], b.accts[1], USD, 5)]);
    assert_eq!(r[0], Err(Refusal::NotAuthorised));
    assert_eq!(n0.take_revocation_lags().len(), 1);
    for n in nodes.iter().rev() {
        n.stop();
    }
}

/// Native decide across hosts (`F-034`, feature `native-decide`): every round decided natively and
/// replayed through its Agent; its credits, trims and audit record leave only once confirmed. The
/// same checks hold.
#[cfg(feature = "native-decide")]
#[test]
fn nfr_21_native_decide_across_hosts_exactly_once() {
    let layout = Layout {
        nodes: 3,
        first: 1,
        per_node: 2,
    };
    let nodes = mesh_with(
        layout,
        Config {
            native: true,
            round_wait: Duration::from_millis(5),
            ..cfg()
        },
    );
    let n0 = &nodes[0];
    let b = setup(n0.as_ref(), 24);
    let (answered, _) = load(n0.as_ref(), &b, 240, 14, |_| {});
    let t = tally(&answered);
    assert!(t.get("Debited").copied().unwrap_or(0) > 100, "{t:?}");
    let reports = verify(n0.as_ref(), &b, &answered);
    let replayed: usize = reports
        .values()
        .flat_map(|r| r.shards.iter())
        .map(|s| s.replay_us.len())
        .sum();
    assert!(replayed > 0, "rounds were confirmed by the replay");
    assert_eq!(n0.take_revocation_lags().len(), 1);
    for n in nodes.iter().rev() {
        n.stop();
    }
}
