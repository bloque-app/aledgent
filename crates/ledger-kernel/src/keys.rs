// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Keys, blindings and signature nonces (`F-001` §6, `DEC-3`, `REQ-RT-7`), and the key boundary
//! (`F-035`, guardrail 13, `NFR-8`).
//!
//! The core holds every kernel account's key (`DEC-3`). [`RootKeys`] derives each one from a
//! single root, so no two kernel accounts share a key and nothing needs storing per account. The
//! same root derives the registrar's key, the blinding of every entry, and the nonce of every
//! signature: all of them are inputs, never host randomness, so replaying a unit reproduces it
//! byte for byte (`REQ-RT-3`, `REQ-RT-7`).
//!
//! **The key boundary** (`F-035`). The book never needs the root, only five answers about it:
//! the registrar's public key, an account's public key, a registrar signature, an account
//! signature, and an entry's blinding. [`KeyRequest`] names them and [`KeyAnswer`] carries the
//! reply; a [`Custody`] answers. A [`KeyRing`] is what the book holds: either the root itself
//! ([`KeyRing::from_root`], tests and development), or any [`Custody`]
//! ([`KeyRing::with_custody`]): in production, the signer service (`services/signer`), a process
//! of its own that holds the root and nothing of the book. Inside the Agent, the answers the host
//! obtained travel as a private input ([`Answers`]), so the Agent never holds the root either.
//!
//! The root cannot be printed or serialised: [`RootKeys`] has no accessor for it, its `Debug`
//! output is redacted, and it is wiped on drop. With `std`, every root loaded in a process is
//! counted ([`local_roots_loaded`]): the book's process asserts it never loads one.
//!
//! **Trade-off, stated plainly.** Signing needs CipherClerk's `signing`
//! feature, which also enables `rand_core/getrandom`. This crate never calls
//! the OS generator (every RNG here is a seeded ChaCha20), but the dependency
//! is in the graph. Whether it builds for the core's guest target is for
//! `F-002` to confirm (`F-001` Open points).
//!
//! **Trade-off, stated plainly** (`F-035`). A custody cannot judge what it signs: it holds no
//! book, so it signs any account's transfer the book's process asks it to. What the boundary
//! buys is that the root never leaves it (a compromised book process can use the key while it
//! runs, never take it away), that every use is attributable to an authenticated caller and
//! logged apart from the book, and that access is revoked by removing a token. A blinding is
//! answered in the clear: it is an opening, which the core keeps anyway as the opening oracle
//! (`DEC-11`); the root that derives it is not.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use cipher_clerk::AccountId;
use cipher_clerk::crypto::{AuthKey, Blinding, SecretKey, Signature, blake2b_256, blake2b_512};
use curve25519_dalek::scalar::Scalar;
use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;
use zeroize::Zeroize;

use crate::posting::PostingMeta;

const ACCOUNT_KEY_DOMAIN: &[u8] = b"aledgent/kernel-account-key/v1";
const REGISTRAR_KEY_DOMAIN: &[u8] = b"aledgent/registrar-key/v1";
const BLINDING_DOMAIN: &[u8] = b"aledgent/entry-blinding/v1";
const NONCE_DOMAIN: &[u8] = b"aledgent/signature-nonce/v1";
const MESSAGE_DOMAIN: &[u8] = b"aledgent/signed-message/v1";

#[cfg(feature = "std")]
static LOCAL_ROOTS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// How many roots this process has loaded ([`RootKeys::new`], [`KeyRing::from_root`]). The
/// book's process asserts it is zero (`F-035`, guardrail 13): its keys are a custody's.
#[cfg(feature = "std")]
pub fn local_roots_loaded() -> usize {
    LOCAL_ROOTS.load(core::sync::atomic::Ordering::SeqCst)
}

/// The key material itself: one 32-byte root from which every kernel key, blinding and nonce is
/// derived. Held by a custody (the signer service), or by a test.
pub struct RootKeys {
    root: [u8; 32],
}

