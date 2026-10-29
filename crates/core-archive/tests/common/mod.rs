// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Keys for the seal's tests (`F-044`): a Transit stand-in that holds RSA private keys in the test,
//! as Vault holds them in a deployment, and seal files written from generated keys.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::OnceLock;

use core_archive::age::{FileKey, Identity};
use core_archive::seal::{Recipients, TransitUnwrap};
use core_archive::store::StoreError;
use rsa::pkcs8::{EncodePublicKey as _, LineEnding};
use rsa::{Oaep, RsaPrivateKey};
use sha2::Sha256;

/// Two versions of one Transit key, generated once per test binary (3072 bits, the least the
/// writer takes; production's is rsa-4096).
pub fn transit_keys() -> &'static [RsaPrivateKey; 2] {
    static KEYS: OnceLock<[RsaPrivateKey; 2]> = OnceLock::new();
    KEYS.get_or_init(|| {
        let k = || RsaPrivateKey::new(&mut rand_core::OsRng, 3072).unwrap();
        [k(), k()]
    })
}

/// The SPKI PEM of a key's public half, as Vault's `GET <mount>/keys/<name>` gives it.
pub fn pem(k: &RsaPrivateKey) -> String {
    k.to_public_key().to_public_key_pem(LineEnding::LF).unwrap()
}

/// A seal file's text.
pub fn seal_json(
    key: &str,
    version: u32,
    transit: &RsaPrivateKey,
    recovery: &[&Identity],
) -> String {
    serde_json::json!({
        "transit": { "key": key, "version": version, "public_key": pem(transit) },
        "recovery": recovery.iter().map(|i| i.recipient().to_bech32()).collect::<Vec<_>>(),
    })
    .to_string()
}

/// Recipients sealing to `archive` at `version` (1 or 2) and to `recovery`.
pub fn recipients(version: u32, recovery: &Identity) -> Recipients {
    let k = &transit_keys()[(version - 1) as usize];
    Recipients::from_json(&seal_json("archive", version, k, &[recovery])).unwrap()
}

/// Transit, in the test: the key `archive`, versions 1 and 2. Counts its calls.
#[derive(Default)]
pub struct LocalTransit {
    pub calls: std::sync::atomic::AtomicU64,
    pub without: Option<u32>,
}

impl TransitUnwrap for LocalTransit {
    fn unwrap(&self, key: &str, version: u32, wrapped: &[u8]) -> Result<FileKey, StoreError> {
        self.calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let keys: HashMap<u32, &RsaPrivateKey> = [(1, &transit_keys()[0]), (2, &transit_keys()[1])]
            .into_iter()
            .collect();
        let k = match keys.get(&version) {
            Some(k) if key == "archive" && self.without != Some(version) => *k,
            _ => return Err(StoreError::Sealed(format!("no key {key} v{version}"))),
        };
        let pt = k
            .decrypt(Oaep::new::<Sha256>(), wrapped)
            .map_err(|_| StoreError::Sealed("does not open".into()))?;
        let mut out = zeroize::Zeroizing::new([0u8; 16]);
        out.copy_from_slice(&pt);
        Ok(out)
    }
}
