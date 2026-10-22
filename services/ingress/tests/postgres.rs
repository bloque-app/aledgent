// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The PostgreSQL received-log (`T-009-02`, `T-009-03`, `T-009-06`), against a real server in a
//! container (testcontainers; Docker on the CI runner).
//!
//! Without Docker these tests say so and pass, except under CI (`CI` set), where a missing Docker
//! fails them: the proposal's store must be exercised on every change.

use std::time::{Duration, Instant};

use axum::http::StatusCode;
use ingress::boundary::InputKey;
use ingress::config::{Profile, SyncStandby};
use ingress::store::pg::{PgOptions, PgStore};
use ingress::store::{
    Answering, Appended, Attempt, DecidedRow, InputKind, NewRow, Recipient, RowCaller, Status,
    Store,
};
use ingress::testkit::{Deployment, USD, fast_config};
use ingress::wire::PassThrough;
use ledger_core::lifecycle::keys::Surface;
use serde_json::json;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;
use testcontainers_modules::testcontainers::runners::AsyncRunner;

async fn server() -> Option<(Option<ContainerAsync<Postgres>>, String)> {
    // A local server instead of a container, for a developer without Docker.
    if let Ok(url) = std::env::var("INGRESS_TEST_PG_URL") {
        return Some((None, url));
    }
    match Postgres::default().start().await {
        Ok(c) => {
            let port = c.get_host_port_ipv4(5432).await.ok()?;
            let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
            Some((Some(c), url))
        }
        Err(e) => {
            assert!(
                std::env::var_os("CI").is_none(),
                "Docker is required under CI: {e}"
            );
            eprintln!("skipped: no Docker ({e})");
            None
        }
    }
}

async fn store(url: &str) -> PgStore {
    PgStore::connect(PgOptions {
        url: url.to_owned(),
        sealing_key: [7; 32],
        require_sync_standby: SyncStandby::NotRequired,
        profile: Profile::Development,
        pool_size: 8,
    })
    .await
    .expect("the store connects")
}

fn command(n: u8, principal: &str, key: &str, reserves: bool) -> NewRow {
    NewRow {
        op_ref: InputKey([n; 16]),
        kind: InputKind::Command,
        caller: Some(RowCaller {
            principal: principal.into(),
            credential: "c".into(),
            on_behalf_of: None,
        }),
        surface: Surface::Command,
        client_key: Some(key.into()),
        key_domain: Some(principal.into()),
        key_derived: false,
        body_digest: [n; 32],
        body: b"{}".to_vec(),
        pass_through: None,
        reserves,
        overtakes: false,
        synchronous: reserves,
        received_at: 1_000 + u64::from(n),
        arrived_at: Some(999 + u64::from(n)),
    }
}

/// `T-037-04` (`REQ-RT-20`, amendment 0035) against the real column and ordering: a row that
/// overtakes goes before reservations and the rest, after the rows already in flight (given a feed
/// sequence), and the flag survives a round trip through the table.
#[tokio::test(flavor = "multi_thread")]
async fn t_037_04_a_row_that_overtakes_is_read_first_by_the_forwarder() {
    let Some((_pg, url)) = server().await else {
        return;
    };
    let s = store(&url).await;
    for (n, reserves, overtakes) in [
        (1, false, false),
        (2, true, false),
        (3, false, false),
        (4, false, true),
        (5, false, true),
    ] {
        let mut r = command(n, "alice", &format!("k{n}"), reserves);
        r.overtakes = overtakes;
        let Appended::New(stored) = s.append(r).await.unwrap() else {
            panic!("a new row");
        };
        assert_eq!(stored.new.overtakes, overtakes);
    }
    let refs = |rows: Vec<ingress::store::Row>| {
        rows.into_iter()
            .map(|r| r.new.op_ref.0[0])
            .collect::<Vec<_>>()
    };
    assert_eq!(
        refs(s.next_undecided_batch(10).await.unwrap()),
        vec![4, 5, 2, 1, 3]
    );
    assert_eq!(
        s.next_undecided().await.unwrap().unwrap().new.op_ref,
        InputKey([4; 16])
    );
    assert!(
        s.get(InputKey([4; 16]))
            .await
            .unwrap()
            .unwrap()
            .new
            .overtakes
    );
    assert!(
        !s.get(InputKey([2; 16]))
            .await
            .unwrap()
            .unwrap()
            .new
            .overtakes
    );
}