impl core::fmt::Debug for RootKeys {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("RootKeys(<redacted>)")
    }
}

impl Drop for RootKeys {
    fn drop(&mut self) {
        self.root.zeroize();
    }
}

/// A scalar derived from the root under a domain, by wide reduction.
fn derive_scalar(root: &[u8; 32], domain: &[u8], parts: &[&[u8]]) -> Scalar {
    let mut input: alloc::vec::Vec<&[u8]> = alloc::vec::Vec::with_capacity(parts.len() + 1);
    input.push(root);
    input.extend_from_slice(parts);
    let mut wide = blake2b_512(domain, &input);
    let s = Scalar::from_bytes_mod_order_wide(&wide);
    wide.zeroize();
    s
}

fn secret_from_scalar(s: Scalar) -> SecretKey {
    SecretKey::from_bytes(s.to_bytes()).expect("a reduced scalar is canonical")
}

impl RootKeys {
    /// The keys over this root. The caller should wipe its own copy.
    pub fn new(root: [u8; 32]) -> Self {
        #[cfg(feature = "std")]
        LOCAL_ROOTS.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
        Self { root }
    }

    /// The secret key of one kernel account.
    pub fn account_secret(&self, account: &AccountId) -> SecretKey {
        secret_from_scalar(derive_scalar(&self.root, ACCOUNT_KEY_DOMAIN, &[&account.0]))
    }

    /// The public key of one kernel account: its `auth_key`.
    pub fn account_key(&self, account: &AccountId) -> AuthKey {
        self.account_secret(account).public_key()
    }

    /// The registrar's secret key: the only key that may create accounts.
    pub fn registrar_secret(&self) -> SecretKey {
        secret_from_scalar(derive_scalar(&self.root, REGISTRAR_KEY_DOMAIN, &[]))
    }

    /// The registrar's public key, as written into the journal.
    pub fn registrar_key(&self) -> AuthKey {
        self.registrar_secret().public_key()
    }

    /// The free blinding an entry's [`BlindingSeed`] names.
    pub fn blinding(&self, seed: &BlindingSeed) -> Scalar {
        derive_scalar(
            &self.root,
            BLINDING_DOMAIN,
            &[
                &seed.operation,
                &[seed.kind],
                &seed.ordinal.to_be_bytes(),
                &seed.index.to_be_bytes(),
            ],
        )
    }

    /// Sign `msg` with `secret`, drawing the nonce from a ChaCha20 stream
    /// seeded by the root, the signer and the message. Deterministic, and
    /// unpredictable to anyone without the root.
    pub fn sign(&self, secret: &SecretKey, msg: &[u8]) -> Signature {
        let signer = secret.public_key();
        let digest = blake2b_256(MESSAGE_DOMAIN, &[msg]);
        let mut seed = blake2b_256(NONCE_DOMAIN, &[&self.root, &signer.0, &digest]);
        let mut rng = ChaCha20Rng::from_seed(seed);
        seed.zeroize();
        secret.sign(msg, &mut rng)
    }
}

impl Custody for RootKeys {
    fn answer(&self, request: &KeyRequest) -> KeyAnswer {
        match request {
            KeyRequest::RegistrarKey => KeyAnswer::Key(self.registrar_key().0),
            KeyRequest::AccountKey(a) => KeyAnswer::Key(self.account_key(&AccountId(*a)).0),
            KeyRequest::SignRegistrar(msg) => {
                KeyAnswer::Signature(sig_bytes(&self.sign(&self.registrar_secret(), msg)))
            }
            KeyRequest::SignAccount(a, msg) => KeyAnswer::Signature(sig_bytes(
                &self.sign(&self.account_secret(&AccountId(*a)), msg),
            )),
            KeyRequest::EntryBlinding(seed) => KeyAnswer::Blinding(self.blinding(seed).to_bytes()),
        }
    }
}

