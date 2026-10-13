// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Custody switching, credential sets and recovery (`F-012`, `US-A5`, `US-A7`, `US-A8`,
//! `REQ-AU-9`, `REQ-AU-11`–`REQ-AU-20`, `DEC-13`).
//!
//! `F-003` models the credential set, custody and the authority kinds; this module holds what the
//! commands of `F-012` need beside them, and [`crate::ledger::custody`] holds the commands:
//!
//! - [`RecoveryConfig`]: the deployment's recovery configuration, checked at start-up
//!   ([`RecoveryConfig::check`]). It has no guardian or threshold field, so guardian recovery
//!   cannot be expressed (`REQ-AU-19`).
//! - [`RecoveryMechanisms`] and its [`MechanismsDigest`]: what a holder is shown before an
//!   account becomes owned, and the acknowledgement every such command carries (`REQ-AU-9`,
//!   `AC-A5.3`).
//! - The two signed messages the library checks: the challenge that proves a holder credential
//!   works before custody is reclaimed ([`reclaim_challenge`], `REQ-AU-13`), and the recovery
//!   authority's approval of one pending recovery ([`approval_message`], `REQ-AU-15`). The
//!   signature scheme is the kernel's own Schnorr signature over Ristretto; a verifier of this
//!   kind is the 32 bytes of the public key. The recovery authority's secret never enters this
//!   process: it is held by `services/recovery` (`DEC-13`, `NFR-8`).
//! - [`machine`]: the closed state machine of a pending recovery or succession, as a constant
//!   table.
//! - [`store`]: the pending changes, named successors and notices in the service store, under
//!   their own rules.
//! - [`recovery_rules`]: the three rules of amendment 0006 item 10 (ruled 2026-09-29), each one
//!   function.

use alloc::vec::Vec;

use cipher_clerk::crypto::{AuthKey, verify_signature};
use ledger_kernel::posting::OperationRef;

use crate::account::{Custody, Time};
use crate::authority::AuthorityKind;
use crate::id::{AccountId, CredentialId, IdentityId, OperationId};
use crate::lifecycle::keys::Canon;
use crate::principal::Verifier;
use crate::state::State;

pub mod machine;
pub mod store;

pub use machine::{
    CHANGE_ENTRY, CHANGE_TRANSITIONS, ChangeDecision, ChangeEdge, ChangeFacts, ChangeState,
    ChangeTrigger, next,
};
pub use store::{
    ChangeKind, ChangeTarget, Closed, CustodyRefusal, CustodyState, CustodyWrite, Notice,
    NoticeEvent, NoticeId, PendingChange, PendingId,
};

/// A signature over one of this module's messages: the kernel's Schnorr signature over
/// Ristretto. Public material, not a secret.
pub type Signature = cipher_clerk::crypto::Signature;

/// The default delay of a recovery (`DEC-13`): 72 hours, in seconds. The deployment states its
/// own in [`RecoveryConfig`]; time is the binding's unit (`REQ-RT-7`).
pub const DEFAULT_RECOVERY_DELAY: Time = 72 * 3_600;

/// The default delay of a succession (`DEC-13`): 30 days, in seconds.
pub const DEFAULT_SUCCESSION_DELAY: Time = 30 * 24 * 3_600;

/// The recovery authority's public key: the 32 bytes of a Ristretto point. Only the public half
/// is ever configured here; the secret stays in the recovery authority's own service.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RecoveryKey(pub [u8; 32]);

impl RecoveryKey {
    /// Whether `v` is this key, as a credential verifier.
    pub fn is(&self, v: &Verifier) -> bool {
        v.as_bytes() == self.0
    }
}

/// The deployment's recovery configuration (`REQ-AU-9`, `REQ-AU-15`–`REQ-AU-19`, `DEC-13`).
///
/// The schema is closed: there is no guardian, threshold or quorum field, so guardian recovery
/// cannot be configured (`REQ-AU-19`).
///
/// ```compile_fail
/// // REQ-AU-19: a guardian threshold cannot be expressed.
/// let _ = ledger_core::custody::RecoveryConfig {
///     guardians: 3,
///     ..ledger_core::custody::RecoveryConfig::not_offered()
/// };
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RecoveryConfig {
    /// Whether the deployment offers owned custody. It may only if it has a recovery authority.
    pub owned_custody: bool,
    /// The recovery authority's public key (`REQ-AU-15`). Distinct from every operator key.
    pub recovery_key: Option<RecoveryKey>,
    /// The delay of a recovery, from initiation (`REQ-AU-16`). Never zero.
    pub recovery_delay: Time,
    /// The delay of a succession, from the successor's claim, if the deployment offers
    /// succession (`REQ-AU-17`). Never zero.
    pub succession_delay: Option<Time>,
    /// Whether a holder may register a holder-held recovery credential (`REQ-AU-18`).
    pub holder_held: bool,
}

