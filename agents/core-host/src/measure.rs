// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `aledgent-core-host measure`: the core Agent's paths through VOS (`T-005-10`, `T-005-14`,
//! `NFR-1`, `REQ-RT-1`), with `F-002`'s method (`notes/MEASUREMENTS-F002.md` §3).
//!
//! One Local Agent under the standard runtime, driven through the ABI as `serve` drives it. The
//! book is founded, an issuance grant, two identities and two funded accounts are opened, then
//! `--warmup` operations are run unrecorded and `--n` recorded. The workloads (`--mix`):
//!
//! - `reserve` (the default, `T-005-10`): reservations on one account, left open;
//! - `mixed` (`T-005-14`): in turn, a reservation and its capture into the other account, a
//!   transfer each way, and a reservation left open. It exercises what grows with the book: the
//!   posting journal, the operation store, the reservation index, the attribution records.
//! - `card` (`rd-shard-scope-2`): a card's pattern inside one account: a reservation, then its
//!   capture into the other account or, every other operation, its release;
//! - `transfers` (`rd-shard-scope-2`): transfers between the two accounts, each way in turn;
//! - `tick` (finding F18 of `F-032`): reservations left open, and every `--tick-every` of them
//!   (250 by default) a lifecycle tick that finds nothing due; then one tick past every deadline,
//!   which fires as many as one tick may. A tick's witness and heap must not grow with the open
//!   operations: before the fix, the tick read the deadline index whole, and past a few hundred
//!   open operations the Agent panicked on it (out of its 256 KiB heap). The run fails (exit 1)
//!   if any input does not commit.
//! - `open` (profiling): in turn, the operator opens an identity and its owner opens a delegated
//!   account, each its own input, batched as ingress forwards them. With `--batch` above 1 each
//!   round also prints a `PROFROUND` line to stderr (the inputs before it, its gas, bytes and
//!   times, as fields `key=value`), and at the end the service tree's leaves by kind
//!   (`leaves: kind K: N`, `leaves: total N`). It measures what opening identities and accounts
//!   costs as the book grows, nothing else.
//! - `custody`, `head`, `positions` (finding F18's class): see the `reads` module below.
//!
//! The runner's CPU model (from `/proc/cpuinfo`, where there is one) goes to stderr first.
//!
//! Each recorded invocation is timed at three points:
//!
//! - `native_us`: the library's decision on the host's replica (what the in-process core costs);
//! - `sync_us`: the driver's whole round: witnesses and private inputs built from the leaf
//!   stores, the Agent invoked and acknowledged, its reply checked against the replica, the
//!   service leaves applied;
//! - `caller_us`: the port's invoke and acknowledge, encode and decode included;
//! - `invoke_*` / `ack_*`: the runtime's two executions, as the Local driver's debug fields
//!   break them down (`notes/MEASUREMENTS-F002.md` §7), with their gas and the actor's own.
//!
//! Every sample is checked: the Agent commits, its roots equal the replica's. Rows are written in
//! `F-002`'s CSV shape (`spike,case,variant,size,sample,metric,value,unit`), `case` being the
//! command and `size` the Agent's runtime state in bytes before the sample; summaries are
//! nearest-rank, and a trend table gives the means of each hundred invocations in order, which
//! is what shows whether a cost grows with the book.
//!
//! **Batches** (`--batch N`, `T-031-05`). With N above 1 each command is staged
//! (`Driver::stage`) and every N inputs are sent as one invocation (`Driver::flush`), or more
//! where N inputs would pass VOS's bounds. Each invocation is recorded as above (case `batch`,
//! without `native_us` and `sync_us`, which belong to one input), with its round's wall time
//! (`round_us`: the commands on the replica, their staging and the flush) and inputs
//! (`round_inputs`). The last lines, `batch:`, give the round's p50 and p99 and the throughput:
//! inputs per second at the round's p50 and over the whole run, in thousandths. With N = 1 the
//! run is the single-input run, and a round is one input's `native_us` plus `sync_us`. These
//! are observations, not requirements.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::num::NonZeroU64;
use std::time::Instant;

use core_binding::host::{BatchLimits, DRIVER, DriveError, Driver, Replica};
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

fn arg(args: &[String], name: &str, default: u64) -> u64 {
    args.windows(2)
        .find(|w| w[0] == name)
        .and_then(|w| w[1].parse().ok())
        .unwrap_or(default)
}

fn arg_str<'a>(args: &'a [String], name: &str, default: &'a str) -> &'a str {
    args.windows(2)
        .find(|w| w[0] == name)
        .map_or(default, |w| w[1].as_str())
}

/// Nearest-rank percentile of sorted `v`.
fn rank(v: &[u64], p: u64) -> u64 {
    if v.is_empty() {
        return 0;
    }
    let n = v.len() as u64;
    let i = (p * n).div_ceil(100).max(1) - 1;
    v[i as usize]
}

/// The book the runs share, and the counter of operation references.
struct Book {
    driver: Driver<VosPort>,
    replica: Replica,
    next: u64,
    /// The time input of the next command.
    now: u64,
}

impl Book {
    fn ctx(&mut self, caller: Caller) -> Ctx {
        self.next += 1;
        Ctx {
            caller,
            op: op(1_000 + self.next),
            now: self.now,
        }
    }

