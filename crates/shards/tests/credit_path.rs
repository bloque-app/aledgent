// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The shard prototype's guarantees (`F-031` lever 3, `T-031-11`): exactly-once credit,
//! ordering per payer, and zero divergence against one book.

use std::collections::BTreeMap;

use proptest::prelude::*;
use shards::{
    Account, Command, Directory, Faults, Outcome, Partition, Refusal, Relay, ShardId, book, feed,
    run,
};

const USD: u32 = 840;
const EUR: u32 = 978;

/// `n` accounts that spread over the shards: account `i`'s bytes are `i`.
fn accounts(n: u32) -> Vec<Account> {
    (0..n)
        .map(|i| {
            let mut a = [0u8; 16];
            a[..4].copy_from_slice(&i.to_be_bytes());
            a
        })
        .collect()
}

fn op(n: u64) -> [u8; 16] {
    let mut o = [0u8; 16];
    o[..8].copy_from_slice(&n.to_be_bytes());
    o[8] = 0x0F;
    o
}

/// Open and fund every account in both assets, and feed every directory copy.
fn opened(n_shards: u16, accts: &[Account], funds: u64) -> (Vec<shards::Shard>, Vec<Command>) {
    let mut s = book(n_shards);
    let mut cmds = Vec::new();
    for a in accts {
        for asset in [USD, EUR] {
            cmds.push(Command::Open { account: *a, asset });
        }
    }
    let r = run(&mut s, &mut Relay::default(), &cmds, 64);
    assert!(r.outcomes.iter().all(|o| *o == Outcome::Opened));
    let openings: Vec<(Account, u32)> = accts.iter().flat_map(|a| [(*a, USD), (*a, EUR)]).collect();
    feed(&mut s, &openings);
    let issues: Vec<Command> = accts
        .iter()
        .enumerate()
        .flat_map(|(i, a)| {
            [USD, EUR].map(|asset| Command::Issue {
                op: op(1_000_000 + 2 * i as u64 + u64::from(asset == EUR)),
                account: *a,
                asset,
                amount: funds,
            })
        })
        .collect();
    let r = run(&mut s, &mut Relay::default(), &issues, 64);
    assert!(r.outcomes.iter().all(|o| *o == Outcome::Issued));
    let mut all = cmds;
    all.extend(issues);
    (s, all)
}

fn balances(s: &[shards::Shard], accts: &[Account]) -> BTreeMap<(Account, u32), i128> {
    let dir = Directory::even(s.len() as u16);
    let mut out = BTreeMap::new();
    for a in accts {
        for asset in [USD, EUR] {
            let b = s[usize::from(dir.shard_of(a).0)]
                .balance(a, asset)
                .expect("open in its shard");
            out.insert((*a, asset), b);
        }
    }
    out
}

/// Replay, on one book, the accepted commands of a sharded run in the order its shards decided
/// them. One book applies each transfer's two sides at once, so a payee is never poorer there than
/// in the shards, and every command the shards accepted is accepted.
fn reference(
    accts: &[Account],
    setup: &[Command],
    cmds: &[Command],
    r: &shards::Run,
) -> Vec<shards::Shard> {
    let mut one = book(1);
    let s = run(&mut one, &mut Relay::default(), setup, 64);
    assert!(
        s.outcomes
            .iter()
            .all(|o| matches!(o, Outcome::Opened | Outcome::Issued))
    );
    let openings: Vec<(Account, u32)> = accts.iter().flat_map(|a| [(*a, USD), (*a, EUR)]).collect();
    feed(&mut one, &openings);
    let accepted: Vec<Command> = r
        .order
        .iter()
        .filter(|i| matches!(r.outcomes[**i], Outcome::Final | Outcome::Debited { .. }))
        .map(|i| cmds[*i])
        .collect();
    let replay = run(&mut one, &mut Relay::default(), &accepted, 1);
    assert!(
        replay.outcomes.iter().all(|o| *o == Outcome::Final),
        "one book refused a transfer the shards accepted"
    );
    one
}

fn transfers(
    accts: &[Account],
    picks: &[(usize, usize, bool, u64)],
    first_op: u64,
) -> Vec<Command> {
    picks
        .iter()
        .enumerate()
        .map(|(i, (p, q, eur, amount))| Command::Transfer {
            op: op(first_op + i as u64),
            payer: accts[p % accts.len()],
            payee: accts[q % accts.len()],
            asset: if *eur { EUR } else { USD },
            amount: *amount,
        })
        .collect()
}

#[test]
fn req_rt_9_accounts_spread_over_shards_by_partition() {
    let dir = Directory::even(4);
    let mut per = [0u32; 4];
    for a in accounts(400) {
        let s = dir.shard_of(&a);
        assert_eq!(s, dir.shard_of_partition(Partition::of(&a)));
        per[usize::from(s.0)] += 1;
    }
    assert!(per.iter().all(|n| *n > 50), "{per:?}");
}

