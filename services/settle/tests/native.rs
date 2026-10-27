// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-045` end to end against the fake settlement layer: the lanes and the anchor key as **native
//! accounts** (Ed25519 keys, each acting as its own account), with no PASS account. A batch is a v4
//! signed transaction from the lane's account; the layer confirms that form on its runtime under its
//! own name before anything signed in it is broadcast; anchors go out the same way; and a key whose
//! engine does not sign fails the step that asked, not the process, with nothing stored or broadcast.

use ingress::testkit::{ANCHOR_INTERVAL_MS, Party};
use ledger_core::anchor::CheckpointStatus;
use settle::chain::kreivo::{self, AnyTx};
use settle::chain::{Chain, ChainError};
use settle::signer::{Public, Signature, Signer};
use settle::store::{ExtrinsicStatus, Store};
use settle::testkit::{Kit, KitOptions, PAYEE, chain_usd};

fn native() -> KitOptions {
    KitOptions {
        native_accounts: true,
        ..KitOptions::default()
    }
}

async fn serving_kit(options: KitOptions) -> Kit {
    let mut k = Kit::start_with(options).await;
    let r = k.step().await.unwrap();
    assert!(r.serving, "{:?}", k.adapter.status());
    k
}

async fn until(k: &mut Kit, p: &Party, op: &str, want: &str, n: usize) {
    for _ in 0..n {
        if k.state(p, op).await == want {
            return;
        }
        k.chain.advance(1);
        k.step().await.unwrap_or_else(|e| panic!("a step: {e}"));
    }
    assert_eq!(k.state(p, op).await, want, "{op}");
}

/// `F-045`: a withdrawal settles from a native account. The transaction is v4 signed, from the
/// lane's own account (its Ed25519 public key), with no PASS credential; its signature verifies
/// over the signed payload; the account's own nonce moves; and the layer's confirmation of the
/// runtime is recorded under the native form's name.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f_045_a_withdrawal_settles_from_a_native_account() {
    let mut k = serving_kit(native()).await;
    let lane = std::sync::Arc::clone(&k.lanes[0]);
    let Public::Ed25519(public) = lane.public() else {
        panic!("a native lane's key is Ed25519");
    };
    assert_eq!(lane.account(), public);
    assert_eq!(lane.pass_user(), None);
    let alice = k.d.party("alice", 1_000).await;
    let op = k.withdrawal(&alice, 10, "native").await;
    until(&mut k, &alice, &op, "Settled", 6).await;
    assert_eq!(k.chain.balance_of(&PAYEE, &chain_usd()), 10);
    let row = k
        .store
        .extrinsics()
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.operation == op)
        .expect("the stored transaction");
    assert!(
        kreivo::decode_extrinsic(&row.extrinsic.bytes).is_err(),
        "not a PASS transaction"
    );
    let tx = kreivo::decode_native(&row.extrinsic.bytes).expect("a v4 signed transaction");
    assert_eq!(tx.account, lane.account());
    assert!(matches!(tx.signature, Signature::Ed25519(_)));
    assert_eq!(u64::from(tx.nonce), row.nonce);
    assert!(matches!(
        AnyTx::decode(&row.extrinsic.bytes),
        Ok(AnyTx::Native(_))
    ));
    let fin = k.chain.finalized().await.unwrap();
    assert_eq!(
        k.chain.account_nonce(&fin, &lane.account()).await.unwrap(),
        1
    );
    // The runtime was confirmed for the native form, by name.
    let runtimes = k.store.runtimes().await.unwrap();
    assert_eq!(runtimes.len(), 1, "{runtimes:?}");
    assert!(runtimes[0].format.ends_with("+native"), "{runtimes:?}");
    assert!(runtimes[0].key.ends_with("+native"), "{runtimes:?}");
}