    fn try_sync(&mut self) -> Result<u64, DriveError> {
        let t = Instant::now();
        self.driver.sync(&mut self.replica, Vec::new())?;
        self.driver.archived();
        Ok(t.elapsed().as_micros() as u64)
    }

    fn sync(&mut self) {
        self.try_sync()
            .unwrap_or_else(|e| panic!("the Agent commits: {e:?}"));
    }

    /// A person with an account holding `amount` USD, issued by `issuer`.
    fn funded(&mut self, operator: Caller, amount: u64) -> (Caller, AccountId) {
        let c = self.ctx(operator);
        let o = self
            .replica
            .open_identity(&c, IdentityKind::Person, None, Verifier::new(vec![1]))
            .expect("identity")
            .value;
        self.sync();
        let person = Caller::new(o.identity, o.credential);
        let c = self.ctx(person);
        let account = self
            .replica
            .open_account(
                &c,
                OpenAccount {
                    owner: person.principal,
                    kind: AccountKind::Customer,
                    custody: Custody::Delegated,
                    treasury_limits: BTreeMap::new(),
                    acknowledged: None,
                },
            )
            .expect("account")
            .value;
        self.sync();
        let c = self.ctx(operator);
        self.replica
            .issue(&c, usd(), account, Minor::new(amount))
            .expect("issue");
        self.sync();
        (person, account)
    }
}

