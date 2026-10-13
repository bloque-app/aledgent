// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The Agent over its own committed maps (`F-031` lever 2, `T-031-07`, `T-031-08`).

mod common;

use std::time::Instant;

use common::*;
use core_binding::envelope::BlobKey;
use core_binding::host::{AgentPort, Driver, NativePort, PortError, rows_bytes};
use core_binding::hot::{Hot, RingEntry};
use core_binding::mapped::LAYOUT_MAPPED;
use ledger_core::asset::Minor;
use ledger_core::ledger::Transfer;

/// A few hundred mixed operations: the driver checks every reply against its replica (roots,
/// summaries, log hash), so a run that completes is a run the Agent reproduced exactly.
fn workload<P: AgentPort>(fx: &mut Fx<P>, n: u64) {
    let (alice, a) = fx.funded(1_000_000);
    let bob = fx.person();
    let b = fx.account(&bob);
    for i in 0..n {
        let ctx = fx.ctx(alice);
        fx.replica
            .transfer(
                &ctx,
                &Transfer {
                    deadline: Some(1_000_000),
                    ..Transfer::single(a, b, usd(), Minor::new(1 + i % 50))
                },
            )
            .unwrap();
        fx.synced();
    }
}

#[test]
fn req_rt_1_a_mapped_agent_commits_what_the_replica_does() {
    let mut fx = Fx::mapped();
    workload(&mut fx, 40);
    let hot = fx.driver.port_mut().hot().unwrap().unwrap();
    assert_eq!(hot.layout, LAYOUT_MAPPED);
    // Genesis, then the setup's six inputs and the forty transfers.
    assert!(hot.inputs >= 46, "{}", hot.inputs);
    assert!(hot.next_posting >= 41, "{}", hot.next_posting);
}

#[test]
fn req_rt_6_no_private_input_reaches_the_mapped_agents_rows() {
    let mut fx = Fx::mapped();
    workload(&mut fx, 5);
    let bytes = rows_bytes(&fx.driver.port().rows);
    assert!(
        !bytes.windows(ROOT.len()).any(|w| w == ROOT),
        "the key-ring root is in the rows"
    );
}

#[test]
fn req_rt_3_a_host_restart_rebuilds_against_a_mapped_agent() {
    let mut fx = Fx::mapped();
    workload(&mut fx, 6);
    let port = std::mem::replace(fx.driver.port_mut(), NativePort::new());
    let mut archive = fx.archive.clone();
    let (driver, _replica) =
        Driver::recover(port, keys(), genesis(), op(0), 1, &mut archive).expect("recovered");
    assert_eq!(driver.seq(), fx.driver.seq());
}

/// The native port, timing each invocation and counting what crosses it.
struct Timed {
    inner: NativePort,
    micros: Vec<u128>,
    bytes: Vec<usize>,
}

impl AgentPort for Timed {
    fn invoke(&mut self, message: Vec<u8>, blobs: Vec<Vec<u8>>) -> Result<Vec<u8>, PortError> {
        self.bytes
            .push(message.len() + blobs.iter().map(Vec::len).sum::<usize>());
        let t = Instant::now();
        let r = self.inner.invoke(message, blobs);
        self.micros.push(t.elapsed().as_micros());
        r
    }
    fn blob_key(&self, bytes: &[u8]) -> BlobKey {
        self.inner.blob_key(bytes)
    }
    fn hot(&mut self) -> Result<Option<Hot>, PortError> {
        self.inner.hot()
    }
    fn entry(&mut self, seq: u64) -> Result<Option<RingEntry>, PortError> {
        self.inner.entry(seq)
    }
}

fn p50(v: &mut [u128]) -> u128 {
    v.sort_unstable();
    v[v.len() / 2]
}

/// Layout 2 (witnesses) against layout 3 (committed maps), natively: the Agent's transition time
/// and the bytes an invocation carries, per transfer. An observation (`NFR-18`).
///
///     ALEDGENT_MAPPED_OPS=500 cargo test --release -p core-binding --test mapped -- --ignored --nocapture
#[test]
#[ignore = "a measurement: run with --ignored --nocapture"]
fn nfr_18_witnessed_against_mapped_natively() {
    let n: u64 = std::env::var("ALEDGENT_MAPPED_OPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(300);
    println!(
        "layout,transfers,transition_us_p50,transition_us_mean,invocation_bytes_p50,rows_after"
    );
    for mapped in [false, true] {
        let port = Timed {
            inner: NativePort::new(),
            micros: Vec::new(),
            bytes: Vec::new(),
        };
        let found = if mapped {
            Driver::found_mapped
        } else {
            Driver::found
        };
        let (driver, replica, founded) = found(
            port,
            keys(),
            genesis(),
            op(0),
            1,
            core_binding::hot::Limits::DEFAULT,
        )
        .unwrap();
        let mut fx = Fx {
            driver,
            replica,
            founded,
            next: 0,
            now: 10,
            archive: Vec::new(),
        };
        let skip = fx.driver.port().micros.len();
        workload(&mut fx, n);
        let p = fx.driver.port();
        // The workload's setup is the first few invocations; the transfers are the last `n`.
        let mut us: Vec<u128> = p.micros[skip..]
            .iter()
            .rev()
            .take(n as usize)
            .copied()
            .collect();
        let mean = us.iter().sum::<u128>() / us.len() as u128;
        let mut bytes: Vec<u128> = p.bytes[skip..]
            .iter()
            .rev()
            .take(n as usize)
            .map(|b| *b as u128)
            .collect();
        println!(
            "{},{n},{},{mean},{},{}",
            if mapped { 3 } else { 2 },
            p50(&mut us),
            p50(&mut bytes),
            p.inner.rows.0.len()
        );
    }
}
