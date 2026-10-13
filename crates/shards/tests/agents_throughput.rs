// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The shard Agents' throughput against their count, natively (`T-031-20`, `NFR-18`): the
//! Agents' own transition over their rows (`NativePort`, the code the VOS guest runs), each
//! reply checked against its replica, the relay between them. An observation, not a
//! requirement; the same workload on VOS is `aledgent-core-host shards` (stress.yml).
//!
//!     ALEDGENT_SHARDS_OPS=4000 cargo test --release -p shards --test agents_throughput -- --ignored --nocapture

use std::time::Instant;

use shards::Command;
use shards::host::{Cluster, NativePort, accounts, open_all};

const USD: u32 = 840;

#[test]
#[ignore = "a measurement: run with --ignored --nocapture"]
fn nfr_18_shard_agents_against_their_count() {
    let ops: u64 = std::env::var("ALEDGENT_SHARDS_OPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000);
    let batch: usize = std::env::var("ALEDGENT_SHARDS_BATCH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64);
    let accts = accounts(256);
    println!("shards,ops,batch,micros,decided_per_s,cross_pct,credit_lag_p99_us,steps");
    for n in [1u16, 2, 4, 8] {
        let ports = (0..n).map(|_| NativePort::new([1; 32])).collect();
        let mut c = Cluster::found(ports, NativePort::new([1; 32]), [3; 32], [4; 32]).unwrap();
        c.batch = batch;
        let opens: Vec<(shards::Account, u32)> = accts.iter().map(|a| (*a, USD)).collect();
        open_all(&mut c, &opens).unwrap();
        for (i, a) in accts.iter().enumerate() {
            let mut op = [0u8; 16];
            op[..8].copy_from_slice(&(1u64 << 40 | i as u64).to_be_bytes());
            c.operator(Command::Issue {
                op,
                account: *a,
                asset: USD,
                amount: 1 << 40,
            });
        }
        c.drain().unwrap();
        let (d0, x0, s0) = (c.metrics.decided, c.metrics.cross, c.metrics.steps);
        c.metrics.credit_lag_us.clear();
        let t = Instant::now();
        let per_step = batch as u64 * u64::from(n);
        let mut sent = 0;
        while sent < ops || c.busy() {
            let mut k = 0;
            while sent < ops && k < per_step {
                let i = sent;
                c.transfer(
                    None,
                    accts[(i * 7919 % 256) as usize],
                    accts[(i * 104_729 % 256) as usize],
                    USD,
                    1 + i % 1_000,
                )
                .unwrap();
                sent += 1;
                k += 1;
            }
            c.step().unwrap();
        }
        let micros = t.elapsed().as_micros().max(1) as u64;
        let decided = c.metrics.decided - d0;
        assert!(c.trial_balance(USD).holds());
        println!(
            "{n},{ops},{batch},{micros},{},{},{},{}",
            u128::from(decided) * 1_000_000 / u128::from(micros),
            (c.metrics.cross - x0) * 100 / decided.max(1),
            c.metrics.credit_lag().p99_us,
            c.metrics.steps - s0
        );
    }
}

/// The same workload under native decide (`F-034`): each round committed on the native shard,
/// then replayed through the Agent (here the guest's code natively, `NativePort`) by a worker.
/// "Decided" is the native rate; "confirmed" the replay's, to the last confirmation.
///
///     cargo test --release -p shards --features native-decide --test agents_throughput -- --ignored --nocapture
#[cfg(feature = "native-decide")]
#[test]
#[ignore = "a measurement: run with --ignored --nocapture"]
fn f034_native_decide_against_shard_count() {
    let ops: u64 = std::env::var("ALEDGENT_SHARDS_OPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4_000);
    let accts = accounts(256);
    println!(
        "shards,ops,decided_per_s,confirmed_per_s,replay_lag_p50_us,replay_lag_p99_us,backlog_max"
    );
    for n in [1u16, 2, 4, 8] {
        let ports = (0..n).map(|_| NativePort::new([1; 32])).collect();
        let mut c = Cluster::found(ports, NativePort::new([1; 32]), [3; 32], [4; 32]).unwrap();
        c.batch = 64;
        let opens: Vec<(shards::Account, u32)> = accts.iter().map(|a| (*a, USD)).collect();
        open_all(&mut c, &opens).unwrap();
        for (i, a) in accts.iter().enumerate() {
            let mut op = [0u8; 16];
            op[..8].copy_from_slice(&(1u64 << 40 | i as u64).to_be_bytes());
            c.operator(Command::Issue {
                op,
                account: *a,
                asset: USD,
                amount: 1 << 40,
            });
        }
        c.drain().unwrap();
        c.enable_native(8).unwrap();
        let (d0, c0) = (c.metrics.decided, c.metrics.confirmed);
        let t = Instant::now();
        let per_step = 64 * u64::from(n);
        let mut sent = 0;
        let mut decided_us = 0;
        while sent < ops || c.busy() {
            let mut k = 0;
            while sent < ops && k < per_step {
                let i = sent;
                c.transfer(
                    None,
                    accts[(i * 7919 % 256) as usize],
                    accts[(i * 104_729 % 256) as usize],
                    USD,
                    1 + i % 1_000,
                )
                .unwrap();
                sent += 1;
                k += 1;
            }
            c.step().unwrap();
            if decided_us == 0 && sent >= ops && c.queued() == 0 {
                decided_us = t.elapsed().as_micros().max(1) as u64;
            }
        }
        let all_us = t.elapsed().as_micros().max(1) as u64;
        assert!(c.trial_balance(USD).holds());
        let lag = c.metrics.replay_lag();
        println!(
            "{n},{ops},{},{},{},{},{}",
            u128::from(c.metrics.decided - d0) * 1_000_000 / u128::from(decided_us),
            u128::from(c.metrics.confirmed - c0) * 1_000_000 / u128::from(all_us),
            lag.p50_us,
            lag.p99_us,
            c.metrics.backlog_max
        );
    }
}