/// `T-009-02`: dedup on (principal, surface, client key), commit settings, the forwarder's
/// order, decisions recorded once per core input number, delivery attempts, sealed secrets.
#[tokio::test(flavor = "multi_thread")]
async fn t_009_02_the_postgres_received_log_keeps_its_rules() {
    let Some((_pg, url)) = server().await else {
        return;
    };
    let s = store(&url).await;
    // Every connection commits synchronously; a second connect migrates nothing twice.
    let (commit, _) = s.commit_settings().await.unwrap();
    assert_eq!(commit, "on");
    let again = store(&url).await;
    // A repeat of a client key returns the first row.
    let Appended::New(first) = s.append(command(1, "alice", "k", false)).await.unwrap() else {
        panic!("a new row");
    };
    let Appended::Existing(same) = again.append(command(2, "alice", "k", false)).await.unwrap()
    else {
        panic!("the client key is taken");
    };
    assert_eq!(same, first);
    assert!(matches!(
        s.append(command(3, "bob", "k", false)).await.unwrap(),
        Appended::New(_)
    ));
    assert!(matches!(
        s.append(command(4, "alice", "r", true)).await.unwrap(),
        Appended::New(_)
    ));
    // Reservations first, then arrival order.
    assert_eq!(
        s.next_undecided().await.unwrap().unwrap().new.op_ref,
        InputKey([4; 16])
    );
    // T-031-04: a batch in the same order, bounded.
    let refs = |rows: Vec<ingress::store::Row>| {
        rows.into_iter()
            .map(|r| r.new.op_ref.0[0])
            .collect::<Vec<_>>()
    };
    assert_eq!(
        refs(s.next_undecided_batch(10).await.unwrap()),
        vec![4, 1, 3]
    );
    assert_eq!(refs(s.next_undecided_batch(2).await.unwrap()), vec![4, 1]);
    // Forwarded once: the forward time is fixed on the first attempt.
    let f1 = s.mark_forwarded(InputKey([1; 16]), 50).await.unwrap();
    let f2 = s.mark_forwarded(InputKey([1; 16]), 99).await.unwrap();
    assert_eq!((f1.forwarded_at, f2.forwarded_at), (Some(50), Some(50)));
    assert_eq!(f2.status, Status::Forwarded);
    // Decided once; one core input number, one row.
    s.mark_decided(InputKey([1; 16]), 1, b"r".to_vec(), 60)
        .await
        .unwrap();
    s.mark_decided(InputKey([1; 16]), 9, b"x".to_vec(), 61)
        .await
        .unwrap();
    let d = s.get(InputKey([1; 16])).await.unwrap().unwrap();
    assert_eq!(
        (d.core_input_seq, d.core_reply.as_deref()),
        (Some(1), Some(&b"r"[..]))
    );
    assert!(
        s.mark_decided(InputKey([3; 16]), 1, b"y".to_vec(), 62)
            .await
            .is_err()
    );
    assert_eq!(s.oldest_undecided().await.unwrap(), Some(1_003));
    // T-009-16: the key's domain, not its principal, is what is unique. Two principals of one
    // integrating party share it: the second gets the first's row (and ingress refuses it, its
    // digest naming another principal). Ties are kept once, for good.
    let mut carol = command(20, "carol", "k2", false);
    carol.key_domain = Some(ingress::store::party_domain("x"));
    let mut dave = command(21, "dave", "k2", false);
    dave.key_domain = Some(ingress::store::party_domain("x"));
    let Appended::New(c) = s.append(carol).await.unwrap() else {
        panic!("a new row");
    };
    let Appended::Existing(d2) = s.append(dave).await.unwrap() else {
        panic!("the party's key is taken");
    };
    assert_eq!(d2, c);
    assert_eq!(
        s.find(&ingress::store::party_domain("x"), Surface::Command, "k2")
            .await
            .unwrap(),
        Some(c)
    );
    assert_eq!(s.tie_of("dave").await.unwrap(), None);
    s.put_tie("dave", "x").await.unwrap();
    s.put_tie("dave", "y").await.unwrap();
    assert_eq!(again.tie_of("dave").await.unwrap(), Some("x".into()));
    // Pending ticks and deliveries.
    assert!(!s.tick_pending().await.unwrap());
    let mut tick = command(5, "", "", false);
    tick.kind = InputKind::Tick;
    tick.caller = None;
    tick.client_key = None;
    tick.key_domain = None;
    s.append(tick).await.unwrap();
    assert!(s.tick_pending().await.unwrap());
    let mut delivery = command(6, "", "", false);
    delivery.kind = InputKind::Delivery;
    delivery.caller = None;
    delivery.client_key = Some("n1".into());
    delivery.key_domain = None;
    s.append(delivery).await.unwrap();
    assert!(s.delivery_pending("n1").await.unwrap());
    assert!(!s.delivery_pending("n2").await.unwrap());
    // Attempts, and the latest.
    for (at, next_at) in [(10, 20), (30, 70)] {
        s.record_attempt(Attempt {
            notification: "n1".into(),
            at,
            outcome: "503".into(),
            next_at,
        })
        .await
        .unwrap();
    }
    assert_eq!(s.attempts("n1").await.unwrap(), 2);
    assert_eq!(s.last_attempt("n1").await.unwrap().unwrap().next_at, 70);
    // Secrets are sealed at rest and open again.
    s.put_recipient(Recipient {
        principal: "alice".into(),
        endpoint: "https://a.example/hook".into(),
        secret: Some(b"plain-secret".to_vec()),
    })
    .await
    .unwrap();
    let r = again.recipient("alice").await.unwrap().unwrap();
    assert_eq!(r.secret.as_deref(), Some(&b"plain-secret"[..]));
    let (client, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(conn);
    let raw: Vec<u8> = client
        .query_one(
            "SELECT secret_sealed FROM recipient WHERE principal = 'alice'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert!(!raw.windows(12).any(|w| w == b"plain-secret"));
    // A store that requires a synchronous standby refuses to start without one.
    assert!(
        PgStore::connect(PgOptions {
            url: url.clone(),
            sealing_key: [7; 32],
            require_sync_standby: SyncStandby::Required,
            profile: Profile::Development,
            pool_size: 1,
        })
        .await
        .is_err()
    );
    // F-031 (stress finding F13): a batch's marks, one statement each, in the order asked; a
    // row is forwarded and decided once.
    for n in [30u8, 31] {
        s.append(command(n, "erin", &format!("k{n}"), false))
            .await
            .unwrap();
    }
    let both = [InputKey([31; 16]), InputKey([30; 16])];
    let rows = s.mark_forwarded_many(&both, 80).await.unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| (r.new.op_ref.0[0], r.status, r.forwarded_at))
            .collect::<Vec<_>>(),
        vec![
            (31, Status::Forwarded, Some(80)),
            (30, Status::Forwarded, Some(80))
        ]
    );
    let again_rows = s.mark_forwarded_many(&both, 99).await.unwrap();
    assert!(again_rows.iter().all(|r| r.forwarded_at == Some(80)));
    let decided = |op_ref, input_seq, reply: &[u8]| DecidedRow {
        op_ref,
        input_seq,
        reply: reply.to_vec(),
        completes: false,
    };
    s.mark_decided_many(
        &[decided(both[0], 40, b"a"), decided(both[1], 41, b"b")],
        81,
    )
    .await
    .unwrap();
    s.mark_decided_many(&[decided(both[0], 50, b"z")], 82)
        .await
        .unwrap();
    let d31 = s.get(both[0]).await.unwrap().unwrap();
    let d30 = s.get(both[1]).await.unwrap().unwrap();
    assert_eq!(
        (d31.status, d31.core_input_seq, d31.core_reply.as_deref()),
        (Status::Decided, Some(40), Some(&b"a"[..]))
    );
    assert_eq!(
        (d30.core_input_seq, d30.core_reply.as_deref()),
        (Some(41), Some(&b"b"[..]))
    );
}

/// `T-009-14`: `rds-multi-az` trusts an RDS instance's Multi-AZ setting, and so starts only on an
/// RDS instance: a plain PostgreSQL server, which defines no `rds.*` parameter, is refused.
#[tokio::test(flavor = "multi_thread")]
async fn t_009_14_rds_multi_az_is_refused_on_a_server_that_is_not_rds() {
    let Some((_pg, url)) = server().await else {
        return;
    };
    let refused = PgStore::connect(PgOptions {
        url: url.clone(),
        sealing_key: [7; 32],
        require_sync_standby: SyncStandby::RdsMultiAz,
        profile: Profile::Development,
        pool_size: 1,
    })
    .await;
    let Err(e) = refused else {
        panic!("rds-multi-az started on a server that is not RDS")
    };
    assert!(e.0.contains("not an Amazon RDS instance"), "{}", e.0);
    // The same server without TLS: refused in production before it is reached.
    let Err(e) = PgStore::connect(PgOptions {
        url,
        sealing_key: [7; 32],
        require_sync_standby: SyncStandby::NotRequired,
        profile: Profile::Production,
        pool_size: 1,
    })
    .await
    else {
        panic!("a production store without TLS started")
    };
    assert!(e.0.contains("sslmode=require"), "{}", e.0);
}

