// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The adapter's PostgreSQL store (the accepted proposal; `REQ-SA-5`, `REQ-SA-6`), against a real
//! server in a container (testcontainers; Docker on the CI runner): its rules, and a withdrawal
//! end to end with a crash between storing and broadcasting.
//!
//! Without Docker these tests say so and pass, except under CI (`CI` set), where a missing Docker
//! fails them.

use ingress::config::Profile;
use settle::adapter::CrashPoint;
use settle::chain::{Mortality, SignedExtrinsic, WithdrawCall};
use settle::store::pg::PgStore;
use settle::store::{
    ExtrinsicRow, ExtrinsicStatus, GapRow, Inserted, InstructionRow, InstructionState, LaneRow,
    ObservationRow, Store, StoreError,
};
use settle::testkit::{Kit, chain_usd};
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::ContainerAsync;
use testcontainers_modules::testcontainers::runners::AsyncRunner;

async fn server() -> Option<(ContainerAsync<Postgres>, String)> {
    match Postgres::default().start().await {
        Ok(c) => {
            let port = c.get_host_port_ipv4(5432).await.ok()?;
            let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");
            Some((c, url))
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

fn instruction(op: &str, digest: &str) -> InstructionRow {
    InstructionRow {
        operation: op.into(),
        digest: digest.into(),
        asset: "USD".into(),
        amount: 5,
        call: WithdrawCall {
            instance: 1,
            operation_ref: [1; 32],
            to: ledger_core::settlement::Destination::Account([2; 32]),
            asset: chain_usd(),
            amount: 5,
        },
        expiry: 50,
        acknowledged: false,
        state: InstructionState::Received,
        received_at: 1,
        updated_at: 1,
    }
}

fn extrinsic(op: &str, byte: u8) -> ExtrinsicRow {
    ExtrinsicRow {
        operation: op.into(),
        lane: "lane".into(),
        nonce: 0,
        era: Mortality::at(16, 3),
        extrinsic: SignedExtrinsic {
            bytes: vec![byte; 8],
            hash: [byte; 32],
        },
        status: ExtrinsicStatus::Signed,
        signed_at: 2,
        batch: vec![op.into()],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn req_sa_5_the_postgres_store_keeps_its_rules() {
    let Some((_c, url)) = server().await else {
        return;
    };
    let s = PgStore::connect(&url, 4, Profile::Development)
        .await
        .unwrap();
    // A second connection applies no migration twice.
    let _again = PgStore::connect(&url, 2, Profile::Development)
        .await
        .unwrap();
    // The lease: one holder at a time, fenced by epoch.
    let lease = s.acquire("a", 0, 100).await.unwrap().unwrap();
    assert_eq!(s.acquire("b", 50, 100).await.unwrap(), None);
    assert!(s.renew("a", lease.epoch, 60, 100).await.unwrap());
    let taken = s.acquire("b", 200, 100).await.unwrap().unwrap();
    assert!(taken.epoch > lease.epoch);
    assert!(!s.renew("a", lease.epoch, 210, 100).await.unwrap());
    // Instructions: the same reference with another digest is a conflict, and changes nothing.
    let i = instruction("op1", "d1");
    assert_eq!(s.insert_instruction(&i).await.unwrap(), Inserted::New);
    assert!(matches!(
        s.insert_instruction(&i).await.unwrap(),
        Inserted::Same(_)
    ));
    assert!(matches!(
        s.insert_instruction(&instruction("op1", "d2"))
            .await
            .unwrap(),
        Inserted::Conflict(_)
    ));
    // One extrinsic per operation reference; a stale epoch cannot store one.
    let lane = LaneRow {
        lane: "lane".into(),
        next_nonce: 1,
        in_flight: Some("op1".into()),
    };
    assert_eq!(
        s.store_signed(lease.epoch, &extrinsic("op1", 1), &lane, &i)
            .await,
        Err(StoreError::Fenced)
    );
    s.store_signed(taken.epoch, &extrinsic("op1", 1), &lane, &i)
        .await
        .unwrap();
    assert_eq!(
        s.store_signed(taken.epoch, &extrinsic("op1", 2), &lane, &i)
            .await,
        Err(StoreError::Duplicate)
    );
    assert_eq!(
        s.instructions().await.unwrap()[0].state,
        InstructionState::Signed
    );
    // Its bytes never change; its status does.
    let mut e = extrinsic("op1", 1);
    e.status = ExtrinsicStatus::Broadcast { attempts: 1 };
    s.put_extrinsic(&e).await.unwrap();
    assert!(s.put_extrinsic(&extrinsic("op1", 3)).await.is_err());
    assert_eq!(s.extrinsics().await.unwrap(), vec![e]);
    // The cursor never goes back; gaps and alerts are kept.
    s.advance_cursor(1, 5).await.unwrap();
    s.advance_cursor(1, 3).await.unwrap();
    assert_eq!(s.cursor(1).await.unwrap(), 5);
    // T-043-04: a cursor from before the accepted head's columns has no accepted head; one is
    // accepted beside it; the chain's reset moves the cursor back and forgets the head.
    assert_eq!(s.accepted_head(1).await.unwrap(), None);
    let head = settle::store::AcceptedHead {
        position: 5,
        digest: [0xD5; 32],
        block: 40,
    };
    s.accept_head(1, &head).await.unwrap();
    assert_eq!(s.accepted_head(1).await.unwrap(), Some(head));
    assert_eq!(s.cursor(1).await.unwrap(), 5);
    s.reset_chain(1, 0).await.unwrap();
    assert_eq!(
        (
            s.cursor(1).await.unwrap(),
            s.accepted_head(1).await.unwrap()
        ),
        (0, None)
    );
    s.advance_cursor(1, 5).await.unwrap();
    s.accept_head(2, &head).await.unwrap();
    assert_eq!(s.cursor(2).await.unwrap(), 5);
    let gap = GapRow {
        instance: 1,
        from: 0,
        to: 3,
        reason: "pruned".into(),
        at: 1,
    };
    s.insert_gap(&gap).await.unwrap();
    s.insert_gap(&gap).await.unwrap();
    assert_eq!(s.gaps().await.unwrap(), vec![gap]);
    assert!(s.raise("position:1:9001", "short", 1).await.unwrap());
    assert!(!s.raise("position:1:9001", "short", 2).await.unwrap());
    assert!(s.clear("position:1:9001", 3).await.unwrap());
    assert!(s.raise("position:1:9001", "short again", 4).await.unwrap());
    assert_eq!(s.alerts().await.unwrap()[0].raised, 2);
    // An observation read provisionally, then at the finalized head.
    let mut o = ObservationRow {
        instance: 1,
        seq: 0,
        digest: "ab".into(),
        at: 3,
        raw: "00".into(),
        finalized: false,
        operation: None,
        summary: "x".into(),
        read_at: 1,
    };
    assert!(s.insert_observation(&o).await.unwrap());
    o.finalized = true;
    assert!(!s.insert_observation(&o).await.unwrap());
    assert!(s.observations().await.unwrap()[0].finalized);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn req_sa_6_on_postgres_a_crash_between_store_and_broadcast_replays_the_same_bytes() {
    let Some((_c, url)) = server().await else {
        return;
    };
    let mut k = Kit::start().await;
    let store = PgStore::connect(&url, 4, Profile::Development)
        .await
        .unwrap();
    let mut adapter = k.adapter_on(store.clone(), "settle-pg");
    assert!(adapter.step().await.unwrap().serving);
    let alice = k.d.party("alice", 1_000).await;
    let op = k.withdrawal(&alice, 10, "w").await;
    adapter.crash_at(CrashPoint::AfterPersist);
    assert!(adapter.step().await.is_err());
    let stored = store.extrinsics().await.unwrap();
    assert_eq!(stored.len(), 1);
    let mut adapter = k.adapter_on(store.clone(), "settle-pg");
    for _ in 0..4 {
        adapter.step().await.unwrap();
        k.chain.advance(1);
    }
    adapter.step().await.unwrap();
    assert_eq!(k.state(&alice, &op).await, "Settled");
    assert_eq!(k.chain.extrinsics_for(&Kit::chain_ref(&op)), 1);
    assert_eq!(
        store.extrinsics().await.unwrap()[0].extrinsic,
        stored[0].extrinsic
    );
    k.restart();
}
