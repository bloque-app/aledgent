// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The shards as Agents (`F-031`, `DEC-22`, `T-031-20`): N book-shard Agents and one principals
//! Agent, through the Agents' own transition over their rows ([`NativePort`]: the code the VOS
//! guest runs), with the host's replicas checking every reply. Exactly-once credits, zero
//! divergence against one book, the revocation fence and its lag, the relay's recovery from the
//! Agents' rows, the anchor over every shard's root, pools, and references that carry the
//! payer's partition.

use std::collections::BTreeMap;

use proptest::prelude::*;
use shards::agent::{self, Envelope, Reply, Role};
use shards::anchor;
use shards::host::{Cluster, HostError, NativePort, Port, accounts, open_all};
use shards::pool::Pool;
use shards::principals::PrincipalsCommand;
use shards::{
    Account, Command, Directory, Faults, Order, Outcome, Partition, Refusal, Relay, ShardId,
};

const USD: u32 = 840;
const EUR: u32 = 978;
const ROOT: [u8; 32] = [0x44; 32];
const DRIVER: [u8; 32] = [0xD1; 32];

fn cluster(n: u16) -> Cluster<NativePort> {
    let ports = (0..n).map(|_| NativePort::new(DRIVER)).collect();
    Cluster::found(ports, NativePort::new(DRIVER), ROOT, [7; 32]).expect("founded")
}

fn op(n: u64) -> [u8; 16] {
    let mut o = [0u8; 16];
    o[..8].copy_from_slice(&n.to_be_bytes());
    o[8] = 0xA6;
    o
}

