// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `aledgent-core-host shards ... --r03-gas [1,16,64]`: R-03's check, its actor gas measured exactly
//! (`T-033-13`; owner ruling `rq-01`).
//!
//! A whole stress run cannot resolve the check: rounds cost about 4.8 G of actor gas each, and two runs
//! of the same workload differ by tenths of a percent. So the probe gives the **same rounds** to two
//! receivers that differ only in their pinned mode: shard 1 twice, one Agent pinned `off`, the other
//! `report`, beside one sender (shard 0). Both keep the chains (they are kept in every mode), so the
//! difference in actor gas between them is the check itself: the certificate's Ed25519 verification,
//! the claim's hash, the judge. For each size it delivers the feed (one entry, its certificate) and
//! then `k` credits with the sender's certificate, and prints the actor gas of each delivery under both.
//! Trims (`rq-05`) go the other way, so the sender is doubled too: shard 0 pinned `off` and `report`
//! take the same trim of the `k` credits, with the receiver's certificate.

use std::sync::Arc;

use shards::Account;
use shards::attest::{Attestor, program_of, test_voters};
use shards::decide::Round;
use shards::host::{HostError, ShardAgent, accounts};
use shards::kv::MemKv;
use shards::principals::{Principals, PrincipalsCommand};
use shards::provenance::{Mode, Pins, Verdict};
use shards::{Command, Directory, Order, ShardId};

use crate::sharded::{VosPort, op};

const ROOT: [u8; 32] = [0x5D; 32];
const USD: u32 = 840;

/// One size's result.
pub(crate) struct GasRow {
    /// Credits delivered.
    pub credits: usize,
    /// The feed delivery's actor gas, `off` and `report`.
    pub feed: (u64, u64),
    /// The credits delivery's actor gas, `off` and `report`.
    pub deliver: (u64, u64),
    /// The trim's actor gas at the sender, `off` and `report` (`rq-05`).
    pub trim: (u64, u64),
}

fn find(dir: &Directory, shard: ShardId) -> Account {
    accounts(1024)
        .into_iter()
        .find(|a| dir.shard_of(a) == shard)
        .expect("an account in the shard")
}

/// The actor gas `agent` spends on `round`.
fn gas(agent: &mut ShardAgent<VosPort>, round: &Round) -> Result<(u64, Vec<Verdict>), HostError> {
    let before = agent.stats().actor_gas;
    let reply = agent.round(round)?;
    Ok((
        agent.stats().actor_gas - before,
        reply.checks.iter().map(|c| c.verdict).collect(),
    ))
}

