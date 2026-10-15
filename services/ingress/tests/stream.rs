// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The core stream through ingress (`F-038`, `DEC-27`): core hosts dial out to ingress over mutual
//! TLS (or a Unix socket), ingress acknowledges with no core connected, feeds under a lease with an
//! epoch, signs every input, reads down the stream, keeps a copy of the verifiers it read, and
//! hands the feed to another instance on a go-away or a crash with nothing decided twice or out of
//! order.
//!
//! The core host's side runs in the test: an in-process core behind the host's sequencer
//! ([`Sequencer`]), dialing with [`ingress::stream::dial`]. A test proxy stands for a load
//! balancer's stable address (round-robin, TLS passed through), and can freeze to stand for an
//! instance that crashed without closing its connections.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use axum::Router;
use axum::http::StatusCode;
use ingress::boundary::Core;
use ingress::clock::SharedClock;
use ingress::delivery::Transport;
use ingress::feed::{BOOK, FeedKeys, FeedSigner, LocalFeedKey, Sequencer};
use ingress::inprocess::{InProcessCore, MemJournal};
use ingress::remote::LinkAuth;
use ingress::service::{Config, Replica, SharedFeedSigner};
use ingress::store::mem::MemStore;
use ingress::store::{Status, Store};
use ingress::stream::tls::TlsFiles;
use ingress::stream::{DialConfig, Listen, StreamCore, Target, Timings};
use ingress::testkit::{
    Deployment, PATIENCE, T0, TestCore, USD, call, fast_config, genesis, test_clerk,
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::task::JoinHandle;

// --- the test PKI, keys and timings ---

fn pki(file: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/stream")
        .join(file)
}

fn tls(name: &str) -> TlsFiles {
    TlsFiles {
        ca: pki("ca.pem"),
        cert: pki(&format!("{name}.pem")),
        key: pki(&format!("{name}.key")),
    }
}

/// Ingress's feed key in these tests: a test value.
fn ingress_key() -> LocalFeedKey {
    LocalFeedKey::from_seed([0x1F; 32])
}

fn timings() -> Timings {
    Timings {
        keepalive: Duration::from_millis(50),
        dead_after: Duration::from_millis(300),
        call_timeout: Duration::from_secs(20),
        hello_timeout: Duration::from_secs(2),
    }
}

fn config(name: &str, key: LocalFeedKey) -> Config {
    let mut c = fast_config(name);
    c.feed_signer = Some(SharedFeedSigner(Arc::new(key)));
    c
}

/// Wait for `f`, for at most [`PATIENCE`].
async fn until(what: &str, f: impl Fn() -> bool) {
    let start = Instant::now();
    while !f() {
        assert!(start.elapsed() < PATIENCE, "never: {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// --- ingress's side: an instance's listener ---

struct Instance {
    hub: StreamCore,
    addr: SocketAddr,
    _serve: JoinHandle<()>,
}

async fn instance(name: &str) -> Instance {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hub = StreamCore::new(name, timings());
    let listen = Listen::Tls {
        listener,
        acceptor: tls("ingress").acceptor().unwrap(),
    };
    let h = hub.clone();
    let serve = tokio::spawn(async move {
        let _ = h.serve(listen).await;
    });
    Instance {
        hub,
        addr,
        _serve: serve,
    }
}

// --- the core host's side ---

fn target(addr: SocketAddr, cert: &str) -> Target {
    Target::Tls {
        addr: addr.to_string(),
        server_name: rustls_pki_types::ServerName::try_from("ingress.test").unwrap(),
        connector: tls(cert).connector().unwrap(),
    }
}

struct Host {
    core: TestCore,
    seq: Arc<Sequencer<TestCore>>,
    founded: ledger_core::ledger::Founded,
}

fn host() -> Host {
    let core = InProcessCore::in_memory(genesis(T0), MemJournal::new()).with_clerk(test_clerk());
    core.start().unwrap();
    let founded = core.founded().unwrap();
    let keys = FeedKeys::of(&[ingress_key().public()]).unwrap();
    Host {
        seq: Arc::new(Sequencer::new(core.clone(), Some(keys))),
        core,
        founded,
    }
}

impl Host {
    /// Dial `targets`; dropping (aborting) the task closes every stream.
    fn dial(&self, targets: Vec<Target>) -> JoinHandle<()> {
        tokio::spawn(ingress::stream::dial(
            DialConfig {
                targets,
                core: "core-test".into(),
                partitions: vec![BOOK.into()],
                principals: true,
                timings: timings(),
            },
            Arc::clone(&self.seq),
        ))
    }

    fn inputs(&self) -> u64 {
        self.core.inputs().unwrap_or(0)
    }
}

/// A replica over `d`'s store, clock, authenticator and recipients, with `core`.
fn replica<C: Core + Clone>(
    d: &Deployment<MemStore, StreamCore>,
    core: C,
    config: Config,
) -> (Replica<MemStore, C>, Router) {
    let clock: SharedClock = Arc::new(d.clock.clone());
    let r = Replica::start(
        d.store.clone(),
        core,
        clock,
        d.auth.clone(),
        Arc::clone(&d.recipients) as Arc<dyn Transport>,
        config,
    );
    let router = ingress::surface::router(Arc::clone(&r.shared));
    (r, router)
}

/// A deployment whose replica's core is `a`'s stream, with the host already connected to it.
async fn deployment(h: &Host, a: &Instance, name: &str) -> Deployment<MemStore, StreamCore> {
    until("the core's stream", || a.hub.can_feed(BOOK)).await;
    let cfg = config(name, ingress_key());
    Deployment::assemble(
        MemStore::new(cfg.lease),
        cfg,
        a.hub.clone(),
        h.founded,
        MemJournal::new(),
    )
    .await
}

// --- a load balancer's stable address: round-robin, TLS passed through, and a freeze ---

struct Proxy {
    addr: SocketAddr,
    frozen: Arc<AtomicBool>,
    _task: JoinHandle<()>,
}

async fn pump(
    mut from: tokio::net::tcp::OwnedReadHalf,
    mut to: tokio::net::tcp::OwnedWriteHalf,
    frozen: Arc<AtomicBool>,
) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => {
                let _ = to.shutdown().await;
                return;
            }
            Ok(n) => n,
        };
        if frozen.load(Ordering::SeqCst) {
            // A crashed instance: nothing more crosses, and nothing closes.
            std::future::pending::<()>().await;
        }
        if to.write_all(&buf[..n]).await.is_err() {
            return;
        }
    }
}

async fn proxy(backends: Vec<SocketAddr>) -> Proxy {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let frozen = Arc::new(AtomicBool::new(false));
    let next = Arc::new(AtomicUsize::new(0));
    let f = Arc::clone(&frozen);
    let task = tokio::spawn(async move {
        loop {
            let Ok((inbound, _)) = listener.accept().await else {
                return;
            };
            let backend = backends[next.fetch_add(1, Ordering::SeqCst) % backends.len()];
            let f = Arc::clone(&f);
            tokio::spawn(async move {
                let Ok(outbound) = tokio::net::TcpStream::connect(backend).await else {
                    return;
                };
                let (ir, iw) = inbound.into_split();
                let (or, ow) = outbound.into_split();
                tokio::join!(pump(ir, ow, Arc::clone(&f)), pump(or, iw, f));
            });
        }
    });
    Proxy {
        addr,
        frozen,
        _task: task,
    }
}

// --- commands ---

fn transfer(from: &str, to: &str) -> Value {
    json!({ "legs": [{ "from": from, "to": to, "asset": USD, "amount": "1" }] })
}

/// The rows by feed sequence: gapless; the decided ones a prefix (none decided after one that is
/// not: the ticker may have logged a tick since); and their core input numbers strictly increasing,
/// so nothing was decided out of the received log's order. The store refuses a second row for one
/// input number, so none was decided twice.
async fn decided_in_feed_order(store: &MemStore) {
    let mut rows: Vec<_> = store
        .rows()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.feed_seq.is_some())
        .collect();
    rows.sort_by_key(|r| r.feed_seq);
    let seqs: Vec<u64> = rows.iter().filter_map(|r| r.feed_seq).collect();
    assert_eq!(
        seqs,
        (1..=seqs.len() as u64).collect::<Vec<_>>(),
        "gapless feed"
    );
    let decided = rows
        .iter()
        .take_while(|r| r.status == Status::Decided)
        .count();
    assert!(
        rows[decided..].iter().all(|r| r.status != Status::Decided),
        "a row decided after one that was not"
    );
    assert!(decided > 0);
    let numbers: Vec<u64> = rows[..decided]
        .iter()
        .filter_map(|r| r.core_input_seq)
        .collect();
    assert!(
        numbers.windows(2).all(|w| w[0] < w[1]),
        "decided in feed order: {numbers:?}"
    );
}

