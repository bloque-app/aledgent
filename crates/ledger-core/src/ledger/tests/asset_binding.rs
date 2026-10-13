// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A book-only asset and its binding (amendment 0011, `REQ-AS-7`–`REQ-AS-9`, `F-029`), at the
//! service layer over the real kernel: the request's checks, the outstanding balance it takes, the
//! bind call in the call outbox, and the layer's final record deciding it.

use super::operator::staff;
use super::settle_adapter::with_adapter;
use super::*;
use crate::asset::{AssetBinding, AssetLocation, Minor};
use crate::ledger::settle_adapter::{AdapterInput, AdapterReply};
use crate::operator::OperatorOpKind;
use crate::settlement::assets::{
    AssetAttestation, AttestationVerdict, BindStatus, LayerAssetFacts, LayerAssetStatus, Mismatch,
};
use crate::settlement::envelope::SettlementCall;
use crate::settlement::{
    Coordinate, EventKind, Incident, Movement, Observation, ObservedFinality, Payload, SourceId,
};

fn copm() -> AssetId {
    AssetId::parse("COPM").unwrap()
}

fn here(n: u32) -> AssetLocation {
    AssetLocation::parse(&alloc::format!("Here({n})")).unwrap()
}

/// COPM registered book-only at 2, an issuance grant over it held by the operator, and 300 minor
/// units issued into an account. Returns the adapter and the account.
fn book_only(fx: &mut Fx) -> (Caller, AccountId) {
    let adapter = with_adapter(fx);
    let registrar = staff(fx, &[OperatorOpKind::RegisterAsset]);
    let ctx = fx.ctx(registrar);
    fx.ledger
        .register_asset(
            &ctx,
            crate::ledger::operator::RegisterAsset {
                id: copm(),
                precision: 2,
                kind: AssetKind::Stable,
                review: crate::ledger::operator::ReviewRef([1; 32]),
            },
            b"CR-8: COPM/2, book-only",
        )
        .unwrap();
    assert_eq!(
        fx.ledger.state().registry().get(&copm()).unwrap().binding(),
        AssetBinding::BookOnly
    );
    let op = fx.operator();
    let ctx = fx.ctx(op);
    fx.ledger
        .grant_authority(
            &ctx,
            GrantRequest {
                kind: GrantKind::Issuance,
                principal: op.principal,
                subject: Subject::Asset(copm()),
                ops: NonEmptySet::new([OperationKind::Issue, OperationKind::Redeem].into())
                    .unwrap(),
                per_operation: None,
                per_window: Some(Window {
                    length: NonZeroU64::new(1_000_000).unwrap(),
                    limit: nz(u64::MAX / 2),
                }),
                assets: Some(NonEmptySet::one(copm())),
                counterparties: None,
                parent: None,
                expires: None,
            },
        )
        .unwrap();
    let alice = fx.person();
    let acct = fx.account(&alice, Custody::Delegated);
    let ctx = fx.ctx(op);
    fx.ledger
        .issue(&ctx, copm(), acct, Minor::new(300))
        .unwrap();
    (adapter, acct)
}

fn attest(fx: &mut Fx, adapter: Caller, facts: Option<LayerAssetFacts>) -> AttestationVerdict {
    let ctx = fx.ctx(adapter);
    match fx.ledger.adapter(
        &ctx,
        AdapterInput::Attest(AssetAttestation {
            location: here(100),
            asset: copm(),
            height: 7,
            facts,
            lease: 50,
        }),
    ) {
        Ok(AdapterReply::Attested(v)) => v,
        other => panic!("{other:?}"),
    }
}

fn kreivo_copm(decimals: u8) -> Option<LayerAssetFacts> {
    Some(LayerAssetFacts {
        symbol: b"COPM".to_vec(),
        decimals,
        status: LayerAssetStatus::Live,
    })
}

fn bind(
    fx: &mut Fx,
    caller: Caller,
    why: &[u8],
) -> Result<crate::settlement::assets::AssetBind, Refusal> {
    let ctx = fx.ctx(caller);
    fx.ledger
        .bind_asset(&ctx, copm(), here(100), why)
        .map(|a| a.value)
}

fn issue(fx: &mut Fx, acct: AccountId, n: u64) -> Result<u64, Refusal> {
    let op = fx.operator();
    let ctx = fx.ctx(op);
    fx.ledger
        .issue(&ctx, copm(), acct, Minor::new(n))
        .map(|a| a.value)
}

