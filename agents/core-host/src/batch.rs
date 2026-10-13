// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `aledgent-core-host batch-curve`: what one invocation of the core Agent costs as the commands
//! it carries grow (`F-032`, `T-032-07`; `NFR-15`, `REQ-RT-1`).
//!
//! The driver already sends every library call the replica made since its last sync as one
//! input (`core_binding::host::Driver::sync`): batching is a matter of making `K` commands
//! natively before one sync. Each size starts its own book (genesis, an issuance grant,
//! `--accounts` funded accounts, opened eight to an invocation), then runs `--rounds` rounds of
//! `K` commands — transfers between random accounts and reservations, alternately — each round
//! one invocation. Every round is checked as `measure` checks it: the Agent commits and its roots
//! equal the replica's.
//!
//! Per round it records the invocation's wall time, its gas (the Invoke's, the actor's own, the
//! Acknowledge's), its sizes, and the native decision time of the same `K` commands on the
//! replica. The summary gives, per size, commands a second through the Agent, and the gas split
//! into the part that does not grow with `K` (the invocation's fixed cost) and the part that does.
//!
//! A size the pinned VOS refuses (a message, reply, heap, gas or availability bound) is reported
//! with the refusal and the larger sizes are still tried: each limit is a wall, and where each
//! one stands is the finding. The x86 bench's `batch-curve` mode builds VOS with those bounds
//! raised (`.github/x86-bench/run.sh`), so that the curve continues past them.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::num::NonZeroU64;
use std::time::Instant;

use core_binding::host::{DRIVER, Driver, Replica};
use core_binding::hot::{Limits, MAX_BUDGET};
use ledger_core::account::{AccountKind, Custody};
use ledger_core::asset::{Minor, NonZeroMinor};
use ledger_core::authority::{GrantKind, OperationKind, Subject, Window};
use ledger_core::id::AccountId;
use ledger_core::identity::IdentityKind;
use ledger_core::ledger::{Ctx, GrantRequest, OpenAccount, Transfer};
use ledger_core::principal::{Caller, Verifier};
use ledger_core::set::NonEmptySet;

use crate::port::VosPort;
use crate::{driver, genesis, keys, op, package, usd};

fn arg(args: &[String], name: &str, default: &str) -> String {
    args.windows(2)
        .find(|w| w[0] == name)
        .map_or_else(|| default.to_owned(), |w| w[1].clone())
}

/// A tiny deterministic generator for the accounts each command touches.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D) % n.max(1)
    }
}

struct Book {
    driver: Driver<VosPort>,
    replica: Replica,
    next: u64,
}

impl Book {
    fn ctx(&mut self, caller: Caller) -> Ctx {
        self.next += 1;
        Ctx {
            caller,
            op: op(1_000 + self.next),
            now: 10,
        }
    }

    fn sync(&mut self) -> Result<u64, String> {
        let t = Instant::now();
        self.driver
            .sync(&mut self.replica, Vec::new())
            .map_err(|e| format!("{e:?}"))?;
        self.driver.archived();
        Ok(t.elapsed().as_micros() as u64)
    }
}