impl RecoveryConfig {
    /// A deployment that offers delegated custody only: no recovery authority, no owned custody.
    pub const fn not_offered() -> RecoveryConfig {
        RecoveryConfig {
            owned_custody: false,
            recovery_key: None,
            recovery_delay: DEFAULT_RECOVERY_DELAY,
            succession_delay: None,
            holder_held: false,
        }
    }

    /// A deployment that offers owned custody, with `key` as its recovery authority, the default
    /// delays of `DEC-13`, succession offered and the holder-held credential not.
    pub const fn owned_custody(key: RecoveryKey) -> RecoveryConfig {
        RecoveryConfig {
            owned_custody: true,
            recovery_key: Some(key),
            recovery_delay: DEFAULT_RECOVERY_DELAY,
            succession_delay: Some(DEFAULT_SUCCESSION_DELAY),
            holder_held: false,
        }
    }

    /// The same, with another recovery delay.
    pub const fn with_recovery_delay(mut self, delay: Time) -> RecoveryConfig {
        self.recovery_delay = delay;
        self
    }

    /// The same, with succession offered after `delay`, or not offered.
    pub const fn with_succession_delay(mut self, delay: Option<Time>) -> RecoveryConfig {
        self.succession_delay = delay;
        self
    }

    /// The same, with the holder-held recovery credential offered or not.
    pub const fn with_holder_held(mut self, offered: bool) -> RecoveryConfig {
        self.holder_held = offered;
        self
    }

    /// The start-up checks (`REQ-AU-9`, `REQ-AU-15`, `REQ-AU-16`, `F-012` §10): owned custody
    /// needs a recovery key; the recovery key is none of `operator_keys`; no delay is zero. The
    /// organisational separation of `DEC-13` cannot be checked in code: the runbook owns it.
    pub fn check<'a>(
        &self,
        operator_keys: impl IntoIterator<Item = &'a Verifier>,
    ) -> Result<(), StartupError> {
        if self.owned_custody && self.recovery_key.is_none() {
            return Err(StartupError::OwnedCustodyWithoutRecoveryKey);
        }
        if let Some(key) = &self.recovery_key {
            if operator_keys.into_iter().any(|v| key.is(v)) {
                return Err(StartupError::RecoveryKeyIsOperatorKey);
            }
        }
        if self.recovery_delay == 0 || self.succession_delay == Some(0) {
            return Err(StartupError::ZeroDelay);
        }
        Ok(())
    }

    /// Whether owned custody is on offer: configured, with a recovery authority.
    pub const fn offers_owned(&self) -> bool {
        self.owned_custody && self.recovery_key.is_some()
    }

    /// The recovery authority as the audit trail names it, if there is one.
    pub fn authority(&self) -> Option<RecoveryAuthority> {
        self.recovery_key.map(|k| RecoveryAuthority::of(&k))
    }
}

/// Why a deployment refused to start (`REQ-AU-8`: fail rather than fall back).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupError {
    /// Owned custody is offered with no recovery authority (`REQ-AU-9`, `REQ-AU-18`: a
    /// holder-held credential is never the only mechanism).
    OwnedCustodyWithoutRecoveryKey,
    /// The recovery authority's key is an operator key (`REQ-AU-15`).
    RecoveryKeyIsOperatorKey,
    /// A recovery or succession delay of zero is no delay (`REQ-AU-16`).
    ZeroDelay,
}

/// The recovery authority, as records name it. It holds no identity in the book (SPEC §3), so its
/// identifiers are derived from its key: stable while the key is, and not any identity's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RecoveryAuthority {
    /// The principal the audit trail names.
    pub principal: IdentityId,
    /// The "credential" it approved with: its key.
    pub credential: CredentialId,
}

