// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Domain-separated blake2b, as CipherClerk computes it: the domain, then each part, hashed in
//! one stream.

use blake2::Digest;
use blake2::digest::consts::{U32, U64};

/// `blake2b-256(domain ‖ parts…)`.
pub fn blake2b_256(domain: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut h = blake2::Blake2b::<U32>::new();
    h.update(domain);
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// `blake2b-512(domain ‖ parts…)`.
pub fn blake2b_512(domain: &[u8], parts: &[&[u8]]) -> [u8; 64] {
    let mut h = blake2::Blake2b::<U64>::new();
    h.update(domain);
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}
