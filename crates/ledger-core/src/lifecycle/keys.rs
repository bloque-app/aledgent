// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The two idempotency keys, and the other identities the lifecycle derives (`F-007` §5–§7,
//! T-007-05, `REQ-OP-10`, `REQ-OP-11`, `REQ-EV-3`).
//!
//! - A **client key** is the caller's: opaque, unique within (client-key domain, surface, key),
//!   and never parsed. The domain ([`KeyDomain`], `REQ-OP-10`, amendment 0011) is the integrating
//!   party the initiating principal is tied to, or the principal itself where it is tied to none
//!   (`REQ-ID-7`). When a caller supplies none, one is derived from the semantic identity of the
//!   request, its principal included, and returned ([`derive_client_key`]).
//! - An **operation reference** is the service's: [`operation_reference`] derives it from
//!   (deployment, domain, surface, client key). It is deterministic and opaque, and two domains'
//!   keys cannot collide into one reference because the domain is part of it. Within one party,
//!   the same key from two principals is one reference, and the core refuses the second principal
//!   with `ERR-ClientKeyReused` (its request names another initiator) instead of answering it with
//!   the first one's reply. A principal tied to no party derives what it derived before the
//!   parties existed.
//!
//! No function here takes a time or a retry counter: nothing that changes between retries can
//! enter a key (`REQ-OP-11`, `REQ-EV-3`). `F-009` calls the same functions.

use alloc::vec::Vec;

use cipher_clerk::crypto::blake2b_256;
use ledger_kernel::posting::OperationRef;

use super::table::State;
use crate::id::OperationId;
use crate::party::KeyDomain;

const CLIENT_KEY_DOMAIN: &[u8] = b"aledgent/ck/v1";
const OPREF_DOMAIN: &[u8] = b"aledgent/opref/v1";
/// The operation reference of a key in an integrating party's domain (`REQ-OP-10`, amendment
/// 0011): a domain of its own, so a party's references never meet a lone principal's.
const OPREF_PARTY_DOMAIN: &[u8] = b"aledgent/opref/party/v1";
const OPREF_INBOUND_DOMAIN: &[u8] = b"aledgent/opref/inbound/v1";
const REQUEST_DOMAIN: &[u8] = b"aledgent/request/v1";
const TRANSITION_DOMAIN: &[u8] = b"aledgent/transition/v1";
const NOTIFICATION_DOMAIN: &[u8] = b"aledgent/notification/v1";

/// The surface a command arrived on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Surface {
    /// `CTR-CMD`.
    Command,
    /// `CTR-OPS`.
    Operator,
}

impl Surface {
    /// A stable one-byte code.
    pub const fn code(self) -> u8 {
        match self {
            Surface::Command => 1,
            Surface::Operator => 2,
        }
    }
}

/// A caller's client key: opaque bytes, compared only for equality.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ClientKey(Vec<u8>);

impl ClientKey {
    /// A key as the caller supplied it.
    pub fn new(bytes: impl Into<Vec<u8>>) -> ClientKey {
        ClientKey(bytes.into())
    }

    /// Its bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// The digest of a request's canonical form, without transport fields and without the time it
/// arrived: what a repeat must match (`REQ-OP-10`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RequestDigest(pub [u8; 32]);

/// A canonical encoding of a request: each field length-prefixed, so no two different requests
/// share an encoding. There is deliberately no method for a time of arrival.
#[derive(Clone, Debug, Default)]
pub struct Canon(Vec<u8>);

impl Canon {
    /// A request of the command `tag`.
    pub fn new(tag: &str) -> Canon {
        let mut c = Canon(Vec::new());
        c.bytes(tag.as_bytes());
        c
    }

    /// A field of raw bytes.
    pub fn bytes(&mut self, b: &[u8]) -> &mut Canon {
        let len = u32::try_from(b.len()).unwrap_or(u32::MAX);
        self.0.extend_from_slice(&len.to_be_bytes());
        self.0.extend_from_slice(b);
        self
    }

    /// A one-byte field.
    pub fn u8(&mut self, v: u8) -> &mut Canon {
        self.bytes(&[v])
    }

    /// An integer field (an amount, a bound, a window length; a deadline or an expiry the request
    /// itself carries).
    pub fn u64(&mut self, v: u64) -> &mut Canon {
        self.bytes(&v.to_be_bytes())
    }

    /// An optional field.
    pub fn opt<T>(&mut self, v: Option<T>, f: impl FnOnce(&mut Canon, T)) -> &mut Canon {
        match v {
            None => {
                self.u8(0);
            }
            Some(v) => {
                self.u8(1);
                f(self, v);
            }
        }
        self
    }