/// One command of an operation, made on the replica and then synced as one input.
type Step = (&'static str, Box<dyn FnOnce(&mut Book)>);

/// A transfer of `amount` minor units, with a deadline (the measurements' configuration has no
/// default for transfers).
fn transfer(from: AccountId, to: AccountId, amount: u64) -> Transfer {
    Transfer {
        deadline: Some(1_000_000),
        ..Transfer::single(from, to, usd(), Minor::new(amount))
    }
}

/// `measure <runtime.pvm> <actor.vos> <out.csv> [--n N] [--warmup W] [--budget B] [--mix M]
/// [--batch N] [--mapped]`.
pub fn measure(args: &[String]) {
    let (runtime, actor, out) = (&args[2], &args[3], &args[4]);
    let n = arg(args, "--n", 60);
    let warmup = arg(args, "--warmup", 5);
    let batch = arg(args, "--batch", 1).max(1);
    let mix = arg_str(args, "--mix", "reserve");
    if let Ok(cpu) = std::fs::read_to_string("/proc/cpuinfo") {
        if let Some(l) = cpu.lines().find(|l| l.starts_with("model name")) {
            eprintln!("CPU {l}");
        }
    }
    let (mixed, tick) = match mix {
        "reserve" => (false, false),
        "mixed" => (true, false),
        "card" | "transfers" => (false, false),
        "tick" => (false, true),
        // Profiling: identities and accounts opened, nothing else.
        "open" => (false, false),
        // The F18 class (`reads`): each grows one collection and measures the path that read it.
        "custody" | "head" | "positions" => (false, false),
        other => panic!(
            "--mix {other}: reserve, mixed, card, transfers, tick, open, custody, head or positions"
        ),
    };
    let tick_every = arg(args, "--tick-every", 250).max(1);
    let runtime = std::fs::read(runtime).expect("runtime");
    let pkg = package::admit_actor(&std::fs::read(actor).expect("package")).expect("admit");
    let mut rt = driver::Runtime::create(&runtime, &keys()).expect("create");
    let installed = rt.install(&pkg, "aledgent-core", None).expect("install");
    let port = VosPort::new(rt, installed, DRIVER);
    // The ring holds a batch until it is archived: two batches' room, as the default does for 32
    // (`Limits::DEFAULT`). Larger batches need a larger ring and budget, and so a larger lane
    // than VOS's standard 48 KiB: the Agent's execution limits (`driver::AGENT_EXECUTION_LIMITS`).
    let ring = Limits::DEFAULT.ring.max(2 * batch);
    let limits = Limits {
        budget: arg(args, "--budget", Limits::DEFAULT.budget.max(ring * 512)).min(MAX_BUDGET),
        ring,
        ..Limits::DEFAULT
    };
    // `--mapped`: the book in the Agent's committed maps (layout 3, `F-031` lever 2).
    let found = if args.iter().any(|a| a == "--mapped") {
        Driver::found_mapped
    } else {
        Driver::found
    };
    let mut genesis = genesis();
    if mix == "custody" {
        // Succession offered, after 5 ms, for the custody workload's claims.
        genesis.lifecycle = genesis.lifecycle.with_recovery(
            ledger_core::custody::RecoveryConfig::not_offered().with_succession_delay(Some(5)),
        );
    }
    let (mut driver, replica, founded) = found(
        port,
        core_binding::host::HostKeys::local([0xC5; 32], None),
        genesis,
        op(0),
        1,
        limits,
    )
    .unwrap_or_else(|e| panic!("genesis: {e:?}"));
    driver.archived();
    let mut batch_limits =
        BatchLimits::with_inputs(batch as usize).with_actor_gas(crate::driver::actor_gas());
    if crate::port::composite_on() {
        // A composite takes one of the invocation's four caller blobs (`F-033`): three packs.
        batch_limits.max_blob_bytes = 3 * batch_limits.max_pack_bytes;
        batch_limits.max_packs = 3;
        batch_limits.max_reply_bytes -= 1024;
    }
    driver.set_batch_limits(batch_limits);
    let mut book = Book {
        driver,
        replica,
        next: 0,
        now: 10,
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
                    .expect("two kinds"),
                per_operation: None,
                per_window: Some(Window {
                    length: NonZeroU64::new(1_000_000).expect("non-zero"),
                    limit: NonZeroMinor::new(Minor::new(u64::MAX / 2)).expect("non-zero"),
                }),
                assets: Some(NonEmptySet::one(usd())),
                counterparties: None,
                parent: None,
                expires: None,
            },
        )
        .expect("grant");
    book.sync();
    let (p, a) = book.funded(operator, 1_000_000_000);
    let (q, b) = book.funded(operator, 1_000_000_000);
    if matches!(mix, "custody" | "head" | "positions") {
        let failed = reads::run(
            &mut book,
            mix,
            warmup + n,
            tick_every,
            operator,
            (p, a),
            out,
        );
        if failed {
            std::process::exit(1);
        }
        return;
    }

    let mut csv = String::from("spike,case,variant,size,sample,metric,value,unit\n");
    let mut series: BTreeMap<(&str, &str), Vec<u64>> = BTreeMap::new();
    let mut order: Vec<BTreeMap<&str, u64>> = Vec::new();
    // Each round (`T-031-05`): its inputs and its wall time, staging included.
    let mut rounds: Vec<(u64, u64)> = Vec::new();
    let mut sample = 0u64;
    let (card, transfers) = (mix == "card", mix == "transfers");
    let steps_of = |i: u64| -> Vec<Step> {
        // One operation: the commands it makes, each one input.
        let mut steps: Vec<Step> = Vec::new();
        if card {
            let reserved = std::rc::Rc::new(std::cell::Cell::new(None));
            let r2 = reserved.clone();
            steps.push((
                "reserve",
                Box::new(move |bk: &mut Book| {
                    let c = bk.ctx(p);
                    bk.replica
                        .reserve(&c, a, usd(), Minor::new(100 + i))
                        .expect("reserve");
                    r2.set(Some(c));
                }),
            ));
            if i % 2 == 0 {
                steps.push((
                    "capture",
                    Box::new(move |bk: &mut Book| {
                        let c = reserved.get().expect("the reservation's context");
                        bk.replica
                            .capture(&c, b, Minor::new(50 + i))
                            .expect("capture");
                    }),
                ));
            } else {
                steps.push((
                    "release",
                    Box::new(move |bk: &mut Book| {
                        let c = reserved.get().expect("the reservation's context");
                        bk.replica.release(&c).expect("release");
                    }),
                ));
            }
            return steps;
        }
        if mix == "open" {
            // One identity (the operator opens it) and its delegated account (the person opens
            // it), each its own input, as the canary's ingress forwards them.
            let who = std::rc::Rc::new(std::cell::Cell::new(None));
            let w2 = who.clone();
            steps.push((
                "open_identity",
                Box::new(move |bk: &mut Book| {
                    let c = bk.ctx(operator);
                    let mut key = vec![0u8; 32];
                    key[..8].copy_from_slice(&(i + 1).to_be_bytes());
                    let o = bk
                        .replica
                        .open_identity(&c, IdentityKind::Person, None, Verifier::new(key))
                        .expect("identity")
                        .value;
                    w2.set(Some(Caller::new(o.identity, o.credential)));
                }),
            ));
            steps.push((
                "open_account",
                Box::new(move |bk: &mut Book| {
                    let person = who.get().expect("the identity just opened");
                    let c = bk.ctx(person);
                    bk.replica
                        .open_account(
                            &c,
                            OpenAccount {
                                owner: person.principal,
                                kind: AccountKind::Customer,
                                custody: Custody::Delegated,
                                treasury_limits: BTreeMap::new(),
                                acknowledged: None,
                            },
                        )
                        .expect("account");
                }),
            ));
            return steps;
        }
        if transfers {
            let (who, from, to) = if i % 2 == 0 { (p, a, b) } else { (q, b, a) };
            steps.push((
                "transfer",
                Box::new(move |bk: &mut Book| {
                    let c = bk.ctx(who);
                    bk.replica
                        .transfer(&c, &transfer(from, to, 10 + i))
                        .expect("transfer");
                }),
            ));
            return steps;
        }
        if tick && i % tick_every == tick_every - 1 {
            // A tick that finds nothing due: every reservation's deadline is 1 000 ms after 10.
            let now = 11 + i / tick_every;
            steps.push((
                "tick",
                Box::new(move |bk: &mut Book| {
                    bk.replica.tick(now).expect("tick");
                }),
            ));
        }
        match (mixed, i % 4) {
            (false, _) => steps.push((
                "reserve",
                Box::new(move |bk: &mut Book| {
                    let c = bk.ctx(p);
                    bk.replica
                        .reserve(&c, a, usd(), Minor::new(100 + i))
                        .expect("reserve");
                }),
            )),
            (true, 0) => {
                let reserved = std::rc::Rc::new(std::cell::Cell::new(None));
                let r2 = reserved.clone();
                steps.push((
                    "reserve",
                    Box::new(move |bk: &mut Book| {
                        let c = bk.ctx(p);
                        bk.replica
                            .reserve(&c, a, usd(), Minor::new(100 + i))
                            .expect("reserve");
                        r2.set(Some(c));
                    }),
                ));
                steps.push((
                    "capture",
                    Box::new(move |bk: &mut Book| {
                        let c = reserved.get().expect("the reservation's context");
                        bk.replica
                            .capture(&c, b, Minor::new(50 + i))
                            .expect("capture");
                    }),
                ));
            }
            (true, 1) => steps.push((
                "transfer",
                Box::new(move |bk: &mut Book| {
                    let c = bk.ctx(p);
                    bk.replica
                        .transfer(&c, &transfer(a, b, 10 + i))
                        .expect("transfer");
                }),
            )),
            (true, 2) => steps.push((
                "transfer",
                Box::new(move |bk: &mut Book| {
                    let c = bk.ctx(q);
                    bk.replica
                        .transfer(&c, &transfer(b, a, 10 + i))
                        .expect("transfer");
                }),
            )),
            (true, _) => steps.push((
                "reserve-open",
                Box::new(move |bk: &mut Book| {
                    let c = bk.ctx(q);
                    bk.replica
                        .reserve(&c, b, usd(), Minor::new(1 + i))
                        .expect("reserve");
                }),
            )),
        }
        steps
    };
    if batch > 1 {
        batched(
            &mut book,
            &steps_of,
            warmup + n,
            warmup,
            batch,
            &mut csv,
            &mut series,
            &mut order,
            &mut rounds,
        );
    }
    let mut failed = false;
    'run: for i in (0..warmup + n).filter(|_| batch == 1) {
        let steps = steps_of(i);
        for (case, step) in steps {
            let size = book.driver.port().runtime().state_bytes() as u64;
            let t = Instant::now();
            step(&mut book);
            let native_us = t.elapsed().as_micros() as u64;
            let sync_us = match book.try_sync() {
                Ok(us) => us,
                Err(e) => {
                    eprintln!("operation {i} ({case}) not committed, the run ends: {e:?}");
                    failed = true;
                    break 'run;
                }
            };
            if i < warmup {
                continue;
            }
            let s = book
                .driver
                .port()
                .samples
                .last()
                .copied()
                .unwrap_or_default();
            let mut row = BTreeMap::new();
            for (metric, value, unit) in [
                ("native_us", native_us, "us"),
                ("sync_us", sync_us, "us"),
                ("caller_us", s.caller_us, "us"),
                ("invoke_us", s.invoke.elapsed_us, "us"),
                ("invoke_encode_us", s.invoke.encode_us, "us"),
                ("invoke_preparation_us", s.invoke.preparation_us, "us"),
                ("invoke_execution_us", s.invoke.execution_us, "us"),
                ("invoke_decode_us", s.invoke.decode_us, "us"),
                ("invoke_input_bytes", s.invoke.input_bytes, "bytes"),
                ("invoke_output_bytes", s.invoke.output_bytes, "bytes"),
                ("invoke_gas", s.invoke.gas_used, "gas"),
                ("ack_us", s.ack.elapsed_us, "us"),
                ("ack_gas", s.ack.gas_used, "gas"),
                ("actor_gas", s.actor_gas, "gas"),
                ("message_bytes", s.message_bytes, "bytes"),
                ("blob_bytes", s.blob_bytes, "bytes"),
                ("state_bytes", size, "bytes"),
            ] {
                let _ = writeln!(
                    csv,
                    "s5,{case},vos-abi,{size},{sample},{metric},{value},{unit}"
                );
                series.entry((case, metric)).or_default().push(value);
                series.entry(("all", metric)).or_default().push(value);
                row.insert(metric, value);
            }
            order.push(row);
            eprintln!(
                "{case} {sample} (operation {}): sync {} ms, actor gas {} M, blobs {} B, state {size} B",
                i - warmup,
                sync_us / 1000,
                s.actor_gas / 1_000_000,
                s.blob_bytes
            );
            rounds.push((1, native_us + sync_us));
            sample += 1;
        }
    }
    if tick && !failed {
        // Past every deadline: one tick fires as many as a tick may (`max_fires_per_tick`).
        let open = book.replica.state().operations().open().count();
        book.replica.tick(5_000).expect("tick");
        match book.try_sync() {
            Ok(us) => {
                let s = book
                    .driver
                    .port()
                    .samples
                    .last()
                    .copied()
                    .unwrap_or_default();
                eprintln!(
                    "tick-fire ({open} open): sync {} ms, actor gas {} M, blobs {} B",
                    us / 1000,
                    s.actor_gas / 1_000_000,
                    s.blob_bytes
                );
            }
            Err(e) => {
                eprintln!("the tick past every deadline ({open} open) not committed: {e:?}");
                failed = true;
            }
        }
    }
    std::fs::write(out, csv).expect("write the samples");
    // Recorded inputs, one invocation each; in a batched run, recorded invocations.
    let recorded = series
        .get(&("all", "sync_us"))
        .or(series.get(&("all", "invoke_gas")))
        .map_or(0, Vec::len);
    let row = |v: &mut Vec<u64>| {
        v.sort_unstable();
        format!(
            "{},{},{},{}",
            rank(v, 50),
            rank(v, 95),
            rank(v, 99),
            v.last().copied().unwrap_or_default()
        )
    };
    // Every invocation, in `T-005-10`'s shape (the x86 bench reads it); then, for the mixed
    // workload, each command apart.
    println!(
        "metric,p50,p95,p99,max (n = {recorded}, warm-up {warmup}, budget {} B)",
        limits.budget
    );
    for ((case, metric), v) in series.iter_mut() {
        if *case == "all" {
            println!("{metric},{}", row(v));
        }
    }
    if mixed {
        println!("case,metric,p50,p95,p99,max");
        for ((case, metric), v) in series.iter_mut() {
            if *case != "all" {
                println!("{case},{metric},{}", row(v));
            }
        }
    }
    println!(
        "trend: invocations,state_bytes_max,invoke_gas_mean,actor_gas_mean,blob_bytes_mean,invoke_us_mean"
    );
    for (w, chunk) in order.chunks(100).enumerate() {
        let mean = |m: &str| chunk.iter().map(|r| r[m]).sum::<u64>() / chunk.len() as u64;
        let max = |m: &str| chunk.iter().map(|r| r[m]).max().unwrap_or_default();
        println!(
            "trend: {}-{},{},{},{},{},{}",
            w * 100 + 1,
            w * 100 + chunk.len(),
            max("state_bytes"),
            mean("invoke_gas"),
            mean("actor_gas"),
            mean("blob_bytes"),
            mean("invoke_us"),
        );
    }
    // Throughput (`T-031-05`): inputs per round over the round's wall time, staging included.
    // An observation, not a requirement.
    let invocations = series.get(&("all", "invoke_gas")).map_or(0, Vec::len);
    let inputs: u64 = rounds.iter().map(|r| r.0).sum();
    let total_us: u64 = rounds.iter().map(|r| r.1).sum();
    let mut round_us: Vec<u64> = rounds.iter().map(|r| r.1).collect();
    round_us.sort_unstable();
    let p50 = |m: &str| {
        let mut v = series.get(&("all", m)).cloned().unwrap_or_default();
        v.sort_unstable();
        rank(&v, 50)
    };
    let per_s = |ops: u64, us: u64| (ops * 1_000_000_000).checked_div(us).unwrap_or(0);
    println!(
        "batch: size,rounds,inputs,invocations,round_p50_us,round_p99_us,invoke_gas_p50,actor_gas_p50,milli_ops_per_s_p50,milli_ops_per_s_mean"
    );
    println!(
        "batch: {batch},{},{inputs},{invocations},{},{},{},{},{},{}",
        rounds.len(),
        rank(&round_us, 50),
        rank(&round_us, 99),
        p50("invoke_gas"),
        p50("actor_gas"),
        per_s(batch, rank(&round_us, 50)),
        per_s(inputs, total_us),
    );
    if let Some(h) = {
        use core_binding::host::AgentPort as _;
        book.driver.port_mut().hot().ok().flatten()
    } {
        println!(
            "hot: inputs {}, state {} B (runtime {} B), budget {} B",
            h.inputs,
            h.bytes(),
            book.driver.port().runtime().state_bytes(),
            h.limits.budget
        );
    }
    if mix == "open" {
        // The service tree's leaves, by kind (profiling).
        let st = book.replica.state();
        let mut total = 0usize;
        for k in st.leaf_kinds() {
            let n = st.leaf_keys(k).map(|v| v.len()).unwrap_or(0);
            total += n;
            println!("leaves: kind {k}: {n}");
        }
        println!("leaves: total {total}");
    }
    // The tick workload checks that every input commits (finding F18).
    if tick && failed {
        std::process::exit(1);
    }
}

