// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A signer's configuration for the tests: every scheme, from files (development keys).

#![allow(dead_code)]

use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use sha2::Digest as _;

pub const ROOT: [u8; 32] = [0x5A; 32];
/// A canonical scalar: the clerk key.
pub const CLERK: [u8; 32] = [0x07; 32];
pub const AUTHORITY: [u8; 32] = [0x72; 32];
pub const LANE: [u8; 32] = [0x11; 32];

/// This process's user id: the owner of a file it just made.
pub fn my_uid(dir: &Path) -> u32 {
    let p = dir.join(".uid");
    std::fs::write(&p, b"").unwrap();
    std::fs::metadata(&p).unwrap().uid()
}

/// Write a token of `fill` to `dir/<name>.token` and return its path and SHA-256.
pub fn token(dir: &Path, name: &str, fill: u8) -> (PathBuf, String) {
    let t = hex::encode([fill; 32]);
    let p = dir.join(format!("{name}.token"));
    std::fs::write(&p, format!("{t}\n")).unwrap();
    (p, hex::encode(sha2::Sha256::digest(t.as_bytes())))
}

pub fn secret_file(dir: &Path, name: &str, secret: [u8; 32]) -> PathBuf {
    let p = dir.join(format!("{name}.hex"));
    std::fs::write(&p, hex::encode(secret)).unwrap();
    p
}

/// A development configuration: the core holds `keyring`, `clerk` and `agent-authority`; the
/// adapter holds `lane-1`. Every key from a file.
pub fn dev_config(dir: &Path, uid: u32, core_hash: &str, settle_hash: &str) -> Value {
    json!({
        "listen": dir.join("signer.sock"),
        "development": true,
        "clients": [
            { "name": "core", "uids": [uid], "token_sha256": [core_hash],
              "keys": ["keyring", "clerk", "agent-authority"] },
            { "name": "settle", "uids": [uid], "token_sha256": [settle_hash],
              "keys": ["lane-1"] }
        ],
        "keys": [
            { "id": "keyring", "scheme": "key_ring", "source": { "file": { "path": secret_file(dir, "root", ROOT) } } },
            { "id": "clerk", "scheme": "clerk", "source": { "file": { "path": secret_file(dir, "clerk", CLERK) } } },
            { "id": "agent-authority", "scheme": "ed25519", "source": { "file": { "path": secret_file(dir, "authority", AUTHORITY) } } },
            { "id": "lane-1", "scheme": "secp256k1", "source": { "file": { "path": secret_file(dir, "lane", LANE) } } }
        ]
    })
}

pub mod suite;
