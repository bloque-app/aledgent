// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The recovery-authority service (`DEC-13`, `DEC-15`, `REQ-AU-15`, `F-012`).
//!
//! A deployment that offers owned custody designates a recovery authority, held by a principal
//! distinct from the operator (`REQ-AU-15`). In the reference deployment it is this small service,
//! run by a different team from the core's operators. It holds the recovery key
//! ([`key::SigningKey`]) and has **one capability**: [`RecoveryService::approve_recovery`], a
//! signature over one pending recovery — the pending change, the account, and the verifier of the
//! credential the recovery issues ([`ledger_core::custody::approval_message`]). The core checks
//! that signature against the recovery key's public half in its configuration when the operator
//! submits `complete_recovery`, after the delay.
//!
//! What this service does **not** do:
//!
//! - It does not verify the holder. That is out-of-band work, done by people, and its steps are
//!   the runbook (`services/recovery/RUNBOOK.md`), not code. Signing is the last step of the
//!   runbook, never the first.
//! - It does not initiate, cancel or complete anything, hold an identity in the book, move value,
//!   or act on an account (`REQ-AU-15`, `RISK-9`). The book cannot even name it as a caller.
//! - It does not talk to the core. Its approval travels with the operator's `complete_recovery`;
//!   an approval is useless for any other recovery, account or credential.

#![forbid(unsafe_code)]

pub mod key;

use ledger_core::custody::{PendingId, RecoveryKey, Signature, approval_message};
use ledger_core::id::AccountId;
use ledger_core::principal::Verifier;

use key::SigningKey;

/// What the recovery authority approves: one pending recovery, as the core recorded it and as
/// the runbook verified it.
#[derive(Clone, PartialEq, Eq)]
pub struct ApprovalRequest {
    /// The pending recovery.
    pub pending: PendingId,
    /// The account it recovers.
    pub account: AccountId,
    /// The verifier of the credential it issues to the holder, with its scheme (`F-028`):
    /// public material.
    pub verifier: Verifier,
}

impl std::fmt::Debug for ApprovalRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // A verifier is never formatted (`REQ-AU-8`); its scheme is.
        f.debug_struct("ApprovalRequest")
            .field("pending", &self.pending)
            .field("account", &self.account)
            .field("scheme", &self.verifier.scheme())
            .finish()
    }
}

/// The recovery-authority service.
#[derive(Debug)]
pub struct RecoveryService {
    key: SigningKey,
}

impl RecoveryService {
    /// The service, holding `key`.
    pub fn new(key: SigningKey) -> RecoveryService {
        RecoveryService { key }
    }

    /// The recovery key's public half, for the core's deployment configuration.
    pub fn public_key(&self) -> RecoveryKey {
        self.key.public_key()
    }

    /// Approve one pending recovery: the service's one capability. Run it only as the last step
    /// of the runbook, once the holder has been verified out of band and the request checked
    /// against the pending recovery the holder can see on their account.
    pub fn approve_recovery(&self, request: &ApprovalRequest) -> Signature {
        self.key.sign(&approval_message(
            &request.pending,
            &request.account,
            &request.verifier,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ledger_core::custody::verifies;
    use ledger_core::id::OperationId;
    use ledger_kernel::posting::OperationRef;

    fn request(n: u8) -> ApprovalRequest {
        let op = OperationId::from_ref(OperationRef([n; 16]));
        ApprovalRequest {
            pending: PendingId(op),
            account: AccountId::mint(&op, 0),
            verifier: Verifier::new(vec![n; 32]),
        }
    }

    #[test]
    fn req_au_15_an_approval_verifies_under_the_public_key_for_its_own_request_only() {
        let s = RecoveryService::new(SigningKey::from_bytes([9; 32]).unwrap());
        let r = request(1);
        let sig = s.approve_recovery(&r);
        let msg = |r: &ApprovalRequest| approval_message(&r.pending, &r.account, &r.verifier);
        assert!(verifies(&s.public_key().0, &msg(&r), &sig));
        assert!(!verifies(&s.public_key().0, &msg(&request(2)), &sig));
        let mut other_verifier = r.clone();
        other_verifier.verifier = Verifier::new(vec![7; 32]);
        assert!(!verifies(&s.public_key().0, &msg(&other_verifier), &sig));
        // DEC-19: the same bytes under another scheme are another verifier.
        let mut other_scheme = r.clone();
        other_scheme.verifier = Verifier::of(ledger_core::principal::Scheme::ApiKey, vec![1; 32]);
        assert!(!verifies(&s.public_key().0, &msg(&other_scheme), &sig));
        let other = SigningKey::from_bytes([8; 32]).unwrap();
        assert!(!verifies(&other.public_key().0, &msg(&r), &sig));
    }
}
