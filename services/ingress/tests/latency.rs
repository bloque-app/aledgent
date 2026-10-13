// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The latency bounds as the received-log records them (`T-009-25`; `NFR-1`, `NFR-15`, `NFR-16`;
//! owner ruling of 2026-10-13): a command's row carries when its request arrived, when ingress
//! replied to it, and when it completed, so the bounds are read from ingress's own database.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use ingress::boundary::{
    CallerBody, Core, CoreError, CoreInput, CoreReply, CredentialBody, InputBody, OutboxItem,
    OutboxPage, Outcome, Query, QueryReply,
};
use ingress::clock::Clock as _;
use ingress::inprocess::{InProcessCore, MemJournal};
use ingress::pass_through::key_of;
use ingress::service::deliver_pass;
use ingress::store::mem::MemStore;
use ingress::store::{Row, Status, Store};
use ingress::testkit::{Deployment, Party, T0, TestCore, USD, fast_config, genesis, test_clerk};
use ingress::wire::{CommandBody, CommandResult, NotificationBody, ShownBody, StateClassBody};
use ledger_core::account::Time;
use serde_json::{Value, json};

/// The row of the command a reply names, once `done` holds of it.
async fn row_when<S: Store, C: Core + Clone>(
    d: &Deployment<S, C>,
    reply: &Value,
    done: impl Fn(&Row) -> bool,
) -> Row {
    let reference = reply["operation_reference"].as_str().unwrap_or_default();
    let key = key_of(reference).unwrap_or_else(|| panic!("an operation reference: {reply}"));
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(row) = d.store.get(key).await.unwrap()
            && done(&row)
        {
            return row;
        }
        assert!(
            Instant::now() < until,
            "the row of {reference} never got there"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn stamped(r: &Row) -> bool {
    r.status == Status::Decided && r.replied_at.is_some() && r.completed_at.is_some()
}

/// `NFR-1`, `NFR-15`, `T-009-25`: an acknowledged transfer's row records its arrival, the
/// acknowledgement's reply, and its completion at the decision that settled it.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_1_an_acknowledged_transfer_records_its_reply_and_completion() {
    let d = Deployment::start().await;
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 0).await;
    d.clock.advance(7);
    let (s, r) = d
        .call(
            "POST",
            "/v1/transfers",
            Some(&alice.token),
            &[("client-key", "t-1")],
            Some(json!({ "legs": [{
                "from": alice.account, "to": bob.account, "asset": USD, "amount": "5"
            }] })),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{r}");
    let row = row_when(&d, &r, stamped).await;
    assert!(!row.new.synchronous);
    let arrived = row.new.arrived_at.expect("a command records its arrival");
    let replied = row.replied_at.expect("stamped");
    assert!(arrived <= row.new.received_at && row.new.received_at <= replied);
    // Completed by its decision, in the same commit.
    assert_eq!(row.completed_at, row.decided_at);
}

/// `NFR-16`, `NFR-15`, `T-009-25`: a command answered with its decision (`REQ-CS-8`) is stamped
/// at the reply that carries it, and completes at that decision.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_16_a_decision_answered_command_records_the_reply_that_carries_it() {
    let d = Deployment::start().await;
    let alice = d.party("alice", 0).await;
    d.clock.advance(11);
    let (s, r) = d
        .call(
            "POST",
            "/v1/accounts",
            Some(&d.operator.token),
            &[("client-key", "second-account")],
            Some(json!({ "owner": alice.identity, "kind": "customer", "custody": "delegated" })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let row = row_when(&d, &r, stamped).await;
    assert!(row.new.synchronous);
    assert_eq!(row.new.arrived_at, Some(T0 + 11));
    assert_eq!(row.completed_at, row.decided_at);
    // The reply carried the decision: it went out once the decision was stored.
    assert!(row.replied_at >= row.decided_at);
}

/// `NFR-15` (`Reserved` for a reservation), `T-009-25`: a reservation completes at the decision
/// that reserves; a repeat's reply does not move the first reply's stamp.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_15_a_reservation_completes_at_reserved_and_a_repeat_keeps_the_stamp() {
    let d = Deployment::start().await;
    let alice = d.party("alice", 1_000).await;
    let body = json!({ "payer": alice.account, "asset": USD, "amount": "10" });
    let key = [("client-key", "r-1")];
    let (s, r) = d
        .call(
            "POST",
            "/v1/reservations",
            Some(&alice.token),
            &key,
            Some(body.clone()),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{r}");
    let first = row_when(&d, &r, stamped).await;
    assert!(first.new.reserves);
    assert_eq!(first.completed_at, first.decided_at);
    d.clock.advance(1_000);
    let (s, again) = d
        .call(
            "POST",
            "/v1/reservations",
            Some(&alice.token),
            &key,
            Some(body),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{again}");
    assert_eq!(again["state"], "Reserved");
    // Give a stamp of the repeat, were there one, time to land.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let row = row_when(&d, &r, stamped).await;
    assert_eq!(row.replied_at, first.replied_at);
    assert_eq!(row.completed_at, first.completed_at);
}

/// The in-process core, with notifications of the test's own added to the outbox's first page,
/// and a withdrawal decided `Reserved` as the core decides one with a settlement layer bound:
/// a withdrawal's path to `Observed` without running one.
#[derive(Clone)]
struct Relaying {
    inner: TestCore,
    extra: Arc<Mutex<Vec<OutboxItem>>>,
}

impl Core for Relaying {
    async fn apply(&self, input: &CoreInput) -> Result<CoreReply, CoreError> {
        if let InputBody::Command {
            command, operation, ..
        } = &input.body
            && let Some((seq, state)) = match command.as_ref() {
                CommandBody::Withdraw(_) => Some((1_000_000, "Reserved")),
                CommandBody::SubmitOutbound(_) => Some((1_000_001, "Submitted")),
                _ => None,
            }
        {
            return Ok(CoreReply {
                input_seq: seq,
                outcome: Outcome::Decided {
                    operation_reference: operation.clone(),
                    state: Some(ShownBody {
                        state: state.into(),
                        class: StateClassBody::OpenNonRefunding,
                        terminal: false,
                    }),
                    result: CommandResult::default(),
                },
            });
        }
        self.inner.apply(input).await
    }

    async fn outbox(
        &self,
        cursor: Option<&str>,
        undelivered: bool,
        now: Time,
    ) -> Result<OutboxPage, CoreError> {
        let mut page = self.inner.outbox(cursor, undelivered, now).await?;
        if cursor.is_none() {
            page.items
                .extend(self.extra.lock().unwrap().drain(..).collect::<Vec<_>>());
        }
        Ok(page)
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
}

/// `NFR-15` (amendment 0018), `T-009-25`: a withdrawal's clock starts when the `submit_outbound`
/// that submits its leg arrives (owner ruling on PR #235), and it completes when its `Observed` is
/// relayed to its requester: not at a state before it, not when another principal is told, and
/// not again when a later state is relayed.
#[tokio::test(flavor = "multi_thread")]
async fn nfr_15_a_withdrawal_completes_when_observed_is_relayed_to_its_requester() {
    let config = fast_config("ingress-0");
    let journal = MemJournal::new();
    let inner = InProcessCore::in_memory(genesis(T0), journal.clone()).with_clerk(test_clerk());
    inner.start().expect("the core starts");
    let founded = inner.founded().expect("founded");
    let core = Relaying {
        inner,
        extra: Arc::default(),
    };
    let store = MemStore::new(config.lease);
    let mut d = Deployment::assemble(store, config, core.clone(), founded, journal).await;
    let alice = d.party("alice", 0).await;
    let bob = d.party("bob", 0).await;
    // A withdrawal, through the surface and the forwarder: decided `Reserved`, it is not complete.
    let (s, r) = d
        .call(
            "POST",
            "/v1/withdrawals",
            Some(&alice.token),
            &[("client-key", "w-1")],
            Some(json!({ "payer": alice.account, "asset": USD, "amount": "5" })),
        )
        .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{r}");
    let row = row_when(&d, &r, |r| {
        r.status == Status::Decided && r.replied_at.is_some()
    })
    .await;
    assert!(row.new.reserves);
    assert_eq!(
        row.completed_at, None,
        "a withdrawal is not complete at Reserved"
    );
    assert_eq!(row.submitted_at, None, "not submitted yet");
    let op_ref = row.new.op_ref;
    let operation = r["operation_reference"].as_str().unwrap().to_owned();
    // The requester submits the leg a while later: the clock starts at that command's arrival.
    d.clock.advance(5_000);
    let submitted_at = d.clock.now();
    let (s, v) = d
        .call(
            "POST",
            &format!("/v1/withdrawals/{operation}/submit"),
            Some(&alice.token),
            &[("client-key", "w-1-submit")],
            Some(json!({ "destination": { "account": hex::encode([7u8; 32]) } })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let until = Instant::now() + Duration::from_secs(10);
    while d
        .store
        .get(op_ref)
        .await
        .unwrap()
        .unwrap()
        .submitted_at
        .is_none()
    {
        assert!(Instant::now() < until, "the submission was never stamped");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let row = d.store.get(op_ref).await.unwrap().unwrap();
    assert_eq!(row.submitted_at, Some(submitted_at));
    assert_eq!(row.completed_at, None, "Submitted is not completion");
    assert!(d.quiesce(Duration::from_secs(10)).await);
    // Every step from here is the test's: no worker delivers.
    d.replica.kill();
    let relay = |id: u8, state: &str, terminal: bool, to: &Party| {
        core.extra.lock().unwrap().push(OutboxItem {
            notification: NotificationBody {
                id: hex::encode([id; 32]),
                operation_reference: operation.clone(),
                state: state.into(),
                class: if terminal {
                    StateClassBody::Terminal
                } else {
                    StateClassBody::OpenNonRefunding
                },
                terminal,
                outcome: None,
                error: None,
                anchor: None,
                reference: None,
            },
            recipient: Some(to.identity.clone()),
            created_at: d.clock.now(),
            delivered_at: None,
        });
    };
    let completed = || async { d.store.get(op_ref).await.unwrap().unwrap().completed_at };
    let shared = Arc::clone(&d.replica.shared);

    d.clock.advance(10);
    relay(1, "Submitted", false, &alice);
    deliver_pass(&shared).await.unwrap();
    assert_eq!(completed().await, None, "Submitted is not completion");

    d.clock.advance(10);
    relay(2, "Observed", false, &bob);
    deliver_pass(&shared).await.unwrap();
    assert_eq!(completed().await, None, "relayed to another principal");

    d.clock.advance(10);
    let observed_at = d.clock.now();
    relay(3, "Observed", false, &alice);
    deliver_pass(&shared).await.unwrap();
    assert_eq!(completed().await, Some(observed_at), "Observed, relayed");

    d.clock.advance(10);
    relay(4, "Settled", true, &alice);
    deliver_pass(&shared).await.unwrap();
    assert_eq!(completed().await, Some(observed_at), "set once");
    assert_eq!(
        d.store.get(op_ref).await.unwrap().unwrap().submitted_at,
        Some(submitted_at),
        "the clock's start is kept"
    );
    let delivered = d.recipients.delivered_ids();
    for id in 1..=4u8 {
        assert!(delivered.contains(&hex::encode([id; 32])), "{id} delivered");
    }
}
