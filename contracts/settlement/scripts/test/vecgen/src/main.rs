// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Vectors for the gate's offline test: each entry kind's event data (as the contract's own event
//! structs encode) and the digests the contract's own `entry_digest`/`chain_digest` give.
use aledgent_settlement_contract::settlement::*;
use aledgent_settlement_contract::types::*;
use scale::Encode;
fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }
fn main() {
    let acct = |b: u8| AccountId::from([b; 32]);
    let big = AssetId::External { network: NetworkId::Ethereum { chain_id: u64::MAX }, child: Some(Para { id: 65535, pallet: 255, index: u32::MAX }) };
    let call = Call::Withdraw { to: Destination::Deployment { contract: acct(0xD1), account_ref: [0xD2; 32] }, asset: big, amount: u128::MAX };
    // (block, event data, InboxEvent)
    let items: Vec<(u32, Vec<u8>, InboxEvent)> = vec![
        (10, Deposited { seq: 0, from: acct(0x0A), account_ref: [0x0B; 32], asset: AssetId::Here(1), amount: 500_000 }.encode(),
            InboxEvent::Deposited { from: acct(0x0A), account_ref: [0x0B; 32], asset: AssetId::Here(1), amount: 500_000 }),
        (11, Withdrawn { seq: 1, operation_ref: [0x11; 32], to: Destination::Account(acct(0x12)), asset: AssetId::Here(1), amount: 1_000 }.encode(),
            InboxEvent::Withdrawn { operation_ref: [0x11; 32], to: Destination::Account(acct(0x12)), asset: AssetId::Here(1), amount: 1_000 }),
        (11, Refused { seq: 2, operation_ref: [0x21; 32], call, reason: Refusal::CallFailed(CallFailure::OutOfGas) }.encode(),
            InboxEvent::Refused { operation_ref: [0x21; 32], call, reason: Refusal::CallFailed(CallFailure::OutOfGas) }),
        (12, Anchored { seq: 3, operation_ref: [0x31; 32], number: 7, commitment: [0x32; 32] }.encode(),
            InboxEvent::Anchored { operation_ref: [0x31; 32], number: 7, commitment: [0x32; 32] }),
        (13, Bound { seq: 4, operation_ref: [0x41; 32], asset: AssetId::Here(3), amount: 0 }.encode(),
            InboxEvent::Bound { operation_ref: [0x41; 32], asset: AssetId::Here(3), amount: 0 }),
        (14, Refused { seq: 5, operation_ref: [0x51; 32], call: Call::Anchor { number: 9, commitment: [0x52; 32] }, reason: Refusal::AnchorOutOfOrder }.encode(),
            InboxEvent::Refused { operation_ref: [0x51; 32], call: Call::Anchor { number: 9, commitment: [0x52; 32] }, reason: Refusal::AnchorOutOfOrder }),
    ];
    let mut d = [0u8; 32];
    println!("[");
    for (i, (at, data, ev)) in items.iter().enumerate() {
        let e = entry_digest(&InboxEntry { at: *at, event: *ev });
        d = chain_digest(&d, i as u64, &e);
        println!("  {{ \"at\": {at}, \"data\": \"0x{}\", \"entry\": \"0x{}\", \"chain\": \"0x{}\" }}{}", hex(data), hex(&e), hex(&d), if i + 1 < items.len() { "," } else { "" });
    }
    println!("]");
    // After a reset the chain starts again from zeros: one anchor at position 0, block 20.
    let data = Anchored { seq: 0, operation_ref: [0x61; 32], number: 1, commitment: [0x62; 32] }.encode();
    let e = entry_digest(&InboxEntry { at: 20, event: InboxEvent::Anchored { operation_ref: [0x61; 32], number: 1, commitment: [0x62; 32] } });
    eprintln!("RESET {{ \"at\": 20, \"data\": \"0x{}\", \"entry\": \"0x{}\", \"chain\": \"0x{}\" }}", hex(&data), hex(&e), hex(&chain_digest(&[0; 32], 0, &e)));
}