/// The layer's record of the binding's outcome, observed and then found final.
fn record(
    fx: &mut Fx,
    adapter: Caller,
    seq: u64,
    kind: EventKind,
    reference: OperationId,
    amount: u64,
) {
    let coordinate = Coordinate {
        instance: 1,
        seq,
        digest: [seq as u8; 32],
    };
    let ctx = fx.ctx(adapter);
    let _ = fx.ledger.adapter(
        &ctx,
        AdapterInput::Observe(Observation {
            source: SourceId([0; 16]),
            coordinate,
            at: 10 + seq,
            kind,
            payload: Payload(Vec::new()),
            observed_finality: ObservedFinality::Final,
            operation_ref: Some(reference),
            account_ref: None,
            movement: Some(Movement {
                asset: copm(),
                amount: Minor::new(amount),
            }),
            anchor: None,
        }),
    );
    let ctx = fx.ctx(adapter);
    fx.ledger
        .adapter(
            &ctx,
            AdapterInput::Check {
                coordinate,
                finalised_through: 10 + seq,
                still_matches: true,
            },
        )
        .unwrap();
}

#[test]
fn ac_f5_1_a_book_only_asset_issues_and_has_no_external_leg() {
    let mut fx = Fx::new();
    let (_, _) = book_only(&mut fx);
    assert_eq!(
        fx.ledger.require_supported(OperationKind::Redeem, &copm()),
        Err(Refusal::UnsupportedLeg)
    );
}

#[test]
fn ac_f5_2_req_as_8_a_binding_is_checked_against_the_layers_own_fields() {
    let mut fx = Fx::new();
    let (adapter, _) = book_only(&mut fx);
    let op = fx.operator();
    // Never attested: the adapter does not support it there.
    assert_eq!(bind(&mut fx, op, b"bind"), Err(Refusal::UnsupportedLeg));
    // Other decimals, or no metadata: refused, and said so to the adapter.
    assert_eq!(
        attest(&mut fx, adapter, kreivo_copm(6)),
        AttestationVerdict::BookOnly {
            asset: copm(),
            mismatch: Some(Mismatch::Decimals)
        }
    );
    assert_eq!(bind(&mut fx, op, b"bind"), Err(Refusal::UnsupportedLeg));
    attest(&mut fx, adapter, None);
    assert_eq!(bind(&mut fx, op, b"bind"), Err(Refusal::UnsupportedLeg));
    // A stale attestation is an unavailable layer.
    attest(&mut fx, adapter, kreivo_copm(2));
    let later = Ctx {
        caller: op,
        op: fx.op(),
        now: 10 + 50,
    };
    assert_eq!(
        fx.ledger
            .bind_asset(&later, copm(), here(100), b"bind")
            .map(|a| a.value),
        Err(Refusal::SettlementLayerUnavailable)
    );
    assert_eq!(
        fx.ledger.state().registry().get(&copm()).unwrap().binding(),
        AssetBinding::BookOnly
    );
}

