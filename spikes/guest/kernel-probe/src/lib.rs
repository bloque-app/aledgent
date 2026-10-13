// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! F-002 S2 and S3 in the guest: CipherClerk's kernel inside a VOS `saga`
//! Agent actor. Never shipped.
//!
//! - `apply(case)`: S2. Decodes a pre-built unit and the touched pre-state
//!   (an in-message `VecLedger`), runs `apply_account_creations` and
//!   `apply_batch` step by step exactly as `Book::apply` does, and checks
//!   every status and the root against the host's.
//! - `verify(witness, roots)`: S3 in the guest. Decodes a
//!   `SuccinctTransitionWitness` and runs CipherClerk's `verify_transition`.
//! - `noop(payload)`: the baseline, same message path, no kernel.
//!
//! Each returns the guest gas spent per phase, read with the `GAS` hostcall
//! (0 when the host does not answer it).

use cipher_clerk::kernel::CreateAccount;
use cipher_clerk::state::LedgerState;
use cipher_clerk::succinct::SuccinctTransitionWitness;
use cipher_clerk::{
    EventStatus, OpeningsOracle, Transfer, VecLedger, apply_account_creations, apply_batch,
};
use vos::prelude::*;

/// One step of a unit, with the seeds `Book::apply` would give it.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct S2Step {
    /// Accounts created first.
    pub creations: Vec<CreateAccount>,
    /// Their batch seed.
    pub creation_seed: u64,
    /// The chain.
    pub events: Vec<Transfer>,
    /// Its batch seed.
    pub seed: u64,
}

/// A unit and the touched pre-state it runs over.
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct S2Case {
    /// The touched leaves, pre-state.
    pub ledger: VecLedger,
    /// The steps.
    pub steps: Vec<S2Step>,
    /// Entry and net openings.
    pub oracle: OpeningsOracle,
    /// The `VecLedger` root the host computed after the same unit.
    pub expected_root: [u8; 32],
}

fn gas_now() -> u64 {
    #[cfg(target_arch = "riscv64")]
    {
        vos::abi::pvm::hostcalls::gas()
    }
    #[cfg(not(target_arch = "riscv64"))]
    {
        0
    }
}

fn aligned(bytes: &[u8]) -> rkyv::util::AlignedVec<16> {
    let mut a = rkyv::util::AlignedVec::<16>::with_capacity(bytes.len());
    a.extend_from_slice(bytes);
    a
}

fn reply(ok: bool, gas: &[u64]) -> Vec<u8> {
    let mut out = Vec::with_capacity(1 + 8 * gas.len());
    out.push(u8::from(ok));
    for g in gas {
        out.extend_from_slice(&g.to_le_bytes());
    }
    out
}

#[actor(agent)]
pub struct KernelProbe {
    runs: u64,
}

#[messages(agent)]
impl KernelProbe {
    fn new() -> Self {
        Self { runs: 0 }
    }

    /// S2. Reply: ok, then gas for decode, apply and root.
    #[msg(linear)]
    fn apply(&mut self, case: Vec<u8>) -> Vec<u8> {
        let g0 = gas_now();
        let bytes = aligned(&case);
        let Ok(mut c) = rkyv::from_bytes::<S2Case, rkyv::rancor::Error>(&bytes) else {
            return reply(false, &[]);
        };
        let g1 = gas_now();
        let mut ok = true;
        for s in &c.steps {
            if !s.creations.is_empty() {
                let r = apply_account_creations(
                    &mut c.ledger,
                    &s.creations,
                    &mut c.oracle,
                    s.creation_seed,
                );
                ok &= r.iter().all(|r| r.status == EventStatus::Created);
            }
            let r = apply_batch(&mut c.ledger, &s.events, &mut c.oracle, s.seed);
            ok &= r.iter().all(|r| r.status == EventStatus::Created);
        }
        let g2 = gas_now();
        let root = c.ledger.root();
        let g3 = gas_now();
        ok &= root == c.expected_root;
        self.runs += 1;
        reply(
            ok,
            &[g0.saturating_sub(g1), g1.saturating_sub(g2), g2.saturating_sub(g3)],
        )
    }

    /// S3 in the guest. `roots` is root_before ‖ root_after. Reply: ok, then
    /// gas for decode and for `verify_transition` (which panics, and so
    /// aborts the invocation, on any mismatch).
    #[msg(linear)]
    fn verify(&mut self, witness: Vec<u8>, roots: Vec<u8>) -> Vec<u8> {
        let g0 = gas_now();
        let bytes = aligned(&witness);
        let Ok(w) = rkyv::from_bytes::<SuccinctTransitionWitness, rkyv::rancor::Error>(&bytes)
        else {
            return reply(false, &[]);
        };
        let g1 = gas_now();
        if roots.len() != 64 {
            return reply(false, &[]);
        }
        let mut before = [0u8; 32];
        let mut after = [0u8; 32];
        before.copy_from_slice(&roots[..32]);
        after.copy_from_slice(&roots[32..]);
        w.verify_transition(before, after);
        let g2 = gas_now();
        self.runs += 1;
        reply(true, &[g0.saturating_sub(g1), g1.saturating_sub(g2)])
    }

    /// The baseline: the same message path with no kernel work.
    #[msg(linear)]
    fn noop(&mut self, payload: Vec<u8>) -> u64 {
        self.runs += 1;
        payload.len() as u64
    }
}