/// Open every account in both assets through the principals Agent, and fund each.
fn funded(n: u16, accts: &[Account], funds: u64) -> Cluster<NativePort> {
    let mut c = cluster(n);
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

fn balances(c: &Cluster<NativePort>, accts: &[Account]) -> BTreeMap<(Account, u32), i128> {
    let dir = c.directory().clone();
    let mut out = BTreeMap::new();
    for a in accts {
        for asset in [USD, EUR] {
            let s = &c.shards()[usize::from(dir.shard_of(a).0)];
            out.insert(
                (*a, asset),
                s.replica().balance(a, asset).expect("open in its shard"),
            );
        }
    }
    out
}

/// One book, fed the transfers the shards accepted in the order they decided them.
fn one_book(accts: &[Account], funds: u64, accepted: &[Command]) -> BTreeMap<(Account, u32), i128> {
    let mut one = shards::book(1);
    let mut setup = Vec::new();
    for a in accts {
        for asset in [USD, EUR] {
            setup.push(Command::Open { account: *a, asset });
        }
    }
    shards::run(&mut one, &mut Relay::default(), &setup, 64);
    shards::feed(
        &mut one,
        &accts
            .iter()
            .flat_map(|a| [(*a, USD), (*a, EUR)])
            .collect::<Vec<_>>(),
    );
    let issues: Vec<Command> = accts
        .iter()
        .flat_map(|a| [USD, EUR].map(|asset| (a, asset)))
        .enumerate()
        .map(|(i, (a, asset))| Command::Issue {
            op: op(1 << 41 | i as u64),
            account: *a,
            asset,
            amount: funds,
        })
        .collect();
    shards::run(&mut one, &mut Relay::default(), &issues, 64);
    let r = shards::run(&mut one, &mut Relay::default(), accepted, 1);
    assert!(
        r.outcomes.iter().all(|o| *o == Outcome::Final),
        "one book refused a transfer the shards accepted: {:?}",
        r.outcomes
    );
    let mut out = BTreeMap::new();
    for a in accts {
        for asset in [USD, EUR] {
            out.insert((*a, asset), one[0].balance(a, asset).expect("open"));
        }
    }
    out
}

/// Every Debited credit applied exactly once: each destination's last sequence applied from a
/// source is the last debit's sequence for that pair.
fn exactly_once(c: &Cluster<NativePort>, orders: &BTreeMap<u64, Order>) {
    let dir = c.directory().clone();
    let mut last: BTreeMap<(ShardId, ShardId), u64> = BTreeMap::new();
    for (ticket, order) in orders {
        if let Some(Outcome::Debited { to, seq }) = c.outcome(*ticket) {
            let from = dir.shard_of(&order.command.home());
            let e = last.entry((from, to)).or_default();
            *e = (*e).max(seq);
        }
    }
    for ((from, to), seq) in last {
        assert_eq!(
            c.shards()[usize::from(to.0)].replica().applied_from(from),
            seq,
            "credits {from:?} → {to:?}"
        );
    }
}

fn run_mixed(
    n: u16,
    faults: Faults,
    batch: usize,
    picks: &[(usize, usize, bool, u64)],
) -> (Cluster<NativePort>, Vec<Account>, BTreeMap<u64, Order>) {
    let accts = accounts(40);
    let mut c = funded(n, &accts, 1_000);
    c.faults = faults;
    c.batch = batch;
    let mut orders = BTreeMap::new();
    for (k, (p, q, eur, amount)) in picks.iter().enumerate() {
        let (ticket, order) = c
            .transfer(
                None,
                accts[p % accts.len()],
                accts[q % accts.len()],
                if *eur { EUR } else { USD },
                *amount,
            )
            .expect("routed");
        orders.insert(ticket, order);
        if k % 7 == 6 {
            c.step().expect("a step");
        }
    }
    c.drain().expect("drained");
    (c, accts, orders)
}

fn accepted_in_order(c: &Cluster<NativePort>, orders: &BTreeMap<u64, Order>) -> Vec<Command> {
    c.decided
        .iter()
        .filter(|t| orders.contains_key(t))
        .filter(|t| c.outcome(**t).is_some_and(|o| o.accepted()))
        .map(|t| orders[t].command)
        .collect()
}

#[test]
fn req_rt_11_credits_through_agents_apply_exactly_once_under_every_fault() {
    let picks: Vec<(usize, usize, bool, u64)> = (0..240)
        .map(|i| {
            (
                i * 7 % 40,
                i * 13 % 40,
                i % 3 == 0,
                1 + (i as u64 * 37) % 300,
            )
        })
        .collect();
    let faults = Faults {
        duplicate: true,
        reorder: true,
        drop_head: 2,
        lose_acks: true,
    };
    let (mut c, accts, orders) = run_mixed(4, faults, 9, &picks);
    assert!(c.metrics.cross > 0, "some transfers cross shards");
    exactly_once(&c, &orders);
    assert_eq!(
        c.verify_exactly_once().unwrap(),
        vec![],
        "read from the Agents"
    );
    let accepted = accepted_in_order(&c, &orders);
    assert_eq!(balances(&c, &accts), one_book(&accts, 1_000, &accepted));
    for asset in [USD, EUR] {
        let tb = c.trial_balance(asset);
        assert!(tb.holds(), "{tb:?}");
        assert_eq!(tb.in_transit, 0, "drained");
    }
}

#[test]
fn inv_5_the_trial_balance_holds_with_credits_in_transit() {
    let accts = accounts(24);
    let mut c = funded(3, &accts, 500);
    for i in 0..60u64 {
        c.transfer(
            None,
            accts[(i * 5 % 24) as usize],
            accts[(i * 11 % 24) as usize],
            USD,
            1 + i,
        )
        .expect("routed");
    }
    // One step: debits commit, their credits are still in transit.
    c.step().expect("a step");
    let tb = c.trial_balance(USD);
    assert!(tb.holds(), "{tb:?}");
    assert!(tb.in_transit > 0, "credits in transit: {tb:?}");
    c.drain().expect("drained");
    let tb = c.trial_balance(USD);
    assert!(tb.holds() && tb.in_transit == 0, "{tb:?}");
}

#[test]
fn req_rt_11_the_relay_recovers_from_the_agents_rows() {
    let accts = accounts(32);
    let mut c = funded(3, &accts, 1_000);
    let mut orders = BTreeMap::new();
    for i in 0..90u64 {
        let (t, o) = c
            .transfer(
                None,
                accts[(i * 3 % 32) as usize],
                accts[(i * 17 % 32) as usize],
                EUR,
                5,
            )
            .expect("routed");
        orders.insert(t, o);
    }
    c.step().expect("a step");
    c.step().expect("a step");
    // A host restart: the relay forgets what it carries and rebuilds it from the Agents.
    c.recover().expect("recovered");
    c.drain().expect("drained");
    exactly_once(&c, &orders);
    let accepted = accepted_in_order(&c, &orders);
    assert_eq!(balances(&c, &accts), one_book(&accts, 1_000, &accepted));
    assert_eq!(c.in_flight(), 0);
}

#[test]
fn req_au_3_a_revocation_fences_older_authority_within_one_round() {
    let accts = accounts(16);
    let mut c = funded(4, &accts, 1_000);
    let agent = [0xA9; 16];
    let payer = accts[2];
    c.principal_command(PrincipalsCommand::Grant {
        principal: agent,
        payer,
        limit: 50,
    });
    c.drain().expect("granted");
    // Within the grant's bound: accepted; past it: refused at ingress (`REQ-AU-2`).
    let (t1, _) = c
        .transfer(Some(agent), payer, accts[9], USD, 40)
        .expect("routed");
    assert_eq!(
        c.transfer(Some(agent), payer, accts[9], USD, 51).err(),
        Some(Refusal::NotAuthorised)
    );
    c.drain().expect("decided");
    assert!(c.outcome(t1).unwrap().accepted());
    // Stamped before the revocation, decided after it: refused by the shard (`c-10`).
    let (late, _) = c
        .transfer(Some(agent), payer, accts[9], USD, 10)
        .expect("routed before the revocation");
    c.principal_command(PrincipalsCommand::Revoke { principal: agent });
    c.step().expect("one step");
    assert_eq!(c.outcome(late), Some(Outcome::Refused(Refusal::Revoked)));
    // Applied in every shard in the step it committed in: the lag is one round (`NFR-20`).
    let lag = c.metrics.revocation_lag();
    assert_eq!(lag.n, 1);
    assert!(
        c.shards()
            .iter()
            .all(|s| s.replica().feed_applied() == c.principals().replica().version())
    );
    // And ingress refuses at once.
    assert_eq!(
        c.transfer(Some(agent), payer, accts[9], USD, 1).err(),
        Some(Refusal::NotAuthorised)
    );
    // A new grant after the revocation is a new authority.
    c.principal_command(PrincipalsCommand::Grant {
        principal: agent,
        payer,
        limit: 50,
    });
    c.drain().expect("granted again");
    let (t2, _) = c
        .transfer(Some(agent), payer, accts[9], USD, 10)
        .expect("routed");
    c.drain().expect("decided");
    assert!(c.outcome(t2).unwrap().accepted());
}

#[test]
fn req_rt_9_an_account_opened_through_the_principals_agent_lives_in_its_shard() {
    let mut c = cluster(4);
    let a: Account = [0x5E; 16];
    open_all(&mut c, &[(a, USD)]).expect("opened");
    let dir = c.directory().clone();
    for s in c.shards() {
        let home = s.replica().id() == dir.shard_of(&a);
        assert_eq!(s.replica().balance(&a, USD).is_some(), home);
    }
    // A second opening is refused by the registry, and changes nothing.
    c.principal_command(PrincipalsCommand::Open {
        account: a,
        asset: USD,
    });
    c.drain().expect("drained");
    assert_eq!(c.principals().replica().version(), 1);
}

#[test]
fn c_13_operation_references_carry_the_payers_partition() {
    let accts = accounts(64);
    let mut c = funded(2, &accts[..8], 100);
    for a in &accts[..8] {
        let (_, order) = c.transfer(None, *a, accts[0], USD, 1).expect("routed");
        let Command::Transfer { op, .. } = order.command else {
            panic!("a transfer");
        };
        assert_eq!(Partition::of_key(&op), Partition::of(a));
    }
    // Two references for one payer differ in their random bits.
    let (_, x) = c.transfer(None, accts[1], accts[0], USD, 1).unwrap();
    let (_, y) = c.transfer(None, accts[1], accts[0], USD, 1).unwrap();
    assert_ne!(x.command, y.command);
}

#[test]
fn inv_19_one_anchor_over_every_shard_root() {
    let accts = accounts(20);
    let mut c = funded(4, &accts, 100);
    for i in 0..20u64 {
        c.transfer(
            None,
            accts[i as usize],
            accts[(i as usize + 7) % 20],
            USD,
            3,
        )
        .expect("routed");
    }
    c.drain().expect("drained");
    let (a, cut) = c.anchor();
    assert_eq!(cut.shards.len(), 4);
    cut.check().expect("a consistent cut");
    let leaves = cut.leaves();
    assert_eq!(leaves.len(), 5, "every shard, then the registry");
    for (i, l) in leaves.iter().enumerate() {
        if let anchor::Leaf::Shard(r) = l {
            // Each root is the Agent's own: the replica equals it after every round.
            assert_eq!(r.root, c.shards()[i].replica().root());
        }
        assert!(anchor::verify(&a, l, &anchor::prove(&leaves, i)));
    }
    // Any later commit in any shard moves the anchor.
    c.transfer(None, accts[0], accts[1], USD, 1)
        .expect("routed");
    c.drain().expect("drained");
    assert_ne!(c.anchor().0, a);
}

#[test]
fn req_rt_12_a_pool_takes_credits_locally_and_is_rebalanced() {
    let accts = accounts(32);
    let mut c = funded(4, &accts, 1_000);
    let dir = c.directory().clone();
    let fees = Pool::new([0xFE; 16], &dir);
    let members: Vec<(Account, u32)> = fees.members.iter().map(|m| (*m, USD)).collect();
    open_all(&mut c, &members).expect("members opened");
    c.router.add_pool(fees.clone());
    let before_cross = c.metrics.cross;
    let mut tickets = Vec::new();
    for (i, a) in accts.iter().enumerate() {
        let (t, _) = c
            .transfer(None, *a, fees.id, USD, 10 + i as u64)
            .expect("routed");
        tickets.push(t);
    }
    c.drain().expect("drained");
    // Every credit to the pool committed with its debit: none travelled.
    assert!(
        tickets
            .iter()
            .all(|t| c.outcome(*t) == Some(Outcome::Final))
    );
    assert_eq!(c.metrics.cross, before_cross);
    let bal = |c: &Cluster<NativePort>, m: &Account| {
        c.shards()[usize::from(dir.shard_of(m).0)]
            .replica()
            .balance(m, USD)
            .unwrap() as u64
    };
    let total: u64 = fees.members.iter().map(|m| bal(&c, m)).sum();
    assert_eq!(total, (0..32u64).map(|i| 10 + i).sum::<u64>());
    // The operator rebalances on a threshold: every member ends at or above it.
    let balances: Vec<u64> = fees.members.iter().map(|m| bal(&c, m)).collect();
    let low = total / 8;
    let mut k = 0u64;
    for cmd in fees.rebalance(&balances, USD, low, || {
        k += 1;
        op(1 << 50 | k)
    }) {
        c.operator(cmd);
    }
    c.drain().expect("rebalanced");
    assert!(fees.members.iter().all(|m| bal(&c, m) >= low));
    assert_eq!(fees.members.iter().map(|m| bal(&c, m)).sum::<u64>(), total);
    // A debit from the pool draws on one member.
    let (t, order) = c.transfer(None, fees.id, accts[0], USD, 5).expect("routed");
    assert!(fees.is_member(&order.command.home()));
    c.drain().expect("drained");
    assert!(c.outcome(t).unwrap().accepted());
}

#[test]
fn the_agent_answers_only_its_driver_and_is_founded_once() {
    let mut port = NativePort::new(DRIVER);
    let dir = Directory::even(2);
    let found = |port: &mut NativePort| {
        let private = agent::encode(&agent::Private {
            root: ROOT,
            openings: Vec::new(),
        });
        let key = port.blob_key(&private);
        let msg = agent::encode(&Envelope::Found {
            role: Role::Shard {
                shard: ShardId(0),
                starts: dir.starts().to_vec(),
            },
            private: Some(key),
        });
        agent::decode::<Reply>(&port.invoke(msg, vec![private]).unwrap()).unwrap()
    };
    assert!(matches!(found(&mut port), Reply::Founded { root: Some(_) }));
    assert!(matches!(found(&mut port), Reply::Refused(_)));
    let msg = agent::encode(&Envelope::Principals(vec![]));
    let r = agent::decode::<Reply>(&port.invoke_as([0xBA; 32], msg, vec![])).unwrap();
    assert_eq!(r, Reply::Refused("not the driver".into()));
}

#[test]
fn req_rt_6_no_private_value_reaches_the_agents_rows() {
    let accts = accounts(8);
    let mut c = funded(2, &accts, 77);
    for i in 0..2u16 {
        let rows = c.shard_mut(ShardId(i)).port().rows().clone();
        for (k, v) in &rows.0 {
            assert!(
                !v.windows(32).any(|w| w == ROOT) && !k.windows(32).any(|w| w == ROOT),
                "the key-ring root in a row"
            );
        }
    }
}

#[test]
fn a_divergent_agent_stops_the_host() {
    // An Agent whose rows were tampered with no longer agrees with its replica.
    let accts = accounts(8);
    let mut c = funded(2, &accts, 100);
    let dir = c.directory().clone();
    let payer = accts[0];
    let home = dir.shard_of(&payer);
    // Forget, in the Agent only, that the payer is open there.
    let mut key = vec![b'n'];
    key.extend_from_slice(&payer);
    key.extend_from_slice(&USD.to_be_bytes());
    c.shard_mut(home).port().rows_mut().0.remove(&key);
    c.transfer(None, payer, accts[1], USD, 1).expect("routed");
    assert!(matches!(c.step(), Err(HostError::Divergence(_))));
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 6, .. ProptestConfig::default() })]

    /// Zero divergence (`NFR-9`, `INV-25`) through the Agents: whatever the shard count, the
    /// batch and the relay's faults, every balance equals one book's, fed what the shards
    /// accepted in the order they decided it, and every credit applied exactly once.
    #[test]
    fn nfr_9_zero_divergence_through_the_agents(
        n in 2u16..=4,
        batch in 1usize..=12,
        duplicate in any::<bool>(),
        reorder in any::<bool>(),
        drop_head in 0usize..3,
        lose_acks in any::<bool>(),
        picks in proptest::collection::vec((0usize..40, 0usize..40, any::<bool>(), 1u64..300), 1..80),
    ) {
        let (c, accts, orders) = run_mixed(
            n,
            Faults { duplicate, reorder, drop_head, lose_acks },
            batch,
            &picks,
        );
        exactly_once(&c, &orders);
        let accepted = accepted_in_order(&c, &orders);
        prop_assert_eq!(balances(&c, &accts), one_book(&accts, 1_000, &accepted));
        for asset in [USD, EUR] {
            prop_assert!(c.trial_balance(asset).holds());
        }
    }
}
