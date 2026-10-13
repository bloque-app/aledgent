// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The principals Agent's state (`DEC-22`, ruling `split-principals`, `c-10`): the registry of
//! accounts, the grants, the revocations, and the feed every shard applies.
//!
//! Every change takes the next **version** and is appended to the feed under it. Ingress reads a
//! caller's authority here and stamps the command with the version it read at; a shard applies
//! the feed in version order (`crate::decide::apply_feed`) and refuses a command stamped before
//! a revocation it has applied. The time from a revocation's commit here to its application in
//! every shard is the revocation lag (`NFR-20`).
//!
//! | Row | Value |
//! |---|---|
//! | `P` | the current version |
//! | `e` ‖ account ‖ asset | the account is open, at that version |
//! | `g` ‖ principal ‖ payer | a grant: its limit per operation and the version it was made at |
//! | `x` ‖ principal | the version of the principal's last revocation |
//! | `l` ‖ version | the feed entry of that version |
//! | `D` | the running digest of the feed: `blake2b-256(D ‖ entry)` per entry (amendment 0015) |
//! | `B` ‖ asset | the asset is being bound, since that version (amendment 0015) |

use alloc::vec::Vec;
use cipher_clerk::crypto::blake2b_256;
use serde::{Deserialize, Serialize};

use crate::kv::Kv;
use crate::model::{Account, Change, FeedEntry, Principal};

/// A change asked of the principals Agent.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrincipalsCommand {
    /// Register `account` open in `asset`.
    Open {
        /// The account.
        account: Account,
        /// The asset.
        asset: u32,
    },
    /// Let `principal` debit `payer`, up to `limit` per operation.
    Grant {
        /// Who.
        principal: Principal,
        /// The account.
        payer: Account,
        /// The bound per operation (`REQ-AU-2`): never zero.
        limit: u64,
    },
    /// Revoke every authority of `principal` (`REQ-AU-3`).
    Revoke {
        /// Who.
        principal: Principal,
    },
    /// Begin binding `asset` (`REQ-AS-8`): every shard freezes its issuance (amendment 0015).
    Bind {
        /// The asset.
        asset: u32,
    },
    /// The binding of `asset` is final or refused: its issuance resumes.
    EndBinding {
        /// The asset.
        asset: u32,
    },
}

/// What a principals command came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrincipalsOutcome {
    /// Done, at this version.
    Done(u64),
    /// The account is registered already.
    AlreadyOpen,
    /// A grant without a bound (`REQ-AU-2`).
    Unbounded,
    /// The asset is being bound already.
    AlreadyBinding,
    /// The asset is not being bound.
    NotBinding,
}

/// An authority as ingress reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Authority {
    /// The grant's bound per operation.
    pub limit: u64,
    /// The version it was read at: the command's stamp.
    pub version: u64,
}

/// The principals Agent's state over rows (module documentation).
#[derive(Clone, Debug, Default)]
pub struct Principals<K>(pub K);

fn u64_of(v: Option<Vec<u8>>) -> Option<u64> {
    v.map(|b| {
        let mut x = [0u8; 8];
        x.copy_from_slice(&b[..8]);
        u64::from_be_bytes(x)
    })
}

fn key(tag: u8, parts: &[&[u8]]) -> Vec<u8> {
    let mut k = alloc::vec![tag];
    for p in parts {
        k.extend_from_slice(p);
    }
    k
}

/// The registry's running digest after `entry`, from the digest before it: the principals feed's
/// chain, which a shard recomputes over the entries it applies (R-03, `provenance`).
pub fn feed_link(prev: &[u8; 32], entry: &FeedEntry) -> [u8; 32] {
    feed_link_bytes(
        prev,
        &postcard::to_allocvec(entry).expect("a feed entry encodes"),
    )
}

fn feed_link_bytes(prev: &[u8; 32], bytes: &[u8]) -> [u8; 32] {
    blake2b_256(b"aledgent/registry/feed/v1", &[prev, bytes])
}

impl<K: Kv> Principals<K> {
    /// The current version (0 before any change).
    pub fn version(&self) -> u64 {
        u64_of(self.0.get(b"P")).unwrap_or(0)
    }

    /// Whether `account` is registered open in `asset`.
    pub fn is_open(&self, account: &Account, asset: u32) -> bool {
        self.0
            .get(&key(b'e', &[account, &asset.to_be_bytes()]))
            .is_some()
    }