/// `T-009-06` on PostgreSQL: one leader at a time; a leader whose connection dies is replaced
/// at the next attempt.
#[tokio::test(flavor = "multi_thread")]
async fn t_009_06_one_leader_and_a_dead_leader_is_replaced() {
    let Some((_pg, url)) = server().await else {
        return;
    };
    let a = store(&url).await;
    let b = store(&url).await;
    assert!(a.try_lead("a").await.unwrap());
    assert!(a.try_lead("a").await.unwrap(), "renewed");
    assert!(!b.try_lead("b").await.unwrap());
    drop(a);
    let until = Instant::now() + Duration::from_secs(5);
    while !b.try_lead("b").await.unwrap() {
        assert!(
            Instant::now() < until,
            "the lock is released with the connection"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// `T-038-03` on PostgreSQL (`DEC-27` (d)): one holder of the book's feed lease at a time, on the
/// database's clock; the epoch rises on a change of holder, never on a renewal; a stale holder's
/// mark is fenced and marks nothing; sequences are gapless and kept by a resend, and the rows in
/// flight come back first. The verifier copy keeps and forgets (`T-038-10`). The roster lists the
/// live instances (`T-038-21`).
#[tokio::test(flavor = "multi_thread")]
async fn t_038_03_the_feed_lease_on_postgres() {
    use ingress::feed::BOOK;
    let Some((_pg, url)) = server().await else {
        return;
    };
    let a = store(&url).await;
    let b = store(&url).await;
    let long = Duration::from_secs(60);
    assert_eq!(a.try_feed(BOOK, "a", long).await.unwrap(), Some(1));
    assert_eq!(
        a.try_feed(BOOK, "a", long).await.unwrap(),
        Some(1),
        "renewed"
    );
    assert_eq!(b.try_feed(BOOK, "b", long).await.unwrap(), None);
    assert!(b.feed_live(BOOK).await.unwrap());
    let mut keys = Vec::new();
    for n in 1..=4u8 {
        let row = command(n, "did:aledgent:id:feed", &format!("f-{n}"), n % 2 == 0);
        keys.push(row.op_ref);
        a.append(row).await.unwrap();
    }
    let seqs = |rows: &[ingress::store::Row]| rows.iter().map(|r| r.feed_seq).collect::<Vec<_>>();
    let fed = a
        .mark_fed(BOOK, "a", 1, &[keys[1], keys[3], keys[0]], 7)
        .await
        .unwrap();
    assert_eq!(seqs(&fed), vec![Some(1), Some(2), Some(3)]);
    assert!(
        b.mark_fed(BOOK, "b", 1, &[keys[2]], 8)
            .await
            .unwrap_err()
            .is_fenced()
    );
    assert!(
        a.mark_fed(BOOK, "a", 2, &[keys[2]], 8)
            .await
            .unwrap_err()
            .is_fenced()
    );
    a.release_feed(BOOK, "a").await.unwrap();
    assert!(!b.feed_live(BOOK).await.unwrap());
    assert_eq!(b.try_feed(BOOK, "b", long).await.unwrap(), Some(2));
    assert!(
        a.mark_fed(BOOK, "a", 1, &[keys[2]], 9)
            .await
            .unwrap_err()
            .is_fenced()
    );
    let next: Vec<InputKey> = b
        .next_undecided_batch(10)
        .await
        .unwrap()
        .iter()
        .map(|r| r.new.op_ref)
        .collect();
    assert_eq!(
        next,
        vec![keys[1], keys[3], keys[0], keys[2]],
        "in flight first"
    );
    let fed = b.mark_fed(BOOK, "b", 2, &next, 9).await.unwrap();
    assert_eq!(seqs(&fed), vec![Some(1), Some(2), Some(3), Some(4)]);
    // Rows that certainly did not run give their sequences back, the holder only.
    assert!(a.unfeed(BOOK, "a", 1, 3).await.unwrap_err().is_fenced());
    b.unfeed(BOOK, "b", 2, 3).await.unwrap();
    assert_eq!(b.get(keys[0]).await.unwrap().unwrap().feed_seq, None);
    assert_eq!(b.get(keys[3]).await.unwrap().unwrap().feed_seq, Some(2));
    let fed = b.mark_fed(BOOK, "b", 2, &[keys[2]], 10).await.unwrap();
    assert_eq!(seqs(&fed), vec![Some(3)]);

    let body = ingress::boundary::CredentialBody {
        identity: "did:aledgent:id:x".into(),
        kind: "person".into(),
        scheme: "api-key".into(),
        verifier: "ab".repeat(32),
    };
    a.copy_verifier("c", Some(&body), 1).await.unwrap();
    assert_eq!(b.copied_verifier("c").await.unwrap(), Some(body));
    b.copy_verifier("c", None, 2).await.unwrap();
    assert_eq!(a.copied_verifier("c").await.unwrap(), None);

    // T-038-21: the roster on the database's clock: both instances listed; one withdrawn on
    // drain; one whose entry lapses leaves it.
    let ttl = Duration::from_millis(300);
    a.advertise("a", "tls:10.0.0.1:7443", ttl).await.unwrap();
    let roster = b.advertise("b", "tls:10.0.0.2:7443", ttl).await.unwrap();
    let names: Vec<&str> = roster.iter().map(|e| e.instance.as_str()).collect();
    assert_eq!(names, ["a", "b"]);
    assert_eq!(roster[1].address, "tls:10.0.0.2:7443");
    b.withdraw("b").await.unwrap();
    let roster = a.advertise("a", "tls:10.0.0.1:7443", ttl).await.unwrap();
    assert_eq!(roster.len(), 1);
    b.advertise("b", "tls:10.0.0.2:7443", Duration::from_millis(1))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let roster = a.advertise("a", "tls:10.0.0.1:7443", ttl).await.unwrap();
    assert_eq!(roster.len(), 1, "a lapsed entry leaves the roster");
}

/// `T-037-18` (amendment 0036, `F-038` §3) on PostgreSQL: giving back the sequences of rows that
/// were in flight raises the epoch by one in the same transaction and returns it; a stale holder
/// or epoch is refused and nothing changes; the rows before the position keep their sequences; the
/// way out, read first, takes the first sequence returned and the rows released follow it; the old
/// epoch can no longer mark; the plain give back and a renewal leave the epoch alone.
#[tokio::test(flavor = "multi_thread")]
async fn t_037_18_a_release_of_rows_in_flight_raises_the_epoch_on_postgres() {
    use ingress::feed::BOOK;
    let Some((_pg, url)) = server().await else {
        return;
    };
    let a = store(&url).await;
    let b = store(&url).await;
    let long = Duration::from_secs(60);
    assert_eq!(a.try_feed(BOOK, "a", long).await.unwrap(), Some(1));
    let mut keys = Vec::new();
    for n in 1..=4u8 {
        let row = command(n, "did:aledgent:id:feed", &format!("f-{n}"), false);
        keys.push(row.op_ref);
        a.append(row).await.unwrap();
    }
    let seqs = |rows: &[ingress::store::Row]| rows.iter().map(|r| r.feed_seq).collect::<Vec<_>>();
    let fed = a.mark_fed(BOOK, "a", 1, &keys, 7).await.unwrap();
    assert_eq!(seqs(&fed), vec![Some(1), Some(2), Some(3), Some(4)]);
    // Not the holder, or not at that epoch: refused, nothing changes.
    assert!(
        b.unfeed_in_flight(BOOK, "b", 1, 2)
            .await
            .unwrap_err()
            .is_fenced()
    );
    assert!(
        a.unfeed_in_flight(BOOK, "a", 2, 2)
            .await
            .unwrap_err()
            .is_fenced()
    );
    assert_eq!(a.get(keys[1]).await.unwrap().unwrap().feed_seq, Some(2));
    // Rows 3 and 4 were answered not run: given back, the epoch rises to 2 with them.
    assert_eq!(a.unfeed_in_flight(BOOK, "a", 1, 3).await.unwrap(), 2);
    assert_eq!(a.get(keys[0]).await.unwrap().unwrap().feed_seq, Some(1));
    assert_eq!(a.get(keys[1]).await.unwrap().unwrap().feed_seq, Some(2));
    assert_eq!(a.get(keys[2]).await.unwrap().unwrap().feed_seq, None);
    assert_eq!(a.get(keys[3]).await.unwrap().unwrap().feed_seq, None);
    // The old epoch can no longer mark; the renewal keeps the new one.
    assert!(
        a.mark_fed(BOOK, "a", 1, &keys[2..], 8)
            .await
            .unwrap_err()
            .is_fenced()
    );
    assert_eq!(a.try_feed(BOOK, "a", long).await.unwrap(), Some(2));
    // A way out logged now overtakes the rows given back and takes the first sequence returned.
    let mut way_out = command(9, "did:aledgent:id:operator", "way-out", false);
    way_out.overtakes = true;
    let way_out_key = way_out.op_ref;
    a.append(way_out).await.unwrap();
    let queue = a.next_undecided_batch(10).await.unwrap();
    let order: Vec<InputKey> = queue.iter().map(|r| r.new.op_ref).collect();
    assert_eq!(
        order,
        vec![keys[0], keys[1], way_out_key, keys[2], keys[3]],
        "in flight first, then the way out, then the rest"
    );
    // Rows 1 and 2 are decided by the core meanwhile; then the way out takes sequence 3.
    for (i, k) in keys[..2].iter().enumerate() {
        a.mark_decided(*k, 1 + i as u64, b"{}".to_vec(), 9)
            .await
            .unwrap();
    }
    let next: Vec<InputKey> = a
        .next_undecided_batch(10)
        .await
        .unwrap()
        .iter()
        .map(|r| r.new.op_ref)
        .collect();
    assert_eq!(next, vec![way_out_key, keys[2], keys[3]]);
    let fed = a.mark_fed(BOOK, "a", 2, &next, 10).await.unwrap();
    assert_eq!(seqs(&fed), vec![Some(3), Some(4), Some(5)]);
    assert_eq!(fed[0].new.op_ref, way_out_key);
    // The plain give back leaves the epoch alone.
    a.unfeed(BOOK, "a", 2, 5).await.unwrap();
    assert_eq!(a.try_feed(BOOK, "a", long).await.unwrap(), Some(2));
}

/// `T-009-03`, `REQ-AR-7` end to end on PostgreSQL: two replicas share the log; transfers are
/// acknowledged while the core is down; the leading replica is dropped; the other decides every
/// row, with gapless input numbers.
#[tokio::test(flavor = "multi_thread")]
async fn req_ar_7_two_replicas_on_postgres_lose_nothing() {
    let Some((_pg, url)) = server().await else {
        return;
    };
    let mut d = Deployment::start_on(store(&url).await, fast_config("pg-0")).await;
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 0).await;
    let (second, surface) = d.replica_on(store(&url).await, fast_config("pg-1"));
    d.core.stop();
    for i in 0..6 {
        let (s, r) = d
            .call(
                "POST",
                "/v1/transfers",
                Some(&alice.token),
                &[("client-key", &format!("pg-{i}"))],
                Some(json!({ "legs": [{ "from": alice.account, "to": bob.account, "asset": USD, "amount": "3" }] })),
            )
            .await;
        assert_eq!(s, StatusCode::ACCEPTED, "{r}");
    }
    // The first replica goes away with its leader connection.
    d.replica.kill();
    d.replica.shared.store.resign("pg-0").await.unwrap();
    d.core.start().unwrap();
    assert!(
        d.quiesce(Duration::from_secs(20)).await,
        "every row decided"
    );
    assert!(second.leads());
    // Gapless, read at one consistent instant: the surviving replica keeps ticking (issue #48).
    if let Err((numbers, n)) = d
        .gapless_inputs(|| d.core.inputs(), Duration::from_secs(10))
        .await
    {
        panic!("rows hold {numbers:?} against the core's {n} inputs");
    }
    let (s, r) = ingress::testkit::call(
        &surface,
        "POST",
        "/v1/transfers",
        Some(&alice.token),
        &[("client-key", "pg-0")],
        Some(json!({ "legs": [{ "from": alice.account, "to": bob.account, "asset": USD, "amount": "3" }] })),
    )
    .await;
    assert_eq!((s, r["state"].clone()), (StatusCode::OK, json!("Settled")));
}

/// `F-028`, `REQ-OP-10`: a credential command looks its client key up before it verifies a
/// single-use proof. `find` returns the row that took (principal, surface, client key), and
/// nothing for another principal, surface or key.
#[tokio::test(flavor = "multi_thread")]
async fn req_op_10_a_command_row_is_found_by_its_client_key() {
    let Some((_pg, url)) = server().await else {
        return;
    };
    let s = store(&url).await;
    assert!(matches!(
        s.append(command(1, "alice", "k1", false)).await.unwrap(),
        Appended::New(_)
    ));
    let found = s.find("alice", Surface::Command, "k1").await.unwrap();
    assert_eq!(found.map(|r| r.new.op_ref), Some(InputKey([1; 16])));
    for (p, surface, k) in [
        ("bob", Surface::Command, "k1"),
        ("alice", Surface::Operator, "k1"),
        ("alice", Surface::Command, "k2"),
    ] {
        assert!(s.find(p, surface, k).await.unwrap().is_none(), "{p} {k}");
    }
}

/// `F-028`: on PostgreSQL, a passkey's sign count only rises, and a challenge is used once, for
/// every replica sharing the store.
#[tokio::test(flavor = "multi_thread")]
async fn f_028_sign_counts_and_challenges_are_shared_and_durable() {
    let Some((_pg, url)) = server().await else {
        return;
    };
    let a = store(&url).await;
    let b = store(&url).await;
    assert_eq!(a.sign_count("cred").await.unwrap(), None);
    assert!(a.raise_sign_count("cred", 3).await.unwrap());
    assert!(!b.raise_sign_count("cred", 3).await.unwrap());
    assert!(b.raise_sign_count("cred", 4).await.unwrap());
    assert_eq!(a.sign_count("cred").await.unwrap(), Some(4));
    assert!(a.use_challenge([9; 16], 1_000, 10).await.unwrap());
    assert!(!b.use_challenge([9; 16], 1_000, 20).await.unwrap());
}

/// `T-040-03` (amendment 0037, `AC-A10.4`): on PostgreSQL, an approval's number is spent once
/// across instances sharing the database, under a race, and after a restart; it is kept past its
/// expiry for the margin that covers clock skew between instances, and dropped after it.
#[tokio::test(flavor = "multi_thread")]
async fn t_040_03_an_approval_number_is_spent_once_everywhere() {
    let Some((_pg, url)) = server().await else {
        return;
    };
    let (a, b) = (store(&url).await, store(&url).await);
    let mut tasks = Vec::new();
    for i in 0..16u64 {
        let s = if i % 2 == 0 { a.clone() } else { b.clone() };
        tasks.push(tokio::spawn(async move {
            s.use_challenge([3; 16], 1_000, 10 + i).await.unwrap()
        }));
    }
    let mut wins = 0;
    for t in tasks {
        wins += usize::from(t.await.unwrap());
    }
    assert_eq!(
        wins, 1,
        "two instances racing on one number accept exactly one"
    );
    // A restart: a new connection still refuses it.
    let again = store(&url).await;
    assert!(!again.use_challenge([3; 16], 1_000, 500).await.unwrap());
    // An instance whose clock runs ahead by less than the margin does not forget it.
    let margin = ingress::store::SPENT_MARGIN_MS;
    assert!(
        !again
            .use_challenge([3; 16], 1_000, 1_000 + margin - 1)
            .await
            .unwrap()
    );
    // Past the margin it is dropped (the challenge itself expired long before, so the seal check
    // refuses it anyway).
    assert!(
        again
            .use_challenge([3; 16], 1_000, 1_000 + margin + 1)
            .await
            .unwrap()
    );
}

/// `T-009-18` (amendment 0013, `REQ-CS-9`): on PostgreSQL, a principal's answering setting is
/// absent by default, kept with who set it and when, replaced in place, and shared by every replica
/// on the store.
#[tokio::test(flavor = "multi_thread")]
async fn t_009_18_the_answering_setting_is_durable_and_shared() {
    let Some((_pg, url)) = server().await else {
        return;
    };
    let s = store(&url).await;
    let again = store(&url).await;
    assert_eq!(s.answering("alice").await.unwrap(), None);
    let set = Answering {
        principal: "alice".into(),
        decided: true,
        wait_ms: 800,
        set_by: "alice".into(),
        set_at: 42,
    };
    s.put_answering(set.clone()).await.unwrap();
    assert_eq!(again.answering("alice").await.unwrap(), Some(set.clone()));
    let back = Answering {
        decided: false,
        wait_ms: 1_000,
        set_at: 43,
        ..set
    };
    again.put_answering(back.clone()).await.unwrap();
    assert_eq!(s.answering("alice").await.unwrap(), Some(back));
    assert_eq!(s.answering("bob").await.unwrap(), None);
}

/// `T-009-21` (amendment 0020 R1, `REQ-PO-5`): on PostgreSQL, a command's pass-through data is
/// kept with its row exactly as sent, read back by a store that connects afterwards (a restart),
/// and found in one read for the rows that carry it; the rows that carry none are left out.
#[tokio::test(flavor = "multi_thread")]
async fn req_po_5_pass_through_data_is_kept_with_its_row() {
    let Some((_pg, url)) = server().await else {
        return;
    };
    let sent = r#"{"order": "o-1", "n": 123456789012345678901234567890.5, "a":[1 ,2]}"#;
    let s = store(&url).await;
    let mut with = command(1, "alice", "k1", false);
    with.pass_through = PassThrough::parse(sent);
    assert!(with.pass_through.is_some());
    s.append(with).await.unwrap();
    s.append(command(2, "alice", "k2", false)).await.unwrap();
    let restarted = store(&url).await;
    let row = restarted.get(InputKey([1; 16])).await.unwrap().unwrap();
    assert_eq!(
        row.new.pass_through.as_ref().map(PassThrough::get),
        Some(sent)
    );
    let none = restarted.get(InputKey([2; 16])).await.unwrap().unwrap();
    assert_eq!(none.new.pass_through, None);
    let found = restarted
        .pass_through_of(&[InputKey([1; 16]), InputKey([2; 16]), InputKey([3; 16])])
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].op_ref, InputKey([1; 16]));
    assert_eq!(found[0].principal, "alice");
    assert_eq!(found[0].pass_through.get(), sent);
    assert!(restarted.pass_through_of(&[]).await.unwrap().is_empty());
}

