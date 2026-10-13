// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The restart matrix and a seeded chaos run (`T-016-10`, `REQ-SA-5`, `REQ-SA-6`, `REQ-SO-7`,
//! `AC-K3.1`): the adapter killed in every submission and observation state, and from storage
//! alone it states what it was asked, what the chain did and what it told the core. It never
//! produces a second extrinsic for one operation reference.

use ingress::testkit::Party;
use ledger_core::id::{AccountId, account_reference};
use settle::adapter::CrashPoint;
use settle::store::{ExtrinsicStatus, InstructionState, Store};
use settle::testkit::{INSTANCE, Kit, KitOptions, PAYEE, chain_usd};

const POINTS: [CrashPoint; 8] = [
    CrashPoint::AfterStore,
    CrashPoint::AfterAck,
    CrashPoint::AfterSign,
    CrashPoint::AfterPersist,
    CrashPoint::AfterBroadcast,
    CrashPoint::AfterObserve,
    CrashPoint::AfterReport,
    CrashPoint::AfterVerdict,
];

/// A step; on a crash, a new process over the same store.
async fn step_or_restart(k: &mut Kit) -> bool {
    match k.step().await {
        Ok(_) => false,
        Err(settle::AdapterError::Crashed(_)) => {
            k.restart();
            true
        }
        Err(e) => panic!("a step: {e}"),
    }
}

async fn all_in(k: &Kit, p: &Party, ops: &[String], states: &[&str]) -> bool {
    for op in ops {
        if !states.contains(&k.state(p, op).await.as_str()) {
            return false;
        }
    }
    true
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ac_k3_1_the_restart_matrix_loses_no_instruction_and_states_everything_from_storage() {
    for point in POINTS {
        let mut k = Kit::start().await;
        assert!(k.step().await.unwrap().serving);
        let alice = k.d.party("alice", 10_000).await;
        let account = AccountId::parse(alice.account.as_deref().unwrap()).unwrap();
        let ops = vec![
            k.withdrawal(&alice, 100, "a").await,
            k.withdrawal(&alice, 200, "b").await,
        ];
        let from = [0x77; 32];
        k.chain.mint(from, &chain_usd(), 1_000);
        k.chain.deposit(
            from,
            INSTANCE,
            account_reference(&account),
            &chain_usd(),
            50,
        );
        k.adapter.crash_at(point);
        let mut crashed = false;
        for _ in 0..12 {
            crashed |= step_or_restart(&mut k).await;
            k.chain.advance(1);
            if all_in(&k, &alice, &ops, &["Settled"]).await {
                break;
            }
        }
        k.step().await.unwrap();
        assert!(crashed, "{point:?} was reached");
        assert!(
            all_in(&k, &alice, &ops, &["Settled"]).await,
            "{point:?}: every instruction settled"
        );
        assert_eq!(
            k.balances(&alice).await,
            (10_000 - 300 + 50, 0),
            "{point:?}"
        );
        assert!(k.chain.duplicated_references().is_empty(), "{point:?}");
        assert_eq!(k.chain.balance_of(&PAYEE, &chain_usd()), 300);

        // From storage alone (REQ-SA-5): a fresh reader of the store, nothing else.
        let store = k.store.clone();
        let instructions = store.instructions().await.unwrap();
        let extrinsics = store.extrinsics().await.unwrap();
        let observations = store.observations().await.unwrap();
        let reports = store.reports().await.unwrap();
        for op in &ops {
            // What it was asked.
            let i = instructions.iter().find(|r| &r.operation == op).unwrap();
            assert!(i.acknowledged, "{point:?}");
            assert_eq!(i.state, InstructionState::Executed, "{point:?}");
            // What the chain did: its extrinsic, included, and the inbox entry naming it.
            let e: Vec<_> = extrinsics.iter().filter(|e| &e.operation == op).collect();
            assert_eq!(e.len(), 1, "{point:?}");
            assert!(matches!(e[0].status, ExtrinsicStatus::Included(ref x) if x.success));
            let o = observations
                .iter()
                .find(|o| o.operation.as_ref() == Some(op) && o.finalized)
                .unwrap_or_else(|| panic!("{point:?}: no observation of {op}"));
            // What it told the core, and the core's verdict.
            assert!(
                reports.iter().any(|r| r.kind == "observe"
                    && r.body["coordinate"]["seq"] == o.seq
                    && r.verdict.is_some()),
                "{point:?}"
            );
            assert!(reports.iter().any(|r| r.kind == "acknowledge"
                && r.body["operation_reference"] == op.as_str()
                && r.verdict.as_deref() == Some("decided")));
        }
        // The deposit: observed and reported.
        assert!(
            observations
                .iter()
                .any(|o| o.summary.starts_with("deposited"))
        );
        assert!(reports.iter().all(|r| r.verdict.is_some()), "{point:?}");
    }
}

/// A small deterministic generator.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn req_so_7_chaos_never_yields_a_second_extrinsic_for_one_operation() {
    for seed in [1u64, 7, 42, 99, 2026] {
        let mut rng = Lcg(seed);
        let mut k = Kit::start_with(KitOptions {
            lanes: 2,
            ..KitOptions::default()
        })
        .await;
        assert!(k.step().await.unwrap().serving);
        let alice = k.d.party("alice", 100_000).await;
        let mut ops: Vec<(String, u64)> = Vec::new();
        for n in 0..120 {
            match rng.next(8) {
                0 | 1 => {
                    let amount = 1 + rng.next(50);
                    let op = k.withdrawal(&alice, amount, &format!("c{n}")).await;
                    ops.push((op, amount));
                }
                2 => {
                    k.chain.produce();
                }
                3 => k.chain.finalize_all(),
                4 => {
                    k.chain.reorg(1 + rng.next(2), false);
                    k.chain.produce();
                }
                5 => {
                    let p = POINTS[usize::try_from(rng.next(8)).unwrap()];
                    k.adapter.crash_at(p);
                }
                6 => k.chain.drop_submissions(rng.next(2) == 0),
                _ => {}
            }
            step_or_restart(&mut k).await;
        }
        k.chain.drop_submissions(false);
        let refs: Vec<String> = ops.iter().map(|(o, _)| o.clone()).collect();
        for _ in 0..300 {
            k.chain.advance(1);
            step_or_restart(&mut k).await;
            if all_in(&k, &alice, &refs, &["Settled", "Released"]).await {
                break;
            }
        }
        assert!(
            all_in(&k, &alice, &refs, &["Settled", "Released"]).await,
            "seed {seed}: every withdrawal reached an outcome"
        );
        assert!(
            k.chain.duplicated_references().is_empty(),
            "seed {seed}: one extrinsic per operation reference"
        );
        // The book and the chain agree on what left.
        let mut settled = 0u64;
        for (op, amount) in &ops {
            let s = k.state(&alice, op).await;
            if s == "Settled" {
                settled += amount;
                assert_eq!(k.chain.extrinsics_for(&Kit::chain_ref(op)), 1);
            } else {
                // Released: provably not executed.
                assert!(k.chain.extrinsics_for(&Kit::chain_ref(op)) <= 1);
            }
        }
        assert_eq!(
            u128::from(settled),
            k.chain.balance_of(&PAYEE, &chain_usd()),
            "seed {seed}"
        );
        eprintln!(
            "seed {seed}: {} withdrawals, {settled} settled, reports {}",
            ops.len(),
            k.store.reports().await.unwrap().len()
        );
        assert_eq!(
            k.balances(&alice).await,
            (100_000 - settled, 0),
            "seed {seed}"
        );
    }
}
