// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-012` through ingress: owned custody opened with the recovery mechanisms acknowledged, a
//! credential registered, custody reclaimed with a proven credential, and a recovery initiated
//! and completed on the operator surface with the recovery authority's approval — every step a
//! route of the surface, answered with the core's decision.

use axum::http::StatusCode;
use cipher_clerk::crypto::SecretKey;
use ingress::testkit::{Deployment, RECOVERY_DELAY_MS, TEST_RECOVERY_KEY};
use ledger_core::Refusal;
use ledger_core::custody::{PendingId, reclaim_challenge};
use ledger_core::id::{AccountId, OperationId};
use recovery::{ApprovalRequest, RecoveryService, key::SigningKey};
use serde_json::{Value, json};

fn is_refusal(v: &Value, r: Refusal) -> bool {
    v["error"] == r.code()
}

fn secret(n: u8) -> SecretKey {
    let mut b = [n; 32];
    b[31] = 0;
    SecretKey::from_bytes(b).expect("a canonical scalar")
}

/// The recovery authority of the test deployments: `services/recovery` over the suite's test key.
fn recovery_authority() -> RecoveryService {
    let mut s = [0x5A; 32];
    s[31] = 0;
    RecoveryService::new(SigningKey::from_bytes(s).expect("a canonical test key"))
}

fn hex_sig(sig: &cipher_clerk::crypto::Signature) -> String {
    let mut b = [0u8; 64];
    b[..32].copy_from_slice(&sig.r);
    b[32..].copy_from_slice(&sig.s);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|x| format!("{x:02x}")).collect()
}

#[test]
fn the_test_recovery_key_is_the_test_recovery_authoritys() {
    assert_eq!(recovery_authority().public_key().0, TEST_RECOVERY_KEY);
}

#[tokio::test(flavor = "multi_thread")]
async fn f_012_custody_and_recovery_through_ingress() {
    let d = Deployment::start().await;
    let op = Some(d.operator.token.as_str());
    let alice = d.party("alice", 100).await;
    let a = alice.account.clone().unwrap();
    let token = Some(alice.token.as_str());

    // Owned custody opens only with the recovery mechanisms acknowledged (`AC-A5.3`).
    let (s, terms) = d
        .call(
            "GET",
            &format!("/v1/identities/{}/recovery-terms", alice.identity),
            token,
            &[],
            None,
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{terms}");
    assert_eq!(terms["mechanisms"][0]["mechanism"], "recovery_authority");
    let open = |ack: Option<Value>| {
        let mut body = json!({ "owner": alice.identity, "kind": "customer", "custody": "owned" });
        if let Some(a) = ack {
            body["acknowledged"] = a;
        }
        body
    };
    let (s, r) = d
        .call("POST", "/v1/accounts", token, &[], Some(open(None)))
        .await;
    assert!(is_refusal(&r, Refusal::NotAuthorised), "{s} {r}");
    let (s, r) = d
        .call(
            "POST",
            "/v1/accounts",
            token,
            &[],
            Some(open(Some(terms["digest"].clone()))),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");

    // A credential with a real key joins alice's set.
    let sk = secret(3);
    let (s, r) = d
        .call(
            "POST",
            "/v1/credentials",
            token,
            &[],
            Some(json!({ "marker": "ordinary", "verifier": hex(&sk.public_key().0) })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let credential = r["result"]["credential"].as_str().unwrap().to_owned();

    // Reclaim the delegated account: the proof signs the account's custody epoch.
    let custody = format!("/v1/accounts/{a}/custody");
    let (s, v) = d.call("GET", &custody, token, &[], None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["custody"], "delegated");
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
    let account = AccountId::parse(&a).unwrap();
    let sig =
        ledger_kernel::keys::RootKeys::new([1; 32]).sign(&sk, &reclaim_challenge(&account, epoch));
    let reclaim = format!("/v1/accounts/{a}/custody/reclaim");
    let bad = ledger_kernel::keys::RootKeys::new([1; 32])
        .sign(&sk, &reclaim_challenge(&account, epoch + 1));
    let (s, r) = d
        .call(
            "POST",
            &reclaim,
            token,
            &[],
            Some(json!({
                "proof_credential": credential,
                "signature": hex_sig(&bad),
                "acknowledged": m["digest"],
            })),
        )
        .await;
    assert!(is_refusal(&r, Refusal::CredentialInvalid), "{s} {r}");
    let (s, r) = d
        .call(
            "POST",
            &reclaim,
            token,
            &[],
            Some(json!({
                "proof_credential": credential,
                "signature": hex_sig(&sig),
                "acknowledged": m["digest"],
            })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let (_, v) = d.call("GET", &custody, token, &[], None).await;
    assert_eq!(v["custody"], "owned");
    assert_eq!(v["epoch"].as_u64(), Some(epoch + 1));

    // A recovery, on the operator surface: initiated, visible to the holder, completed with the
    // recovery authority's approval after its delay.
    let new_key = secret(4);
    let new_verifier = new_key.public_key().0.to_vec();
    let (s, r) = d
        .call(
            "POST",
            &format!("/v1/ops/accounts/{a}/recovery"),
            op,
            &[],
            Some(json!({ "verifier": hex(&new_verifier), "justification": "case 7" })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    let pending = r["result"]["pending"].as_str().unwrap().to_owned();
    let (_, v) = d.call("GET", &custody, token, &[], None).await;
    assert_eq!(v["changes"][0]["pending"], pending.as_str());
    assert_eq!(v["changes"][0]["state"], "pending");
    let complete = format!("/v1/ops/recoveries/{pending}/complete");
    let approval = recovery_authority().approve_recovery(&ApprovalRequest {
        pending: PendingId(OperationId::parse(&pending).unwrap()),
        account,
        verifier: ledger_core::principal::Verifier::new(new_verifier),
    });
    let body = json!({ "approval": hex_sig(&approval), "justification": "case 7 verified" });
    let (s, r) = d.call("POST", &complete, op, &[], Some(body.clone())).await;
    assert!(is_refusal(&r, Refusal::RecoveryNotElapsed), "{s} {r}");
    d.clock.advance(RECOVERY_DELAY_MS);
    // A new attempt, under a new client key: the same key would get the stored refusal.
    let (s, r) = d
        .call(
            "POST",
            &complete,
            op,
            &[("client-key", "complete-2")],
            Some(body),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{r}");
    assert!(r["result"]["credential"].is_string());
    // Every credential that covered the account stopped working, the one alice calls with too.
    let (s, r) = d.call("GET", &custody, token, &[], None).await;
    assert!(is_refusal(&r, Refusal::CredentialInvalid), "{s} {r}");
}