#[test]
fn ac_f5_4_req_as_8_a_binding_takes_the_outstanding_balance_and_freezes_issuance_until_final() {
    let mut fx = Fx::new();
    let (adapter, acct) = book_only(&mut fx);
    attest(&mut fx, adapter, kreivo_copm(2));
    // Without issuance authority over the asset, or without a justification: refused.
    let alice = fx.person();
    assert_eq!(bind(&mut fx, alice, b"bind"), Err(Refusal::NotAuthorised));
    let op = fx.operator();
    assert_eq!(bind(&mut fx, op, b""), Err(Refusal::JustificationRequired));
    let b = bind(&mut fx, op, b"CR-9: bind COPM").unwrap();
    assert_eq!((b.amount, b.status), (Minor::new(300), BindStatus::Pending));
    assert_eq!(
        fx.ledger.state().registry().get(&copm()).unwrap().binding(),
        AssetBinding::Binding(here(100))
    );
    // The bind call is in the call outbox, for the outstanding balance.
    let calls = fx.ledger.call_outbox();
    assert!(calls.iter().any(|c| c.envelope.call
        == SettlementCall::Bind {
            asset: copm(),
            location: here(100),
            amount: Minor::new(300),
        }));
    // While it is pending the amount cannot move, and it is not asked twice.
    assert_eq!(issue(&mut fx, acct, 1), Err(Refusal::NotAuthorised));
    assert_eq!(bind(&mut fx, op, b"again"), Err(Refusal::NotAuthorised));
    // The layer's final Bound record of that amount binds it.
    record(&mut fx, adapter, 1, EventKind::Bound, b.reference, 300);
    let a = *fx.ledger.state().registry().get(&copm()).unwrap();
    assert_eq!(a.binding(), AssetBinding::Bound(here(100)));
    assert!(
        fx.ledger
            .call_outbox()
            .iter()
            .all(|c| !matches!(c.envelope.call, SettlementCall::Bind { .. }))
    );
    // After the binding, issuance goes on (REQ-AS-6), and later metadata is ignored (REQ-AS-9).
    assert!(issue(&mut fx, acct, 1).is_ok());
    assert_eq!(
        attest(&mut fx, adapter, kreivo_copm(6)),
        AttestationVerdict::Bound { asset: copm() }
    );
    assert_eq!(
        fx.ledger.state().registry().get(&copm()).unwrap().binding(),
        AssetBinding::Bound(here(100))
    );
}

/// Finding F18's class, `REQ-RT-1`: a binding reads its asset's binding and the book's
/// `Issuance` accounts by key, never every ledger account or every binding; the layer's record
/// of it finds the binding by its reference, by key. The opening positions are read whole: one
/// per registered asset, as the registry itself.
#[test]
fn req_rt_1_a_binding_and_its_record_read_no_growing_kind_whole() {
    use crate::leaves::kind;
    let mut fx = Fx::new();
    let (adapter, _) = book_only(&mut fx);
    for _ in 0..5 {
        let p = fx.person();
        fx.account(&p, Custody::Delegated);
    }
    attest(&mut fx, adapter, kreivo_copm(2));
    fx.ledger.record_calls(true);
    let _ = fx.ledger.state().take_touched();
    let whole = |fx: &Fx| -> Vec<u8> {
        fx.ledger
            .state()
            .take_touched()
            .into_iter()
            .filter(|t| t.whole && t.kind != kind::OPENING_POSITION)
            .map(|t| t.kind)
            .collect()
    };
    let op = fx.operator();
    let b = bind(&mut fx, op, b"CR-9: bind COPM").unwrap();
    assert_eq!(b.amount, Minor::new(300));
    assert_eq!(whole(&fx), Vec::<u8>::new(), "the binding");
    record(&mut fx, adapter, 1, EventKind::Bound, b.reference, 300);
    assert_eq!(
        fx.ledger.state().registry().get(&copm()).unwrap().binding(),
        AssetBinding::Bound(here(100))
    );
    assert_eq!(whole(&fx), Vec::<u8>::new(), "its record");
}

#[test]
fn ac_f5_3_a_refused_binding_returns_the_asset_to_book_only() {
    let mut fx = Fx::new();
    let (adapter, acct) = book_only(&mut fx);
    attest(&mut fx, adapter, kreivo_copm(2));
    let op = fx.operator();
    let b = bind(&mut fx, op, b"CR-9").unwrap();
    // A Bound record of another amount under its reference decides nothing: an incident.
    record(&mut fx, adapter, 1, EventKind::Bound, b.reference, 299);
    assert_eq!(
        fx.ledger.state().registry().get(&copm()).unwrap().binding(),
        AssetBinding::Binding(here(100))
    );
    // The layer's refusal, final: book-only again, the incident recorded, issuance resumes.
    record(
        &mut fx,
        adapter,
        2,
        EventKind::BindRefused,
        b.reference,
        300,
    );
    assert_eq!(
        fx.ledger.state().registry().get(&copm()).unwrap().binding(),
        AssetBinding::BookOnly
    );
    let refused = fx
        .ledger
        .state()
        .settlement()
        .incidents()
        .iter()
        .filter(|i| matches!(i, Incident::BindRefused { .. }))
        .count();
    assert_eq!(refused, 2);
    assert!(issue(&mut fx, acct, 1).is_ok());
    // And it can be asked again, for the new outstanding balance.
    let again = bind(&mut fx, op, b"CR-10").unwrap();
    assert_eq!(again.amount, Minor::new(301));
}