/// The batched run (`T-031-05`): each operation's commands are staged as inputs, and every
/// `batch` inputs are flushed as one invocation, or more where a batch would pass VOS's bounds
/// (`core_binding::host::BatchLimits`). A round is the staging of its inputs (each command on the
/// replica, then its witnesses) and its flush; its time is wall time. Every invocation is
/// recorded as the single-input run records one, under the case `batch`, and the round's inputs
/// and time in `rounds`. Rounds do not cross the end of the warm-up.
#[allow(clippy::too_many_arguments)]
fn batched<'a>(
    book: &mut Book,
    steps_of: &dyn Fn(u64) -> Vec<Step>,
    operations: u64,
    warmup: u64,
    batch: u64,
    csv: &mut String,
    series: &mut BTreeMap<(&'a str, &'a str), Vec<u64>>,
    order: &mut Vec<BTreeMap<&'a str, u64>>,
    rounds: &mut Vec<(u64, u64)>,
) {
    let mut staged = 0u64;
    let mut started: Option<Instant> = None;
    let mut first = book.driver.port().samples.len();
    let mut size = book.driver.port().runtime().state_bytes() as u64;
    let mut sample = 0u64;
    let mut flush = |book: &mut Book,
                     staged: &mut u64,
                     started: &mut Option<Instant>,
                     first: &mut usize,
                     size: &mut u64,
                     record: bool|
     -> bool {
        if *staged == 0 {
            return true;
        }
        if let Err(e) = book.driver.flush() {
            eprintln!("a round of {staged} inputs not committed, the run ends: {e:?}");
            return false;
        }
        book.driver.archived();
        let us = started.take().map_or(0, |t| t.elapsed().as_micros() as u64);
        let samples = book.driver.port().samples[*first..].to_vec();
        *first = book.driver.port().samples.len();
        if record {
            rounds.push((*staged, us));
            for s in &samples {
                let mut row = BTreeMap::new();
                for (metric, value, unit) in [
                    ("caller_us", s.caller_us, "us"),
                    ("invoke_us", s.invoke.elapsed_us, "us"),
                    ("invoke_encode_us", s.invoke.encode_us, "us"),
                    ("invoke_preparation_us", s.invoke.preparation_us, "us"),
                    ("invoke_execution_us", s.invoke.execution_us, "us"),
                    ("invoke_decode_us", s.invoke.decode_us, "us"),
                    ("invoke_input_bytes", s.invoke.input_bytes, "bytes"),
                    ("invoke_output_bytes", s.invoke.output_bytes, "bytes"),
                    ("invoke_gas", s.invoke.gas_used, "gas"),
                    ("ack_us", s.ack.elapsed_us, "us"),
                    ("ack_gas", s.ack.gas_used, "gas"),
                    ("actor_gas", s.actor_gas, "gas"),
                    ("message_bytes", s.message_bytes, "bytes"),
                    ("blob_bytes", s.blob_bytes, "bytes"),
                    ("state_bytes", *size, "bytes"),
                    ("round_us", us, "us"),
                    ("round_inputs", *staged, "inputs"),
                ] {
                    let _ = writeln!(
                        csv,
                        "s5,batch,vos-abi-batch{batch},{size},{sample},{metric},{value},{unit}"
                    );
                    series.entry(("all", metric)).or_default().push(value);
                    row.insert(metric, value);
                }
                order.push(row);
                sample += 1;
            }
            static SEEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let before = SEEN.fetch_add(*staged, std::sync::atomic::Ordering::Relaxed);
            eprintln!(
                "round of {staged}: {} ms, {} invocations, actor gas {} M",
                us / 1000,
                samples.len(),
                samples.iter().map(|s| s.actor_gas).sum::<u64>() / 1_000_000
            );
            eprintln!(
                "PROFROUND before={before} inputs={staged} invocations={} actor_gas={} invoke_gas={} ack_gas={} blob_bytes={} message_bytes={} invoke_input_bytes={} caller_us={} exec_us={} invoke_us={} ack_us={} round_us={us}",
                samples.len(),
                samples.iter().map(|s| s.actor_gas).sum::<u64>(),
                samples.iter().map(|s| s.invoke.gas_used).sum::<u64>(),
                samples.iter().map(|s| s.ack.gas_used).sum::<u64>(),
                samples.iter().map(|s| s.blob_bytes).sum::<u64>(),
                samples.iter().map(|s| s.message_bytes).sum::<u64>(),
                samples.iter().map(|s| s.invoke.input_bytes).sum::<u64>(),
                samples.iter().map(|s| s.caller_us).sum::<u64>(),
                samples.iter().map(|s| s.invoke.execution_us).sum::<u64>(),
                samples.iter().map(|s| s.invoke.elapsed_us).sum::<u64>(),
                samples.iter().map(|s| s.ack.elapsed_us).sum::<u64>(),
            );
        }
        *size = book.driver.port().runtime().state_bytes() as u64;
        *staged = 0;
        true
    };
    for i in 0..operations {
        if i == warmup
            && !flush(
                book,
                &mut staged,
                &mut started,
                &mut first,
                &mut size,
                false,
            )
        {
            return;
        }
        for (case, step) in steps_of(i) {
            started.get_or_insert_with(Instant::now);
            step(book);
            match book.driver.stage(&mut book.replica, Vec::new()) {
                Ok(Some(_)) => staged += 1,
                Ok(None) => {}
                Err(e) => {
                    eprintln!("operation {i} ({case}) not staged, the run ends: {e:?}");
                    return;
                }
            }
            if staged == batch
                && !flush(
                    book,
                    &mut staged,
                    &mut started,
                    &mut first,
                    &mut size,
                    i >= warmup,
                )
            {
                return;
            }
        }
    }
    flush(book, &mut staged, &mut started, &mut first, &mut size, true);
}

