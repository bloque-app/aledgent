// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Reads off the decision lock (`T-031-12`, finding F3 of `F-032`): the in-process core decides
//! under an exclusive lock and answers reads under a shared one that lets a waiting decision in
//! first (`ingress::gate`). Before, about 1 000 reads a second stopped the core for thirteen
//! minutes after a burst.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ingress::boundary::{CallerBody, Core, CoreInput, InputBody, InputKey, Query};
use ingress::inprocess::{InProcessCore, Journal, MemJournal};
use ingress::query::AssetParams;
use ingress::testkit::{T0, genesis};

/// A journal whose append takes 2 ms, as a sync to a slow disk does: the decision path then waits
/// on I/O, not on the CPU, so the comparison measures the lock and not the machine's cores (a
/// 2-vCPU runner shares them with the readers).
#[derive(Clone)]
struct SyncedJournal(MemJournal);

impl Journal for SyncedJournal {
    fn entries(&self) -> Result<Vec<(u64, CoreInput)>, String> {
        self.0.entries()
    }
    fn append(&mut self, seq: u64, input: &CoreInput) -> Result<(), String> {
        std::thread::sleep(Duration::from_millis(2));
        self.0.append(seq, input)
    }
}

fn tick(n: u64) -> CoreInput {
    let mut key = [0u8; 16];
    key[..8].copy_from_slice(&n.to_be_bytes());
    key[8] = 0x7C;
    CoreInput {
        key: InputKey(key),
        body: InputBody::Tick { now: T0 + n },
    }
}

/// `n` a second over `d`, in whole units (no floating point anywhere, `INV-7`'s lint).
fn per_second(n: u64, d: Duration) -> u64 {
    n.saturating_mul(1_000_000) / u64::try_from(d.as_micros()).unwrap_or(u64::MAX).max(1)
}

/// Decisions a second for `window`, from input number `from` on.
async fn decide_for<C: Core>(core: &C, from: &mut u64, window: Duration) -> u64 {
    let start = Instant::now();
    let mut n = 0u64;
    while start.elapsed() < window {
        *from += 1;
        core.apply(&tick(*from))
            .await
            .unwrap_or_else(|e| panic!("a tick applies: {e:?}"));
        n += 1;
    }
    per_second(n, start.elapsed())
}

/// `NFR-18`, `REQ-CS-7`, `T-031-12`: a read storm does not starve decisions. Sixteen readers
/// query the core back to back (the trial balance, the asset list and the credential lookup an
/// API key makes) while it decides ticks, each journaled first with a 2 ms sync. The rate of
/// decisions under the storm stays within a small factor of the rate without it. With one lock
/// for both (before `T-031-12`) the same storm let no decision through: a 1.5 s window of
/// decisions did not complete in three minutes behind a file journal, nor in ten behind a journal
/// in memory.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nfr_18_req_cs_7_a_read_storm_does_not_starve_decisions() {
    let core = InProcessCore::in_memory(genesis(T0), SyncedJournal(MemJournal::new()));
    core.start()
        .unwrap_or_else(|e| panic!("the core starts: {e}"));
    let founded = core
        .founded()
        .unwrap_or_else(|| unreachable!("a started core is founded"));
    let caller = CallerBody {
        principal: founded.operator.to_did(),
        credential: founded.operator_credential.to_did(),
        on_behalf_of: None,
    };
    let window = Duration::from_millis(1_500);
    let mut at = 0u64;
    // Warm up, then the rate alone.
    decide_for(&core, &mut at, Duration::from_millis(200)).await;
    let alone = decide_for(&core, &mut at, window).await;

    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU64::new(0));
    let mut readers = Vec::new();
    for r in 0..16u64 {
        let (core, caller, stop, reads) = (
            core.clone(),
            caller.clone(),
            Arc::clone(&stop),
            Arc::clone(&reads),
        );
        let credential = founded.operator_credential.to_did();
        readers.push(tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                let _ = match r % 3 {
                    0 => core
                        .query(
                            &caller,
                            &Query::TrialBalance(AssetParams {
                                asset: Some("USD".into()),
                                amount: None,
                            }),
                            T0,
                        )
                        .await
                        .map(|_| ()),
                    1 => core.query(&caller, &Query::Assets, T0).await.map(|_| ()),
                    _ => core.credential(&credential).await.map(|_| ()),
                };
                reads.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let before = reads.load(Ordering::Relaxed);
    let stormed = decide_for(&core, &mut at, window).await;
    let storm = per_second(reads.load(Ordering::Relaxed) - before, window);
    stop.store(true, Ordering::Relaxed);
    for r in readers {
        let _ = r.await;
    }
    println!(
        "T-031-12 in-process: {alone} decisions/s alone; {stormed} decisions/s under {storm} \
         reads/s"
    );
    assert!(storm > 100, "the storm was a storm: {storm} reads/s");
    // Conservative: a tenth of the rate alone, whatever the machine's load. One lock for both
    // gave no decision at all under this storm.
    assert!(
        stormed * 10 >= alone,
        "decisions under the storm: {stormed}/s against {alone}/s alone"
    );
}
