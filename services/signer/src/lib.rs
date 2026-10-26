// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! # signer
//!
//! `aledgent-signer` (`F-035`): the key boundary of guardrail 13, `NFR-8`, `REQ-AR-3` and
//! `REQ-SA-8`. Key material never lives in a process that holds the book; it lives here, in a
//! process that holds no book state, and the processes that need a signature ask for one.
//!
//! | Key | Scheme | Who asks | Backend in production |
//! |---|---|---|---|
//! | The key-ring root | [`proto::Scheme::KeyRing`]: answers [`ledger_kernel::keys::KeyRequest`]s | The core host (its replica) | Vault |
//! | The clerk key | [`proto::Scheme::Clerk`]: Schnorr over Ristretto (`F-017`) | The core host (disclosures) | Vault |
//! | The Agent's authority key, the package key | [`proto::Scheme::Ed25519`] | The core host (VOS receipts, the runtime package) | Vault |
//! | The lane keys, the anchor key | [`proto::Scheme::Secp256k1`]: a recoverable ECDSA signature over a 32-byte digest | The settlement adapter (PASS device keys) | Vault; AWS KMS `ECC_SECG_P256K1` per key, opt-in |
//!
//! **The default backend is Vault** (owner ruling `key-custody-opt-in-kms`): the signer reads each
//! key itself at start, a field of a KV v1 secret, after logging in with Vault's AWS IAM auth under
//! its instance or task role, so no secret is in its environment or configuration ([`vault`]).
//! `NFR-8` is the requirement (key material never in the book's process, never readable from a
//! process environment); KMS is not, and stays an opt-in for the secp256k1 keys. A holder's passkey
//! (P-256) is never the signer's: it is the holder's key, never held by the operator.
//!
//! - [`proto`]: the wire protocol, one request and one answer at a time on a Unix socket.
//! - [`client`]: the blocking client every caller links, and its adapters: a
//!   [`ledger_kernel::keys::Custody`], a [`ledger_core::anchor::clerk::ClerkSigner`], an Ed25519
//!   and a secp256k1 signer.
//! - `server` (feature `server`): the listener, the configuration, the backends, Vault (feature
//!   `vault`) and the KMS client (feature `kms`).
//!
//! **Authentication.** Every connection is checked twice: the peer's user id, from the socket's
//! peer credentials, must be one the caller is configured with, and the first frame must carry
//! the caller's token, whose SHA-256 the configuration lists. Then each request names a key the
//! caller is granted. A token removed from the configuration (a reload, on `SIGHUP`) is refused
//! on the very next request, even on a connection already open.
//!
//! **What this does not do** (trade-off, stated plainly). The signer cannot judge a request: it
//! holds no book, so it signs what an authenticated, granted caller asks. A compromised book
//! process can use a key for as long as it runs; it can never take the key away, every use is
//! attributable and logged here, apart from the book, and removing its token ends it.

pub mod client;
pub mod engine;
pub mod proto;

#[cfg(feature = "server")]
pub mod backend;
#[cfg(feature = "server")]
pub mod config;
#[cfg(feature = "server")]
pub mod kms;
#[cfg(feature = "server")]
pub mod server;
#[cfg(feature = "server")]
pub mod vault;

pub mod vault_config;

#[cfg(feature = "vault-session")]
pub mod vault_session;