/// The F18 class (`--mix custody | head | positions`): finding F18 of `F-032` was a lifecycle
/// tick that read a growing collection whole, so the witness the Agent was given grew with the
/// book until the Agent's 256 KiB heap ran out. Each workload here grows one more such collection
/// and, every `--tick-every` entries, runs the path that read it, through VOS:
///
/// - `custody`: successions named, claimed and completed on one account, back and forth; the
///   measured input is the replay of the last completing claim (custody's pending changes);
/// - `head`: withdrawals instructed, acknowledged and signed, left in flight; the measured input
///   is a finalised head that decides nothing, and at the end one past every expiry (the
///   settlement instructions in flight and awaiting a nonce);
/// - `positions`: deposits observed and found final, a lifecycle tick after each; the measured
///   inputs are the adapter's positions at the inbox's length and a re-scan of its last four
///   positions (the ingested records).
///
/// Every measured input must commit (exit 1 otherwise), and its actor gas and blob bytes are
/// printed with the collection's size, so a run shows whether they grow with it.
mod reads {
    use super::*;
    use ledger_core::anchor::PositionCut;
    use ledger_core::authority::OperationKind;
    use ledger_core::ledger::settle_adapter::AdapterInput;
    use ledger_core::ledger::settlement::Withdrawal;
    use ledger_core::settlement::{
        CanonicalCheck, Coordinate, Destination, EventKind, FinalisedHead, KeyNonce, Movement,
        Observation, ObservedFinality, Payload, SettlementConfig, Signing, SigningRecord, SourceId,
        SupportDeclaration,
    };

