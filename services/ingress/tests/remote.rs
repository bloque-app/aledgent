// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The core across a process boundary (`F-005`): ingress's client of the core host, against the
//! server the host answers with, over an in-process core.

use std::sync::Arc;
use std::time::Duration;

use ingress::boundary::{Core, CoreError, CoreInput, InputBody, InputKey};
use ingress::inprocess::{InProcessCore, MemJournal};
use ingress::remote::{Endpoint, LinkAuth, LinkToken, RemoteCore, serve, serve_tcp};
use ingress::testkit::genesis;

/// The link's test token: a test value, never a default of a production crate.
fn token() -> LinkToken {
    LinkToken::new(&"ab".repeat(32)).unwrap()
}

fn auth(uids: &[u32]) -> LinkAuth {
    LinkAuth::new(&[token().sha256_hex()], uids).unwrap()
}

async fn served() -> (RemoteCore, tokio::task::JoinHandle<std::io::Result<()>>) {
    let core = InProcessCore::in_memory(genesis(1), MemJournal::new());
    core.start().expect("the core starts");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(serve_tcp(listener, Arc::new(core), auth(&[])));
    let client = RemoteCore::new(
        Endpoint::Tcp(addr.to_string()),
        Duration::from_secs(10),
        token(),
    );
    (client, task)
}

/// `F-035`: the core link refuses a caller without the token, before it reads the request:
/// nothing runs, and the caller sees `Unavailable` (not run), never `Ambiguous`.
#[tokio::test]
async fn nfr_8_an_unauthenticated_caller_runs_nothing() {
    let core = InProcessCore::in_memory(genesis(1), MemJournal::new());
    core.start().expect("the core starts");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let at = listener.local_addr().unwrap().to_string();
    let core = Arc::new(core);
    let task = tokio::spawn(serve_tcp(listener, Arc::clone(&core), auth(&[])));
    let intruder = RemoteCore::new(
        Endpoint::Tcp(at.clone()),
        Duration::from_secs(10),
        LinkToken::new(&"cd".repeat(32)).unwrap(),
    );
    assert_eq!(
        intruder.apply(&tick(1, 1_000)).await,
        Err(CoreError::Unavailable)
    );
    assert_eq!(intruder.last_tick().await, Err(CoreError::Unavailable));
    // A raw connection that skips the hello gets `unauthenticated`, and its request never ran.
    {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let mut raw = tokio::net::TcpStream::connect(&at).await.unwrap();
        let req = serde_json::to_vec(&ingress::remote::Request::Apply {
            input: tick(1, 1_000),
        })
        .unwrap();
        raw.write_all(&(req.len() as u32).to_be_bytes())
            .await
            .unwrap();
        raw.write_all(&req).await.unwrap();
        let mut len = [0u8; 4];
        raw.read_exact(&mut len).await.unwrap();
        let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
        raw.read_exact(&mut body).await.unwrap();
        let r: ingress::remote::Response = serde_json::from_slice(&body).unwrap();
        assert_eq!(r, ingress::remote::Response::Unauthenticated);
    }
    assert_eq!(core.last_tick().await, Ok(None), "nothing ran");
    let honest = RemoteCore::new(Endpoint::Tcp(at), Duration::from_secs(10), token());
    assert_eq!(
        honest.apply(&tick(1, 1_000)).await.map(|r| r.input_seq),
        Ok(1)
    );
    task.abort();
    // No token configured: the host does not start.
    assert!(LinkAuth::new(&[], &[]).is_err());
    assert!(LinkToken::new("short").is_err());
}

