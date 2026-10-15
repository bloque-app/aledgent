// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-038` §12 step 0: this ingress, configured as a running deployment is before its core host
//! is rebuilt, against a core host of the release before `F-038`.
//!
//! The node never updates its core host in place (its image carries the genesis package's
//! bytes), so a release of ingress meets the core host it already had. That host speaks the
//! socket link of `F-035` and nothing else: the requests below, each one framed and answered in
//! turn after the link's token, and `unavailable` for any request it cannot read ("did not run").
//! [`old_host`] is that host: its connection loop is the earlier release's (the token checked
//! by [`LinkAuth`], then one frame in, one frame out), and it reads only that release's
//! requests, by their `op` tag, answering each through [`ingress::remote::answer`], whose arms for
//! them are unchanged; anything else, `feed` included, is answered `unavailable` without running.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use ingress::boundary::Core;
use ingress::config::DeploymentConfig;
use ingress::inprocess::{InProcessCore, MemJournal};
use ingress::remote::{Endpoint, Hello, LinkAuth, LinkToken, RemoteCore, Request, Response};
use ingress::service::ForwardLink;
use ingress::store::mem::MemStore;
use ingress::store::{InputKind, Status, Store};
use ingress::testkit::{Deployment, T0, TestCore, USD, fast_config, genesis, test_clerk};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

/// The requests a core host of the release before `F-038` reads (`ingress::remote::Request` as
/// it was): their `op` tags. `feed` is not one of them.
const EARLIER_OPS: [&str; 9] = [
    "apply",
    "apply_batch",
    "outbox",
    "last_tick",
    "next_due",
    "query",
    "credential",
    "party_of",
    "genesis_open",
];

/// The link's test token: a test value, never a default of a production crate.
fn token() -> LinkToken {
    LinkToken::new(&"5e".repeat(32)).unwrap()
}

/// Every request the old host was sent, by `op` tag, and how many it could not read.
#[derive(Default)]
struct Seen {
    ops: BTreeMap<String, u64>,
    unread: u64,
}

async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let mut buf = vec![0u8; u32::from_be_bytes(len) as usize];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, bytes: &[u8]) -> std::io::Result<()> {
    w.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    w.write_all(bytes).await?;
    w.flush().await
}

/// One connection to the old host: the token first, then each request answered in turn.
async fn old_connection(
    core: Arc<TestCore>,
    auth: Arc<LinkAuth>,
    seen: Arc<Mutex<Seen>>,
    uid: Option<u32>,
    mut s: tokio::net::UnixStream,
) {
    let hello = match read_frame(&mut s).await {
        Ok(frame) => serde_json::from_slice::<Hello>(&frame).ok(),
        Err(_) => None,
    };
    if !hello.is_some_and(|h| auth.admits(uid, &h.token)) {
        let bytes = serde_json::to_vec(&Response::Unauthenticated).unwrap();
        let _ = write_frame(&mut s, &bytes).await;
        return;
    }
    loop {
        let Ok(frame) = read_frame(&mut s).await else {
            return;
        };
        let op = serde_json::from_slice::<Value>(&frame)
            .ok()
            .and_then(|v| v.get("op").and_then(Value::as_str).map(str::to_owned))
            .unwrap_or_default();
        let known = EARLIER_OPS.contains(&op.as_str());
        {
            let mut seen = seen.lock().unwrap();
            *seen.ops.entry(op).or_default() += 1;
            if !known {
                seen.unread += 1;
            }
        }
        let response = match serde_json::from_slice::<Request>(&frame) {
            Ok(request) if known => ingress::remote::answer(&*core, request).await,
            // A request the core cannot read did not run.
            _ => Response::Unavailable,
        };
        let bytes = serde_json::to_vec(&response).unwrap();
        if write_frame(&mut s, &bytes).await.is_err() {
            return;
        }
    }
}

/// The core host of the release before `F-038`, on a Unix socket at `path` (module
/// documentation).
async fn old_host(path: &Path, core: Arc<TestCore>, uid: u32, seen: Arc<Mutex<Seen>>) {
    let auth = Arc::new(LinkAuth::new(&[token().sha256_hex()], &[uid]).unwrap());
    let listener = tokio::net::UnixListener::bind(path).unwrap();
    loop {
        let Ok((s, _)) = listener.accept().await else {
            return;
        };
        let uid = s.peer_cred().ok().map(|c| c.uid());
        tokio::spawn(old_connection(
            Arc::clone(&core),
            Arc::clone(&auth),
            Arc::clone(&seen),
            uid,
            s,
        ));
    }
}

/// The deployment file of a running deployment before its core host is rebuilt: the template's,
/// with the production profile, `core_endpoint` on a Unix socket, no `stream` section and so no
/// feed key, exactly the shape the test environment runs.
fn running_deployment_config(socket: &Path) -> DeploymentConfig {
    let text = include_str!("../../../deploy/templates/ingress.json");
    let mut filled = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("\"<FILL-NUMBER") {
        filled.push_str(&rest[..at]);
        let end = rest[at + 1..].find('"').map(|e| at + e + 2).unwrap();
        filled.push('1');
        rest = &rest[end..];
    }
    filled.push_str(rest);
    let mut v: Value = serde_json::from_str(&filled).unwrap();
    v["profile"] = "production".into();
    v["core_endpoint"] = format!("unix:{}", socket.display()).into();
    v["stream"] = Value::Null;
    serde_json::from_value(v).unwrap()
}

