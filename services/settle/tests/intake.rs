// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Intake against a scripted ingress (`T-016-04`, `REQ-SO-5`, `REQ-SO-6`, `INV-13`): what the
//! core could never send, sent anyway, through the call outbox (`F-027`). An instruction without a
//! reservation, or whose digest does not match its envelope, is never acknowledged; a second
//! digest for one operation reference raises an alarm and nothing else.

use std::sync::{Arc, Mutex};

use ingress::clock::ManualClock;
use ledger_core::asset::{AssetId, Minor, NonZeroMinor};
use ledger_core::id::OperationId;
use ledger_core::settlement::{Destination, instruction_digest};
use ledger_kernel::posting::OperationRef;
use serde_json::{Value, json};
use settle::adapter::Adapter;
use settle::chain::fake::{FakeChain, FakeConfig};
use settle::link::{LinkError, Transport};
use settle::signer::{BoxFuture, Signer};
use settle::store::Store;
use settle::store::mem::MemStore;
use settle::testkit::{
    INSTANCE, TEST_LANE_SECRET, TEST_PASS_USER, adapter_config, chain_usd, pass_lane,
};

/// Ingress, scripted: the outbox it serves, and every command it was sent.
#[derive(Clone, Default)]
struct Scripted {
    outbox: Arc<Mutex<Vec<Value>>>,
    sent: Arc<Mutex<Vec<(String, Value)>>>,
}

impl Transport for Scripted {
    fn call<'a>(
        &'a self,
        method: &'a str,
        path: &'a str,
        _client_key: Option<&'a str>,
        body: Option<Value>,
    ) -> BoxFuture<'a, Result<(u16, Value), LinkError>> {
        Box::pin(async move {
            if method == "GET" {
                return Ok((200, Value::Array(self.outbox.lock().unwrap().clone())));
            }
            self.sent
                .lock()
                .unwrap()
                .push((path.to_owned(), body.unwrap_or(Value::Null)));
            Ok((
                200,
                json!({
                    "operation_reference": "x", "terminal": false,
                    "client_key": "k", "client_key_derived": false,
                    "state": "Submitted", "result": {}
                }),
            ))
        })
    }
}

impl Scripted {
    fn acks(&self) -> Vec<Value> {
        self.sent
            .lock()
            .unwrap()
            .iter()
            .filter(|(p, _)| p == "/v1/settlement/acknowledgements")
            .map(|(_, b)| b.clone())
            .collect()
    }
}

fn op(n: u8) -> OperationId {
    OperationId::from_ref(OperationRef([n; 16]))
}

fn body(op: &OperationId, amount: u64, expiry: u64, reserved: bool) -> Value {
    let asset = AssetId::parse("USD").unwrap();
    let to = Destination::Account([0xAB; 32]);
    let digest = instruction_digest(
        op,
        &asset,
        NonZeroMinor::new(Minor::new(amount)).unwrap(),
        to,
        expiry,
    );
    // A withdrawal of the call outbox (`F-027`): its envelope, and the instruction's identity.
    json!({
        "reference": hex::encode(settle::adapter::chain_ref(op)),
        "target": INSTANCE,
        "call": { "withdraw": {
            "asset": "USD",
            "amount": amount.to_string(),
            "destination": { "account": hex::encode([0xAB; 32]) },
        } },
        "expiry": expiry,
        "acknowledged": false,
        "reserved": reserved,
        "instruction": hex::encode(digest),
    })
}

fn adapter(t: Scripted) -> Adapter<FakeChain, MemStore, Scripted> {
    let lane: Arc<dyn Signer> = pass_lane(&TEST_LANE_SECRET, TEST_PASS_USER);
    let chain = FakeChain::new(FakeConfig::default());
    chain.register_instance(INSTANCE, &[lane.account()]);
    chain.set_decimals(&chain_usd(), "USD", 2);
    chain.set_native(lane.account(), 1_000_000);
    chain.fund_position(INSTANCE, &chain_usd(), 1_000_000);
    chain.advance(1);
    Adapter::new(
        adapter_config(FakeConfig::default().genesis),
        chain,
        MemStore::new(),
        t,
        vec![lane],
        Arc::new(ManualClock::at(1_000)),
    )
}

async fn alerts(a: &Adapter<FakeChain, MemStore, Scripted>) -> Vec<String> {
    a.store()
        .alerts()
        .await
        .unwrap()
        .into_iter()
        .map(|x| x.key)
        .collect()
}

#[tokio::test]
async fn inv_13_an_instruction_without_a_reservation_is_never_acknowledged() {
    let t = Scripted::default();
    t.outbox.lock().unwrap().push(body(&op(1), 10, 50, false));
    let mut a = adapter(t.clone());
    a.step().await.unwrap();
    assert!(t.acks().is_empty());
    assert!(a.store().instructions().await.unwrap().is_empty());
    assert!(a.store().extrinsics().await.unwrap().is_empty());
    assert!(
        alerts(&a)
            .await
            .contains(&format!("instruction:{}", op(1).to_did()))
    );
}

#[tokio::test]
async fn req_so_5_an_instruction_whose_digest_does_not_match_its_fields_is_never_acknowledged() {
    let t = Scripted::default();
    let mut b = body(&op(2), 10, 50, true);
    b["call"]["withdraw"]["amount"] = json!("11");
    t.outbox.lock().unwrap().push(b);
    let mut a = adapter(t.clone());
    a.step().await.unwrap();
    assert!(t.acks().is_empty());
    assert!(a.store().instructions().await.unwrap().is_empty());
}

#[tokio::test]
async fn req_so_6_a_second_digest_for_one_reference_raises_an_alarm_and_nothing_else() {
    let t = Scripted::default();
    t.outbox.lock().unwrap().push(body(&op(3), 10, 50, true));
    let mut a = adapter(t.clone());
    a.step().await.unwrap();
    assert_eq!(t.acks().len(), 1);
    let before = a.store().instructions().await.unwrap();
    // The same reference, another instruction.
    *t.outbox.lock().unwrap() = vec![body(&op(3), 10, 60, true)];
    a.step().await.unwrap();
    assert_eq!(t.acks().len(), 1, "nothing else: no second acknowledgement");
    let after = a.store().instructions().await.unwrap();
    assert_eq!(before[0].digest, after[0].digest);
    assert_eq!(a.store().extrinsics().await.unwrap().len(), 1);
    assert!(
        alerts(&a)
            .await
            .contains(&format!("digest:{}", op(3).to_did()))
    );
}