/// `F-035`: on a Unix socket the peer's user id must be one the host lists, as well as the token.
#[tokio::test]
async fn nfr_8_the_unix_link_checks_the_peers_user_id() {
    use std::os::unix::fs::MetadataExt as _;
    let dir = tempfile::tempdir().unwrap();
    let probe = dir.path().join(".uid");
    std::fs::write(&probe, b"").unwrap();
    let me = std::fs::metadata(&probe).unwrap().uid();
    for (uids, ok) in [(vec![me], true), (vec![me.wrapping_add(1)], false)] {
        let core = InProcessCore::in_memory(genesis(1), MemJournal::new());
        core.start().expect("the core starts");
        let sock = dir.path().join(format!("core-{ok}.sock"));
        let task = tokio::spawn(serve(
            Endpoint::Unix(sock.clone()),
            Arc::new(core),
            auth(&uids),
        ));
        for _ in 0..100 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let client = RemoteCore::new(Endpoint::Unix(sock), Duration::from_secs(10), token());
        let r = client.apply(&tick(1, 1_000)).await;
        assert_eq!(r.is_ok(), ok, "{uids:?}: {r:?}");
        if !ok {
            assert_eq!(r, Err(CoreError::Unavailable));
        }
        task.abort();
    }
    // A TCP link on a routable address is refused.
    let core = InProcessCore::in_memory(genesis(1), MemJournal::new());
    let r = serve(Endpoint::Tcp("0.0.0.0:0".into()), Arc::new(core), auth(&[])).await;
    assert!(r.is_err());
}

fn tick(key: u8, now: u64) -> CoreInput {
    CoreInput {
        key: InputKey([key; 16]),
        body: InputBody::Tick { now },
    }
}

#[tokio::test]
async fn inv_18_an_input_through_the_socket_is_applied_once() {
    let (core, task) = served().await;
    let first = core.apply(&tick(1, 1_000)).await.expect("applied");
    let again = core.apply(&tick(1, 1_000)).await.expect("answered");
    assert_eq!(first, again, "a repeated input gets its first reply");
    let next = core.apply(&tick(2, 2_000)).await.expect("applied");
    assert_eq!(next.input_seq, first.input_seq + 1);
    assert_eq!(core.last_tick().await, Ok(Some(2_000)));
    let page = core.outbox(None, true, 2_000).await.expect("a page");
    assert!(page.items.is_empty());
    task.abort();
}

#[tokio::test]
async fn req_cs_7_a_core_that_is_not_there_is_unavailable() {
    let (core, task) = served().await;
    task.abort();
    let _ = task.await;
    assert_eq!(core.apply(&tick(3, 1)).await, Err(CoreError::Unavailable));
    assert_eq!(core.last_tick().await, Err(CoreError::Unavailable));
}

/// `T-031-04`, `INV-18`: a batch through the socket is one request; each input is answered in
/// order, under consecutive input numbers, and a repeated batch gets its first replies.
#[tokio::test]
async fn t_031_04_a_batch_through_the_socket_answers_each_input_in_order() {
    let (core, task) = served().await;
    let inputs = vec![tick(1, 1_000), tick(2, 2_000), tick(3, 3_000)];
    let replies: Vec<_> = core
        .apply_batch(&inputs)
        .await
        .into_iter()
        .map(|r| r.expect("applied"))
        .collect();
    assert_eq!(
        replies.iter().map(|r| r.input_seq).collect::<Vec<_>>(),
        vec![
            replies[0].input_seq,
            replies[0].input_seq + 1,
            replies[0].input_seq + 2
        ]
    );
    let again = core.apply_batch(&inputs).await;
    assert_eq!(again, replies.iter().cloned().map(Ok).collect::<Vec<_>>());
    assert_eq!(core.last_tick().await, Ok(Some(3_000)));
    task.abort();
    let _ = task.await;
    assert_eq!(
        core.apply_batch(&[tick(4, 4_000), tick(5, 5_000)]).await,
        vec![Err(CoreError::Unavailable); 2]
    );
}

#[test]
fn endpoints_parse() {
    assert_eq!(
        Endpoint::parse("unix:/run/aledgent/core.sock"),
        Some(Endpoint::Unix("/run/aledgent/core.sock".into()))
    );
    assert_eq!(
        Endpoint::parse("tcp:127.0.0.1:7070"),
        Some(Endpoint::Tcp("127.0.0.1:7070".into()))
    );
    assert_eq!(Endpoint::parse("127.0.0.1:7070"), None);
}