impl RecoveryAuthority {
    fn of(key: &RecoveryKey) -> RecoveryAuthority {
        let digest = Canon::new("aledgent/recovery-authority/v1")
            .bytes(&key.0)
            .digest()
            .0;
        let mut r = [0u8; 16];
        r.copy_from_slice(&digest[..16]);
        let op = OperationId::from_ref(OperationRef(r));
        RecoveryAuthority {
            principal: IdentityId::mint(&op, 0),
            credential: CredentialId::mint(&op, 1),
        }
    }

    /// Its approval, as an authority used (`REQ-AU-7`, `AC-A8.3`).
    pub fn used(&self) -> crate::authority::AuthorityUsed {
        crate::authority::AuthorityUsed {
            kind: AuthorityKind::Recovery,
            grant: None,
            principal: self.principal,
            credential: self.credential,
            on_behalf_of: None,
        }
    }
}

// ---------------------------------------------------------------------- signed messages

/// Whether `signature` is `verifier`'s over `message`. A verifier that is not a 32-byte public
/// key verifies nothing.
pub fn verifies(verifier: &[u8], message: &[u8; 32], signature: &Signature) -> bool {
    let Ok(key) = <[u8; 32]>::try_from(verifier) else {
        return false;
    };
    verify_signature(&AuthKey(key), message, signature)
}

/// The challenge a holder credential signs to prove it works before custody is reclaimed
/// (`REQ-AU-13`, `F-012` §4): it binds the account and its **custody epoch**, the number of
/// custody changes it has had ([`crate::custody::CustodyState::epoch`], shown on the account's
/// custody view). A proof is good for the one reclaim that ends the current epoch and is useless
/// after it. The epoch, not the operation reference, is bound because the holder must be able to
/// sign before submitting, and ingress mints references the caller cannot know (`F-009`).
pub fn reclaim_challenge(account: &AccountId, epoch: u64) -> [u8; 32] {
    Canon::new("aledgent/reclaim-custody/v2")
        .bytes(account.as_bytes())
        .u64(epoch)
        .digest()
        .0
}

/// What the recovery authority signs to approve one pending recovery (`REQ-AU-15`, `DEC-13`):
/// the pending change, the account, and the verifier of the credential it issues, with its
/// scheme (`DEC-19`, `F-028`: v2). An approval of one recovery approves nothing else, and an
/// approval of a verifier under one scheme does not approve the same bytes under another.
pub fn approval_message(pending: &PendingId, account: &AccountId, verifier: &Verifier) -> [u8; 32] {
    let mut c = Canon::new("aledgent/approve-recovery/v2");
    c.bytes(&pending.0.to_ref().0).bytes(account.as_bytes());
    verifier.canon_into(&mut c);
    c.digest().0
}

// ---------------------------------------------------------------------- mechanisms

/// One recovery mechanism that applies to an account (`REQ-AU-9`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mechanism {
    /// The recovery authority, after its delay (`REQ-AU-15`, `REQ-AU-16`). Always listed.
    RecoveryAuthority {
        /// Who it is.
        authority: IdentityId,
        /// The delay.
        delay: Time,
    },
    /// A named successor's claim, after its delay, if the deployment offers succession
    /// (`REQ-AU-17`).
    Succession {
        /// The delay.
        delay: Time,
        /// The successor named for the account, if one is.
        named: Option<IdentityId>,
    },
    /// The holder's own holder-held recovery credentials that cover the account, if the
    /// deployment offers them and the holder registered one (`REQ-AU-18`). No delay.
    HolderHeld {
        /// The credentials.
        credentials: Vec<CredentialId>,
    },
}

/// The recovery mechanisms that apply to an account: what the holder is shown before it becomes
/// owned (`REQ-AU-9`, `AC-A5.3`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryMechanisms {
    /// The mechanisms, in the order above.
    pub mechanisms: Vec<Mechanism>,
}

/// The digest of a [`RecoveryMechanisms`]: the acknowledgement a command that makes an account
/// owned carries. A missing or stale digest is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct MechanismsDigest(pub [u8; 32]);

