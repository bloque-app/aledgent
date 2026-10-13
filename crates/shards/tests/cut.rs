// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The consistent cut across shards (amendment 0015, `REQ-RT-14`), through the shard Agents'
//! own transition ([`NativePort`]) with the host's replicas checking every reply: the anchor at a
//! live cut (`INV-19`), and a binding's amount at one (`REQ-AS-8`). The relay duplicates,
//! reorders, drops and delays both the credits and the audit records that carry the stream heads.
//!
//! At every cut checked, for every asset and every ordered pair of shards, the two in-transit
//! positions add up to exactly the value of the credits the cut has in transit (the source's
//! record enqueued them, the destination's had not applied them): nothing is counted twice and
//! nothing is lost. And the customers' balances plus that value equal the book's issued value.

use std::collections::BTreeMap;

use proptest::prelude::*;
use shards::agent::{Answer, Query};
use shards::cut::{Assembler, BindRefusal, Cut, Heads, Inconsistency};
use shards::host::{Cluster, NativePort, accounts, open_all};
use shards::principals::PrincipalsCommand;
use shards::{Account, Command, Faults, Outcome, Refusal, ShardId};

const USD: u32 = 840;
const EUR: u32 = 978;
const ROOT: [u8; 32] = [0x45; 32];
const DRIVER: [u8; 32] = [0xD2; 32];

fn op(n: u64) -> [u8; 16] {
    let mut o = [0u8; 16];
    o[..8].copy_from_slice(&n.to_be_bytes());
    o[8] = 0xC7;
    o
}

/// `n` shards, every account open in both assets and funded, every position observed.
fn funded(n: u16, accts: &[Account], funds: u64) -> Cluster<NativePort> {
    let ports = (0..n).map(|_| NativePort::new(DRIVER)).collect();
    let mut c = Cluster::found(ports, NativePort::new(DRIVER), ROOT, [9; 32]).expect("founded");
    c.observe = vec![USD, EUR];
    let all: Vec<(Account, u32)> = accts.iter().flat_map(|a| [(*a, USD), (*a, EUR)]).collect();
    open_all(&mut c, &all).expect("opened");
    for (i, (a, asset)) in all.iter().enumerate() {
        c.operator(Command::Issue {
            op: op(1 << 40 | i as u64),
            account: *a,
            asset: *asset,
            amount: funds,
        });
    }
    c.drain().expect("funded");
    c
}

/// What a cut holds in one asset, from the observed positions at its records.
#[derive(Debug, Default, PartialEq, Eq)]
struct Held {
    customers: i128,
    issued: i128,
    in_transit: i128,
}

/// Check `cut` in every asset (module documentation); what it holds per asset.
fn check(c: &Cluster<NativePort>, cut: &Cut) -> BTreeMap<u32, Held> {
    assert_eq!(cut.check(), Ok(()), "the cut is consistent");
    let n = cut.shards.len() as u16;
    let mut out = BTreeMap::new();
    for asset in [USD, EUR] {
        let at = |s: ShardId| {
            let h = &cut.shards[usize::from(s.0)];
            // A record before any round holds nothing.
            c.observed(s, h.seq)
                .map(|p| {
                    p.iter()
                        .find(|x| x.asset == asset)
                        .cloned()
                        .unwrap_or_default()
                })
                .unwrap_or_default()
        };
        let mut held = Held::default();
        for i in (0..n).map(ShardId) {
            let pi = at(i);
            held.customers += pi.customers;
            held.issued += pi.issued;
            // One in-transit position per (shard, peer) carries both directions: credited by
            // the debits it sends the peer, debited by the credits it applies from it. So a
            // pair's two positions hold the credits in transit both ways.
            for j in (0..n).map(ShardId).filter(|j| *j > i) {
                let value = |from: ShardId, to: ShardId| {
                    let (applied, enqueued) = cut.in_transit(from, to);
                    assert!(applied <= enqueued);
                    c.credit_value(from, to, asset, applied, enqueued)
                };
                let both = value(i, j) + value(j, i);
                let ij = pi.transit.iter().find(|(p, _)| *p == j).map_or(0, |x| x.1);
                let ji = at(j)
                    .transit
                    .iter()
                    .find(|(p, _)| *p == i)
                    .map_or(0, |x| x.1);
                assert_eq!(
                    ij + ji,
                    both,
                    "{asset}: {i:?} ⇄ {j:?}, in transit {:?} and {:?}",
                    cut.in_transit(i, j),
                    cut.in_transit(j, i)
                );
                held.in_transit += both;
            }
        }
        assert_eq!(
            held.customers + held.in_transit,
            -held.issued,
            "{asset}: the book's value, counted once"
        );
        out.insert(asset, held);
    }
    out
}