/// A book with `n` funded accounts, each with its owner.
fn found(
    runtime: &[u8],
    actor: &[u8],
    n: usize,
) -> Result<(Book, Vec<(Caller, AccountId)>), String> {
    let pkg = package::admit_actor(actor).map_err(|e| format!("admit: {e:?}"))?;
    let mut rt = driver::Runtime::create(runtime, &keys()).map_err(|e| format!("{e:?}"))?;
    let installed = rt
        .install(&pkg, "aledgent-core", None)
        .map_err(|e| format!("install: {e:?}"))?;
    let port = VosPort::new(rt, installed, DRIVER);
    // The largest state budget, and an outbox that never fills within a run (nothing is
    // delivered here): the bounds under test are VOS's, not the binding's.
    let limits = Limits {
        budget: MAX_BUDGET,
        ring: 16,
        outbox: u64::MAX / 2,
    };
    let (mut driver, replica, founded) = Driver::found(
        port,
        core_binding::host::HostKeys::local([0xC5; 32], None),
        genesis(),
        op(0),
        1,
        limits,
    )
    .map_err(|e| format!("genesis: {e:?}"))?;
    driver.archived();
    let mut book = Book {
        driver,
        replica,
        next: 0,
    };
    let operator = Caller::new(founded.operator, founded.operator_credential);
    let c = book.ctx(operator);
    book.replica
        .grant_authority(
            &c,
            GrantRequest {
                kind: GrantKind::Issuance,
                principal: operator.principal,
                subject: Subject::Asset(usd()),
                ops: NonEmptySet::new([OperationKind::Issue, OperationKind::Redeem].into())
                    .ok_or("two kinds")?,
                per_operation: None,
                per_window: Some(Window {
                    length: NonZeroU64::new(1_000_000).ok_or("non-zero")?,
                    limit: NonZeroMinor::new(Minor::new(u64::MAX / 2))
                        .map_err(|e| format!("{e:?}"))?,
                }),
                assets: Some(NonEmptySet::one(usd())),
                counterparties: None,
                parent: None,
                expires: None,
            },
        )
        .map_err(|e| format!("grant: {e:?}"))?;
    book.sync()?;
    let mut accounts = Vec::new();
    for chunk in 0..n.div_ceil(8) {
        let mut people = Vec::new();
        for _ in 0..(n - chunk * 8).min(8) {
            let c = book.ctx(operator);
            let o = book
                .replica
                .open_identity(&c, IdentityKind::Person, None, Verifier::new(vec![1]))
                .map_err(|e| format!("identity: {e:?}"))?
                .value;
            people.push(Caller::new(o.identity, o.credential));
        }
        book.sync()?;
        let mut opened = Vec::new();
        for p in &people {
            let c = book.ctx(*p);
            let a = book
                .replica
                .open_account(
                    &c,
                    OpenAccount {
                        owner: p.principal,
                        kind: AccountKind::Customer,
                        custody: Custody::Delegated,
                        treasury_limits: BTreeMap::new(),
                        acknowledged: None,
                    },
                )
                .map_err(|e| format!("account: {e:?}"))?
                .value;
            opened.push((*p, a));
        }
        book.sync()?;
        for (_, a) in &opened {
            let c = book.ctx(operator);
            book.replica
                .issue(&c, usd(), *a, Minor::new(1_000_000_000_000))
                .map_err(|e| format!("issue: {e:?}"))?;
        }
        book.sync()?;
        accounts.extend(opened);
    }
    Ok((book, accounts))
}

/// One round's numbers.
#[derive(Clone, Copy, Default)]
struct Round {
    sync_us: u64,
    native_us: u64,
    invoke_gas: u64,
    actor_gas: u64,
    ack_gas: u64,
    invoke_us: u64,
    input_bytes: u64,
    message_bytes: u64,
    blob_bytes: u64,
}