impl RecoveryMechanisms {
    /// The mechanisms that apply to an account of `owner` — `account`, or, before it exists, a
    /// new one — under `config` and the store's current state.
    pub fn of(
        config: &RecoveryConfig,
        state: &State,
        owner: &IdentityId,
        account: Option<&AccountId>,
    ) -> RecoveryMechanisms {
        let mut mechanisms = Vec::new();
        if let Some(authority) = config.authority() {
            mechanisms.push(Mechanism::RecoveryAuthority {
                authority: authority.principal,
                delay: config.recovery_delay,
            });
        }
        if let Some(delay) = config.succession_delay {
            mechanisms.push(Mechanism::Succession {
                delay,
                named: account.and_then(|a| state.custody().successor(a)),
            });
        }
        if config.holder_held {
            let credentials: Vec<CredentialId> = state
                .credentials_of(owner)
                .filter(|c| {
                    c.marker() == crate::principal::CredentialMarker::RecoveryHeld
                        && c.status() == crate::principal::CredentialStatus::Active
                        && match (c.scope(), account) {
                            (None, _) => true,
                            (Some(s), Some(a)) => s.contains(a),
                            (Some(_), None) => false,
                        }
                })
                .map(crate::principal::Credential::id)
                .collect();
            if !credentials.is_empty() {
                mechanisms.push(Mechanism::HolderHeld { credentials });
            }
        }
        RecoveryMechanisms { mechanisms }
    }

    /// Its digest.
    pub fn digest(&self) -> MechanismsDigest {
        let mut c = Canon::new("aledgent/recovery-mechanisms/v1");
        c.u64(self.mechanisms.len() as u64);
        for m in &self.mechanisms {
            match m {
                Mechanism::RecoveryAuthority { authority, delay } => {
                    c.u8(1).bytes(authority.as_bytes()).u64(*delay);
                }
                Mechanism::Succession { delay, named } => {
                    c.u8(2).u64(*delay);
                    c.opt(named.as_ref(), |c, i| {
                        c.bytes(i.as_bytes());
                    });
                }
                Mechanism::HolderHeld { credentials } => {
                    c.u8(3).u64(credentials.len() as u64);
                    for id in credentials {
                        c.bytes(id.as_bytes());
                    }
                }
            }
        }
        MechanismsDigest(c.digest().0)
    }
}

// ---------------------------------------------------------------------- in-flight authority

/// The custody an operation's later transitions are checked against (`REQ-AU-13`, `AC-A7.3`,
/// `F-012` §4): the snapshot taken at its acceptance, in its attribution record. Operator
/// authority exists only in delegated custody, so an operation accepted under operator authority
/// was accepted in delegated custody, and its capture or release is checked as if it still were.
/// Owner and delegated authority do not depend on custody: for them there is no snapshot, and the
/// account's current custody applies. `None` if the operation has no record.
pub fn snapshot(state: &State, op: &OperationId) -> Option<Custody> {
    let record = state.record(op)?;
    match record.authorities.first()?.kind {
        AuthorityKind::Operator => Some(Custody::Delegated),
        _ => None,
    }
}

// ---------------------------------------------------------------------- amendment 0006 item 10

/// The three recovery rules of amendment 0006 item 10, ruled by the owner on 2026-09-29
/// (`REQ-AU-16`, `REQ-AU-17`, `REQ-AU-20`, `AC-A8.2`). Each is one function here.
pub mod recovery_rules {
    use super::ChangeKind;

    /// Item 10, first point (`REQ-AU-20`): may a removal leave `active_after` active credentials
    /// in the identity's set? Removing the **last** credential is refused (`ERR-NotAuthorised`),
    /// so a holder can never lock themselves out outside a recovery.
    pub const fn may_remove(active_after: usize) -> bool {
        active_after > 0
    }

    /// Item 10, second point (`REQ-AU-16`, `AC-A8.2`): does ordinary use of an existing
    /// credential cancel a pending recovery? No: only an explicit `cancel_recovery` does.
    pub const fn use_cancels_pending() -> bool {
        false
    }