    const KEY: [u8; 32] = [0x5E; 32];

    /// Sync what the replica recorded; on failure, say which input and stop.
    fn step(book: &mut Book, case: &str, size: u64) -> bool {
        match book.try_sync() {
            Ok(_) => true,
            Err(e) => {
                eprintln!("{case} at {size} not committed, the run ends: {e:?}");
                false
            }
        }
    }

    /// [`step`], measured: one CSV row per metric, and a line on stderr.
    fn measured(book: &mut Book, case: &str, size: u64, csv: &mut String) -> bool {
        let Ok(us) = book.try_sync().map_err(|e| {
            eprintln!("{case} at {size} not committed, the run ends: {e:?}");
        }) else {
            return false;
        };
        let s = book
            .driver
            .port()
            .samples
            .last()
            .copied()
            .unwrap_or_default();
        for (metric, value, unit) in [
            ("sync_us", us, "us"),
            ("actor_gas", s.actor_gas, "gas"),
            ("invoke_gas", s.invoke.gas_used, "gas"),
            ("message_bytes", s.message_bytes, "bytes"),
            ("blob_bytes", s.blob_bytes, "bytes"),
        ] {
            let _ = writeln!(
                csv,
                "f18-class,{case},vos-abi,{size},0,{metric},{value},{unit}"
            );
        }
        eprintln!(
            "{case} at {size}: sync {} ms, actor gas {} M, blobs {} B, message {} B",
            us / 1000,
            s.actor_gas / 1_000_000,
            s.blob_bytes,
            s.message_bytes
        );
        println!(
            "{case},{size},{},{},{}",
            s.actor_gas, s.blob_bytes, s.message_bytes
        );
        true
    }