// --- the tests ---

/// `T-038-01`: a core that speaks no version this ingress speaks is refused before any call, and
/// one that does is welcomed with the version both speak.
#[tokio::test(flavor = "multi_thread")]
async fn t_038_01_a_core_speaking_no_common_version_is_refused() {
    let a = instance("ingress-a").await;
    for (versions, expect) in [
        (
            json!([99]),
            json!({ "greeting": "refused", "reason": "version" }),
        ),
        (
            json!([1, 99]),
            json!({ "greeting": "welcome", "version": 1, "instance": "ingress-a" }),
        ),
    ] {
        let tcp = tokio::net::TcpStream::connect(a.addr).await.unwrap();
        let name = rustls_pki_types::ServerName::try_from("ingress.test").unwrap();
        let mut s = tls("core")
            .connector()
            .unwrap()
            .connect(name, tcp)
            .await
            .unwrap();
        let hello = serde_json::to_vec(
            &json!({ "versions": versions, "core": "c", "partitions": ["book"] }),
        )
        .unwrap();
        s.write_all(&(hello.len() as u32).to_be_bytes())
            .await
            .unwrap();
        s.write_all(&hello).await.unwrap();
        let mut len = [0u8; 4];
        s.read_exact(&mut len).await.unwrap();
        let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
        s.read_exact(&mut body).await.unwrap();
        let greeting: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(greeting, expect);
    }
}

