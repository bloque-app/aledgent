// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The `verify-genesis` gate at ingress (`F-036` `T-036-25`, `REQ-RC-6`, rulings `a0022-q2` and
//! `a0022-q2b`): while the core says a chainspec book is not open, every command is refused
//! `ERR-InitialStateNotSealed` before it is logged, and reads are answered.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use axum::http::StatusCode;
use ingress::auth::{Authenticator, CredentialKind, Token};
use ingress::boundary::{
    CallerBody, Core, CoreError, CoreInput, CoreReply, CredentialBody, OutboxPage, Query,
    QueryReply,
};
use ingress::clock::{ManualClock, SharedClock};
use ingress::delivery::Transport;
use ingress::inprocess::{InProcessCore, MemJournal};
use ingress::service::Replica;
use ingress::store::mem::MemStore;
use ingress::store::{InputKind, Store as _};
use ingress::surface::router;
use ingress::testkit::{Recipients, T0, TestCore, call, fast_config, genesis, test_clerk};
use ledger_core::account::Time;
use serde_json::json;

/// The in-process core behind a gate the test opens.
#[derive(Clone)]
struct Gated {
    inner: TestCore,
    open: Arc<AtomicBool>,
    asked: Arc<std::sync::atomic::AtomicU64>,
}

impl Core for Gated {
    async fn apply(&self, input: &CoreInput) -> Result<CoreReply, CoreError> {
        self.inner.apply(input).await
    }

    async fn apply_batch(&self, inputs: &[CoreInput]) -> Vec<Result<CoreReply, CoreError>> {
        self.inner.apply_batch(inputs).await
    }

    async fn outbox(
        &self,
        cursor: Option<&str>,
        undelivered: bool,
        now: Time,
    ) -> Result<OutboxPage, CoreError> {
        self.inner.outbox(cursor, undelivered, now).await
    }

    async fn last_tick(&self) -> Result<Option<Time>, CoreError> {
        self.inner.last_tick().await
    }

    async fn query(
        &self,
        caller: &CallerBody,
        query: &Query,
        now: Time,
    ) -> Result<QueryReply, CoreError> {
        self.inner.query(caller, query, now).await
    }

    async fn credential(&self, credential: &str) -> Result<Option<CredentialBody>, CoreError> {
        self.inner.credential(credential).await
    }

    async fn party_of(&self, principal: &str) -> Result<Option<String>, CoreError> {
        self.inner.party_of(principal).await
    }

    async fn genesis_open(&self) -> Result<bool, CoreError> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Ok(self.open.load(Ordering::SeqCst))
    }
}

/// `T-036-25`, `REQ-RC-6`, `a0022-q2b`: until the report is installed a command is refused
/// `ERR-InitialStateNotSealed` (409) and nothing of it is logged, while a read is answered; once the
/// core says the book is open, the same command is taken, and ingress stops asking.
#[tokio::test(flavor = "multi_thread")]
async fn t_036_25_commands_wait_for_the_verify_genesis_report_and_reads_do_not() {
    let journal = MemJournal::new();
    let inner = InProcessCore::in_memory(genesis(T0), journal).with_clerk(test_clerk());
    inner
        .start()
        .unwrap_or_else(|e| panic!("the core starts: {e}"));
    let founded = inner
        .founded()
        .unwrap_or_else(|| unreachable!("a started core is founded"));
    let open = Arc::new(AtomicBool::new(false));
    let asked = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let core = Gated {
        inner,
        open: Arc::clone(&open),
        asked: Arc::clone(&asked),
    };
    let mut config = fast_config("ingress-0");
    config.genesis_gate = true;
    let store = MemStore::new(Duration::from_millis(50));
    let clock: SharedClock = Arc::new(ManualClock::at(T0));
    let auth = Authenticator::new();
    auth.add_token(
        &Token::new(b"operator-token"),
        &founded.operator.to_did(),
        &founded.operator_credential.to_did(),
        CredentialKind::Service,
    )
    .unwrap_or_else(|e| panic!("a token registers: {e}"));
    let transport = Arc::new(Recipients::new(Arc::clone(&clock))) as Arc<dyn Transport>;
    let replica = Replica::start(store.clone(), core, clock, auth, transport, config);
    let router = router(Arc::clone(&replica.shared));
    let open_identity = || {
        (
            "POST",
            "/v1/identities",
            Some("operator-token"),
            [("client-key", "identity-gated")],
            Some(json!({ "kind": "person", "verifier": "0a" })),
        )
    };

    let (m, p, t, h, b) = open_identity();
    let (s, v) = call(&router, m, p, t, &h, b).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    assert_eq!(v["error"], "ERR-InitialStateNotSealed", "{v}");
    let (s, v) = call(
        &router,
        "POST",
        "/v1/principals/self/answering",
        Some("operator-token"),
        &[],
        Some(json!({ "decided": true })),
    )
    .await;
    assert_eq!(
        (s, v["error"].clone()),
        (StatusCode::CONFLICT, json!("ERR-InitialStateNotSealed")),
        "set_answering is a command too"
    );
    let (s, v) = call(
        &router,
        "GET",
        "/v1/assets",
        Some("operator-token"),
        &[],
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "a read is answered meanwhile: {v}");
    let rows = store
        .rows()
        .await
        .unwrap_or_else(|e| panic!("the log reads: {e}"));
    assert!(
        rows.iter().all(|r| r.new.kind != InputKind::Command),
        "nothing refused by the gate is logged"
    );

    open.store(true, Ordering::SeqCst);
    let (m, p, t, h, b) = open_identity();
    let (s, v) = call(&router, m, p, t, &h, b).await;
    assert_eq!(s, StatusCode::OK, "the book is open: {v}");
    let before = asked.load(Ordering::SeqCst);
    let (m, p, t, h, b) = open_identity();
    let (s, _) = call(&router, m, p, t, &h, b).await;
    assert_eq!(s, StatusCode::OK, "a repeat is answered from the log");
    assert_eq!(
        asked.load(Ordering::SeqCst),
        before,
        "once open, ingress does not ask again"
    );
}