/// Step with transfers (and, with `issue_every`, the operator's issuance), checking the live
/// cut after every step and anchoring every `anchor_every` steps.
#[allow(clippy::too_many_arguments)]
fn drive(
    c: &mut Cluster<NativePort>,
    accts: &[Account],
    picks: &[(usize, usize, bool, u64)],
    per_step: usize,
    anchor_every: u64,
    issue_every: usize,
    mut on_step: impl FnMut(&mut Cluster<NativePort>, usize),
) -> Vec<Cut> {
    let mut anchored: Vec<Cut> = Vec::new();
    let mut step = 0usize;
    for (k, (p, q, eur, amount)) in picks.iter().enumerate() {
        let asset = if *eur { EUR } else { USD };
        c.transfer(
            None,
            accts[p % accts.len()],
            accts[q % accts.len()],
            asset,
            *amount,
        )
        .expect("routed");
        if issue_every > 0 && k % issue_every == 0 {
            c.operator(Command::Issue {
                op: op(1 << 42 | k as u64),
                account: accts[q % accts.len()],
                asset,
                amount: 1 + *amount % 50,
            });
        }
        if k % per_step == per_step - 1 {
            c.step().expect("a step");
            step += 1;
            on_step(c, step);
            check(c, &c.cut());
            if step as u64 % anchor_every == 0 {
                let (a, cut) = c.anchor();
                assert_eq!(a, cut.anchor());
                if let Some(prev) = anchored.last() {
                    assert!(cut.dominates(prev), "anchors move forward");
                }
                anchored.push(cut);
            }
        }
    }
    while c.busy() {
        c.step().expect("a step");
        step += 1;
        on_step(c, step);
        check(c, &c.cut());
    }
    anchored
}

/// Idle, the greatest consistent cut is every writer's latest record, with nothing in transit.
fn idle_cut_is_latest(c: &mut Cluster<NativePort>) {
    let cut = c.cut();
    let n = cut.shards.len() as u16;
    for i in (0..n).map(ShardId) {
        let Answer::Heads(h) = c.shard_mut(i).read(&Query::Heads).expect("read") else {
            panic!("heads");
        };
        assert_eq!(
            cut.shards[usize::from(i.0)],
            h,
            "idle: the latest record of {i:?}"
        );
        for j in (0..n).map(ShardId).filter(|j| *j != i) {
            let (a, e) = cut.in_transit(i, j);
            assert_eq!(a, e, "idle: nothing in transit");
        }
    }
    assert_eq!(cut.registry, c.principals().registry_root());
}

#[test]
fn req_rt_14_an_anchor_taken_mid_flight_is_consistent_under_every_fault() {
    let accts = accounts(24);
    let mut c = funded(4, &accts, 1_000);
    c.faults = Faults {
        duplicate: true,
        reorder: true,
        drop_head: 2,
        lose_acks: true,
    };
    c.lag = 2;
    c.batch = 7;
    let picks: Vec<(usize, usize, bool, u64)> = (0..300)
        .map(|i| {
            (
                i * 5 % 24,
                i * 11 % 24,
                i % 4 == 0,
                1 + (i as u64 * 29) % 90,
            )
        })
        .collect();
    let mut live = 0;
    let anchored = drive(&mut c, &accts, &picks, 6, 2, 0, |c, _| {
        let cut = c.cut();
        let n = cut.shards.len() as u16;
        let in_transit = (0..n).any(|i| {
            (0..n).filter(|j| *j != i).any(|j| {
                let (a, e) = cut.in_transit(ShardId(i), ShardId(j));
                a < e
            })
        });
        if in_transit {
            live += 1;
        }
    });
    assert!(
        live > 0,
        "some cuts had credits in transit: anchored while busy"
    );
    assert!(anchored.len() > 10);
    idle_cut_is_latest(&mut c);
    // History is kept only from the last anchor on.
    c.anchor();
    assert_eq!(
        c.assembler().kept(),
        5,
        "one record per writer after an idle anchor"
    );
}