/// `T-038-02` (`NFR-7`, `DEC-27` (c)): a core host gets a stream only with a client certificate
/// from the operator's CA, and on a Unix socket only with the link's token and a listed user id;
/// a refused peer runs nothing.
#[tokio::test(flavor = "multi_thread")]
async fn t_038_02_only_an_authenticated_core_gets_a_stream() {
    let a = instance("ingress-a").await;
    let h = host();
    // A certificate from another CA: the handshake fails, nothing is served.
    let rogue = h.dial(vec![target(a.addr, "rogue")]);
    // A peer speaking the protocol without TLS at all.
    let mut raw = tokio::net::TcpStream::connect(a.addr).await.unwrap();
    let hello = br#"{"versions":[1],"core":"x","partitions":["book"]}"#;
    let _ = raw.write_all(&(hello.len() as u32).to_be_bytes()).await;
    let _ = raw.write_all(hello).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        a.hub.sessions().is_empty(),
        "no stream for an unauthenticated core"
    );
    rogue.abort();
    let honest = h.dial(vec![target(a.addr, "core")]);
    until("the honest core's stream", || a.hub.sessions().len() == 1).await;
    honest.abort();

    // The same-host transport: the peer's user id and the token.
    let dir = tempfile::tempdir().unwrap();
    let token = ingress::remote::LinkToken::new(&"ab".repeat(32)).unwrap();
    for (uids, presented, admitted) in [
        (vec![], "cd".repeat(32), false),
        (vec![u32::MAX - 7], "ab".repeat(32), false),
        (vec![], "ab".repeat(32), true),
    ] {
        let path = dir.path().join(format!("s-{admitted}-{}.sock", uids.len()));
        let hub = StreamCore::new("ingress-unix", timings());
        let listen = Listen::Unix {
            listener: tokio::net::UnixListener::bind(&path).unwrap(),
            auth: LinkAuth::new(&[token.sha256_hex()], &uids).unwrap(),
        };
        let h2 = hub.clone();
        let serve = tokio::spawn(async move {
            let _ = h2.serve(listen).await;
        });
        let dial = h.dial(vec![Target::Unix {
            path: path.clone(),
            peer_uids: vec![my_uid()],
            token: ingress::remote::LinkToken::new(&presented).unwrap(),
        }]);
        if admitted {
            until("the socket's stream", || hub.sessions().len() == 1).await;
        } else {
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert!(hub.sessions().is_empty(), "refused: {uids:?}");
        }
        dial.abort();
        serve.abort();
    }
    assert_eq!(h.inputs(), 0, "nothing ran");
}

/// This process's user id, from a socket pair's peer credentials.
fn my_uid() -> u32 {
    let (a, _b) = tokio::net::UnixStream::pair().unwrap();
    a.peer_cred().unwrap().uid()
}

/// `T-038-02` (`DEC-27` (h′), `REQ-AU-8`): on a Unix socket the core host checks the peer's user
/// id against ingress's before it sends its token. A socket served by a process of another user
/// id (a path taken over) gets a connection and nothing on it: no hello, no token; ingress's own
/// user id gets the hello.
#[tokio::test(flavor = "multi_thread")]
async fn t_038_02_the_core_host_sends_its_token_only_to_ingress_s_user_id() {
    let h = host();
    let dir = tempfile::tempdir().unwrap();
    let secret = "ab".repeat(32);
    let me = my_uid();
    for (uids, sent) in [(vec![me.wrapping_add(1)], false), (vec![me], true)] {
        let path = dir.path().join(format!("taken-{sent}.sock"));
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let dial = h.dial(vec![Target::Unix {
            path,
            peer_uids: uids.clone(),
            token: ingress::remote::LinkToken::new(&secret).unwrap(),
        }]);
        let (mut s, _) = tokio::time::timeout(PATIENCE, listener.accept())
            .await
            .unwrap()
            .unwrap();
        let mut got = Vec::new();
        let _ = tokio::time::timeout(Duration::from_millis(500), s.read_to_end(&mut got)).await;
        if sent {
            assert!(
                String::from_utf8_lossy(&got).contains(&secret),
                "the hello carries the token"
            );
        } else {
            assert!(got.is_empty(), "nothing sent to user ids {uids:?}");
        }
        dial.abort();
    }
    assert_eq!(h.inputs(), 0, "nothing ran");
}