/// `batch-curve <runtime.pvm> <actor.vos> <out.csv> [--sizes 1,8,32,128,512,2048] [--rounds 5]
/// [--accounts 64]`.
pub fn batch_curve(args: &[String]) {
    let (runtime, actor, out) = (&args[2], &args[3], &args[4]);
    let sizes: Vec<u64> = arg(args, "--sizes", "1,8,32,128,512,2048")
        .split(',')
        .filter_map(|s| s.trim().parse().ok())
        .collect();
    let rounds: u64 = arg(args, "--rounds", "5").parse().unwrap_or(5);
    let accounts: usize = arg(args, "--accounts", "64").parse().unwrap_or(64);
    let runtime = std::fs::read(runtime).expect("runtime");
    let actor = std::fs::read(actor).expect("package");
    let mut csv = String::from("size,round,metric,value\n");
    let mut summary: Vec<(u64, Vec<Round>, Option<String>)> = Vec::new();
    for &k in &sizes {
        let (mut book, accts) = match found(&runtime, &actor, accounts) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("size {k}: the book was not founded: {e}");
                summary.push((k, Vec::new(), Some(format!("setup: {e}"))));
                continue;
            }
        };
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ k);
        let n = u64::try_from(accts.len()).unwrap_or(1);
        let mut rs = Vec::new();
        let mut wall = None;
        for round in 0..rounds + 1 {
            let t = Instant::now();
            for i in 0..k {
                let fi = rng.below(n);
                let (p, a) = accts[fi as usize];
                let c = book.ctx(p);
                let r = if i % 2 == 0 {
                    let to = accts[((fi + 1 + rng.below(n - 1)) % n) as usize].1;
                    book.replica
                        .transfer(
                            &c,
                            &Transfer {
                                deadline: Some(1_000_000),
                                ..Transfer::single(a, to, usd(), Minor::new(10 + i))
                            },
                        )
                        .map(|_| ())
                } else {
                    book.replica
                        .reserve(&c, a, usd(), Minor::new(1 + i))
                        .map(|_| ())
                };
                if let Err(e) = r {
                    eprintln!("size {k}: a command was refused natively: {e:?}");
                }
            }
            let native_us = t.elapsed().as_micros() as u64;
            let sync_us = match book.sync() {
                Ok(us) => us,
                Err(e) => {
                    eprintln!("size {k}, round {round}: not committed: {e}");
                    wall = Some(e);
                    break;
                }
            };
            // The first round warms the runtime's caches; it is not recorded.
            if round == 0 {
                continue;
            }
            let s = book
                .driver
                .port()
                .samples
                .last()
                .copied()
                .unwrap_or_default();
            let r = Round {
                sync_us,
                native_us,
                invoke_gas: s.invoke.gas_used,
                actor_gas: s.actor_gas,
                ack_gas: s.ack.gas_used,
                invoke_us: s.invoke.elapsed_us,
                input_bytes: s.invoke.input_bytes,
                message_bytes: s.message_bytes,
                blob_bytes: s.blob_bytes,
            };
            for (m, v) in [
                ("sync_us", r.sync_us),
                ("native_us", r.native_us),
                ("invoke_gas", r.invoke_gas),
                ("actor_gas", r.actor_gas),
                ("ack_gas", r.ack_gas),
                ("invoke_us", r.invoke_us),
                ("input_bytes", r.input_bytes),
                ("message_bytes", r.message_bytes),
                ("blob_bytes", r.blob_bytes),
            ] {
                let _ = writeln!(csv, "{k},{round},{m},{v}");
            }
            eprintln!(
                "size {k}, round {round}: {} ms, invoke gas {} M, actor gas {} M, message {} B, blobs {} B",
                sync_us / 1000,
                r.invoke_gas / 1_000_000,
                r.actor_gas / 1_000_000,
                r.message_bytes,
                r.blob_bytes
            );
            rs.push(r);
        }
        summary.push((k, rs, wall));
    }
    std::fs::write(out, csv).expect("write the samples");
    println!(
        "batch,size,rounds,sync_ms_p50,sync_ms_max,per_command_us,commands_per_s,invoke_gas_M,actor_gas_M,ack_gas_M,invoke_gas_per_command_M,actor_gas_per_command_M,native_us_per_command,message_bytes,blob_bytes,input_bytes,wall"
    );
    let median = |mut v: Vec<u64>| -> u64 {
        v.sort_unstable();
        v.get(v.len().saturating_sub(1) / 2).copied().unwrap_or(0)
    };
    let mut points: Vec<(u64, u64, u64)> = Vec::new();
    for (k, rs, wall) in &summary {
        let k = (*k).max(1);
        let m = |f: fn(&Round) -> u64| median(rs.iter().map(f).collect());
        let sync = m(|r| r.sync_us);
        let max = rs.iter().map(|r| r.sync_us).max().unwrap_or(0);
        let ig = m(|r| r.invoke_gas);
        let ag = m(|r| r.actor_gas);
        if !rs.is_empty() {
            points.push((k, ig, ag));
        }
        println!(
            "batch,{k},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            rs.len(),
            sync / 1000,
            max / 1000,
            if rs.is_empty() { 0 } else { sync / k },
            (k * 1_000_000).checked_div(sync).unwrap_or(0),
            ig / 1_000_000,
            ag / 1_000_000,
            m(|r| r.ack_gas) / 1_000_000,
            ig / k / 1_000_000,
            ag / k / 1_000_000,
            m(|r| r.native_us) / k,
            m(|r| r.message_bytes),
            m(|r| r.blob_bytes),
            m(|r| r.input_bytes),
            wall.as_deref().unwrap_or("").replace(',', ";"),
        );
    }
    // The invocation's fixed cost and the per-command cost, from the smallest and the largest
    // size that committed: gas = fixed + k × per-command.
    if let (Some(a), Some(b)) = (points.first(), points.last()) {
        if b.0 > a.0 {
            let slope_i = b.1.saturating_sub(a.1) / (b.0 - a.0);
            let slope_a = b.2.saturating_sub(a.2) / (b.0 - a.0);
            println!(
                "fit: invoke gas = {} M + k × {} k; actor gas = {} M + k × {} k (sizes {} and {})",
                a.1.saturating_sub(slope_i * a.0) / 1_000_000,
                slope_i / 1_000,
                a.2.saturating_sub(slope_a * a.0) / 1_000_000,
                slope_a / 1_000,
                a.0,
                b.0
            );
        }
    }
}