#[test]
fn req_rt_14_records_read_at_different_times_count_a_transfer_twice() {
    // Why the condition: an anchor that takes the source's record before a credit was enqueued
    // and the destination's after it was applied counts the value at both ends.
    let accts = accounts(16);
    let mut c = funded(2, &accts, 1_000);
    let dir = c.directory().clone();
    let (s0, s1) = (ShardId(0), ShardId(1));
    let payer = *accts.iter().find(|a| dir.shard_of(a) == s0).expect("in 0");
    let payee = *accts.iter().find(|a| dir.shard_of(a) == s1).expect("in 1");
    let read = |c: &mut Cluster<NativePort>, s: ShardId| -> Heads {
        match c.shard_mut(s).read(&Query::Heads).expect("read") {
            Answer::Heads(h) => h,
            other => panic!("{other:?}"),
        }
    };
    let early_source = read(&mut c, s0);
    c.transfer(None, payer, payee, USD, 40).expect("routed");
    c.drain().expect("drained");
    let late_destination = read(&mut c, s1);
    let naive = Cut {
        shards: vec![early_source.clone(), late_destination.clone()],
        registry: c.principals().registry_root(),
    };
    assert!(matches!(
        naive.check(),
        Err(Inconsistency::Ahead { from, to, .. }) if from == s0 && to == s1
    ));
    let t = |s: ShardId, h: &Heads| {
        c.observed(s, h.seq)
            .and_then(|p| p.iter().find(|x| x.asset == USD).cloned())
            .unwrap_or_default()
    };
    let p0 = t(s0, &early_source);
    let p1 = t(s1, &late_destination);
    let in_transit = p0.transit[0].1 + p1.transit[0].1;
    assert_eq!(
        in_transit, -40,
        "the credit counted at the payee and the payer both"
    );
    assert_eq!(p0.customers + p1.customers, -(p0.issued + p1.issued) + 40);
    // The assembler, given both records and the ones between, moves the destination back.
    let mut a = Assembler::new(
        vec![
            Heads::genesis(s0, early_source.root),
            Heads::genesis(s1, late_destination.root),
        ],
        c.principals().registry_root(),
    );
    a.deliver(Heads {
        seq: 1,
        ..early_source.clone()
    });
    let mut d = late_destination.clone();
    d.seq = 1;
    a.deliver(d);
    let cut = a.cut();
    assert_eq!(cut.check(), Ok(()));
    assert_eq!(
        cut.shards[1].seq, 0,
        "the destination moved back to its floor"
    );
}