    /// The settlement adapter's principal, the configuration naming it, its support for USD both
    /// ways, and a first finalised head (an instruction's expiry is decided from it).
    fn settlement(book: &mut Book, operator: Caller) -> Option<(Caller, SourceId)> {
        let c = book.ctx(operator);
        let o = book
            .replica
            .open_identity(&c, IdentityKind::Organisation, None, Verifier::new(vec![9]))
            .expect("adapter identity")
            .value;
        if !step(book, "adapter", 0) {
            return None;
        }
        let adapter = Caller::new(o.identity, o.credential);
        let source = SourceId::of_identity(&adapter.principal);
        let c = book.ctx(operator);
        book.replica
            .configure_settlement(
                &c,
                SettlementConfig {
                    authoritative: source,
                    instance: 1,
                    suspense: None,
                    inbound_deadline: 1_000_000,
                    expiry_window: 1_000,
                    block_time: 1,
                    finality_margin: 1,
                },
            )
            .expect("configure");
        if !step(book, "configure", 0) {
            return None;
        }
        book.replica
            .declare_support(
                source,
                SupportDeclaration {
                    legs: [
                        (OperationKind::Redeem, usd()),
                        (OperationKind::Issue, usd()),
                    ]
                    .into(),
                },
            )
            .expect("support");
        if !step(book, "support", 0) {
            return None;
        }
        let now = book.now;
        book.replica
            .observe_head(
                source,
                FinalisedHead {
                    height: 1,
                    next_seq: 0,
                },
                &[],
                now,
            )
            .expect("head");
        step(book, "head", 0).then_some((adapter, source))
    }

    fn coordinate(seq: u64) -> Coordinate {
        let mut digest = [0x5A; 32];
        digest[..8].copy_from_slice(&seq.to_be_bytes());
        Coordinate {
            instance: 1,
            seq,
            digest,
        }
    }