fn sig_bytes(s: &Signature) -> [u8; 64] {
    let mut out = [0u8; 64];
    out[..32].copy_from_slice(&s.r);
    out[32..].copy_from_slice(&s.s);
    out
}

/// What names one entry's free blinding: the posting's operation, kind and ordinal, and the
/// entry's index.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct BlindingSeed {
    /// The posting's operation.
    pub operation: [u8; 16],
    /// The posting's kind code.
    pub kind: u8,
    /// The posting's ordinal.
    pub ordinal: u32,
    /// The entry's index.
    pub index: u32,
}

impl BlindingSeed {
    /// The seed of entry `index` of the posting `meta`.
    pub fn of(meta: &PostingMeta, index: u32) -> Self {
        BlindingSeed {
            operation: meta.operation.0,
            kind: meta.kind.code(),
            ordinal: meta.ordinal,
            index,
        }
    }
}

/// One question about the root (module documentation). Its [`KeyRequest::encode`] is canonical.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KeyRequest {
    /// The registrar's public key.
    RegistrarKey,
    /// A kernel account's public key.
    AccountKey([u8; 16]),
    /// A registrar signature over these bytes (an account's creation).
    SignRegistrar(Vec<u8>),
    /// An account's signature over these bytes (a transfer it debits).
    SignAccount([u8; 16], Vec<u8>),
    /// An entry's free blinding.
    EntryBlinding(BlindingSeed),
}

/// A custody's answer to a [`KeyRequest`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum KeyAnswer {
    /// A public key, compressed Ristretto.
    Key([u8; 32]),
    /// A Schnorr signature, `R ‖ s`.
    Signature([u8; 64]),
    /// A blinding, a canonical scalar.
    Blinding([u8; 32]),
}

impl core::fmt::Debug for KeyAnswer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // A blinding opens a commitment: never formatted.
        match self {
            KeyAnswer::Key(_) => f.write_str("KeyAnswer::Key"),
            KeyAnswer::Signature(_) => f.write_str("KeyAnswer::Signature"),
            KeyAnswer::Blinding(_) => f.write_str("KeyAnswer::Blinding(<redacted>)"),
        }
    }
}

fn take<const N: usize>(b: &[u8], at: &mut usize) -> Option<[u8; N]> {
    let out: [u8; N] = b.get(*at..*at + N)?.try_into().ok()?;
    *at += N;
    Some(out)
}

impl KeyRequest {
    /// The canonical encoding: a tag, then the fields, fixed width; a message is last.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            KeyRequest::RegistrarKey => out.push(1),
            KeyRequest::AccountKey(a) => {
                out.push(2);
                out.extend_from_slice(a);
            }
            KeyRequest::SignRegistrar(msg) => {
                out.push(3);
                out.extend_from_slice(msg);
            }
            KeyRequest::SignAccount(a, msg) => {
                out.push(4);
                out.extend_from_slice(a);
                out.extend_from_slice(msg);
            }
            KeyRequest::EntryBlinding(s) => {
                out.push(5);
                out.extend_from_slice(&s.operation);
                out.push(s.kind);
                out.extend_from_slice(&s.ordinal.to_be_bytes());
                out.extend_from_slice(&s.index.to_be_bytes());
            }
        }
        out
    }

    /// The request `bytes` encode, if they are one.
    pub fn decode(bytes: &[u8]) -> Option<KeyRequest> {
        let (&tag, rest) = bytes.split_first()?;
        let mut at = 0usize;
        let r = match tag {
            1 if rest.is_empty() => KeyRequest::RegistrarKey,
            2 => KeyRequest::AccountKey(take::<16>(rest, &mut at)?),
            3 => KeyRequest::SignRegistrar(rest.to_vec()),
            4 => {
                let a = take::<16>(rest, &mut at)?;
                KeyRequest::SignAccount(a, rest[at..].to_vec())
            }
            5 => KeyRequest::EntryBlinding(BlindingSeed {
                operation: take::<16>(rest, &mut at)?,
                kind: take::<1>(rest, &mut at)?[0],
                ordinal: u32::from_be_bytes(take::<4>(rest, &mut at)?),
                index: u32::from_be_bytes(take::<4>(rest, &mut at)?),
            }),
            _ => return None,
        };
        if matches!(tag, 2 | 5) && at != rest.len() {
            return None;
        }
        Some(r)
    }

    /// What kind of request this is, for an audit line: never its message.
    pub fn kind(&self) -> &'static str {
        match self {
            KeyRequest::RegistrarKey => "registrar_key",
            KeyRequest::AccountKey(_) => "account_key",
            KeyRequest::SignRegistrar(_) => "sign_registrar",
            KeyRequest::SignAccount(..) => "sign_account",
            KeyRequest::EntryBlinding(_) => "entry_blinding",
        }
    }
}