fn one(runtime: &Arc<Vec<u8>>, package: &Arc<Vec<u8>>, k: usize) -> Result<GasRow, HostError> {
    let spawn = |d: u8| {
        VosPort::spawn(Arc::clone(runtime), Arc::clone(package), d, None).map_err(HostError::Port)
    };
    let dir = Directory::even(2);
    let mut sender = ShardAgent::found(spawn(0x60)?, ShardId(0), &dir, ROOT)?;
    let mut sender_off = ShardAgent::found(spawn(0x63)?, ShardId(0), &dir, ROOT)?;
    let mut off = ShardAgent::found(spawn(0x61)?, ShardId(1), &dir, ROOT)?;
    let mut report = ShardAgent::found(spawn(0x62)?, ShardId(1), &dir, ROOT)?;
    let mut attestor = Attestor::new(
        Mode::Report,
        program_of(runtime),
        test_voters(&[0x77; 32], 2),
        2,
    )
    .map_err(HostError::Refused)?;
    let pins = attestor.pins();
    let pins_off = Pins {
        mode: Mode::Off,
        ..pins.clone()
    };
    sender.pin(&pins)?;
    sender_off.pin(&pins_off)?;
    report.pin(&pins)?;
    off.pin(&pins_off)?;

    // The feed: the payer and the payee opened, certified by the principals Agent's voter.
    let (payer, payee) = (find(&dir, ShardId(0)), find(&dir, ShardId(1)));
    let mut registry: Principals<MemKv> = Principals::default();
    for a in [payer, payee] {
        registry.apply(&PrincipalsCommand::Open {
            account: a,
            asset: USD,
        });
    }
    attestor.certify_principals(1, registry.version(), registry.digest());
    let feed = Round {
        feed: registry.feed(0, usize::MAX),
        attest: attestor.for_feed(registry.version()).into_iter().collect(),
        ..Round::default()
    };
    sender.round(&feed)?;
    sender_off.round(&feed)?;
    let (feed_off, _) = gas(&mut off, &feed)?;
    let (feed_report, v) = gas(&mut report, &feed)?;
    if !matches!(v.as_slice(), [Verdict::Verified { .. }]) {
        return Err(HostError::Refused(format!("the feed's check: {v:?}")));
    }

    // The sender funds its payer, then decides `k` transfers to the payee: `k` credits.
    let fund = Round {
        orders: vec![Order::from(Command::Issue {
            op: op(1 << 50),
            account: payer,
            asset: USD,
            amount: 1_000_000_000,
        })],
        ..Round::default()
    };
    let transfers = Round {
        orders: (0..k as u64)
            .map(|i| {
                Order::from(Command::Transfer {
                    op: op(1 << 51 | i),
                    payer,
                    payee,
                    asset: USD,
                    amount: 1 + i,
                })
            })
            .collect(),
        ..Round::default()
    };
    sender.round(&fund)?;
    sender_off.round(&fund)?;
    let sent = sender.round(&transfers)?;
    if sender_off.round(&transfers)?.credits != sent.credits {
        return Err(HostError::Divergence(
            "the two senders sent other credits".into(),
        ));
    }
    if sent.credits.len() != k {
        return Err(HostError::Refused(format!(
            "{} credits for {k} transfers",
            sent.credits.len()
        )));
    }
    attestor.certify_shard(ShardId(0), sent.heads.seq, &sent.out, &sent.acks);
    let deliver = Round {
        credits: vec![(ShardId(0), sent.credits.clone())],
        attest: attestor
            .for_credits(ShardId(0), ShardId(1), k as u64)
            .into_iter()
            .collect(),
        ..Round::default()
    };
    let (deliver_off, _) = gas(&mut off, &deliver)?;
    let (deliver_report, v) = gas(&mut report, &deliver)?;
    if v != [Verdict::Verified { through: k as u64 }] {
        return Err(HostError::Refused(format!("the credits' check: {v:?}")));
    }

    // The trim back (`rq-05`): the receiver's commit certified with its applied head.
    let acks = report.replica().ack_heads();
    attestor.certify_shard(ShardId(1), 3, &[], &acks);
    let trim = Round {
        trims: vec![(ShardId(1), k as u64)],
        attest: attestor
            .for_trim(ShardId(1), ShardId(0), k as u64)
            .into_iter()
            .collect(),
        ..Round::default()
    };
    let (trim_off, _) = gas(&mut sender_off, &trim)?;
    let (trim_report, v) = gas(&mut sender, &trim)?;
    if v != [Verdict::Verified { through: k as u64 }] {
        return Err(HostError::Refused(format!("the trim's check: {v:?}")));
    }
    Ok(GasRow {
        credits: k,
        feed: (feed_off, feed_report),
        deliver: (deliver_off, deliver_report),
        trim: (trim_off, trim_report),
    })
}

/// Run the probe for each size in `sizes` and print its table (markdown, to stdout).
pub(crate) fn probe(runtime: &Arc<Vec<u8>>, package: &Arc<Vec<u8>>, sizes: &[usize]) -> bool {
    println!();
    println!(
        "R-03's check, actor gas exact: the same deliveries to shard 1 pinned `off` and pinned `report` (one sender, one voter)."
    );
    println!();
    println!(
        "| Credits | Feed delivery off / report | The check on the feed | Credits delivery off / report | The check on the credits | Trim off / report | The check on the trim |"
    );
    println!("|---:|---:|---:|---:|---:|---:|---:|");
    let mut ok = true;
    for &k in sizes {
        match one(runtime, package, k) {
            Ok(r) => println!(
                "| {} | {} / {} | {} | {} / {} | {} | {} / {} | {} |",
                r.credits,
                r.feed.0,
                r.feed.1,
                r.feed.1 as i128 - r.feed.0 as i128,
                r.deliver.0,
                r.deliver.1,
                r.deliver.1 as i128 - r.deliver.0 as i128,
                r.trim.0,
                r.trim.1,
                r.trim.1 as i128 - r.trim.0 as i128,
            ),
            Err(e) => {
                println!("| {k} | failed: {e} | | | | | |");
                eprintln!("r03-gas {k}: {e}");
                ok = false;
            }
        }
    }
    ok
}