    /// The running digest of the feed through the current version (zero before any change):
    /// what an anchor commits to for the registry (`INV-19`).
    pub fn digest(&self) -> [u8; 32] {
        self.0
            .get(b"D")
            .map(|b| {
                let mut d = [0u8; 32];
                d.copy_from_slice(&b[..32]);
                d
            })
            .unwrap_or([0; 32])
    }

    /// The version since which `asset` is being bound, if it is.
    pub fn binding(&self, asset: u32) -> Option<u64> {
        u64_of(self.0.get(&key(b'B', &[&asset.to_be_bytes()])))
    }

    fn record(&mut self, change: Change) -> u64 {
        let version = self.version() + 1;
        let entry = FeedEntry { version, change };
        let bytes = postcard::to_allocvec(&entry).expect("a feed entry encodes");
        let digest = feed_link_bytes(&self.digest(), &bytes);
        self.0.put(&key(b'l', &[&version.to_be_bytes()]), bytes);
        self.0.put(b"D", digest.to_vec());
        self.0.put(b"P", version.to_be_bytes().to_vec());
        version
    }

    /// Apply one command.
    pub fn apply(&mut self, c: &PrincipalsCommand) -> PrincipalsOutcome {
        match *c {
            PrincipalsCommand::Open { account, asset } => {
                if self.is_open(&account, asset) {
                    return PrincipalsOutcome::AlreadyOpen;
                }
                let v = self.record(Change::Opened { account, asset });
                self.0.put(
                    &key(b'e', &[&account, &asset.to_be_bytes()]),
                    v.to_be_bytes().to_vec(),
                );
                PrincipalsOutcome::Done(v)
            }
            PrincipalsCommand::Grant {
                principal,
                payer,
                limit,
            } => {
                if limit == 0 {
                    return PrincipalsOutcome::Unbounded;
                }
                let v = self.record(Change::Granted {
                    principal,
                    payer,
                    limit,
                });
                let mut value = limit.to_be_bytes().to_vec();
                value.extend_from_slice(&v.to_be_bytes());
                self.0.put(&key(b'g', &[&principal, &payer]), value);
                PrincipalsOutcome::Done(v)
            }
            PrincipalsCommand::Revoke { principal } => {
                let v = self.record(Change::Revoked { principal });
                self.0
                    .put(&key(b'x', &[&principal]), v.to_be_bytes().to_vec());
                PrincipalsOutcome::Done(v)
            }
            PrincipalsCommand::Bind { asset } => {
                if self.binding(asset).is_some() {
                    return PrincipalsOutcome::AlreadyBinding;
                }
                let v = self.record(Change::Binding { asset });
                self.0.put(
                    &key(b'B', &[&asset.to_be_bytes()]),
                    v.to_be_bytes().to_vec(),
                );
                PrincipalsOutcome::Done(v)
            }
            PrincipalsCommand::EndBinding { asset } => {
                if self.binding(asset).is_none() {
                    return PrincipalsOutcome::NotBinding;
                }
                let v = self.record(Change::BindingEnded { asset });
                self.0.remove(&key(b'B', &[&asset.to_be_bytes()]));
                PrincipalsOutcome::Done(v)
            }
        }
    }

    /// `principal`'s authority to debit `payer`, if it holds one not revoked since.
    pub fn authority(&self, principal: &Principal, payer: &Account) -> Option<Authority> {
        let g = self.0.get(&key(b'g', &[principal, payer]))?;
        let limit = u64_of(Some(g[..8].to_vec()))?;
        let granted = u64_of(Some(g[8..16].to_vec()))?;
        let revoked = u64_of(self.0.get(&key(b'x', &[principal]))).unwrap_or(0);
        (granted > revoked).then(|| Authority {
            limit,
            version: self.version(),
        })
    }

    /// The feed entries after `after`, at most `max`, in version order.
    pub fn feed(&self, after: u64, max: usize) -> Vec<FeedEntry> {
        let last = self.version();
        let mut out = Vec::new();
        let mut v = after + 1;
        while v <= last && out.len() < max {
            if let Some(b) = self.0.get(&key(b'l', &[&v.to_be_bytes()])) {
                out.push(postcard::from_bytes(&b).expect("a feed row decodes"));
            }
            v += 1;
        }
        out
    }
}