/// `F-045`, `F-017`: anchors are published by the native anchor account, with its own nonces.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f_045_anchors_are_published_by_the_native_anchor_account() {
    let mut k = serving_kit(KitOptions {
        anchoring: true,
        ..native()
    })
    .await;
    let anchor = k.anchor_signer.clone().expect("an anchor key");
    assert_eq!(anchor.pass_user(), None);
    let _alice = k.d.party("alice", 1_000).await;
    k.pass(ANCHOR_INTERVAL_MS).await;
    let mut done = false;
    for _ in 0..12 {
        done =
            k.d.core
                .with_ledger(|l| {
                    let s = l.state().anchoring();
                    s.in_flight().is_none()
                        && s.latest_final()
                            .is_some_and(|c| c.number >= 1 && c.status == CheckpointStatus::Final)
                })
                .expect("the core runs");
        if done {
            break;
        }
        k.chain.advance(1);
        k.step().await.unwrap();
    }
    assert!(done, "checkpoint 1 is final");
    let rows = k.store.anchors().await.unwrap();
    let ext = rows[0]
        .extrinsic
        .as_ref()
        .expect("the anchor's transaction");
    assert!(matches!(&ext.status, ExtrinsicStatus::Included(i) if i.success));
    let tx = kreivo::decode_native(&ext.extrinsic.bytes).expect("a v4 signed transaction");
    assert_eq!(tx.account, anchor.account());
    let fin = k.chain.finalized().await.unwrap();
    assert_eq!(
        k.chain
            .account_nonce(&fin, &anchor.account())
            .await
            .unwrap(),
        1
    );
}

/// A signer that does not answer: Vault Transit down, say.
struct Down(std::sync::Arc<dyn Signer>);

impl Signer for Down {
    fn public(&self) -> Public {
        self.0.public()
    }

    fn sign(
        &self,
        _message: Vec<u8>,
    ) -> settle::signer::BoxFuture<'_, Result<Signature, settle::signer::SignerError>> {
        Box::pin(async {
            Err(settle::signer::SignerError::Unavailable(
                "Vault does not answer".into(),
            ))
        })
    }
}

/// `F-045`: a key whose engine does not sign fails the signing with [`ChainError::Signer`], which is
/// not the settlement layer's unavailability (the binary does not restart over it); nothing is
/// signed, so nothing can be stored or broadcast.
#[tokio::test]
async fn f_045_a_signer_that_does_not_answer_fails_the_signing_not_the_layer() {
    let k = Kit::start_with(native()).await;
    let down = Down(std::sync::Arc::clone(&k.lanes[0]));
    let fin = k.chain.finalized().await.unwrap();
    let runtime = k.chain.runtime().await.unwrap();
    let format = kreivo::TxFormat::by_surface(&runtime.surface).expect("a known surface");
    let signing = kreivo::Signing {
        spec_version: runtime.spec_version,
        transaction_version: runtime.surface.transaction_version,
        format,
    };
    let era = settle::chain::Mortality::at(64, fin.number);
    let era = settle::chain::Mortality {
        birth: fin.number,
        checkpoint: fin.number,
        ..era
    };
    let e = k
        .chain
        .sign_batch(
            settle::testkit::INSTANCE,
            &[],
            &down,
            0,
            &era,
            &fin,
            &kreivo::CallLimits::default(),
            &signing,
        )
        .await
        .unwrap_err();
    assert!(matches!(e, ChainError::Signer(_)), "{e:?}");
    assert!(e.to_string().contains("Vault does not answer"), "{e}");
}

/// `F-045`, `REQ-SA-9`: a restarted adapter over the same store finds the runtime confirmed for the
/// native form (the row it stored, under `+native`): it neither asks the layer again nor refuses, and
/// the next withdrawal goes out.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn f_045_a_restart_keeps_the_native_confirmation() {
    let mut k = serving_kit(native()).await;
    let alice = k.d.party("alice", 1_000).await;
    let first = k.withdrawal(&alice, 10, "before").await;
    until(&mut k, &alice, &first, "Settled", 6).await;
    let asked = k.chain.validations();
    assert!(asked >= 1, "the layer was asked once");
    k.restart();
    let r = k.step().await.unwrap();
    assert!(r.serving, "{:?}", k.adapter.status());
    let second = k.withdrawal(&alice, 5, "after").await;
    until(&mut k, &alice, &second, "Settled", 6).await;
    assert_eq!(
        k.chain.validations(),
        asked,
        "not asked again after the restart"
    );
    let runtimes = k.store.runtimes().await.unwrap();
    assert_eq!(runtimes.len(), 1, "{runtimes:?}");
    assert!(runtimes[0].format.ends_with("+native"));
}