#[test]
fn req_rt_11_a_transfer_between_shards_is_final_at_its_debit_and_credited_once() {
    let accts = accounts(64);
    let (mut s, _) = opened(2, &accts, 1_000);
    let dir = Directory::even(2);
    let payer = accts[0];
    let payee = *accts
        .iter()
        .find(|a| dir.shard_of(a) != dir.shard_of(&payer))
        .expect("accounts in both shards");
    let t = Command::Transfer {
        op: op(1),
        payer,
        payee,
        asset: USD,
        amount: 250,
    };
    let home = usize::from(dir.shard_of(&payer).0);
    let away = usize::from(dir.shard_of(&payee).0);
    let out = s[home].execute(&[t]);
    assert_eq!(
        out,
        vec![Outcome::Debited {
            to: dir.shard_of(&payee),
            seq: 1
        }]
    );
    // Final for the payer at once; the value is in transit; the payee has nothing yet.
    assert_eq!(s[home].balance(&payer, USD), Some(750));
    assert_eq!(s[home].in_transit(USD), 250);
    assert_eq!(s[away].balance(&payee, USD), Some(1_000));
    // The same operation again is a duplicate at the kernel (`INV-18`).
    assert_eq!(
        s[home].execute(&[t]),
        vec![Outcome::Refused(Refusal::Duplicate)]
    );
    // Delivered three times, applied once.
    let mut relay = Relay::new(Faults {
        duplicate: true,
        ..Faults::default()
    });
    relay.pump(&mut s);
    relay.pump(&mut s);
    let credit = s[home].outbound(dir.shard_of(&payee));
    assert!(credit.is_empty(), "acknowledged and trimmed");
    assert_eq!(s[away].balance(&payee, USD), Some(1_250));
    assert_eq!(s[away].in_transit(USD), -250);
    assert_eq!(s[home].in_transit(USD) + s[away].in_transit(USD), 0);
    // A credit delivered again, long after, changes nothing.
    let again = shards::Credit {
        from: dir.shard_of(&payer),
        seq: 1,
        op: op(1),
        payee,
        asset: USD,
        amount: 250,
    };
    assert_eq!(s[away].apply_credits(dir.shard_of(&payer), &[again]), 1);
    assert_eq!(s[away].balance(&payee, USD), Some(1_250));
}

