// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The evidence check of an operation in `Needs-Review` (`REQ-RC-5`, step 2, amendment 0006 as
//! refined 2026-09-29), from the settlement adapter's side.
//!
//! An authority submits where the proof is: a block and a transaction (`submit_evidence`). The
//! adapter reads the pending submissions ([`crate::Ledger::adapter_evidence`]), fetches each
//! transaction **from its own view of the settlement layer**, and checks it there. It never takes
//! the submitter's word for anything: what it reports is only what the layer shows it.
//!
//! What it reports goes through its ordinary inputs, so the core holds the evidence to exactly
//! the checks it holds every other record to: each inbox record the transaction wrote that names
//! the operation (an observation), the canonical check of each (finality and a canonical match),
//! and, for an outbound leg, the signing key's nonce at a finalised head. It ends with
//! [`AdapterInput::EvidenceChecked`], on which the core resolves the operation on what those
//! records prove, or finds the submission unprovable, which opens step 3.
//!
//! [`EvidenceFetch`] is the piece the adapter supplies. The core does not depend on how it is
//! built; `F-027`'s adapter over the Kreivo extension implements it (see `PLAN.md`, `F-027`).

use alloc::vec::Vec;

use super::{Coordinate, FinalisedHead, KeyNonce, Observation};
use crate::ledger::settle_adapter::AdapterInput;
use crate::operator::EvidenceSubmission;

/// What the adapter found at a submission's block and transaction, on its own view of the layer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fetched {
    /// Every inbox record the transaction wrote, as observations. Records that do not name the
    /// operation are reported too: the core associates them, or ignores them, itself.
    pub records: Vec<Observation>,
    /// For each record: its coordinate, and whether the layer's record at that position is still
    /// the one reported.
    pub canonical: Vec<(Coordinate, bool)>,
    /// The height the layer's finality point has reached, read with the records.
    pub finalised_through: u64,
    /// The finalised head read with them, and the account nonce of each signing key at it. `None`
    /// if the adapter reports no head with this evidence.
    pub head: Option<(FinalisedHead, Vec<KeyNonce>)>,
}

/// Fetch and verify the evidence a submission names, on the adapter's own view of the settlement
/// layer (`REQ-RC-5`, step 2). What `F-027`'s adapter must provide.
pub trait EvidenceFetch {
    /// Why a fetch failed: the layer unreachable, the block not found. A failed fetch reports
    /// nothing; the submission stays pending and is fetched again.
    type Error;

    /// Fetch the transaction `submission.transaction` in block `submission.block`, check that
    /// the block is finalised and on the canonical chain and that the transaction is in it, and
    /// return the inbox records it wrote. A transaction that is not in that block, or a block
    /// that is not final, is `Ok` with no records: nothing is proven, and the core says so.
    fn fetch(&mut self, submission: &EvidenceSubmission) -> Result<Fetched, Self::Error>;
}

/// The adapter inputs that report `fetched` for `submission`, in the order the core takes them:
/// the head and nonces, the records, their canonical checks, then
/// [`AdapterInput::EvidenceChecked`]. A record the core already holds is refused as a duplicate
/// (`ERR-ObservationDuplicate`); the adapter carries on with the next input.
pub fn evidence_inputs(submission: &EvidenceSubmission, fetched: Fetched) -> Vec<AdapterInput> {
    let mut out = Vec::new();
    if let Some((head, nonces)) = fetched.head {
        out.push(AdapterInput::Head { head, nonces });
    }
    out.extend(fetched.records.into_iter().map(AdapterInput::Observe));
    out.extend(
        fetched
            .canonical
            .into_iter()
            .map(|(coordinate, still_matches)| AdapterInput::Check {
                coordinate,
                finalised_through: fetched.finalised_through,
                still_matches,
            }),
    );
    out.push(AdapterInput::EvidenceChecked {
        op: submission.op,
        submission: submission.id,
    });
    out
}