    /// The encoding itself: a canonical form to compare byte for byte (`F-011`, `AC-H3.2`).
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// The digest.
    pub fn digest(&self) -> RequestDigest {
        RequestDigest(blake2b_256(REQUEST_DOMAIN, &[&self.0]))
    }
}

/// The client key of a request submitted without one: `H("aledgent/ck/v1", surface, request)`.
/// It depends on the request's semantic identity only, so a retry derives the same key.
pub fn derive_client_key(surface: Surface, request: &RequestDigest) -> ClientKey {
    ClientKey(blake2b_256(CLIENT_KEY_DOMAIN, &[&[surface.code()], &request.0]).to_vec())
}

/// The first 16 bytes of a digest, as an operation reference.
fn reference(digest: [u8; 32]) -> OperationId {
    let mut body = [0u8; 16];
    body.copy_from_slice(&digest[..16]);
    OperationId::from_ref(OperationRef(body))
}

/// The operation reference of a command, in its client key's domain (`REQ-OP-10`):
/// `H("aledgent/opref/v1", deployment, principal, surface, client key)` for a principal tied to
/// no party, `H("aledgent/opref/party/v1", deployment, party, surface, client key)` for an
/// integrating party's; first 16 bytes. Accepted proposal of `F-007` §7, amendment 0011.
pub fn operation_reference(
    deployment: &[u8; 16],
    domain: &KeyDomain,
    surface: Surface,
    key: &ClientKey,
) -> OperationId {
    let (tag, id) = match domain {
        KeyDomain::Principal(p) => (OPREF_DOMAIN, *p),
        KeyDomain::Party(p) => (OPREF_PARTY_DOMAIN, p.identity()),
    };
    reference(blake2b_256(
        tag,
        &[deployment, id.as_bytes(), &[surface.code()], &key.0],
    ))
}

/// The operation reference of an inbound arrival aledgent did not originate:
/// `H("aledgent/opref/inbound/v1", coordinate)`, first 16 bytes.
pub fn inbound_reference(coordinate: &[u8]) -> OperationId {
    reference(blake2b_256(OPREF_INBOUND_DOMAIN, &[coordinate]))
}

/// The idempotency key of one transition (`REQ-OP-3`, `INV-18`):
/// `H("aledgent/transition/v1", operation, from, to, evidence)`. For a deadline trigger the
/// evidence is the deadline itself, not the tick's time, so repeated ticks give one key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TransitionKey(pub [u8; 32]);

impl TransitionKey {
    /// The key of `op` moving from `from` to `to` on `evidence`.
    pub fn derive(op: &OperationId, from: State, to: State, evidence: &Evidence) -> TransitionKey {
        TransitionKey(blake2b_256(
            TRANSITION_DOMAIN,
            &[op.as_bytes(), &[from.code(), to.code()], &evidence.0],
        ))
    }
}

/// The digest of the evidence a transition rests on: a request's digest, an observation's, or a
/// deadline's value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Evidence(pub [u8; 32]);

impl Evidence {
    /// Evidence that is the request itself.
    pub const fn request(r: &RequestDigest) -> Evidence {
        Evidence(r.0)
    }

