// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The settlement adapter's surface on the core (`F-016`): its principal, its three
//! capabilities, and the availability gate. At the service layer, over the real kernel.

use super::*;
use crate::ledger::settle_adapter::{AdapterInput, AdapterReply};
use crate::ledger::settlement::Withdrawal;
use crate::settlement::{
    Destination, FinalisedHead, MAX_ADAPTER_LEASE, SettlementConfig, SourceId, SupportDeclaration,
    instruction_digest,
};

/// A ledger whose settlement configuration names a new organisation as the adapter.
pub(super) fn with_adapter(fx: &mut Fx) -> Caller {
    let ctx = fx.ctx(fx.operator());
    let o = fx
        .ledger
        .open_identity(
            &ctx,
            IdentityKind::Organisation,
            None,
            Verifier::new(vec![9]),
        )
        .unwrap()
        .value;
    let adapter = Caller::new(o.identity, o.credential);
    let ctx = fx.ctx(fx.operator());
    fx.ledger
        .configure_settlement(
            &ctx,
            SettlementConfig {
                authoritative: SourceId::of_identity(&adapter.principal),
                instance: 1,
                suspense: None,
                inbound_deadline: 100,
                expiry_window: 6,
                block_time: 1,
                finality_margin: 1,
            },
        )
        .unwrap();
    adapter
}

pub(super) fn declare(
    fx: &mut Fx,
    adapter: Caller,
    serving: bool,
    now: Time,
) -> Result<AdapterReply, Refusal> {
    let mut ctx = fx.ctx(adapter);
    ctx.now = now;
    fx.ledger.adapter(
        &ctx,
        AdapterInput::Declare {
            serving,
            lease: 50,
            support: Some(SupportDeclaration {
                legs: [(OperationKind::Redeem, usd())].into(),
            }),
        },
    )
}

fn withdrawal(amount: u64, payer: AccountId) -> Withdrawal {
    Withdrawal {
        payer,
        asset: usd(),
        amount: Minor::new(amount),
        deadline: Some(10_000),
    }
}

#[test]
fn req_sa_8_the_adapter_principal_is_refused_everything_but_its_capabilities() {
    let mut fx = Fx::new();
    let (alice, acct) = funded(&mut fx, 100);
    let adapter = with_adapter(&mut fx);
    // Its capabilities.
    assert!(matches!(
        declare(&mut fx, adapter, true, 10),
        Ok(AdapterReply::Declared(_))
    ));
    let ctx = fx.ctx(adapter);
    assert!(matches!(
        fx.ledger.adapter(
            &ctx,
            AdapterInput::Head {
                head: FinalisedHead {
                    height: 1,
                    next_seq: 0,
                },
                nonces: Vec::new(),
            }
        ),
        Ok(AdapterReply::Head(_))
    ));
    assert!(fx.ledger.adapter_instructions(&adapter).is_ok());
    // Nothing else: no reservation, no account, no read.
    let ctx = fx.ctx(adapter);
    assert_eq!(
        fx.ledger
            .reserve(&ctx, acct, usd(), Minor::new(1))
            .map(|_| ()),
        Err(Refusal::NotAuthorised)
    );
    let ctx = fx.ctx(adapter);
    assert_eq!(
        fx.ledger.withdraw(&ctx, &withdrawal(1, acct)).map(|_| ()),
        Err(Refusal::NotAuthorised)
    );
    assert_eq!(
        crate::principal::resolve(fx.ledger.state(), &adapter).map(|_| ()),
        Err(Refusal::NotAuthorised)
    );
    // And no one else holds them.
    let ctx = fx.ctx(alice);
    assert_eq!(
        fx.ledger
            .adapter(
                &ctx,
                AdapterInput::Head {
                    head: FinalisedHead {
                        height: 2,
                        next_seq: 0,
                    },
                    nonces: Vec::new(),
                }
            )
            .map(|_| ()),
        Err(Refusal::NotAuthorised)
    );
    assert_eq!(
        fx.ledger.adapter_instructions(&fx.operator()).map(|_| ()),
        Err(Refusal::NotAuthorised)
    );
}

#[test]
fn req_sa_8_the_operator_cannot_be_named_the_adapter() {
    let mut fx = Fx::new();
    let op = fx.operator();
    let ctx = fx.ctx(op);
    assert_eq!(
        fx.ledger
            .configure_settlement(
                &ctx,
                SettlementConfig {
                    authoritative: SourceId::of_identity(&op.principal),
                    instance: 1,
                    suspense: None,
                    inbound_deadline: 1,
                    expiry_window: 1,
                    block_time: 1,
                    finality_margin: 1,
                },
            )
            .map(|_| ()),
        Err(Refusal::NotAuthorised)
    );
}