impl KeyAnswer {
    /// The canonical encoding: a tag, then the bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(65);
        match self {
            KeyAnswer::Key(k) => {
                out.push(1);
                out.extend_from_slice(k);
            }
            KeyAnswer::Signature(s) => {
                out.push(2);
                out.extend_from_slice(s);
            }
            KeyAnswer::Blinding(b) => {
                out.push(3);
                out.extend_from_slice(b);
            }
        }
        out
    }

    /// The answer `bytes` encode, if they are one.
    pub fn decode(bytes: &[u8]) -> Option<KeyAnswer> {
        let (&tag, rest) = bytes.split_first()?;
        match tag {
            1 => Some(KeyAnswer::Key(rest.try_into().ok()?)),
            2 => Some(KeyAnswer::Signature(rest.try_into().ok()?)),
            3 => Some(KeyAnswer::Blinding(rest.try_into().ok()?)),
            _ => None,
        }
    }
}

/// What answers [`KeyRequest`]s: the root itself ([`RootKeys`]), the signer service's client, a
/// recorder over either ([`Recording`]), or the answers a host recorded ([`Answers`]).
///
/// Answering never fails: a custody that cannot answer does not make a value up. The signer's
/// client waits for the signer and stops the process if it is refused (fail closed); the
/// Agent's [`Answers`] panic on a question its host did not answer, which aborts the invocation.
pub trait Custody: Send + Sync {
    /// The answer to `request`.
    fn answer(&self, request: &KeyRequest) -> KeyAnswer;
}

#[cfg(target_has_atomic = "ptr")]
impl<C: Custody + ?Sized> Custody for alloc::sync::Arc<C> {
    fn answer(&self, request: &KeyRequest) -> KeyAnswer {
        (**self).answer(request)
    }
}

/// The answers a host obtained for one invocation, as the Agent receives them in its private
/// input: encoded request to encoded answer.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Answers {
    entries: BTreeMap<Vec<u8>, Vec<u8>>,
}

impl core::fmt::Debug for Answers {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Answers")
            .field("entries", &self.entries.len())
            .finish()
    }
}

impl Answers {
    /// The answers in `entries` (encoded request, encoded answer).
    pub fn new(entries: impl IntoIterator<Item = (Vec<u8>, Vec<u8>)>) -> Self {
        Answers {
            entries: entries.into_iter().collect(),
        }
    }

    /// Add one answer.
    pub fn insert(&mut self, request: &KeyRequest, answer: &KeyAnswer) {
        self.entries.insert(request.encode(), answer.encode());
    }

    /// Every answer, in request order.
    pub fn entries(&self) -> impl Iterator<Item = (&Vec<u8>, &Vec<u8>)> {
        self.entries.iter()
    }

    /// Every answer, in request order, taken.
    pub fn into_entries(self) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.entries.into_iter().collect()
    }

    /// How many there are.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Custody for Answers {
    fn answer(&self, request: &KeyRequest) -> KeyAnswer {
        self.entries
            .get(&request.encode())
            .and_then(|a| KeyAnswer::decode(a))
            .unwrap_or_else(|| panic!("no custody answer for a {} request", request.kind()))
    }
}