/// `DEC-27` (b), (e′), (e″), (g); `T-038-06`, `T-038-09`, `T-038-10`, `T-038-12`: commands are decided
/// and reads answered over the stream; with no core connected, a command that moves value is
/// acknowledged and logged, a command answered with its decision gets `ERR-DecisionUnavailable`
/// within `NFR-16`, an API key the book admitted authenticates from the durable copy and one it
/// never admitted does not; once a core connects, everything logged is decided.
#[tokio::test(flavor = "multi_thread")]
async fn dec_27_b_g_with_no_core_ingress_acknowledges_and_answers_decision_unavailable() {
    let a = instance("ingress-a").await;
    let h = host();
    let dial = h.dial(vec![target(a.addr, "core")]);
    let d = deployment(&h, &a, "ingress-a").await;
    let alice = d.party("alice", 100).await;
    let bob = d.party("bob", 0).await;
    let (from, to) = (alice.account.clone().unwrap(), bob.account.clone().unwrap());

    // Reads ride the stream: a query, a credential, a tie.
    let custody = format!("/v1/accounts/{from}/custody");
    let (s, v) = d.call("GET", &custody, Some(&alice.token), &[], None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(
        a.hub
            .credential(&d.operator.credential)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(a.hub.party_of(&alice.identity).await, Ok(None));

    // An API key, used once while the core answers: the copy keeps its verifier.
    let (s, v) = d
        .call(
            "POST",
            "/v1/credentials",
            Some(&alice.token),
            &[("client-key", "ak")],
            Some(json!({ "marker": "ordinary", "scheme": "api-key" })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let key = v["api_key"].as_str().unwrap().to_owned();
    let credential = v["result"]["credential"].as_str().unwrap().to_owned();
    let (s, v) = d.call("GET", &custody, Some(&key), &[], None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let store = d.store.clone();
    let start = Instant::now();
    while store.copied_verifier(&credential).await.unwrap().is_none() {
        assert!(start.elapsed() < PATIENCE, "the copy is written");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // The core goes away.
    dial.abort();
    until("the stream closes", || !a.hub.can_feed(BOOK)).await;
    let shared = Arc::clone(&d.replica.shared);
    until("no live feed lease", || {
        !shared.feed_live.load(Ordering::SeqCst)
    })
    .await;
    d.auth.verifiers().set_ttl(Duration::ZERO).unwrap();

    // Acknowledged, with the API key from the copy.
    let started = Instant::now();
    let (s, v) = d
        .call(
            "POST",
            "/v1/transfers",
            Some(&key),
            &[("client-key", "t-1")],
            Some(transfer(&from, &to)),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(v["state"], "Received");
    // An API key the book never admitted is not accepted for its form alone.
    let forged = authn::token::ApiKeySecret::generate().unwrap().key_for(
        &ledger_core::id::CredentialId::mint(
            &ledger_core::id::OperationId::from_ref(ledger_kernel::posting::OperationRef(
                [0x99; 16],
            )),
            0,
        ),
    );
    let (s, v) = d
        .call(
            "POST",
            "/v1/transfers",
            Some(forged.expose()),
            &[("client-key", "t-forged")],
            Some(transfer(&from, &to)),
        )
        .await;
    assert!(!s.is_success(), "{s} {v}");
    // A command answered with its decision: ERR-DecisionUnavailable within NFR-16, still logged.
    let open = json!({ "owner": alice.identity, "kind": "customer", "custody": "delegated" });
    let started = Instant::now();
    let (s, v) = d
        .call(
            "POST",
            "/v1/accounts",
            Some(&d.operator.token),
            &[("client-key", "acct-late")],
            Some(open.clone()),
        )
        .await;
    let took = started.elapsed();
    assert_eq!(v["error"], "ERR-DecisionUnavailable", "{s} {v}");
    assert!(
        took < Duration::from_millis(1_500),
        "within NFR-16: {took:?}"
    );
    assert!(d.store.oldest_undecided().await.unwrap().is_some(), "kept");
    let inputs = h.inputs();

    // A core connects: everything logged is decided, and the retry gets its decision.
    let dial = h.dial(vec![target(a.addr, "core")]);
    assert!(d.quiesce(PATIENCE).await, "every row decided");
    assert!(h.inputs() > inputs);
    let (s, v) = d
        .call(
            "POST",
            "/v1/accounts",
            Some(&d.operator.token),
            &[("client-key", "acct-late")],
            Some(open),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    decided_in_feed_order(&d.store).await;
    dial.abort();
}

/// `DEC-27` (e), `T-038-08`: an instance that drains releases the feed and sends its core away; the
/// core dials the stable address again and lands on another instance, which takes the feed under a
/// new epoch within a second. Commands sent to both instances throughout are acknowledged, and none
/// is decided twice or out of the received log's order.
#[tokio::test(flavor = "multi_thread")]
async fn dec_27_e_a_go_away_hands_the_feed_over_with_nothing_decided_twice_or_out_of_order() {
    let a = instance("ingress-a").await;
    let b = instance("ingress-b").await;
    let lb = proxy(vec![a.addr, b.addr]).await;
    let h = host();
    let dial = h.dial(vec![target(lb.addr, "core")]);
    let d = deployment(&h, &a, "ingress-a").await;
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 0).await;
    let (from, to) = (alice.account.clone().unwrap(), bob.account.clone().unwrap());
    let (rb, router_b) = replica(&d, b.hub.clone(), config("ingress-b", ingress_key()));
    let first = d.replica.shared.feeds().expect("a feeds");

    let owned = (alice.token.clone(), from.clone(), to.clone());
    let send = move |router: Router, i: usize| {
        let (token, from, to) = owned.clone();
        async move {
            let (s, v) = call(
                &router,
                "POST",
                "/v1/transfers",
                Some(&token),
                &[("client-key", &format!("t-{i}"))],
                Some(transfer(&from, &to)),
            )
            .await;
            assert_eq!(s, StatusCode::ACCEPTED, "{v}");
        }
    };
    for i in 0..20 {
        let r = if i % 2 == 0 {
            d.router.clone()
        } else {
            router_b.clone()
        };
        send(r, i).await;
    }
    // A drains: no new feed, the lease released, its core sent away.
    let went = Instant::now();
    ingress::service::drain(&d.replica.shared, Duration::from_secs(5)).await;
    a.hub.go_away("release");
    // Commands keep coming to both instances while the feed moves.
    let during = tokio::spawn({
        let (a_router, b_router) = (d.router.clone(), router_b.clone());
        let send = send.clone();
        async move {
            for i in 20..40 {
                let r = if i % 2 == 0 {
                    a_router.clone()
                } else {
                    b_router.clone()
                };
                send(r, i).await;
            }
        }
    });
    until("b feeds", || rb.shared.feeds().is_some()).await;
    let took = went.elapsed();
    assert!(
        took < Duration::from_secs(1),
        "decisions resumed in {took:?}"
    );
    during.await.unwrap();
    assert!(rb.shared.feeds().unwrap() > first, "a new epoch");
    assert!(d.quiesce(PATIENCE).await, "every row decided");
    if let Err((numbers, n)) = d
        .gapless_inputs(|| h.core.inputs(), Duration::from_secs(10))
        .await
    {
        panic!("rows hold {numbers:?} against the core's {n} inputs");
    }
    decided_in_feed_order(&d.store).await;
    dial.abort();
}

/// `DEC-27` (d), `T-038-04`: after a go-away hands the feed to another instance under a higher
/// epoch, the old feeder's next item (a fresh input, signed with ingress's key, at the next sequence
/// the old feeder would use) is refused `Fenced` by the core host's sequencer itself, and the
/// Agent's core runs nothing. A fresh input is one the core's reply log cannot absorb as a replay:
/// only the epoch fence stops it.
#[tokio::test(flavor = "multi_thread")]
async fn dec_27_d_a_fenced_feeder_is_refused_by_the_core_host_itself() {
    use ingress::boundary::{CoreInput, InputBody, InputKey};
    use ingress::clock::Clock as _;
    use ingress::feed::{Feed, FeedReply};
    // Replicas whose tickers stay quiet, so the core's input count moves only with the test.
    let quiet = |name: &str| {
        let mut c = config(name, ingress_key());
        c.tick_interval = Duration::from_secs(365 * 86_400);
        c
    };
    let a = instance("ingress-a").await;
    let b = instance("ingress-b").await;
    let lb = proxy(vec![a.addr, b.addr]).await;
    let h = host();
    let dial = h.dial(vec![target(lb.addr, "core")]);
    let mut d = deployment(&h, &a, "ingress-a").await;
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 0).await;
    let (from, to) = (alice.account.clone().unwrap(), bob.account.clone().unwrap());
    d.replica.kill();
    let (ra, router_a) = d.replica_on(d.store.clone(), quiet("ingress-a2"));
    d.replica = ra;
    d.router = router_a;
    until("a feeds again", || d.replica.shared.feeds().is_some()).await;
    let (rb, router_b) = replica(&d, b.hub.clone(), quiet("ingress-b"));
    let old = d.replica.shared.feeds().expect("a feeds");
    let transfer_on = |router: Router, key: String| {
        let (token, from, to) = (alice.token.clone(), from.clone(), to.clone());
        async move {
            let (s, v) = call(
                &router,
                "POST",
                "/v1/transfers",
                Some(&token),
                &[("client-key", &key)],
                Some(transfer(&from, &to)),
            )
            .await;
            assert_eq!(s, StatusCode::ACCEPTED, "{v}");
        }
    };
    for i in 0..5 {
        transfer_on(d.router.clone(), format!("f-{i}")).await;
    }
    assert!(d.quiesce(PATIENCE).await, "a's rows decided");
    // The next sequence the old feeder would give a row of its own.
    let old_next = d
        .store
        .rows()
        .await
        .unwrap()
        .iter()
        .filter_map(|r| r.feed_seq)
        .max()
        .unwrap()
        + 1;
    ingress::service::drain(&d.replica.shared, Duration::from_secs(5)).await;
    a.hub.go_away("release");
    until("b feeds", || rb.shared.feeds().is_some()).await;
    let new = rb.shared.feeds().unwrap();
    assert!(new > old, "a new epoch");
    for i in 5..10 {
        transfer_on(router_b.clone(), format!("f-{i}")).await;
    }
    assert!(d.quiesce(PATIENCE).await, "b's rows decided");
    assert!(
        d.all_delivered(PATIENCE).await,
        "every notification delivered"
    );
    assert!(d.quiesce(PATIENCE).await, "their records decided");
    let inputs = h.inputs();
    let fresh = CoreInput {
        key: InputKey([0xEE; 16]),
        body: InputBody::Tick { now: d.clock.now() },
    };
    let stale = Feed::build(
        BOOK,
        old,
        &[(old_next, &fresh)],
        Some(&ingress_key() as &dyn FeedSigner),
    )
    .unwrap();
    assert_eq!(
        h.seq.feed(&stale).await,
        FeedReply::Fenced { epoch: new },
        "the core host refuses the old feeder"
    );
    assert_eq!(h.inputs(), inputs, "the core ran nothing");
    dial.abort();
}

/// `DEC-27` (e), `T-038-07`: an instance that crashes without closing its connections is noticed by
/// keepalive within `dead_after`, its lease runs out, and another instance the core reaches takes
/// the feed; acknowledgements never pause, and nothing is decided twice or out of order.
#[tokio::test(flavor = "multi_thread")]
async fn dec_27_e_a_crashed_instance_is_noticed_by_keepalive_and_its_feed_moves() {
    let a = instance("ingress-a").await;
    let b = instance("ingress-b").await;
    let to_a = proxy(vec![a.addr]).await;
    let h = host();
    let dial = h.dial(vec![target(to_a.addr, "core"), target(b.addr, "core")]);
    let mut d = deployment(&h, &a, "ingress-a").await;
    until("b's stream", || b.hub.can_feed(BOOK)).await;
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 0).await;
    let (from, to) = (alice.account.clone().unwrap(), bob.account.clone().unwrap());
    let (rb, router_b) = replica(&d, b.hub.clone(), config("ingress-b", ingress_key()));
    assert!(d.replica.shared.feeds().is_some() && rb.shared.feeds().is_none());

    // A crashes: its process stops, its connections stay open and silent.
    to_a.frozen.store(true, Ordering::SeqCst);
    d.replica.kill();
    for i in 0..20 {
        let started = Instant::now();
        let (s, v) = call(
            &router_b,
            "POST",
            "/v1/transfers",
            Some(&alice.token),
            &[("client-key", &format!("c-{i}"))],
            Some(transfer(&from, &to)),
        )
        .await;
        assert_eq!(s, StatusCode::ACCEPTED, "{v}");
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "acknowledged at once"
        );
    }
    until("b feeds", || rb.shared.feeds().is_some()).await;
    // A's side of the silent stream is dropped by keepalive.
    let noticed = Instant::now();
    until("a's stream dropped", || a.hub.sessions().is_empty()).await;
    assert!(noticed.elapsed() < timings().dead_after * 4);
    assert!(d.quiesce(PATIENCE).await, "every row decided");
    decided_in_feed_order(&d.store).await;
    dial.abort();
}

/// `DEC-27` (f), `T-038-05`: a replica whose feed key is not one the core host accepts forwards
/// nothing the core runs: its feeds are refused whole, the core's input count does not move, and
/// the rows stay undecided until a replica with ingress's key takes the feed.
#[tokio::test(flavor = "multi_thread")]
async fn dec_27_f_a_forwarded_command_without_a_valid_signature_never_reaches_the_core() {
    let a = instance("ingress-a").await;
    let h = host();
    let dial = h.dial(vec![target(a.addr, "core")]);
    let mut d = deployment(&h, &a, "ingress-a").await;
    let alice = d.party("alice", 100).await;
    let bob = d.party("bob", 0).await;
    let (from, to) = (alice.account.clone().unwrap(), bob.account.clone().unwrap());
    d.replica.kill();
    let forger = LocalFeedKey::from_seed([0x66; 32]);
    let (rx, router_x) = replica(&d, a.hub.clone(), config("ingress-x", forger));
    until("x feeds", || rx.shared.feeds().is_some()).await;
    // Whatever the killed replica had already sent has landed by now.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let before = h.inputs();
    let (s, v) = call(
        &router_x,
        "POST",
        "/v1/transfers",
        Some(&alice.token),
        &[("client-key", "unsigned")],
        Some(transfer(&from, &to)),
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(h.inputs(), before, "nothing reached the core");
    let row = || async {
        d.store
            .find(
                &ingress::store::principal_domain(&alice.identity),
                ledger_core::lifecycle::keys::Surface::Command,
                "unsigned",
            )
            .await
            .unwrap()
            .unwrap()
    };
    assert_ne!(row().await.status, Status::Decided);
    drop(rx);
    let (_ry, _) = replica(&d, a.hub.clone(), config("ingress-y", ingress_key()));
    let start = Instant::now();
    while row().await.status != Status::Decided {
        assert!(start.elapsed() < PATIENCE, "decided once signed");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(h.inputs() > before);
    dial.abort();
}

/// `T-038-15`: during the transition both links feed one core through one sequencer: a replica
/// on the socket and a replica on the stream, taking the lease in turn, decide every command once
/// and in order.
#[tokio::test(flavor = "multi_thread")]
async fn t_038_15_the_socket_and_the_stream_feed_in_turn() {
    let a = instance("ingress-a").await;
    let h = host();
    let dial = h.dial(vec![target(a.addr, "core")]);
    let mut d = deployment(&h, &a, "ingress-a").await;
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 0).await;
    let (from, to) = (alice.account.clone().unwrap(), bob.account.clone().unwrap());
    // The node's ingress: the socket link to the same host's sequencer.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = listener.local_addr().unwrap().to_string();
    let token = ingress::remote::LinkToken::new(&"ab".repeat(32)).unwrap();
    let auth = LinkAuth::new(&[token.sha256_hex()], &[]).unwrap();
    let served = tokio::spawn(ingress::remote::serve_tcp(
        listener,
        Arc::clone(&h.seq),
        auth,
    ));
    let socket = ingress::remote::RemoteCore::new(
        ingress::remote::Endpoint::Tcp(at),
        Duration::from_secs(20),
        token,
    );
    d.replica.kill();
    let (rs, router_s) = replica(&d, socket, config("ingress-node", ingress_key()));
    until("the socket replica feeds", || rs.shared.feeds().is_some()).await;
    let send = |router: Router, i: usize| {
        let (token, from, to) = (alice.token.clone(), from.clone(), to.clone());
        async move {
            let (s, v) = call(
                &router,
                "POST",
                "/v1/transfers",
                Some(&token),
                &[("client-key", &format!("m-{i}"))],
                Some(transfer(&from, &to)),
            )
            .await;
            assert_eq!(s, StatusCode::ACCEPTED, "{v}");
        }
    };
    for i in 0..15 {
        send(router_s.clone(), i).await;
    }
    // The stream replica starts; the node's ingress stops: the stream takes the feed.
    let (rt, router_t) = replica(&d, a.hub.clone(), config("ingress-fleet", ingress_key()));
    for i in 15..30 {
        send(router_t.clone(), i).await;
    }
    drop(rs);
    until("the fleet feeds", || rt.shared.feeds().is_some()).await;
    assert!(d.quiesce(PATIENCE).await, "every row decided");
    if let Err((numbers, n)) = d
        .gapless_inputs(|| h.core.inputs(), Duration::from_secs(10))
        .await
    {
        panic!("rows hold {numbers:?} against the core's {n} inputs");
    }
    decided_in_feed_order(&d.store).await;
    served.abort();
    dial.abort();
}

fn hex_sig(sig: &cipher_clerk::crypto::Signature) -> String {
    let mut b = [0u8; 64];
    b[..32].copy_from_slice(&sig.r);
    b[32..].copy_from_slice(&sig.s);
    hex::encode(b)
}

fn secret(n: u8) -> cipher_clerk::crypto::SecretKey {
    let mut b = [n; 32];
    b[31] = 0;
    cipher_clerk::crypto::SecretKey::from_bytes(b).unwrap()
}

/// `DEC-27` (e″), `T-038-11`: a reclaim's `key` proof is checked at the door, against the
/// credential's verifier, before the command is logged: a forged or mismatched one is refused
/// `ERR-CredentialInvalid` and never reaches the received log; a good one is logged and decided,
/// and its repeat answered from its row.
#[tokio::test(flavor = "multi_thread")]
async fn dec_27_e2_a_forged_key_proof_is_refused_at_the_door() {
    use ledger_core::custody::reclaim_challenge;
    let d = Deployment::start().await;
    let alice = d.party("alice", 100).await;
    let a = alice.account.clone().unwrap();
    let token = Some(alice.token.as_str());
    let sk = secret(3);
    let (s, r) = d
        .call(
            "POST",
            "/v1/credentials",
            token,
            &[],
            Some(json!({ "marker": "ordinary", "verifier": hex::encode(sk.public_key().0) })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let credential = r["result"]["credential"].as_str().unwrap().to_owned();
    let (_, v) = d
        .call(
            "GET",
            &format!("/v1/accounts/{a}/custody"),
            token,
            &[],
            None,
        )
        .await;
    let epoch = v["epoch"].as_u64().unwrap();
    let (_, m) = d
        .call(
            "GET",
            &format!("/v1/accounts/{a}/recovery-mechanisms"),
            token,
            &[],
            None,
        )
        .await;
    let account = ledger_core::id::AccountId::parse(&a).unwrap();
    let ring = ledger_kernel::keys::RootKeys::new([1; 32]);
    let reclaim = format!("/v1/accounts/{a}/custody/reclaim");
    // Commands logged (ticks may be logged meanwhile; they are not counted).
    let commands = || async {
        d.store
            .rows()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.new.kind == ingress::store::InputKind::Command)
            .count()
    };
    let before = commands().await;
    let forged = [
        ring.sign(&sk, &reclaim_challenge(&account, epoch + 1)),
        ring.sign(&secret(4), &reclaim_challenge(&account, epoch)),
    ];
    for (i, f) in forged.iter().enumerate() {
        let (s, r) = d
            .call(
                "POST",
                &reclaim,
                token,
                &[("client-key", &format!("forged-{i}"))],
                Some(json!({
                    "proof_credential": credential,
                    "signature": hex_sig(f),
                    "acknowledged": m["digest"],
                })),
            )
            .await;
        assert_eq!(r["error"], "ERR-CredentialInvalid", "{s} {r}");
    }
    assert_eq!(commands().await, before, "nothing logged");
    let good = ring.sign(&sk, &reclaim_challenge(&account, epoch));
    let body = json!({
        "proof_credential": credential,
        "signature": hex_sig(&good),
        "acknowledged": m["digest"],
    });
    for _ in 0..2 {
        // The second is a repeat, answered from its row: not checked again, though the epoch has
        // moved on since.
        let (s, r) = d
            .call(
                "POST",
                &reclaim,
                token,
                &[("client-key", "good")],
                Some(body.clone()),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{r}");
    }
}

/// `T-038-13` (`NFR-5`, `NFR-17`): `/ready` answers 200 while the replica serves and 503 once it
/// drains; `/live` answers 200 throughout; neither carries a ledger fact; the surface keeps
/// answering while it drains.
#[tokio::test(flavor = "multi_thread")]
async fn t_038_13_readiness_turns_on_drain() {
    let d = Deployment::start().await;
    let ready = ingress::readiness::router(Arc::clone(&d.replica.shared));
    let (s, v) = call(&ready, "GET", "/ready", None, &[], None).await;
    assert_eq!((s, v), (StatusCode::OK, json!("ready")));
    d.replica.drain(Duration::from_secs(5)).await;
    let (s, _) = call(&ready, "GET", "/ready", None, &[], None).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    let (s, v) = call(&ready, "GET", "/live", None, &[], None).await;
    assert_eq!((s, v), (StatusCode::OK, json!("live")));
    let (s, _) = d
        .call("GET", "/openapi.json", Some(&d.operator.token), &[], None)
        .await;
    assert_eq!(s, StatusCode::OK);
}