/// `T-009-25` (`NFR-1`, `NFR-15`, `NFR-16`): on PostgreSQL, a command row keeps its arrival from
/// the `INSERT`; its first reply stamp is kept and a later one ignored; a decision that completes
/// it sets `completed_at` in the decision's statement, and one that does not leaves it NULL; a
/// later completion is set once, only on a command row, only for the principal named.
#[tokio::test(flavor = "multi_thread")]
async fn t_009_25_the_latency_columns_are_kept_on_postgres() {
    let Some((_pg, url)) = server().await else {
        return;
    };
    let s = store(&url).await;
    let (acked, withdrawn) = (InputKey([60; 16]), InputKey([61; 16]));
    s.append(command(60, "lat", "k60", false)).await.unwrap();
    s.append(command(61, "lat", "k61", true)).await.unwrap();
    let row = s.get(acked).await.unwrap().unwrap();
    assert_eq!(row.new.arrived_at, Some(1_059));
    assert_eq!((row.replied_at, row.completed_at), (None, None));
    s.mark_replied_many(&[(acked, 1_070), (withdrawn, 1_071)])
        .await
        .unwrap();
    s.mark_replied_many(&[(acked, 9_999)]).await.unwrap();
    assert_eq!(s.get(acked).await.unwrap().unwrap().replied_at, Some(1_070));
    // A row another writer holds is skipped, never waited on, and handed back to try again.
    s.append(command(64, "lat", "k64", false)).await.unwrap();
    let held = InputKey([64; 16]);
    let (mut raw, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(conn);
    let tx = raw.transaction().await.unwrap();
    tx.execute(
        "SELECT 1 FROM received_log WHERE op_ref = $1 FOR UPDATE",
        &[&held.0.as_slice()],
    )
    .await
    .unwrap();
    let skipped = tokio::time::timeout(
        Duration::from_secs(5),
        s.mark_replied_many(&[(held, 1_064), (withdrawn, 1_071)]),
    )
    .await
    .expect("a stamp never waits on a row lock")
    .unwrap();
    assert_eq!(skipped, vec![(held, 1_064)]);
    assert_eq!(s.get(held).await.unwrap().unwrap().replied_at, None);
    tx.commit().await.unwrap();
    assert!(
        s.mark_replied_many(&[(held, 1_064)])
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(s.get(held).await.unwrap().unwrap().replied_at, Some(1_064));
    s.mark_forwarded_many(&[acked, withdrawn], 1_080)
        .await
        .unwrap();
    s.mark_decided_many(
        &[
            DecidedRow {
                op_ref: acked,
                input_seq: 600,
                reply: b"a".to_vec(),
                completes: true,
            },
            DecidedRow {
                op_ref: withdrawn,
                input_seq: 601,
                reply: b"w".to_vec(),
                completes: false,
            },
        ],
        1_090,
    )
    .await
    .unwrap();
    let a = s.get(acked).await.unwrap().unwrap();
    assert_eq!((a.decided_at, a.completed_at), (Some(1_090), Some(1_090)));
    let w = s.get(withdrawn).await.unwrap().unwrap();
    assert_eq!((w.decided_at, w.completed_at), (Some(1_090), None));
    // Relayed to another principal: nothing. To its requester: once.
    assert_eq!(
        s.mark_completed(withdrawn, 1_200, Some("other"))
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        s.mark_completed(withdrawn, 1_300, Some("lat"))
            .await
            .unwrap(),
        Some(1_061)
    );
    assert_eq!(
        s.mark_completed(withdrawn, 1_400, Some("lat"))
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        s.get(withdrawn).await.unwrap().unwrap().completed_at,
        Some(1_300)
    );
    // Only a withdrawal's row takes a submission, once; its completion then reports it as the
    // start of its clock.
    assert!(
        !s.mark_submitted(acked, 1_250).await.unwrap(),
        "not a withdrawal"
    );
    let mut w = command(63, "lat", "k63", true);
    w.body = br#"{"command":"withdraw"}"#.to_vec();
    s.append(w).await.unwrap();
    let w63 = InputKey([63; 16]);
    assert!(s.mark_submitted(w63, 1_150).await.unwrap());
    assert!(!s.mark_submitted(w63, 1_160).await.unwrap(), "set once");
    assert_eq!(s.get(w63).await.unwrap().unwrap().submitted_at, Some(1_150));
    assert_eq!(
        s.mark_completed(w63, 1_900, Some("lat")).await.unwrap(),
        Some(1_150)
    );
    // A row that is not a command is never completed.
    let mut tick = command(62, "", "", false);
    tick.kind = InputKind::Tick;
    tick.caller = None;
    tick.client_key = None;
    tick.key_domain = None;
    tick.arrived_at = None;
    s.append(tick).await.unwrap();
    assert_eq!(
        s.mark_completed(InputKey([62; 16]), 1_500, None)
            .await
            .unwrap(),
        None
    );
}

/// Migrations 1–5, as a received-log deployed before `T-009-25` holds them.
const BEFORE_0006: &[(i32, &str)] = &[
    (1, include_str!("../migrations/0001_received_log.sql")),
    (2, include_str!("../migrations/0002_passkeys.sql")),
    (3, include_str!("../migrations/0003_client_key_domain.sql")),
    (4, include_str!("../migrations/0004_answering.sql")),
    (5, include_str!("../migrations/0005_pass_through.sql")),
];

/// `T-009-25`: migration 6 on a live received-log. On an empty database every migration applies
/// in order. On one at version 5 with rows, 6 adds its four columns without touching a row (each
/// is NULL on the rows already there) and the store works on. Behind a transaction that holds the
/// table, 6 fails within its `lock_timeout` instead of queuing every append behind it, records
/// nothing, and applies on the next start once the table is free.
#[tokio::test(flavor = "multi_thread")]
async fn t_009_25_migration_6_applies_on_a_live_log_and_fails_fast_behind_a_lock() {
    let Some((_pg, url)) = server().await else {
        return;
    };
    let (raw, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(conn);
    let version = |db: &'static str| {
        let url = format!("{}/{db}", url.rsplit_once('/').unwrap().0);
        async move {
            let (c, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
                .await
                .unwrap();
            tokio::spawn(conn);
            (url, c)
        }
    };
    // An empty database: 1 to 6, in order.
    raw.batch_execute("CREATE DATABASE fresh").await.unwrap();
    let (fresh_url, fresh) = version("fresh").await;
    store(&fresh_url).await;
    let applied: Vec<i32> = fresh
        .query("SELECT version FROM ingress_schema ORDER BY version", &[])
        .await
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    // And every later one (7, F-038).
    let latest = ingress::store::migrate::MIGRATIONS.len() as i32;
    assert_eq!(applied, (1..=latest).collect::<Vec<_>>());

    // A database at version 5, with rows, as a deployment is before the upgrade.
    raw.batch_execute("CREATE DATABASE live").await.unwrap();
    let (live_url, mut live) = version("live").await;
    live.batch_execute(
        "CREATE TABLE ingress_schema (version INTEGER PRIMARY KEY, \
         applied_at TIMESTAMPTZ NOT NULL DEFAULT now())",
    )
    .await
    .unwrap();
    for (v, sql) in BEFORE_0006 {
        let tx = live.transaction().await.unwrap();
        tx.batch_execute(sql).await.unwrap();
        tx.execute("INSERT INTO ingress_schema (version) VALUES ($1)", &[v])
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    live.batch_execute(
        "INSERT INTO received_log (op_ref, input_kind, principal, credential, surface, \
         client_key, key_derived, body_digest, body, reserves, synchronous, received_at, status, \
         client_key_domain) \
         SELECT decode(lpad(to_hex(g), 32, '0'), 'hex'), 'command', 'p', 'c', 1, 'k' || g, \
         false, decode(repeat('00', 32), 'hex'), '\\x7b7d', false, false, 1000 + g, \
         'Received', 'p' FROM generate_series(1, 500) g",
    )
    .await
    .unwrap();

    // A reader holds the table: 6 gives up within its lock_timeout, and records nothing.
    let mut holder = version("live").await.1;
    let held = holder.transaction().await.unwrap();
    held.batch_execute("LOCK TABLE received_log IN ACCESS SHARE MODE")
        .await
        .unwrap();
    let started = Instant::now();
    let blocked = PgStore::connect(PgOptions {
        url: live_url.clone(),
        sealing_key: [7; 32],
        require_sync_standby: SyncStandby::NotRequired,
        profile: Profile::Development,
        pool_size: 2,
    })
    .await;
    assert!(blocked.is_err(), "the migration waited out the lock");
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "it failed fast: {:?}",
        started.elapsed()
    );
    let at: i32 = live
        .query_one("SELECT max(version) FROM ingress_schema", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(at, 5, "a failed migration records nothing");
    held.commit().await.unwrap();

    // The table free, the next start applies 6; every row is kept, its new columns NULL.
    let s = store(&live_url).await;
    let at: i32 = live
        .query_one("SELECT max(version) FROM ingress_schema", &[])
        .await
        .unwrap()
        .get(0);
    assert_eq!(at, latest, "6 and every later one");
    let columns: Vec<(String, String, Option<String>)> = live
        .query(
            "SELECT column_name, is_nullable, column_default FROM information_schema.columns \
             WHERE table_name = 'received_log' AND column_name IN \
             ('arrived_at', 'replied_at', 'completed_at', 'submitted_at') ORDER BY column_name",
            &[],
        )
        .await
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect();
    assert_eq!(columns.len(), 4);
    assert!(
        columns.iter().all(|(_, n, d)| n == "YES" && d.is_none()),
        "nullable, no default: {columns:?}"
    );
    let rows = s.rows().await.unwrap();
    assert_eq!(rows.len(), 500);
    assert!(rows.iter().all(|r| r.new.arrived_at.is_none()
        && r.replied_at.is_none()
        && r.completed_at.is_none()
        && r.submitted_at.is_none()));
    s.append(command(70, "after", "k70", false)).await.unwrap();
    assert_eq!(
        s.get(InputKey([70; 16]))
            .await
            .unwrap()
            .unwrap()
            .new
            .arrived_at,
        Some(1_069)
    );
}

// --- T-009-26: the migration runner, hardened (owner ruling `v0w-ingress-migrations`) ---

/// Migrations 1–6, as a deployment holds them when they were applied before checksums were kept.
const BEFORE_0007: &[(i32, &str)] = &[
    (1, include_str!("../migrations/0001_received_log.sql")),
    (2, include_str!("../migrations/0002_passkeys.sql")),
    (3, include_str!("../migrations/0003_client_key_domain.sql")),
    (4, include_str!("../migrations/0004_answering.sql")),
    (5, include_str!("../migrations/0005_pass_through.sql")),
    (6, include_str!("../migrations/0006_latency_columns.sql")),
];

/// A new, empty database on the test server: its URL and a raw client.
async fn database(url: &str, name: &str) -> (String, tokio_postgres::Client) {
    let (raw, conn) = tokio_postgres::connect(url, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(conn);
    raw.batch_execute(&format!("CREATE DATABASE {name}"))
        .await
        .unwrap();
    let url = format!("{}/{name}", url.rsplit_once('/').unwrap().0);
    let (c, conn) = tokio_postgres::connect(&url, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(conn);
    (url, c)
}

/// Bring a database to `upto` the way a build before `T-009-26` did: `ingress_schema` without a
/// checksum column, one row per version.
async fn at_version_without_checksums(c: &mut tokio_postgres::Client, upto: i32) {
    c.batch_execute(
        "CREATE TABLE ingress_schema (version INTEGER PRIMARY KEY, \
         applied_at TIMESTAMPTZ NOT NULL DEFAULT now())",
    )
    .await
    .unwrap();
    for (v, sql) in BEFORE_0007.iter().filter(|(v, _)| *v <= upto) {
        let tx = c.transaction().await.unwrap();
        tx.batch_execute(sql).await.unwrap();
        tx.execute("INSERT INTO ingress_schema (version) VALUES ($1)", &[v])
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
}

fn options(url: &str) -> PgOptions {
    PgOptions {
        url: url.to_owned(),
        sealing_key: [7; 32],
        require_sync_standby: SyncStandby::NotRequired,
        profile: Profile::Development,
        pool_size: 2,
    }
}

async fn recorded(c: &tokio_postgres::Client) -> Vec<(i32, String, Option<String>)> {
    c.query(
        // The checksum column may not exist yet: read it through the row's JSON.
        "SELECT version, applied_at::text, to_jsonb(s) ->> 'checksum' FROM ingress_schema s \
         ORDER BY version",
        &[],
    )
    .await
    .unwrap()
    .iter()
    .map(|r| (r.get(0), r.get(1), r.get(2)))
    .collect()
}

/// This build's versions above `after`: what a database at `after` has pending. Derived from the
/// embedded set, so a new migration does not break these tests.
fn versions_after(after: i32) -> Vec<i32> {
    ingress::store::migrate::MIGRATIONS
        .iter()
        .map(|m| m.version)
        .filter(|v| *v > after)
        .collect()
}

fn sha256_hex(text: &str) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(text.as_bytes()))
}

/// `T-009-26`: a database at version 6 with no checksums, as an earlier deployment has, gains the checksum
/// column on the first start and every version's checksum is backfilled from the embedded file,
/// its `applied_at` untouched; `--status` reads each state; an applied migration whose recorded
/// checksum differs from the file refuses the start, naming the version, and so does a synthetic
/// migration edited after it was applied.
#[tokio::test(flavor = "multi_thread")]
async fn t_009_26_checksums_backfill_at_6_and_a_mismatch_refuses_to_start() {
    use ingress::store::migrate::{self, MIGRATIONS, Migration, State};
    let Some((_pg, url)) = server().await else {
        return;
    };
    // The embedded set is the files on disk, byte for byte.
    assert!(MIGRATIONS.len() >= BEFORE_0007.len());
    for (m, (v, sql)) in MIGRATIONS.iter().zip(BEFORE_0007) {
        assert_eq!((m.version, m.checksum()), (*v, sha256_hex(sql)));
    }

    let (prod_url, mut prod) = database(&url, "prod_like").await;
    at_version_without_checksums(&mut prod, 6).await;
    let before = recorded(&prod).await;
    let status = migrate::status(&prod, MIGRATIONS).await.unwrap();
    assert_eq!(status.backfillable(), vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(status.pending(), versions_after(6));
    assert!(status.mismatched().is_empty());

    // First start: backfill, nothing applied, applied_at kept.
    PgStore::connect(options(&prod_url)).await.unwrap();
    let after = recorded(&prod).await;
    assert_eq!(after.len(), MIGRATIONS.len());
    for ((v, at, sum), (v0, at0, _)) in after.iter().zip(&before) {
        assert_eq!((v, at), (v0, at0), "applied_at is kept");
        assert_eq!(
            sum.as_deref(),
            Some(sha256_hex(BEFORE_0007[(*v - 1) as usize].1).as_str())
        );
    }
    let status = migrate::status(&prod, MIGRATIONS).await.unwrap();
    assert!(status.entries.iter().all(|e| e.state == State::Ok));
    // Every later start compares, and passes.
    PgStore::connect(options(&prod_url)).await.unwrap();
    assert_eq!(recorded(&prod).await, after);

    // An applied migration edited: a hard error naming it, and nothing changes.
    prod.execute(
        "UPDATE ingress_schema SET checksum = repeat('0', 64) WHERE version = 3",
        &[],
    )
    .await
    .unwrap();
    let e = PgStore::connect(options(&prod_url))
        .await
        .err()
        .expect("a mismatch refuses the start")
        .to_string();
    assert!(
        e.contains("migration 3 (0003_client_key_domain.sql)") && e.contains("edited"),
        "{e}"
    );
    let e = PgStore::connect_with(
        options(&prod_url),
        &ingress::config::MigrationsConfig {
            on_start: ingress::config::OnStart::Verify,
            ..Default::default()
        },
    )
    .await
    .err()
    .expect("verify refuses a mismatch too")
    .to_string();
    assert!(e.contains("migration 3"), "{e}");
    let status = migrate::status(&prod, MIGRATIONS).await.unwrap();
    assert_eq!(status.mismatched(), vec![3]);
    assert!(matches!(
        &status.entries[2].state,
        State::Mismatch { recorded } if recorded == &"0".repeat(64)
    ));

    // A file edited after it was applied, through the runner itself.
    let (_, mut syn) = database(&url, "synthetic").await;
    let first = [Migration {
        version: 1,
        name: "0001_t.sql",
        sql: "CREATE TABLE t (x INTEGER);",
    }];
    let applied = migrate::apply(&mut syn, &first, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!(applied.applied, vec![1]);
    let edited = [Migration {
        sql: "CREATE TABLE t (x BIGINT);",
        ..first[0]
    }];
    let e = migrate::apply(&mut syn, &edited, Duration::from_secs(5))
        .await
        .expect_err("an edited migration never passes")
        .to_string();
    assert!(e.contains("migration 1 (0001_t.sql)"), "{e}");
    // The previous build's runner on the new table: it reads by version and inserts without a
    // checksum; the next migrator backfills what it inserted.
    let second = Migration {
        version: 2,
        name: "0002_t.sql",
        sql: "CREATE TABLE u (x INTEGER);",
    };
    {
        let tx = syn.transaction().await.unwrap();
        tx.execute("LOCK TABLE ingress_schema IN EXCLUSIVE MODE", &[])
            .await
            .unwrap();
        assert!(
            tx.query_opt("SELECT 1 FROM ingress_schema WHERE version = $1", &[&1i32])
                .await
                .unwrap()
                .is_some()
        );
        tx.batch_execute(second.sql).await.unwrap();
        tx.execute("INSERT INTO ingress_schema (version) VALUES ($1)", &[&2i32])
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }
    let both = [first[0], second];
    let done = migrate::apply(&mut syn, &both, Duration::from_secs(5))
        .await
        .unwrap();
    assert_eq!((done.applied, done.backfilled), (vec![], vec![2]));
    // A migration the build does not have (a later build's) is reported, never refused.
    syn.execute(
        "INSERT INTO ingress_schema (version, checksum) VALUES (3, 'x')",
        &[],
    )
    .await
    .unwrap();
    let status = migrate::status(&syn, &both).await.unwrap();
    assert_eq!(status.unknown(), vec![3]);
    migrate::apply(&mut syn, &both, Duration::from_secs(5))
        .await
        .unwrap();
}

/// `T-009-26`: the runner's own lock timeout bounds a migration that sets none. Migration 5 sets
/// no `lock_timeout`; behind a reader holding `received_log`, with `lock_timeout_ms` at 300, it
/// gives up well within 0006's own 5 s, says why, and records nothing; the versions before it
/// keep their backfilled checksums; the next start, the table free, applies 5 and 6.
#[tokio::test(flavor = "multi_thread")]
async fn t_009_26_the_runner_bounds_every_migration_with_its_lock_timeout() {
    let Some((_pg, url)) = server().await else {
        return;
    };
    let (db_url, mut db) = database(&url, "at_four").await;
    at_version_without_checksums(&mut db, 4).await;
    let (mut holder, conn) = tokio_postgres::connect(&db_url, tokio_postgres::NoTls)
        .await
        .unwrap();
    tokio::spawn(conn);
    let held = holder.transaction().await.unwrap();
    held.batch_execute("LOCK TABLE received_log IN ACCESS SHARE MODE")
        .await
        .unwrap();
    let started = Instant::now();
    // An outer bound, so a runner that sets no timeout fails this test instead of hanging it.
    let e = tokio::time::timeout(
        Duration::from_secs(15),
        PgStore::connect_with(
            options(&db_url),
            &ingress::config::MigrationsConfig {
                lock_timeout_ms: 300,
                ..Default::default()
            },
        ),
    )
    .await
    .expect("the runner bounded the wait")
    .err()
    .expect("the migration gives up behind the lock")
    .to_string();
    let took = started.elapsed();
    assert!(
        took < Duration::from_secs(3),
        "bounded by the runner's 300 ms, not left to wait: {took:?}"
    );
    assert!(
        e.contains("migration 5 (0005_pass_through.sql)")
            && e.contains("gave up waiting for a lock")
            && e.contains("300 ms")
            && e.contains("the next start retries"),
        "{e}"
    );
    let rows = recorded(&db).await;
    assert_eq!(
        rows.iter().map(|r| r.0).collect::<Vec<_>>(),
        vec![1, 2, 3, 4],
        "a migration that gave up records nothing"
    );
    assert!(rows.iter().all(|r| r.2.is_some()), "1–4 were backfilled");
    let column: Option<i32> = db
        .query_opt(
            "SELECT 1 FROM information_schema.columns WHERE table_name = 'received_log' AND \
             column_name = 'pass_through'",
            &[],
        )
        .await
        .unwrap()
        .map(|r| r.get(0));
    assert_eq!(column, None, "nothing of 5 was applied");
    held.commit().await.unwrap();
    PgStore::connect(options(&db_url)).await.unwrap();
    assert_eq!(
        recorded(&db).await.len(),
        ingress::store::migrate::MIGRATIONS.len()
    );
}

/// `T-009-26`: `migrations.on_start = "verify"` changes nothing and refuses to start while a
/// migration is pending, on an empty database and on one at 4; `aledgent-ingress migrate
/// --status` lists each version's state and `--apply` applies the pending ones and exits, with
/// `serve`'s configuration and database URL; verify then starts.
#[tokio::test(flavor = "multi_thread")]
async fn t_009_26_verify_refuses_pending_and_migrate_status_and_apply() {
    use ingress::config::{MigrationsConfig, OnStart};
    let Some((_pg, url)) = server().await else {
        return;
    };
    let verify = MigrationsConfig {
        on_start: OnStart::Verify,
        ..Default::default()
    };
    let (empty_url, empty) = database(&url, "empty").await;
    let e = PgStore::connect_with(options(&empty_url), &verify)
        .await
        .err()
        .expect("verify refuses an empty database")
        .to_string();
    assert!(
        e.contains(&format!("{:?} are pending", versions_after(0))),
        "{e}"
    );
    let exists: bool = empty
        .query_one("SELECT to_regclass('ingress_schema') IS NOT NULL", &[])
        .await
        .unwrap()
        .get(0);
    assert!(!exists, "verify changes nothing");

    let (db_url, mut db) = database(&url, "at_four").await;
    at_version_without_checksums(&mut db, 4).await;
    let before = recorded(&db).await;
    let e = PgStore::connect_with(options(&db_url), &verify)
        .await
        .err()
        .expect("verify refuses pending migrations")
        .to_string();
    assert!(
        e.contains(&format!("{:?} are pending", versions_after(4)))
            && e.contains("migrate --apply"),
        "{e}"
    );
    assert_eq!(recorded(&db).await, before, "verify writes nothing");

    // The binary, with a configuration as `serve` reads it.
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("ingress.json");
    std::fs::write(
        &config,
        json!({
            "listen": "127.0.0.1:0",
            "replica": "r0",
            "profile": "development",
            "require_sync_standby": false,
            "tokens": [],
            "tick_interval_ms": 1000,
            "decision_bound_ms": 60000,
            "core_journal": "unused",
            "genesis": {
                "assets": [],
                "operator_verifier": "00",
                "deployment": "test",
                "decision_bound_ms": 60000,
                "review_service_level_ms": 60000,
                "genesis_op": "00",
                "genesis_at": 0
            },
            "migrations": { "on_start": "verify", "lock_timeout_ms": 2000 }
        })
        .to_string(),
    )
    .unwrap();
    let run = |flag: &str| {
        std::process::Command::new(env!("CARGO_BIN_EXE_aledgent-ingress"))
            .args(["migrate", "--config", config.to_str().unwrap(), flag])
            .env("ALEDGENT_INGRESS_DATABASE_URL", &db_url)
            .env_remove("ALEDGENT_INGRESS_SEALING_KEY")
            .output()
            .unwrap()
    };
    let out = run("--status");
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    let state = |text: &str, v: &str| {
        text.lines()
            .find(|l| l.split_whitespace().next() == Some(v))
            .and_then(|l| l.split_whitespace().nth(2))
            .unwrap_or_else(|| panic!("no line for {v}: {text}"))
            .to_owned()
    };
    for v in ["1", "2", "3", "4"] {
        assert_eq!(state(&text, v), "applied/backfillable", "{text}");
    }
    for v in ["5", "6"] {
        assert_eq!(state(&text, v), "pending", "{text}");
    }
    assert!(
        text.contains(&format!(
            "0 applied/ok, 4 applied/backfillable, {} pending, 0 mismatch",
            versions_after(4).len()
        )),
        "{text}"
    );
    assert_eq!(recorded(&db).await, before, "--status writes nothing");

    let out = run("--apply");
    assert!(out.status.success(), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains(&format!(
            "applied {:?}, checksums backfilled [1, 2, 3, 4]",
            versions_after(4)
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "{} applied/ok, 0 applied/backfillable, 0 pending",
            versions_after(0).len()
        )),
        "{text}"
    );

    // An edited applied migration shows as such, and `--apply` refuses it.
    db.execute(
        "UPDATE ingress_schema SET checksum = repeat('f', 64) WHERE version = 2",
        &[],
    )
    .await
    .unwrap();
    let out = run("--status");
    assert!(!out.status.success(), "a mismatch fails --status");
    let text = String::from_utf8(out.stdout).unwrap();
    assert_eq!(state(&text, "2"), "mismatch", "{text}");
    let out = run("--apply");
    assert!(!out.status.success());
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("migration 2 (0002_passkeys.sql)"), "{err}");
    db.execute(
        "UPDATE ingress_schema SET checksum = $1 WHERE version = 2",
        &[&sha256_hex(BEFORE_0007[1].1)],
    )
    .await
    .unwrap();

    // Everything applied: verify starts, and changes nothing.
    let before = recorded(&db).await;
    PgStore::connect_with(options(&db_url), &verify)
        .await
        .expect("verify starts on an up-to-date schema");
    assert_eq!(recorded(&db).await, before);
}

/// `T-009-26`: migrators that start together on an empty database all succeed (creating
/// `ingress_schema` is serialised), and a serving role
/// (rows only, `SELECT` on `ingress_schema`) starts in `"verify"`, passes `apply` when there is
/// nothing to do, and is refused a pending migration with the server's own reason.
#[tokio::test(flavor = "multi_thread")]
async fn t_009_26_concurrent_migrators_and_a_row_only_role() {
    use ingress::config::{MigrationsConfig, OnStart};
    use ingress::store::migrate::{self, MIGRATIONS, Migration};
    let Some((_pg, url)) = server().await else {
        return;
    };
    let (db_url, db) = database(&url, "together").await;
    let runners: Vec<_> = (0..4)
        .map(|_| {
            let url = db_url.clone();
            tokio::spawn(async move {
                let mut c = migrate::connect(&url, Profile::Development).await?;
                migrate::apply(&mut c, MIGRATIONS, Duration::from_secs(5)).await
            })
        })
        .collect();
    let mut applied = Vec::new();
    for r in runners {
        let done = r.await.unwrap().expect("every migrator succeeds");
        applied.extend(done.applied);
    }
    applied.sort();
    assert_eq!(applied, versions_after(0), "each version applied once");
    assert_eq!(recorded(&db).await.len(), MIGRATIONS.len());

    // The serving role, granted as the operations page says.
    db.batch_execute(
        "CREATE ROLE ingress_app LOGIN PASSWORD 'app'; \
         GRANT CONNECT ON DATABASE together TO ingress_app; \
         GRANT USAGE ON SCHEMA public TO ingress_app; \
         GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO ingress_app; \
         REVOKE INSERT, UPDATE, DELETE ON ingress_schema FROM ingress_app; \
         GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO ingress_app; \
         REVOKE CREATE ON SCHEMA public FROM PUBLIC;",
    )
    .await
    .unwrap();
    let app_url = db_url.replacen("postgres:postgres@", "ingress_app:app@", 1);
    assert_ne!(app_url, db_url);
    let verify = MigrationsConfig {
        on_start: OnStart::Verify,
        ..Default::default()
    };
    let s = PgStore::connect_with(options(&app_url), &verify)
        .await
        .expect("the serving role starts in verify");
    s.append(command(80, "app", "k80", false)).await.unwrap();
    PgStore::connect(options(&app_url))
        .await
        .expect("apply with nothing to do only reads");
    let mut c = migrate::connect(&app_url, Profile::Development)
        .await
        .unwrap();
    let mut with_next: Vec<Migration> = MIGRATIONS.to_vec();
    with_next.push(Migration {
        version: 1_000,
        name: "1000_next.sql",
        sql: "CREATE TABLE next_one (x INTEGER);",
    });
    let e = migrate::apply(&mut c, &with_next, Duration::from_secs(5))
        .await
        .expect_err("the serving role cannot change the schema")
        .to_string();
    assert!(
        e.contains("permission denied") && e.contains("SQLSTATE 42501"),
        "{e}"
    );
    assert_eq!(
        recorded(&db).await.len(),
        MIGRATIONS.len(),
        "nothing recorded"
    );
}
