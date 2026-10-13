// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The wire protocol: frames of at most [`MAX_FRAME`] bytes, each a four-byte big-endian length
//! then a `postcard` value. The client sends a [`Hello`] and reads a [`Welcome`]; then any number
//! of [`Request`]s, each answered by one [`Response`], in turn.
//!
//! Nothing here formats a secret: a token, a message or a blinding has no `Debug` that shows it
//! (`REQ-AU-8`).

use std::io::{Read, Write};

use serde::{Deserialize, Serialize};

/// The protocol's version: a [`Hello`] of another is refused.
pub const VERSION: u16 = 1;

/// The largest frame either side accepts: well above a kernel transfer's signing payload or a
/// package's signing bytes, well below anything that would let a peer exhaust memory.
pub const MAX_FRAME: usize = 1024 * 1024;

/// What a key signs with, and so which [`Op`]s it answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Scheme {
    /// The key-ring root: [`Op::KeyRing`] (`DEC-3`, `F-001`).
    KeyRing,
    /// The clerk key: [`Op::Sign`], CipherClerk's Schnorr over Ristretto (`F-017`).
    Clerk,
    /// An Ed25519 key: [`Op::Sign`].
    Ed25519,
    /// A secp256k1 key: [`Op::SignDigest`], a recoverable ECDSA signature.
    Secp256k1,
}

/// The first frame of a connection.
#[derive(Clone, Serialize, Deserialize)]
pub struct Hello {
    /// [`VERSION`].
    pub version: u16,
    /// The caller's name, as the signer's configuration lists it.
    pub client: String,
    /// The caller's token. Never formatted.
    pub token: Vec<u8>,
}

impl core::fmt::Debug for Hello {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Hello")
            .field("version", &self.version)
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}

impl Drop for Hello {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.token);
    }
}

/// One key the caller may use, with its public half.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyInfo {
    /// The key's identifier in the signer's configuration.
    pub id: String,
    /// Its scheme.
    pub scheme: Scheme,
    /// Its public half: the registrar's key for a key ring, a compressed Ristretto point for the
    /// clerk, 32 bytes for Ed25519, a compressed SEC1 point (33 bytes) for secp256k1.
    pub public: Vec<u8>,
}

/// The answer to a [`Hello`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Welcome {
    /// Authenticated: the keys this caller is granted.
    Ok {
        /// The keys.
        keys: Vec<KeyInfo>,
    },
    /// Not authenticated. The connection is closed.
    Refused(Refusal),
}

/// What a request asks of a key.
#[derive(Clone, Serialize, Deserialize)]
pub enum Op {
    /// The key's public half ([`KeyInfo::public`]).
    Public,
    /// A key ring's answer to an encoded [`ledger_kernel::keys::KeyRequest`]; the answer is an
    /// encoded [`ledger_kernel::keys::KeyAnswer`].
    KeyRing(Vec<u8>),
    /// A signature over these bytes: 64 bytes.
    Sign(Vec<u8>),
    /// A recoverable ECDSA signature over this digest: `r ‖ s ‖ v`, 65 bytes, `s` low.
    SignDigest([u8; 32]),
}

impl Op {
    /// Its name, for an audit line: never its bytes.
    pub fn kind(&self) -> &'static str {
        match self {
            Op::Public => "public",
            Op::KeyRing(r) => ledger_kernel::keys::KeyRequest::decode(r)
                .map(|r| r.kind())
                .unwrap_or("key_ring"),
            Op::Sign(_) => "sign",
            Op::SignDigest(_) => "sign_digest",
        }
    }
}

impl core::fmt::Debug for Op {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.kind())
    }
}

/// One request.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    /// The key's identifier.
    pub key: String,
    /// What is asked of it.
    pub op: Op,
}

/// Why the signer did not answer. Never carries key material.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Refusal {
    /// The peer's user id, the caller's name or its token is not one the signer knows.
    Unauthenticated,
    /// No key has this identifier.
    UnknownKey,
    /// The caller is not granted this key.
    NotGranted,
    /// The key does not answer this operation.
    WrongScheme,
    /// The request could not be read.
    Malformed,
    /// The message is over the backend's limit.
    TooLarge,
    /// The backend did not answer (a KMS that is unreachable or throttles). Worth a retry.
    Unavailable(String),
}

impl core::fmt::Display for Refusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Refusal::Unauthenticated => f.write_str("not authenticated"),
            Refusal::UnknownKey => f.write_str("no such key"),
            Refusal::NotGranted => f.write_str("the caller is not granted this key"),
            Refusal::WrongScheme => f.write_str("the key does not answer this operation"),
            Refusal::Malformed => f.write_str("the request could not be read"),
            Refusal::TooLarge => f.write_str("the message is over the backend's limit"),
            Refusal::Unavailable(e) => write!(f, "the backend is unavailable: {e}"),
        }
    }
}

/// The answer to a [`Request`].
#[derive(Clone, Serialize, Deserialize)]
pub enum Response {
    /// The answer's bytes (see [`Op`]).
    Ok(Vec<u8>),
    /// Refused.
    Refused(Refusal),
}

impl core::fmt::Debug for Response {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Response::Ok(_) => f.write_str("Ok(<bytes>)"),
            Response::Refused(r) => write!(f, "Refused({r:?})"),
        }
    }
}

/// Encode `v` as one frame.
pub fn frame<T: Serialize>(v: &T) -> std::io::Result<Vec<u8>> {
    let body = postcard::to_stdvec(v).map_err(std::io::Error::other)?;
    if body.len() > MAX_FRAME {
        return Err(std::io::Error::other("a frame over the limit"));
    }
    let mut out = (body.len() as u32).to_be_bytes().to_vec();
    out.extend_from_slice(&body);
    Ok(out)
}

/// Decode one frame's body.
pub fn unframe<T: for<'de> Deserialize<'de>>(body: &[u8]) -> std::io::Result<T> {
    postcard::from_bytes(body).map_err(std::io::Error::other)
}

/// Write `v` as one frame (blocking).
pub fn write<T: Serialize, W: Write>(w: &mut W, v: &T) -> std::io::Result<()> {
    w.write_all(&frame(v)?)?;
    w.flush()
}

/// Read one frame (blocking).
pub fn read<T: for<'de> Deserialize<'de>, R: Read>(r: &mut R) -> std::io::Result<T> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(std::io::Error::other("a frame over the limit"));
    }
    let mut body = zeroize::Zeroizing::new(vec![0u8; len]);
    r.read_exact(&mut body)?;
    unframe(&body)
}
