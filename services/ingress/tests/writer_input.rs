// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The in-process core numbers its inputs for the writer as the core Agent does (`T-037-05`): the
//! ledger decides each input knowing its number, so a change of the upgrade-authority mode takes
//! force at its input and a delay counts inputs, live and after a replay of the journal.

use std::time::Duration;

use ingress::inprocess::{InProcessCore, MemJournal};
use ingress::store::mem::MemStore;
use ingress::testkit::{Deployment, T0, fast_config, genesis, test_clerk};

#[tokio::test(flavor = "multi_thread")]
async fn t037_05_the_in_process_core_numbers_every_input_it_decides() {
    let journal = MemJournal::new();
    let core = InProcessCore::in_memory(genesis(T0), journal.clone()).with_clerk(test_clerk());
    core.start()
        .unwrap_or_else(|e| panic!("the core starts: {e}"));
    // A fresh core has decided nothing.
    assert_eq!(core.with_ledger(|l| l.input()), Some(0));
    let founded = core
        .founded()
        .unwrap_or_else(|| unreachable!("a started core is founded"));
    let mut d = Deployment::assemble(
        MemStore::new(Duration::from_millis(50)),
        fast_config("ingress-0"),
        core.clone(),
        founded,
        journal,
    )
    .await;
    // No ticker: the test's own commands are the only inputs.
    d.drive_ticks();
    let _ = d.party("alice", 1_000).await;
    let decided = core.inputs().unwrap_or(0);
    assert!(decided > 0);
    // The last input decided is the last input number.
    assert_eq!(core.with_ledger(|l| l.input()), Some(decided));
    // A restart replays the journal through the same path, numbering every input again.
    core.stop();
    core.start()
        .unwrap_or_else(|e| panic!("the core restarts: {e}"));
    assert_eq!(core.inputs(), Some(decided));
    assert_eq!(core.with_ledger(|l| l.input()), Some(decided));
}