    /// Run workload `mix` over `n` entries, measuring every `every`. Whether an input failed.
    pub(super) fn run(
        book: &mut Book,
        mix: &str,
        n: u64,
        every: u64,
        operator: Caller,
        (p, a): (Caller, AccountId),
        out: &str,
    ) -> bool {
        let mut csv = String::from("spike,case,variant,size,sample,metric,value,unit\n");
        println!("case,size,actor_gas,blob_bytes,message_bytes");
        let due = |i: u64| i % every == every - 1 || i + 1 == n;
        let ok = match mix {
            "custody" => {
                let (mut owner, account) = book.funded(operator, 1);
                let (mut other, _) = book.funded(operator, 1);
                let mut ok = true;
                for i in 0..n {
                    let c = book.ctx(owner);
                    book.replica
                        .name_successor(&c, account, Some(other.principal))
                        .expect("name");
                    let c = book.ctx(other);
                    ok = step(book, "name", i)
                        && book.replica.claim_succession(&c, account).is_ok()
                        && step(book, "claim", i);
                    if !ok {
                        break;
                    }
                    book.now += 5;
                    let completing = book.ctx(other);
                    book.replica
                        .claim_succession(&completing, account)
                        .expect("complete");
                    if !step(book, "complete", i) {
                        ok = false;
                        break;
                    }
                    if due(i) {
                        // The completing claim again: answered from the book as the first time.
                        book.replica
                            .claim_succession(&completing, account)
                            .expect("replay");
                        if !measured(book, "claim-replay", i + 1, &mut csv) {
                            ok = false;
                            break;
                        }
                    }
                    std::mem::swap(&mut owner, &mut other);
                }
                ok
            }
            "head" => {
                let Some((_, source)) = settlement(book, operator) else {
                    return true;
                };
                let head = |height| FinalisedHead {
                    height,
                    next_seq: 0,
                };
                let mut ok = true;
                for i in 0..n {
                    let c = book.ctx(p);
                    book.replica
                        .withdraw(
                            &c,
                            &Withdrawal {
                                payer: a,
                                asset: usd(),
                                amount: Minor::new(1 + i),
                                deadline: Some(1_000_000),
                            },
                        )
                        .expect("withdraw");
                    if !step(book, "withdraw", i) {
                        ok = false;
                        break;
                    }
                    let instruction = book
                        .replica
                        .submit_outbound(&c, Destination::Account([0x70; 32]))
                        .expect("submit")
                        .value;
                    let now = book.now;
                    ok = step(book, "submit", i)
                        && book
                            .replica
                            .acknowledge(source, c.op, instruction.digest(), now)
                            .is_ok()
                        && step(book, "acknowledge", i)
                        && book
                            .replica
                            .record_signing(
                                source,
                                c.op,
                                instruction.digest(),
                                SigningRecord::Signed(Signing {
                                    key: KEY,
                                    nonce: 1 + i,
                                    last_valid: 1_000,
                                }),
                                now,
                            )
                            .is_ok()
                        && step(book, "sign", i);
                    if !ok {
                        break;
                    }
                    if due(i) {
                        book.replica
                            .observe_head(source, head(1), &[KeyNonce { key: KEY, nonce: 0 }], now)
                            .expect("head");
                        if !measured(book, "head", i + 1, &mut csv) {
                            ok = false;
                            break;
                        }
                    }
                }
                if ok {
                    // Past every leg's expiry and last valid height.
                    let now = book.now;
                    let r = book
                        .replica
                        .observe_head(
                            source,
                            head(1_000_000),
                            &[KeyNonce { key: KEY, nonce: 0 }],
                            now,
                        )
                        .expect("head");
                    eprintln!("head-fire: moved {}", r.moved.len());
                    ok = measured(book, "head-fire", n, &mut csv);
                }
                ok
            }
            _ => {
                let Some((adapter, source)) = settlement(book, operator) else {
                    return true;
                };
                let mut ok = true;
                for seq in 0..n {
                    let now = book.now;
                    let o = Observation {
                        source,
                        coordinate: coordinate(seq),
                        at: 1,
                        kind: EventKind::Deposited,
                        payload: Payload::default(),
                        observed_finality: ObservedFinality::Final,
                        operation_ref: None,
                        account_ref: Some(ledger_core::id::account_reference(&a)),
                        movement: Some(Movement {
                            asset: usd(),
                            amount: Minor::new(1),
                        }),
                        anchor: None,
                    };
                    book.replica.record_observation(o, now).expect("observe");
                    ok = step(book, "observe", seq)
                        && book
                            .replica
                            .verify(
                                CanonicalCheck {
                                    source,
                                    coordinate: coordinate(seq),
                                    finalised_through: 100,
                                    still_matches: true,
                                },
                                now,
                            )
                            .is_ok()
                        && step(book, "verify", seq);
                    if !ok {
                        break;
                    }
                    // The ticker: after the fix, a tick brings the positions' tally on.
                    book.now += 1;
                    let now = book.now;
                    book.replica.tick(now).expect("tick");
                    if !step(book, "tick", seq) {
                        ok = false;
                        break;
                    }
                    if due(seq) {
                        let c = book.ctx(adapter);
                        book.replica
                            .adapter(
                                &c,
                                AdapterInput::Positions(PositionCut {
                                    next_seq: seq + 1,
                                    height: 1,
                                    positions: vec![(usd(), Minor::new(seq + 1))],
                                }),
                            )
                            .expect("positions");
                        if !measured(book, "positions", seq + 1, &mut csv) {
                            ok = false;
                            break;
                        }
                        let from = (seq + 1).saturating_sub(4);
                        let layer: Vec<(u64, [u8; 32])> =
                            (from..=seq).map(|s| (s, coordinate(s).digest)).collect();
                        book.replica
                            .rescan(source, from, seq + 1, &layer, now)
                            .expect("rescan");
                        if !measured(book, "rescan", seq + 1, &mut csv) {
                            ok = false;
                            break;
                        }
                    }
                }
                ok
            }
        };
        std::fs::write(out, csv).expect("write the samples");
        !ok
    }
}
