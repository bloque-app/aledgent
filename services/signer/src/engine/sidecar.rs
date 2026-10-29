// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The sidecar engine (`F-042`): the signer service on its Unix socket, through [`SignerClient`]. The
//! keys it offers are the ones the signer grants this caller, so what it can answer is known only when
//! a key is opened ([`SidecarEngine::handle`]).

use std::sync::Arc;

use serde::Deserialize;

use super::{Engine, EngineKind, KeyHandle};
use crate::client::{ClientError, SignerClient, SignerEndpoint};
use crate::proto::{Op, Scheme};

const ALL: [Scheme; 4] = [
    Scheme::KeyRing,
    Scheme::Clerk,
    Scheme::Ed25519,
    Scheme::Secp256k1,
];

/// The locator of a sidecar key: its identifier in the signer's configuration.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Locator {
    /// The key's identifier.
    pub id: String,
}

impl Locator {
    /// Read a locator from its JSON.
    pub fn parse(v: &serde_json::Value) -> Result<Locator, String> {
        let l = Locator::deserialize(v)
            .map_err(|e| format!("a sidecar key is {{\"id\": \"<key id>\"}}: {e}"))?;
        if l.id.is_empty() {
            return Err("a sidecar key has an id".into());
        }
        Ok(l)
    }
}

/// The signer service (module documentation).
pub struct SidecarEngine {
    client: Arc<SignerClient>,
}

impl SidecarEngine {
    /// An engine over a connection already made.
    pub fn new(client: Arc<SignerClient>) -> SidecarEngine {
        SidecarEngine { client }
    }

    /// Connect to the signer `endpoint` names and authenticate. Fails if the signer is not there or
    /// refuses: a process that needs its keys does not start without them.
    pub fn connect(endpoint: &SignerEndpoint) -> Result<SidecarEngine, ClientError> {
        Ok(SidecarEngine::new(Arc::new(SignerClient::connect(
            endpoint,
        )?)))
    }

    /// The key `id`, which must be of `scheme` and granted to this caller. The public half is the one
    /// the signer listed at the handshake.
    pub fn handle(&self, id: &str, scheme: Scheme) -> Result<Arc<SidecarKey>, ClientError> {
        let info = self.client.key_of(id, scheme)?;
        Ok(Arc::new(SidecarKey {
            client: Arc::clone(&self.client),
            id: id.to_owned(),
            scheme,
            public: info.public,
        }))
    }
}

impl Engine for SidecarEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::Sidecar
    }

    fn schemes(&self) -> &[Scheme] {
        &ALL
    }

    fn open(
        &self,
        scheme: Scheme,
        locator: &serde_json::Value,
    ) -> Result<Arc<dyn KeyHandle>, ClientError> {
        let l = Locator::parse(locator).map_err(ClientError::Config)?;
        Ok(self.handle(&l.id, scheme)? as Arc<dyn KeyHandle>)
    }
}

/// An open key of the signer.
pub struct SidecarKey {
    client: Arc<SignerClient>,
    id: String,
    scheme: Scheme,
    public: Vec<u8>,
}

impl core::fmt::Debug for SidecarKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SidecarKey")
            .field("id", &self.id)
            .field("scheme", &self.scheme)
            .finish_non_exhaustive()
    }
}

impl KeyHandle for SidecarKey {
    fn kind(&self) -> EngineKind {
        EngineKind::Sidecar
    }

    fn scheme(&self) -> Scheme {
        self.scheme
    }

    fn public(&self) -> Vec<u8> {
        self.public.clone()
    }

    fn call(&self, op: &Op) -> Result<Vec<u8>, ClientError> {
        self.client.call(&self.id, op.clone())
    }
}