#[test]
fn req_as_8_a_binding_takes_the_outstanding_balance_once_while_the_book_moves() {
    let accts = accounts(24);
    let mut c = funded(3, &accts, 500);
    c.faults = Faults {
        duplicate: true,
        reorder: true,
        drop_head: 1,
        lose_acks: true,
    };
    c.lag = 1;
    c.batch = 5;
    let picks: Vec<(usize, usize, bool, u64)> = (0..240)
        .map(|i| {
            (
                i * 7 % 24,
                i * 13 % 24,
                i % 3 == 0,
                1 + (i as u64 * 31) % 70,
            )
        })
        .collect();
    let mut version = None;
    let mut taken: Option<(Cut, u64, usize)> = None;
    drive(&mut c, &accts, &picks, 5, 3, 4, |c, step| {
        if step == 10 {
            c.principal_command(PrincipalsCommand::Bind { asset: USD });
        }
        if version.is_none() {
            version = c.principals().replica().binding(USD);
        }
        if let (Some(v), None) = (version, &taken) {
            match c.bind_amount(USD, v) {
                Ok((cut, amount)) => taken = Some((cut, amount, step)),
                Err(BindRefusal::NotFrozen(_) | BindRefusal::NoShare(_)) => {}
                Err(e) => panic!("{e:?}"),
            }
        }
    });
    let v = version.expect("bound");
    let (cut, amount, step) = taken.expect("a binding cut while the book moved");
    assert!(
        step < 60,
        "taken while transfers were still flowing (step {step})"
    );
    let held = check(&c, &cut);
    assert_eq!(
        held[&USD].customers + held[&USD].in_transit,
        i128::from(amount),
        "the amount is the outstanding balance at the cut, in transit counted once"
    );
    // Idle, every credit applied: the customers hold exactly the amount, and no issuance moved.
    let mut customers = 0i128;
    let mut issued = 0i128;
    let mut in_transit = 0i128;
    for s in c.shards() {
        let r = s.replica();
        issued += r.issued(USD);
        in_transit += r.in_transit(USD);
        customers += r.trial_balance(USD) - r.issued(USD) - r.in_transit(USD);
    }
    assert_eq!(in_transit, 0, "idle: nothing in transit over the book");
    assert_eq!(customers, i128::from(amount));
    assert_eq!(-issued, i128::from(amount));
    // Issuance is refused while binding, and resumes when it ends.
    let t = c.operator(Command::Issue {
        op: op(1 << 50),
        account: accts[0],
        asset: USD,
        amount: 5,
    });
    c.drain().expect("drained");
    assert_eq!(c.outcome(t), Some(Outcome::Refused(Refusal::NotAuthorised)));
    c.principal_command(PrincipalsCommand::EndBinding { asset: USD });
    c.drain().expect("drained");
    let t = c.operator(Command::Issue {
        op: op(1 << 51),
        account: accts[0],
        asset: USD,
        amount: 5,
    });
    c.drain().expect("drained");
    assert_eq!(c.outcome(t), Some(Outcome::Issued));
    assert!(
        c.bind_amount(USD, v).is_ok(),
        "the binding's cut stays answerable"
    );
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 16, .. ProptestConfig::default() })]

    /// Every live cut is consistent and counts each value once (no double count, no lost value)
    /// whatever the shard count, the batch, the relay's faults on credits and audit records, and
    /// its lag; anchors only move forward; idle, the cut is every writer's latest record.
    #[test]
    fn req_rt_14_no_double_count_no_lost_value_under_reordering_duplication_and_lag(
        n in 2u16..=4,
        batch in 1usize..=12,
        duplicate in any::<bool>(),
        reorder in any::<bool>(),
        drop_head in 0usize..3,
        lose_acks in any::<bool>(),
        lag in 0u64..=3,
        anchor_every in 1u64..=4,
        issue_every in 0usize..=6,
        picks in proptest::collection::vec((0usize..30, 0usize..30, any::<bool>(), 1u64..200), 10..120),
    ) {
        let accts = accounts(30);
        let mut c = funded(n, &accts, 1_000);
        c.faults = Faults { duplicate, reorder, drop_head, lose_acks };
        c.lag = lag;
        c.batch = batch;
        drive(&mut c, &accts, &picks, 4, anchor_every, issue_every, |_, _| {});
        idle_cut_is_latest(&mut c);
        for asset in [USD, EUR] {
            prop_assert!(c.trial_balance(asset).holds());
        }
    }

    /// A binding's amount, taken at the first consistent cut in which every shard has applied
    /// the freeze, is the outstanding balance once every credit has landed, whatever the relay
    /// does (`REQ-AS-8`).
    #[test]
    fn req_as_8_the_binding_amount_holds_under_reordering_duplication_and_lag(
        n in 2u16..=4,
        batch in 1usize..=12,
        duplicate in any::<bool>(),
        reorder in any::<bool>(),
        drop_head in 0usize..3,
        lag in 0u64..=3,
        bind_at in 1usize..12,
        picks in proptest::collection::vec((0usize..30, 0usize..30, any::<bool>(), 1u64..200), 20..120),
    ) {
        let accts = accounts(30);
        let mut c = funded(n, &accts, 1_000);
        c.faults = Faults { duplicate, reorder, drop_head, lose_acks: false };
        c.lag = lag;
        c.batch = batch;
        let mut taken = None;
        drive(&mut c, &accts, &picks, 4, 1_000, 3, |c, step| {
            if step == bind_at {
                c.principal_command(PrincipalsCommand::Bind { asset: EUR });
            }
            if taken.is_none()
                && let Some(v) = c.principals().replica().binding(EUR)
                && let Ok((cut, amount)) = c.bind_amount(EUR, v)
            {
                taken = Some((cut, amount));
            }
        });
        if c.principals().replica().binding(EUR).is_none() {
            // The run ended before the binding was asked: ask it now, on the idle book.
            c.principal_command(PrincipalsCommand::Bind { asset: EUR });
            c.drain().expect("drained");
        }
        if taken.is_none() {
            let v = c.principals().replica().binding(EUR).expect("binding");
            taken = Some(c.bind_amount(EUR, v).expect("idle, every shard has applied it"));
        }
        let (cut, amount) = taken.expect("taken");
        let held = check(&c, &cut);
        prop_assert_eq!(held[&EUR].customers + held[&EUR].in_transit, i128::from(amount));
        let issued: i128 = c.shards().iter().map(|s| s.replica().issued(EUR)).sum();
        prop_assert_eq!(-issued, i128::from(amount), "no issuance moved after the freeze");
    }
}
