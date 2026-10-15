// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The stream's mutual TLS (`F-038` §2, `DEC-27` (c)): rustls on ring, the provider the rest of
//! the workspace builds.
//!
//! Ingress presents a server certificate and requires a client certificate; the core host checks
//! ingress's certificate against the same CA and the configured server name. Both chains end at
//! the operator's own CA (`ca`), never the public roots: a peer without a certificate from it gets
//! nothing, as the handshake fails before any frame. Keys and certificates are PEM files named in
//! the configuration (`REQ-AU-8`: never the environment, never a log).

use std::path::PathBuf;
use std::sync::Arc;

use rustls::RootCertStore;
use rustls::server::WebPkiClientVerifier;
use rustls_pki_types::pem::PemObject as _;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use serde::{Deserialize, Serialize};

/// The PEM files of one side's TLS: the operator's CA, this side's certificate chain, and its key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsFiles {
    /// The operator's CA certificate(s): the only roots either side trusts.
    pub ca: PathBuf,
    /// This side's certificate chain.
    pub cert: PathBuf,
    /// This side's private key (PKCS#8, SEC1 or PKCS#1).
    pub key: PathBuf,
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

impl TlsFiles {
    fn roots(&self) -> Result<RootCertStore, String> {
        let mut roots = RootCertStore::empty();
        let certs = CertificateDer::pem_file_iter(&self.ca)
            .map_err(|e| format!("{}: {e}", self.ca.display()))?;
        for c in certs {
            roots
                .add(c.map_err(|e| format!("{}: {e}", self.ca.display()))?)
                .map_err(|e| format!("{}: {e}", self.ca.display()))?;
        }
        if roots.is_empty() {
            return Err(format!("{}: no CA certificate", self.ca.display()));
        }
        Ok(roots)
    }

    fn chain(&self) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), String> {
        let chain = CertificateDer::pem_file_iter(&self.cert)
            .map_err(|e| format!("{}: {e}", self.cert.display()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("{}: {e}", self.cert.display()))?;
        if chain.is_empty() {
            return Err(format!("{}: no certificate", self.cert.display()));
        }
        // The key's own error never carries its bytes.
        let key = PrivateKeyDer::from_pem_file(&self.key)
            .map_err(|_| format!("{}: not a PEM private key", self.key.display()))?;
        Ok((chain, key))
    }

    /// Ingress's acceptor: its certificate, and a client certificate from the CA required.
    pub fn acceptor(&self) -> Result<tokio_rustls::TlsAcceptor, String> {
        let roots = Arc::new(self.roots()?);
        let verifier = WebPkiClientVerifier::builder_with_provider(roots, provider())
            .build()
            .map_err(|e| format!("the stream's client verifier: {e}"))?;
        let (chain, key) = self.chain()?;
        let config = rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_client_cert_verifier(verifier)
            .with_single_cert(chain, key)
            .map_err(|e| format!("{}: {e}", self.cert.display()))?;
        Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
    }

    /// The core host's connector: ingress's certificate checked against the CA, and this side's
    /// certificate presented.
    pub fn connector(&self) -> Result<tokio_rustls::TlsConnector, String> {
        let roots = self.roots()?;
        let (chain, key) = self.chain()?;
        let config = rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_root_certificates(roots)
            .with_client_auth_cert(chain, key)
            .map_err(|e| format!("{}: {e}", self.cert.display()))?;
        Ok(tokio_rustls::TlsConnector::from(Arc::new(config)))
    }
}
