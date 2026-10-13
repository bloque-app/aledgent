// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Throughput of the shard prototype against its shard count (`F-031` lever 3, `T-031-11`,
//! `NFR-18`): an observation, not a requirement.
//!
//! Each round, every shard decides a batch of its own commands on its own thread, then the relay
//! carries the credits. Natively, on the kernel binding alone, without the service library and
//! without VOS: it shows how the split scales, not what an Agent decides.
//!
//!     ALEDGENT_SHARDS_OPS=4000 cargo test --release -p shards --test throughput -- --ignored --nocapture

use std::collections::VecDeque;
use std::time::Instant;

use shards::{Account, Command, Directory, Outcome, Relay, Shard, book, feed};

const USD: u32 = 840;

fn account(i: u32) -> Account {
    let mut a = [0u8; 16];
    a[..4].copy_from_slice(&i.to_be_bytes());
    a
}

fn op(n: u64) -> [u8; 16] {
    let mut o = [0u8; 16];
    o[..8].copy_from_slice(&n.to_be_bytes());
    o[8] = 0x7A;
    o
}

/// Decide `cmds`, routed to their shards, `batch` per shard and round, shards in parallel.
fn parallel(shards: &mut [Shard], cmds: &[Command], batch: usize) -> Vec<Outcome> {
    let dir = Directory::even(shards.len() as u16);
    let mut queues: Vec<VecDeque<Command>> = vec![VecDeque::new(); shards.len()];
    for c in cmds {
        queues[usize::from(dir.shard_of(&c.home()).0)].push_back(*c);
    }
    let mut out = Vec::with_capacity(cmds.len());
    let mut relay = Relay::default();
    relay.parallel = true;
    while queues.iter().any(|q| !q.is_empty()) {
        let rounds: Vec<Vec<Command>> = queues
            .iter_mut()
            .map(|q| q.drain(..batch.min(q.len())).collect())
            .collect();
        let results: Vec<Vec<Outcome>> = std::thread::scope(|s| {
            let handles: Vec<_> = shards
                .iter_mut()
                .zip(&rounds)
                .map(|(shard, round)| s.spawn(move || shard.execute(round)))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("a shard thread"))
                .collect()
        });
        out.extend(results.into_iter().flatten());
        relay.pump(shards);
    }
    relay.drain(shards);
    out
}

#[test]
#[ignore = "a measurement: run with --ignored --nocapture"]
fn nfr_18_throughput_against_shard_count() {
    let ops: u64 = std::env::var("ALEDGENT_SHARDS_OPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000);
    let batch: usize = std::env::var("ALEDGENT_SHARDS_BATCH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(32);
    let n_accounts = 256u32;
    println!("shards,ops,batch,seconds,ops_per_s,cross_shard_share_pct,commits");
    for n in [1u16, 2, 4, 8] {
        let mut s = book(n);
        let accts: Vec<Account> = (0..n_accounts).map(account).collect();
        let opens: Vec<Command> = accts
            .iter()
            .map(|a| Command::Open {
                account: *a,
                asset: USD,
            })
            .collect();
        parallel(&mut s, &opens, 256);
        feed(&mut s, &accts.iter().map(|a| (*a, USD)).collect::<Vec<_>>());
        let issues: Vec<Command> = accts
            .iter()
            .enumerate()
            .map(|(i, a)| Command::Issue {
                op: op(1 << 40 | i as u64),
                account: *a,
                asset: USD,
                amount: 1 << 40,
            })
            .collect();
        parallel(&mut s, &issues, 256);
        let cmds: Vec<Command> = (0..ops)
            .map(|i| Command::Transfer {
                op: op(i),
                payer: accts[(i * 7919 % u64::from(n_accounts)) as usize],
                payee: accts[(i * 104_729 % u64::from(n_accounts)) as usize],
                asset: USD,
                amount: 1 + i % 1_000,
            })
            .collect();
        let before: u64 = s.iter().map(Shard::commits).sum();
        let t = Instant::now();
        let out = parallel(&mut s, &cmds, batch);
        let micros = t.elapsed().as_micros().max(1);
        let cross = out
            .iter()
            .filter(|o| matches!(o, Outcome::Debited { .. }))
            .count();
        assert!(
            out.iter()
                .all(|o| matches!(o, Outcome::Final | Outcome::Debited { .. }))
        );
        let commits: u64 = s.iter().map(Shard::commits).sum::<u64>() - before;
        let rate = u128::from(ops) * 1_000_000 / micros;
        println!(
            "{n},{ops},{batch},{}.{:03},{rate},{},{commits}",
            micros / 1_000_000,
            micros / 1_000 % 1_000,
            cross as u64 * 100 / ops
        );
    }
}