/// A custody that records every answer it gives, for the Agent's private input: the host's
/// replica runs over it, and the driver takes what it recorded with each input (`F-035`).
#[cfg(feature = "std")]
pub struct Recording {
    inner: alloc::sync::Arc<dyn Custody>,
    log: alloc::sync::Arc<std::sync::Mutex<Answers>>,
}

#[cfg(feature = "std")]
impl Recording {
    /// A recorder over `inner`, and the handle its answers are taken from.
    pub fn new(
        inner: alloc::sync::Arc<dyn Custody>,
    ) -> (Recording, alloc::sync::Arc<std::sync::Mutex<Answers>>) {
        let log = alloc::sync::Arc::new(std::sync::Mutex::new(Answers::default()));
        (
            Recording {
                inner,
                log: alloc::sync::Arc::clone(&log),
            },
            log,
        )
    }
}

#[cfg(feature = "std")]
impl Custody for Recording {
    fn answer(&self, request: &KeyRequest) -> KeyAnswer {
        let a = self.inner.answer(request);
        self.log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(request, &a);
        a
    }
}

/// What the book holds: the root ([`KeyRing::from_root`]) or a custody that answers for it
/// ([`KeyRing::with_custody`]). See the module documentation.
pub struct KeyRing {
    inner: Inner,
}

enum Inner {
    Root(RootKeys),
    Custody(Box<dyn Custody>),
}

impl core::fmt::Debug for KeyRing {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("KeyRing(<redacted>)")
    }
}

fn wrong(request: &KeyRequest) -> ! {
    panic!(
        "the custody gave the wrong kind of answer to a {} request",
        request.kind()
    )
}

impl KeyRing {
    /// A key ring over this root, held in this process: tests and development. The caller should
    /// wipe its own copy.
    pub fn from_root(root: [u8; 32]) -> Self {
        KeyRing {
            inner: Inner::Root(RootKeys::new(root)),
        }
    }

    /// A key ring whose every key, blinding and signature is `custody`'s.
    pub fn with_custody(custody: Box<dyn Custody>) -> Self {
        KeyRing {
            inner: Inner::Custody(custody),
        }
    }

    /// The local root, if this ring holds one.
    pub fn root_keys(&self) -> Option<&RootKeys> {
        match &self.inner {
            Inner::Root(r) => Some(r),
            Inner::Custody(_) => None,
        }
    }

    /// The answer to `request`.
    pub fn answer(&self, request: &KeyRequest) -> KeyAnswer {
        match &self.inner {
            Inner::Root(r) => r.answer(request),
            Inner::Custody(c) => c.answer(request),
        }
    }

    fn key(&self, request: KeyRequest) -> AuthKey {
        match self.answer(&request) {
            KeyAnswer::Key(k) => AuthKey(k),
            _ => wrong(&request),
        }
    }

    fn signature(&self, request: KeyRequest) -> Signature {
        match self.answer(&request) {
            KeyAnswer::Signature(s) => {
                let mut r = [0u8; 32];
                let mut t = [0u8; 32];
                r.copy_from_slice(&s[..32]);
                t.copy_from_slice(&s[32..]);
                Signature { r, s: t }
            }
            _ => wrong(&request),
        }
    }

    /// The public key of one kernel account: its `auth_key`.
    pub fn account_key(&self, account: &AccountId) -> AuthKey {
        if let Inner::Root(r) = &self.inner {
            return r.account_key(account);
        }
        self.key(KeyRequest::AccountKey(account.0))
    }

    /// The registrar's public key, as written into the journal.
    pub fn registrar_key(&self) -> AuthKey {
        if let Inner::Root(r) = &self.inner {
            return r.registrar_key();
        }
        self.key(KeyRequest::RegistrarKey)
    }

    /// The registrar's signature over `msg`: an account's creation.
    pub fn sign_registrar(&self, msg: &[u8]) -> Signature {
        if let Inner::Root(r) = &self.inner {
            return r.sign(&r.registrar_secret(), msg);
        }
        self.signature(KeyRequest::SignRegistrar(msg.to_vec()))
    }