/// `F-038` §12 step 0 (`NFR-17`, `REQ-CS-7`, `REQ-CS-8`): this ingress, with a running
/// deployment's configuration (production, `core_endpoint` on a Unix socket, no `stream`, no
/// feed key), passes its start-up checks on the legacy link and, against a core host that speaks
/// only the link of the release before `F-038` and answers `unavailable` to a feed, acknowledges
/// commands, gets the decision of every command answered with its decision, decides what it
/// acknowledged, and ticks. The old host is never sent a request it cannot read.
#[tokio::test(flavor = "multi_thread")]
async fn f_038_s12_a_new_ingress_runs_against_a_core_host_of_the_earlier_release() {
    use std::os::unix::fs::MetadataExt as _;
    let dir = tempfile::tempdir().unwrap();
    let probe = dir.path().join(".uid");
    std::fs::write(&probe, b"").unwrap();
    let me = std::fs::metadata(&probe).unwrap().uid();
    let socket = dir.path().join("core.sock");

    // It starts: the start-up checks `serve` runs pass, and the link is the legacy one.
    let c = running_deployment_config(&socket);
    assert!(c.stream.is_none());
    c.check_forward_signing()
        .expect("a production ingress on the legacy socket link needs no feed key");
    assert_eq!(c.forward_link(), ForwardLink::Legacy);
    let mut config = c.service();
    assert_eq!(config.link, ForwardLink::Legacy);
    assert!(config.feed_signer.is_none());
    // The test's own intervals; the link stays the deployment's.
    let fast = fast_config(&config.replica);
    config.poll = fast.poll;
    config.tick_interval = fast.tick_interval;
    config.tick_heartbeat = fast.tick_heartbeat;
    config.lease = fast.lease;
    config.delivery_interval = fast.delivery_interval;
    config.retry_base = fast.retry_base;
    config.retry_max = fast.retry_max;
    config.genesis_gate = false;

    // The core host of the earlier release.
    let inner = InProcessCore::in_memory(genesis(T0), MemJournal::new()).with_clerk(test_clerk());
    inner.start().unwrap();
    let founded = inner.founded().unwrap();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let host = tokio::spawn({
        let socket = socket.clone();
        let seen = Arc::clone(&seen);
        let inner = Arc::new(inner);
        async move { old_host(&socket, inner, me, seen).await }
    });
    let start = Instant::now();
    while !socket.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "the old host listens"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let endpoint = Endpoint::parse(c.core_endpoint.as_deref().unwrap()).unwrap();
    let core = RemoteCore::new(endpoint, Duration::from_secs(60), token());

    // Assembling the deployment takes a grant, a command answered with its decision.
    let d = Deployment::assemble(
        MemStore::new(config.lease),
        config,
        core,
        founded,
        MemJournal::new(),
    )
    .await;
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 0).await;

    // A transfer is acknowledged, then decided; a repeat under its client key gets the decision.
    let body = json!({ "legs": [{ "from": alice.account, "to": bob.account, "asset": USD, "amount": "7" }] });
    let (s, r) = d
        .call(
            "POST",
            "/v1/transfers",
            Some(&alice.token),
            &[("client-key", "legacy-1")],
            Some(body.clone()),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{r}");
    let (s, r) = d
        .call_decided(
            "POST",
            "/v1/transfers",
            Some(&alice.token),
            &[("client-key", "legacy-1")],
            Some(body),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "the transfer's decision: {r}");

    // Ticks: the ticker logs them and the old host applies them, at the ingress clock's time.
    d.clock.set(T0 + 50_000);
    let start = Instant::now();
    while d.core.last_tick().await != Ok(Some(T0 + 50_000)) {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "the old host applies a tick at the clock's time"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        d.quiesce(Duration::from_secs(30)).await,
        "every row decided"
    );
    let rows = d.store.rows().await.unwrap();
    assert!(rows.iter().all(|r| r.status == Status::Decided));
    assert!(rows.iter().any(|r| r.new.kind == InputKind::Tick));
    // The legacy link: no row was given a feed sequence, and no feed lease was taken.
    assert!(
        rows.iter().all(|r| r.feed_seq.is_none()),
        "no feed sequence"
    );
    assert_eq!(d.replica.shared.feeds(), None, "no feed lease");

    // The old host was sent only what it reads: no feed, nothing unavailable for want of reading.
    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.unread, 0,
        "requests the old host cannot read: {:?}",
        seen.ops
    );
    assert!(!seen.ops.contains_key("feed"), "{:?}", seen.ops);
    assert!(
        seen.ops.get("apply_batch").copied().unwrap_or(0) > 0,
        "{:?}",
        seen.ops
    );
    host.abort();
}