#[test]
fn req_rt_11_credits_apply_in_sequence_and_wait_for_a_gap() {
    let accts = accounts(64);
    let (mut s, _) = opened(2, &accts, 1_000);
    let dir = Directory::even(2);
    let payer = accts[0];
    let home = dir.shard_of(&payer);
    let payee = *accts.iter().find(|a| dir.shard_of(a) != home).unwrap();
    let cmds: Vec<Command> = (0..3)
        .map(|i| Command::Transfer {
            op: op(10 + i),
            payer,
            payee,
            asset: USD,
            amount: 10,
        })
        .collect();
    s[usize::from(home.0)].execute(&cmds);
    let credits = s[usize::from(home.0)].outbound(dir.shard_of(&payee));
    assert_eq!(
        credits.iter().map(|c| c.seq).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    let away = usize::from(dir.shard_of(&payee).0);
    // 2 and 3 before 1: nothing applies until 1 arrives.
    assert_eq!(s[away].apply_credits(home, &credits[1..]), 0);
    assert_eq!(s[away].balance(&payee, USD), Some(1_000));
    assert_eq!(s[away].apply_credits(home, &credits[..1]), 1);
    assert_eq!(s[away].apply_credits(home, &credits), 3);
    assert_eq!(s[away].balance(&payee, USD), Some(1_030));
}

#[test]
fn req_rt_10_debits_on_one_payer_are_ordered_and_never_overdraw() {
    let accts = accounts(32);
    let (mut s, _) = opened(4, &accts, 100);
    let payer = accts[3];
    // Twenty spends of 15 against 100, to payees in every shard, in batches of 7: exactly six
    // succeed, whatever the batching (`AC-B2.1`).
    let cmds: Vec<Command> = (0..20u64)
        .map(|i| Command::Transfer {
            op: op(500 + i),
            payer,
            payee: accts[(i as usize % 31 + 4) % 32],
            asset: USD,
            amount: 15,
        })
        .collect();
    let r = run(&mut s, &mut Relay::default(), &cmds, 7);
    let ok = r
        .outcomes
        .iter()
        .filter(|o| matches!(o, Outcome::Final | Outcome::Debited { .. }))
        .count();
    assert_eq!(ok, 6);
    assert!(r.outcomes.iter().all(|o| matches!(
        o,
        Outcome::Final | Outcome::Debited { .. } | Outcome::Refused(Refusal::Insufficient)
    )));
    let dir = Directory::even(4);
    assert_eq!(
        s[usize::from(dir.shard_of(&payer).0)].balance(&payer, USD),
        Some(10)
    );
}

#[test]
fn req_rt_11_a_lagging_directory_refuses_and_never_strands_a_credit() {
    let accts = accounts(16);
    let (mut s, _) = opened(2, &accts, 100);
    let dir = Directory::even(2);
    let payer = accts[0];
    let home = usize::from(dir.shard_of(&payer).0);
    // A new account in the other shard; the payer's copy has not heard of it yet.
    let fresh = (0u8..=255)
        .map(|n| [n; 16])
        .find(|a| dir.shard_of(a) != dir.shard_of(&payer))
        .expect("an account in the other shard");
    let away = usize::from(dir.shard_of(&fresh).0);
    assert_eq!(
        s[away].execute(&[Command::Open {
            account: fresh,
            asset: USD
        }]),
        vec![Outcome::Opened]
    );
    let t = Command::Transfer {
        op: op(77),
        payer,
        payee: fresh,
        asset: USD,
        amount: 5,
    };
    assert_eq!(
        s[home].execute(&[t]),
        vec![Outcome::Refused(Refusal::UnknownAccount)]
    );
    s[home].learn(fresh, USD);
    assert!(matches!(s[home].execute(&[t])[0], Outcome::Debited { .. }));
    Relay::default().drain(&mut s);
    assert_eq!(s[away].balance(&fresh, USD), Some(5));
}

#[test]
fn inv_5_trial_balance_and_in_transit_under_load() {
    let accts = accounts(48);
    let (mut s, _) = opened(3, &accts, 10_000);
    let picks: Vec<(usize, usize, bool, u64)> = (0..300)
        .map(|i| {
            (
                i * 7 % 48,
                i * 13 % 48,
                i % 3 == 0,
                1 + (i as u64 * 37) % 400,
            )
        })
        .collect();
    let cmds = transfers(&accts, &picks, 10_000);
    let mut relay = Relay::new(Faults {
        lose_acks: true,
        ..Faults::default()
    });
    run(&mut s, &mut relay, &cmds, 16);
    let in_transit: i128 = s
        .iter()
        .map(|x| x.in_transit(USD) + x.in_transit(EUR))
        .sum();
    // Every credit not yet acknowledged has either applied (and nets to zero) or not (and is in
    // transit): in transit is at most what is outstanding, never negative.
    assert!(in_transit >= 0, "{in_transit}");
    relay.drain(&mut s);
    for asset in [USD, EUR] {
        assert_eq!(s.iter().map(|x| x.in_transit(asset)).sum::<i128>(), 0);
        for x in &s {
            assert_eq!(x.trial_balance(asset), 0);
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 12, .. ProptestConfig::default() })]

    /// Zero divergence (`NFR-9`, `INV-25`): whatever the shard count, the batch size and the
    /// relay's faults, once every credit is delivered each balance equals one book's, fed the
    /// commands the shards accepted in the order they decided them; every credit applied once.
    #[test]
    fn nfr_9_zero_divergence_against_one_book(
        n_shards in 2u16..=4,
        batch in 1usize..=16,
        duplicate in any::<bool>(),
        reorder in any::<bool>(),
        drop_head in 0usize..3,
        lose_acks in any::<bool>(),
        picks in proptest::collection::vec((0usize..40, 0usize..40, any::<bool>(), 1u64..300), 1..120),
    ) {
        let accts = accounts(40);
        let (mut s, setup) = opened(n_shards, &accts, 1_000);
        let cmds = transfers(&accts, &picks, 1);
        let mut relay = Relay::new(Faults { duplicate, reorder, drop_head, lose_acks });
        let r = run(&mut s, &mut relay, &cmds, batch);
        relay.drain(&mut s);
        // Exactly once: every debit's credit is applied by its destination, and nothing more.
        let mut max_seq: BTreeMap<(ShardId, ShardId), u64> = BTreeMap::new();
        let dir = Directory::even(n_shards);
        for (c, o) in cmds.iter().zip(&r.outcomes) {
            if let Outcome::Debited { to, seq } = o {
                let from = dir.shard_of(&c.home());
                let e = max_seq.entry((from, *to)).or_default();
                *e = (*e).max(*seq);
            }
        }
        for ((from, to), seq) in &max_seq {
            prop_assert_eq!(s[usize::from(to.0)].applied_from(*from), *seq);
        }
        let one = reference(&accts, &setup, &cmds, &r);
        prop_assert_eq!(balances(&s, &accts), balances(&one, &accts));
        for asset in [USD, EUR] {
            prop_assert_eq!(s.iter().map(|x| x.in_transit(asset)).sum::<i128>(), 0);
            prop_assert_eq!(
                s.iter().map(|x| x.issued(asset)).sum::<i128>(),
                one[0].issued(asset)
            );
        }
    }
}