    /// Evidence that a deadline passed: the deadline's value.
    pub fn deadline(deadline: u64) -> Evidence {
        let mut c = Canon::new("deadline");
        c.u64(deadline);
        Evidence(c.digest().0)
    }
}

/// A notification's stable identity, `H("aledgent/notification/v1", operation, transition key)`:
/// no timestamp and nothing that varies between attempts, so a redelivery is recognisable
/// (`REQ-EV-3`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NotificationId(pub [u8; 32]);

impl NotificationId {
    /// The notification of transition `key` of `op`.
    pub fn derive(op: &OperationId, key: &TransitionKey) -> NotificationId {
        NotificationId(blake2b_256(NOTIFICATION_DOMAIN, &[op.as_bytes(), &key.0]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::id::IdentityId;
    use crate::party::PartyId;

    fn alice() -> KeyDomain {
        KeyDomain::Principal(IdentityId::mint(
            &OperationId::from_ref(OperationRef([1; 16])),
            0,
        ))
    }

    fn bob() -> KeyDomain {
        KeyDomain::Principal(IdentityId::mint(
            &OperationId::from_ref(OperationRef([2; 16])),
            0,
        ))
    }

    const DEPLOYMENT: [u8; 16] = [9; 16];

    #[test]
    fn a_callers_key_cannot_collide_with_another_callers() {
        let k = ClientKey::new(*b"order-17");
        let a = operation_reference(&DEPLOYMENT, &alice(), Surface::Command, &k);
        let b = operation_reference(&DEPLOYMENT, &bob(), Surface::Command, &k);
        assert_ne!(a, b);
        // Nor across surfaces or deployments.
        assert_ne!(
            a,
            operation_reference(&DEPLOYMENT, &alice(), Surface::Operator, &k)
        );
        assert_ne!(
            a,
            operation_reference(&[8; 16], &alice(), Surface::Command, &k)
        );
        // And it is deterministic.
        assert_eq!(
            a,
            operation_reference(&DEPLOYMENT, &alice(), Surface::Command, &k)
        );
    }

    /// `REQ-OP-10`: a key in an integrating party's domain is not the same key in a lone
    /// principal's, even where the party's identity and the principal are the same identity; two
    /// parties' keys never meet; and every principal of one party derives one reference for one
    /// key (the core then tells the principals apart by the request).
    #[test]
    fn req_op_10_a_partys_domain_is_its_own() {
        let k = ClientKey::new(*b"order-17");
        let org = IdentityId::mint(&OperationId::from_ref(OperationRef([3; 16])), 0);
        let party = KeyDomain::Party(PartyId::of(org));
        let other = KeyDomain::Party(PartyId::of(IdentityId::mint(
            &OperationId::from_ref(OperationRef([4; 16])),
            0,
        )));
        let r = operation_reference(&DEPLOYMENT, &party, Surface::Command, &k);
        assert_ne!(
            r,
            operation_reference(
                &DEPLOYMENT,
                &KeyDomain::Principal(org),
                Surface::Command,
                &k
            )
        );
        assert_ne!(
            r,
            operation_reference(&DEPLOYMENT, &other, Surface::Command, &k)
        );
        assert_eq!(
            KeyDomain::of(org, Some(PartyId::of(org))),
            party,
            "a tied principal's domain is its party's"
        );
        assert_eq!(
            KeyDomain::of(org, None),
            KeyDomain::Principal(org),
            "an untied principal is its own domain"
        );
    }

    #[test]
    fn a_derived_key_depends_on_the_request_only() {
        let mut r1 = Canon::new("reserve");
        r1.u64(100);
        let mut r2 = Canon::new("reserve");
        r2.u64(101);
        let k1 = derive_client_key(Surface::Command, &r1.digest());
        assert_eq!(k1, derive_client_key(Surface::Command, &r1.digest()));
        assert_ne!(k1, derive_client_key(Surface::Command, &r2.digest()));
        assert_ne!(k1, derive_client_key(Surface::Operator, &r1.digest()));
    }

    #[test]
    fn canonical_fields_are_unambiguous() {
        let mut a = Canon::new("x");
        a.bytes(b"ab").bytes(b"c");
        let mut b = Canon::new("x");
        b.bytes(b"a").bytes(b"bc");
        assert_ne!(a.digest(), b.digest());
        let mut none = Canon::new("x");
        none.opt(None::<u64>, |c, v| {
            c.u64(v);
        });
        let mut zero = Canon::new("x");
        zero.opt(Some(0u64), |c, v| {
            c.u64(v);
        });
        assert_ne!(none.digest(), zero.digest());
    }

    #[test]
    fn transition_keys_name_operation_pair_and_evidence() {
        let op = OperationId::from_ref(OperationRef([3; 16]));
        let other = OperationId::from_ref(OperationRef([4; 16]));
        let e = Evidence::deadline(50);
        let k = TransitionKey::derive(&op, State::Submitted, State::NeedsReview, &e);
        assert_eq!(
            k,
            TransitionKey::derive(&op, State::Submitted, State::NeedsReview, &e)
        );
        assert_ne!(
            k,
            TransitionKey::derive(&other, State::Submitted, State::NeedsReview, &e)
        );
        assert_ne!(
            k,
            TransitionKey::derive(&op, State::Observed, State::NeedsReview, &e)
        );
        assert_ne!(
            k,
            TransitionKey::derive(
                &op,
                State::Submitted,
                State::NeedsReview,
                &Evidence::deadline(51)
            )
        );
        assert_ne!(
            NotificationId::derive(&op, &k),
            NotificationId::derive(&other, &k)
        );
        // Inbound references depend on the coordinate only.
        assert_eq!(
            inbound_reference(b"block-1/ev-2"),
            inbound_reference(b"block-1/ev-2")
        );
        assert_ne!(
            inbound_reference(b"block-1/ev-2"),
            inbound_reference(b"block-1/ev-3")
        );
    }
}
