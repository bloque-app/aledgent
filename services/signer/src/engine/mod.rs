// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Signature engines (`F-042`): one interface to every way of holding a key and signing with it.
//!
//! A component asks for a key by **scheme** ([`crate::proto::Scheme`]) and gets a [`KeyHandle`], whatever
//! holds the key: the memory of its own process ([`in_process`]), the signer service on its socket
//! ([`sidecar`]), or an API provider such as Vault Transit (`crate::transit`, feature `transit`). A handle answers the
//! calls the signer's wire protocol carries ([`crate::proto::Op`]), bytes in and bytes out, and has no
//! way to be asked for its secret.
//!
//! - [`config`]: how a component's configuration names an engine for each key, and the checks that
//!   refuse a key whose engine lacks its scheme, or is of a kind the key may not use.
//! - Errors are [`ClientError`], with its `is_transient()`; an engine maps its own failures onto it.
//! - The interface is blocking. An engine that is async inside keeps a thread of its own.
//! - A handle of an API provider is used through `try_*` and never under a patience that stops the
//!   process ([`crate::client::Patience`]).
//!
//! The design, its rulings and what is out of scope: `features/F-042-signature-engines/plan.md`.

use std::sync::Arc;

use crate::client::ClientError;
use crate::proto::{Op, Scheme};

pub mod config;
pub mod in_process;
pub mod sidecar;

/// Where an engine's keys are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EngineKind {
    /// In the memory of the process that signs, read from a file.
    InProcess,
    /// In the operating system's keystore, or a local agent. Supported by the interface; no engine
    /// of this kind is built.
    InSystem,
    /// In another process on the same host: the signer service.
    Sidecar,
    /// In a service called over the network: Vault Transit, and later others.
    ApiProvider,
}

impl EngineKind {
    /// Its name, as an error message says it.
    pub fn as_str(self) -> &'static str {
        match self {
            EngineKind::InProcess => "in_process",
            EngineKind::InSystem => "in_system",
            EngineKind::Sidecar => "sidecar",
            EngineKind::ApiProvider => "api_provider",
        }
    }
}

/// Whether an engine can sign now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Health {
    /// Ready, or not.
    pub ready: bool,
    /// Why not, in words; empty when ready.
    pub detail: String,
}

impl Health {
    /// Ready.
    pub fn ready() -> Health {
        Health {
            ready: true,
            detail: String::new(),
        }
    }

    /// Not ready, for `why`.
    pub fn not_ready(why: impl Into<String>) -> Health {
        Health {
            ready: false,
            detail: why.into(),
        }
    }
}

/// One way of holding keys and signing with them (module documentation).
pub trait Engine: Send + Sync {
    /// Its kind.
    fn kind(&self) -> EngineKind;

    /// The schemes it can answer at all. A list per engine, so an engine for a provider can answer for
    /// what that provider has. What a given sidecar or provider actually holds is known only at
    /// [`Engine::open`].
    fn schemes(&self) -> &[Scheme];

    /// Find the key `locator` names and check it now: that it exists, is of `scheme`, and cannot
    /// leave. A component that needs a key does not start without it.
    fn open(
        &self,
        scheme: Scheme,
        locator: &serde_json::Value,
    ) -> Result<Arc<dyn KeyHandle>, ClientError>;

    /// Whether the engine can sign now. No I/O. Ready unless said otherwise.
    fn health(&self) -> Health {
        Health::ready()
    }

    /// The engine's own metrics, as Prometheus text. None unless said otherwise.
    fn metrics_text(&self) -> String {
        String::new()
    }
}

/// One open key. There is no way to ask it for its secret, and its `Debug` shows no more than its
/// identifier, scheme and kind.
pub trait KeyHandle: Send + Sync + core::fmt::Debug {
    /// The kind of engine it came from.
    fn kind(&self) -> EngineKind;

    /// Its scheme.
    fn scheme(&self) -> Scheme;

    /// Its public half, in the scheme's encoding (32 bytes for Ed25519, 33 for secp256k1, ...).
    fn public(&self) -> Vec<u8>;

    /// The call the signer's wire protocol carries: bytes in, bytes out.
    fn call(&self, op: &Op) -> Result<Vec<u8>, ClientError>;
}