    /// `account`'s signature over `msg`: a transfer that debits it.
    pub fn sign_account(&self, account: &AccountId, msg: &[u8]) -> Signature {
        if let Inner::Root(r) = &self.inner {
            return r.sign(&r.account_secret(account), msg);
        }
        self.signature(KeyRequest::SignAccount(account.0, msg.to_vec()))
    }

    /// The free blinding of entry `index` of the posting `meta`. The builder
    /// uses it for every entry but the last of each asset, whose blinding is
    /// solved so that the asset balances.
    pub(crate) fn entry_blinding(&self, meta: &PostingMeta, index: u32) -> Scalar {
        let seed = BlindingSeed::of(meta, index);
        if let Inner::Root(r) = &self.inner {
            return r.blinding(&seed);
        }
        let request = KeyRequest::EntryBlinding(seed);
        match self.answer(&request) {
            KeyAnswer::Blinding(b) => Option::<Scalar>::from(Scalar::from_canonical_bytes(b))
                .unwrap_or_else(|| wrong(&request)),
            _ => wrong(&request),
        }
    }
}

/// A blinding from a scalar.
pub(crate) fn blinding_of(s: &Scalar) -> Blinding {
    Blinding::from_bytes(s.to_bytes()).expect("a reduced scalar is canonical")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::posting::{OperationRef, PostingKind};
    use cipher_clerk::crypto::verify_signature;

    #[test]
    fn keys_are_derived_per_account_and_stable() {
        let k = KeyRing::from_root([1; 32]);
        let a = AccountId([1; 16]);
        let b = AccountId([2; 16]);
        assert_eq!(k.account_key(&a), k.account_key(&a));
        assert_ne!(k.account_key(&a), k.account_key(&b));
        assert_ne!(k.account_key(&a), k.registrar_key());
        assert_ne!(
            k.account_key(&a),
            KeyRing::from_root([2; 32]).account_key(&a)
        );
    }

    #[test]
    fn signatures_are_deterministic_and_verify() {
        let k = RootKeys::new([3; 32]);
        let sk = k.account_secret(&AccountId([1; 16]));
        let s1 = k.sign(&sk, b"message");
        let s2 = k.sign(&sk, b"message");
        assert_eq!(s1, s2);
        assert_ne!(s1, k.sign(&sk, b"other message"));
        assert!(verify_signature(&sk.public_key(), b"message", &s1));
    }

    #[test]
    fn blindings_depend_on_the_posting_and_the_entry() {
        let k = KeyRing::from_root([4; 32]);
        let m = PostingMeta::new(PostingKind::Transfer, OperationRef([1; 16]), 0);
        assert_eq!(k.entry_blinding(&m, 0), k.entry_blinding(&m, 0));
        assert_ne!(k.entry_blinding(&m, 0), k.entry_blinding(&m, 1));
        let m2 = PostingMeta::new(PostingKind::Capture, OperationRef([1; 16]), 0);
        assert_ne!(k.entry_blinding(&m, 0), k.entry_blinding(&m2, 0));
    }

    #[test]
    fn the_root_is_never_printed() {
        let k = KeyRing::from_root([0xAB; 32]);
        let shown = alloc::format!("{k:?}");
        assert_eq!(shown, "KeyRing(<redacted>)");
        let r = RootKeys::new([0xAB; 32]);
        assert_eq!(alloc::format!("{r:?}"), "RootKeys(<redacted>)");
        let b = KeyAnswer::Blinding([0xAB; 32]);
        assert!(!alloc::format!("{b:?}").contains("171"));
    }

    fn requests() -> Vec<KeyRequest> {
        alloc::vec![
            KeyRequest::RegistrarKey,
            KeyRequest::AccountKey([7; 16]),
            KeyRequest::SignRegistrar(b"an account".to_vec()),
            KeyRequest::SignAccount([7; 16], b"a transfer".to_vec()),
            KeyRequest::SignAccount([8; 16], Vec::new()),
            KeyRequest::EntryBlinding(BlindingSeed {
                operation: [9; 16],
                kind: 4,
                ordinal: 2,
                index: 3,
            }),
        ]
    }

    /// `F-035`: a ring over a custody gives the bytes the root gives, so a book moved behind the
    /// signer is the same book (`REQ-RT-3`).
    #[test]
    fn req_rt_3_a_custody_ring_is_the_root_ring() {
        let local = KeyRing::from_root([5; 32]);
        let remote = KeyRing::with_custody(Box::new(RootKeys::new([5; 32])));
        let a = AccountId([1; 16]);
        assert_eq!(local.account_key(&a), remote.account_key(&a));
        assert_eq!(local.registrar_key(), remote.registrar_key());
        assert_eq!(local.sign_registrar(b"m"), remote.sign_registrar(b"m"));
        assert_eq!(local.sign_account(&a, b"m"), remote.sign_account(&a, b"m"));
        let m = PostingMeta::new(PostingKind::Transfer, OperationRef([1; 16]), 0);
        assert_eq!(local.entry_blinding(&m, 1), remote.entry_blinding(&m, 1));
        assert!(remote.root_keys().is_none());
        assert!(verify_signature(
            &remote.account_key(&a),
            b"m",
            &remote.sign_account(&a, b"m")
        ));
    }

    #[test]
    fn requests_and_answers_round_trip() {
        let k = RootKeys::new([6; 32]);
        for r in requests() {
            assert_eq!(KeyRequest::decode(&r.encode()), Some(r.clone()));
            let a = k.answer(&r);
            assert_eq!(KeyAnswer::decode(&a.encode()), Some(a));
        }
        assert_eq!(KeyRequest::decode(&[2, 0]), None);
        assert_eq!(KeyRequest::decode(&[1, 0]), None);
        assert_eq!(KeyRequest::decode(&[9]), None);
        assert_eq!(KeyAnswer::decode(&[1; 5]), None);
    }

    /// The Agent's side: what the host recorded answers every question it asked again.
    #[test]
    fn answers_replay_what_was_recorded() {
        let k = RootKeys::new([6; 32]);
        let mut answers = Answers::default();
        for r in requests() {
            answers.insert(&r, &k.answer(&r));
        }
        let ring = KeyRing::with_custody(Box::new(Answers::new(answers.clone().into_entries())));
        for r in requests() {
            assert_eq!(ring.answer(&r), k.answer(&r));
        }
    }

    #[test]
    #[should_panic(expected = "no custody answer for a sign_account request")]
    fn an_unanswered_question_aborts() {
        let ring = KeyRing::with_custody(Box::new(Answers::default()));
        let _ = ring.sign_account(&AccountId([1; 16]), b"m");
    }

    /// The tripwire the book's process checks (`F-035`): every root loaded is counted.
    #[cfg(feature = "std")]
    #[test]
    fn nfr_8_every_root_loaded_is_counted() {
        let before = local_roots_loaded();
        let _ = KeyRing::from_root([9; 32]);
        let _ = RootKeys::new([9; 32]);
        assert!(local_roots_loaded() >= before + 2);
        let _ = KeyRing::with_custody(Box::new(Answers::default()));
    }

    #[cfg(feature = "std")]
    #[test]
    fn a_recording_keeps_every_answer() {
        let (rec, log) = Recording::new(alloc::sync::Arc::new(RootKeys::new([6; 32])));
        let ring = KeyRing::with_custody(Box::new(rec));
        let _ = ring.registrar_key();
        let _ = ring.sign_account(&AccountId([1; 16]), b"m");
        let taken = core::mem::take(&mut *log.lock().unwrap());
        assert_eq!(taken.len(), 2);
        let replay = KeyRing::with_custody(Box::new(taken));
        assert_eq!(replay.registrar_key(), ring.registrar_key());
    }
}
