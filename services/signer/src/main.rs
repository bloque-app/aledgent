// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `aledgent-signer` (`F-035`): see the crate documentation.
//!
//! ```text
//! aledgent-signer serve <config.json>     # or ALEDGENT_SIGNER_CONFIG; SIGHUP reloads it
//! aledgent-signer token-sha256 <file>     # the hash a client's token is listed under
//! aledgent-signer public <config.json> <key>  # a key's public half, hex (e.g. the clerk's, for
//!                                         # the contract's attestation key)
//! ```
//!
//! Keys live in Vault (owner ruling `key-custody-opt-in-kms`): each a field of a KV v1 secret,
//! 32 bytes of hex, written by an operator once (generated on a trusted terminal, never echoed).
//! The signer logs in with Vault's AWS IAM auth under its instance or task role and reads them
//! itself; `SIGHUP` reloads the configuration and reads them again.

use std::path::PathBuf;
use std::sync::Arc;

use signer::config::{Config, KeySource};
use signer::server::Server;

/// The backends `config` needs: Vault for `vault` sources, KMS for `kms` ones (opt-in).
async fn backends(config: &Config) -> Result<signer::backend::Backends, String> {
    let mut b = signer::backend::Backends::default();
    let wants = |f: fn(&KeySource) -> bool| config.keys.iter().any(|k| f(&k.source));
    if wants(|s| matches!(s, KeySource::Vault { .. })) {
        #[cfg(feature = "vault")]
        if let Some(v) = &config.vault {
            b.secrets = Some(Arc::new(signer::vault::Vault::new(v)?));
        }
    }
    if wants(|s| matches!(s, KeySource::Kms { .. })) {
        #[cfg(feature = "kms")]
        {
            b.kms = Some(Arc::new(
                signer::kms::AwsKms::from_env(config.aws_region.clone()).await,
            ));
        }
    }
    Ok(b)
}

async fn serve(path: PathBuf) -> Result<(), String> {
    let config = Config::load(&path)?;
    let server = Server::load(&config, backends(&config).await?).await?;
    let listener = server
        .bind()
        .map_err(|e| format!("{}: {e}", config.listen.display()))?;
    tracing::info!(socket = %config.listen.display(), "serving");
    let serving = tokio::spawn(Arc::clone(&server).serve(listener));
    let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .map_err(|e| e.to_string())?;
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|e| e.to_string())?;
    let mut minute = tokio::time::interval(std::time::Duration::from_secs(60));
    loop {
        tokio::select! {
            _ = hup.recv() => {
                // A rotation: the file is re-read whole; if it does not load, the old one stays.
                match Config::load(&path) {
                    Ok(c) => {
                        if let Err(e) = server.reload(&c).await {
                            tracing::error!("reload refused: {e}");
                        }
                    }
                    Err(e) => tracing::error!("reload refused: {e}"),
                }
            }
            _ = minute.tick() => server.log_counts(),
            _ = term.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    serving.abort();
    let _ = std::fs::remove_file(&config.listen);
    Ok(())
}

async fn public(path: &str, id: &str) -> Result<(), String> {
    let config = Config::load(std::path::Path::new(path))?;
    let key = config
        .keys
        .iter()
        .find(|k| k.id == id)
        .ok_or_else(|| format!("no key {id}"))?;
    let b = backends(&config).await?;
    let key = signer::backend::Key::load(key, &b).await?;
    println!("{}", hex::encode(&key.public));
    Ok(())
}

fn token_sha256(path: &str) -> Result<(), String> {
    use sha2::Digest as _;
    let token =
        signer::client::Token::from_file(std::path::Path::new(path)).map_err(|e| e.to_string())?;
    println!("{}", hex::encode(sha2::Sha256::digest(token.bytes())));
    Ok(())
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let args: Vec<String> = std::env::args().collect();
    let result = match args.get(1).map(String::as_str) {
        Some("serve") => match args
            .get(2)
            .cloned()
            .or_else(|| std::env::var("ALEDGENT_SIGNER_CONFIG").ok())
        {
            Some(p) => serve(PathBuf::from(p)).await,
            None => Err("usage: aledgent-signer serve <config.json>".into()),
        },
        Some("public") => match (args.get(2), args.get(3)) {
            (Some(c), Some(k)) => public(c, k).await,
            _ => Err("usage: aledgent-signer public <config.json> <key>".into()),
        },
        Some("token-sha256") => match args.get(2) {
            Some(p) => token_sha256(p),
            None => Err("usage: aledgent-signer token-sha256 <file>".into()),
        },
        _ => Err(
            "usage: aledgent-signer serve <config.json> | token-sha256 <file> | public <config.json> <key>"
                .into(),
        ),
    };
    if let Err(e) = result {
        eprintln!("aledgent-signer: {e}");
        std::process::exit(1);
    }
}