    /// Item 10, third point (`REQ-AU-17`): on completing a change of `kind`, are the delegated
    /// authorities the previous holder granted over the account revoked? On succession, yes: the
    /// successor grants again. On recovery, no (`REQ-AU-14`).
    pub const fn grants_revoked_on(kind: ChangeKind) -> bool {
        matches!(kind, ChangeKind::Succession)
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A recovery authority for the unit tests: a test key, never a default of production code.
    use super::{RecoveryConfig, RecoveryKey};
    use cipher_clerk::crypto::SecretKey;

    /// The recovery authority's test secret.
    pub fn secret() -> SecretKey {
        let mut b = [0x2A; 32];
        b[31] = 0;
        SecretKey::from_bytes(b).expect("a canonical scalar")
    }

    /// A configuration that offers owned custody with the test recovery authority.
    pub fn config() -> RecoveryConfig {
        RecoveryConfig::owned_custody(RecoveryKey(secret().public_key().0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn key() -> RecoveryKey {
        testing::config().recovery_key.unwrap()
    }

    #[test]
    fn req_au_15_startup_refuses_a_recovery_key_that_is_an_operator_key() {
        let c = testing::config();
        let op = Verifier::new(key().0.to_vec());
        assert_eq!(
            c.check([&Verifier::new(vec![1]), &op]),
            Err(StartupError::RecoveryKeyIsOperatorKey)
        );
        assert_eq!(c.check([&Verifier::new(vec![1])]), Ok(()));
    }

    #[test]
    fn req_au_9_startup_refuses_owned_custody_without_a_recovery_key() {
        let mut c = testing::config();
        c.recovery_key = None;
        assert_eq!(
            c.check([]),
            Err(StartupError::OwnedCustodyWithoutRecoveryKey)
        );
        // A holder-held credential is never the only mechanism (REQ-AU-18).
        assert_eq!(
            c.with_holder_held(true).check([]),
            Err(StartupError::OwnedCustodyWithoutRecoveryKey)
        );
        assert_eq!(RecoveryConfig::not_offered().check([]), Ok(()));
    }

    #[test]
    fn req_au_16_startup_refuses_a_zero_delay() {
        assert_eq!(
            testing::config().with_recovery_delay(0).check([]),
            Err(StartupError::ZeroDelay)
        );
        assert_eq!(
            testing::config().with_succession_delay(Some(0)).check([]),
            Err(StartupError::ZeroDelay)
        );
        assert_eq!(
            testing::config().with_succession_delay(None).check([]),
            Ok(())
        );
    }

    /// `REQ-AU-19`: the configuration's schema is exactly these fields. Destructuring it
    /// exhaustively fails to compile if a field — a guardian set, a threshold — is ever added.
    #[test]
    fn req_au_19_the_configuration_has_no_guardian_field() {
        let RecoveryConfig {
            owned_custody,
            recovery_key,
            recovery_delay,
            succession_delay,
            holder_held,
        } = testing::config();
        assert!(owned_custody && recovery_key.is_some() && !holder_held);
        assert_eq!(recovery_delay, DEFAULT_RECOVERY_DELAY);
        assert_eq!(succession_delay, Some(DEFAULT_SUCCESSION_DELAY));
    }

    #[test]
    fn the_recovery_authority_is_named_by_its_key_and_is_no_identity_it_could_collide_with() {
        let a = testing::config().authority().unwrap();
        assert_eq!(a, testing::config().authority().unwrap());
        let other = RecoveryConfig::owned_custody(RecoveryKey([9; 32]))
            .authority()
            .unwrap();
        assert_ne!(a.principal, other.principal);
        assert_eq!(a.used().kind, AuthorityKind::Recovery);
    }

    #[test]
    fn signatures_verify_only_over_their_own_message_and_key() {
        let keys = ledger_kernel::keys::RootKeys::new([3; 32]);
        let sk = testing::secret();
        let account = AccountId::mint(&OperationId::from_ref(OperationRef([1; 16])), 0);
        let op = OperationId::from_ref(OperationRef([2; 16]));
        let msg = reclaim_challenge(&account, 1);
        let sig = keys.sign(&sk, &msg);
        assert!(verifies(&key().0, &msg, &sig));
        assert!(!verifies(&key().0, &reclaim_challenge(&account, 3), &sig));
        assert!(!verifies(&[1, 2, 3], &msg, &sig));
        // The two messages never coincide.
        let pending = PendingId(op);
        let v = Verifier::new(key().0.to_vec());
        assert_ne!(approval_message(&pending, &account, &v), msg);
    }

    #[test]
    fn dec_19_an_approval_binds_the_verifiers_scheme() {
        let op = OperationId::from_ref(OperationRef([2; 16]));
        let account = AccountId::mint(&op, 0);
        let pending = PendingId(op);
        let bytes = [7u8; 32].to_vec();
        let key = approval_message(&pending, &account, &Verifier::new(bytes.clone()));
        let api = approval_message(
            &pending,
            &account,
            &Verifier::of(crate::scheme::Scheme::ApiKey, bytes),
        );
        assert_ne!(key, api);
    }
}