/// `T-027-01`, `REQ-SO-5`, `DEC-18`: an outbound instruction leaves the core as a call envelope
/// in the call outbox — its operation's reference, the deployment's instance, the call, the
/// expiry — whose identity recomputes from the envelope alone; it leaves the outbox once the
/// adapter acknowledges it; and only the adapter reads it.
#[test]
fn t_027_01_an_instruction_leaves_the_core_as_an_envelope_in_the_call_outbox() {
    use crate::settlement::envelope::operation_reference;
    use crate::settlement::{CallEnvelope, SettlementCall, Target};
    let mut fx = Fx::new();
    let (alice, acct) = funded(&mut fx, 100);
    let adapter = with_adapter(&mut fx);
    declare(&mut fx, adapter, true, 10).unwrap();
    let ctx = fx.ctx(alice);
    fx.ledger.withdraw(&ctx, &withdrawal(10, acct)).unwrap();
    let i = fx
        .ledger
        .submit_outbound(&ctx, Destination::Account([1; 32]))
        .unwrap()
        .value;
    let calls = fx.ledger.adapter_calls(&adapter).unwrap();
    assert_eq!(calls.len(), 1);
    let e = calls[0].envelope;
    assert_eq!(e, CallEnvelope::withdrawal(1, &i));
    assert_eq!(e.reference, operation_reference(&i.operation_ref()));
    assert_eq!(e.target, Target { instance: 1 });
    assert_eq!(
        e.call,
        SettlementCall::Withdraw {
            asset: usd(),
            amount: i.amount(),
            to: Destination::Account([1; 32]),
        }
    );
    assert_eq!(e.expiry, Some(i.expiry()));
    assert_eq!(e.instruction(), Some(i.digest()));
    assert!(!calls[0].acknowledged);
    // Only the adapter reads it.
    assert_eq!(
        fx.ledger.adapter_calls(&alice).map(|_| ()),
        Err(Refusal::NotAuthorised)
    );
    // Acknowledged: taken over, out of the outbox.
    let ctx = fx.ctx(adapter);
    fx.ledger
        .adapter(
            &ctx,
            AdapterInput::Acknowledge {
                op: i.operation_ref(),
                instruction: i.digest(),
            },
        )
        .unwrap();
    assert!(fx.ledger.adapter_calls(&adapter).unwrap().is_empty());
}

#[test]
fn ac_e4_2_a_refusing_or_stale_adapter_makes_the_layer_unavailable() {
    let mut fx = Fx::new();
    let (alice, acct) = funded(&mut fx, 100);
    let adapter = with_adapter(&mut fx);
    // Serving: a withdrawal is reserved, and its instruction assembled.
    declare(&mut fx, adapter, true, 10).unwrap();
    let ctx = fx.ctx(alice);
    fx.ledger.withdraw(&ctx, &withdrawal(10, acct)).unwrap();
    let submitted = fx
        .ledger
        .submit_outbound(&ctx, Destination::Account([1; 32]));
    let i = submitted.unwrap().value;
    // The adapter recomputes the instruction's identity from its fields.
    assert_eq!(
        instruction_digest(
            &i.operation_ref(),
            &i.asset(),
            i.amount(),
            i.destination(),
            i.expiry()
        ),
        i.digest()
    );
    // Refusing: withdrawals are refused before anything is reserved.
    declare(&mut fx, adapter, false, 20).unwrap();
    let mut ctx = fx.ctx(alice);
    ctx.now = 20;
    assert_eq!(
        fx.ledger.withdraw(&ctx, &withdrawal(10, acct)).map(|_| ()),
        Err(Refusal::SettlementLayerUnavailable)
    );
    assert_eq!(fx.ledger.available(&alice, &acct, &usd(), 20), Ok(90));
    // Serving again, then stale once its lease runs out.
    declare(&mut fx, adapter, true, 30).unwrap();
    let mut ctx = fx.ctx(alice);
    ctx.now = 79;
    assert!(fx.ledger.withdraw(&ctx, &withdrawal(1, acct)).is_ok());
    let mut ctx = fx.ctx(alice);
    ctx.now = 80;
    assert_eq!(
        fx.ledger.withdraw(&ctx, &withdrawal(1, acct)).map(|_| ()),
        Err(Refusal::SettlementLayerUnavailable)
    );
    // A lease is capped.
    let mut ctx = fx.ctx(adapter);
    ctx.now = 100;
    let Ok(AdapterReply::Declared(s)) = fx.ledger.adapter(
        &ctx,
        AdapterInput::Declare {
            serving: true,
            lease: u64::MAX,
            support: None,
        },
    ) else {
        panic!("declared")
    };
    assert_eq!(s.fresh_until, 100 + MAX_ADAPTER_LEASE);
}
