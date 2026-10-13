// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `aledgent-sidecar`: aledgent as a CI service container, book only, in memory, with test-only
//! credentials (the crate docs; `deploy/docker/SIDECAR.md`).
//!
//! ```text
//! aledgent-sidecar [serve]     found the book, seed it, then listen ($ALEDGENT_SIDECAR_LISTEN,
//!                              default 0.0.0.0:8080)
//! aledgent-sidecar health      exit 0 if the sidecar on that port is serving: the container's
//!                              health check (the image has no shell)
//! aledgent-sidecar describe    what a start founds (operator, credential, issuance account),
//!                              the test token, the registry and the service principals with
//!                              their grants (never their tokens), as JSON; serves nothing
//! ```
//!
//! While serving, `GET /sidecar` (no token) answers the same JSON without the token.

use aledgent_sidecar::{
    DEFAULT_ASSETS, DEFAULT_LISTEN, ENV_ASSETS, ENV_EXTRA_TOKENS, ENV_LISTEN, ENV_MAX_LEGS,
    ENV_RECIPIENTS, ENV_RP_ID, ENV_RP_ORIGINS, ENV_SERVICE_PRINCIPALS, ENV_SERVICE_PRINCIPALS_FILE,
    ENV_SYSTEM_ACCOUNTS, Options, Sidecar, health, load_principals, parse_assets, parse_max_legs,
    parse_origins, parse_recipients, parse_system_accounts, parse_tokens, probe_address,
};

fn listen() -> String {
    std::env::var(ENV_LISTEN).unwrap_or_else(|_| DEFAULT_LISTEN.to_owned())
}

/// A variable's value when it is set, an error when it is set but not UTF-8 (never read as
/// unset).
fn set_var(name: &str) -> Result<Option<String>, String> {
    match std::env::var(name) {
        Ok(v) => Ok(Some(v)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!("{name} is not UTF-8")),
    }
}

fn assets() -> Result<Vec<ingress::config::AssetConfig>, String> {
    parse_assets(&std::env::var(ENV_ASSETS).unwrap_or_else(|_| DEFAULT_ASSETS.to_owned()))
        .map_err(|e| format!("{ENV_ASSETS}: {e}"))
}

/// `$ALEDGENT_SIDECAR_EXTRA_TOKENS`, `$ALEDGENT_SIDECAR_MAX_LEGS`,
/// `$ALEDGENT_SIDECAR_RECIPIENTS`, `$ALEDGENT_SIDECAR_SERVICE_PRINCIPALS` or
/// `$ALEDGENT_SIDECAR_SERVICE_PRINCIPALS_FILE` (else the sample configuration),
/// `$ALEDGENT_SIDECAR_SYSTEM_ACCOUNTS`, `$ALEDGENT_SIDECAR_RP_ID` and
/// `$ALEDGENT_SIDECAR_RP_ORIGINS`. No error repeats a token or a secret.
fn options() -> Result<Options, String> {
    let extra_tokens = match std::env::var(ENV_EXTRA_TOKENS) {
        Ok(v) => parse_tokens(&v).map_err(|e| format!("{ENV_EXTRA_TOKENS}: {e}"))?,
        Err(_) => Vec::new(),
    };
    let max_legs = match std::env::var(ENV_MAX_LEGS) {
        Ok(v) if !v.trim().is_empty() => {
            Some(parse_max_legs(&v).map_err(|e| format!("{ENV_MAX_LEGS}: {e}"))?)
        }
        _ => None,
    };
    let recipients = match std::env::var(ENV_RECIPIENTS) {
        Ok(v) => parse_recipients(&v).map_err(|e| format!("{ENV_RECIPIENTS}: {e}"))?,
        Err(_) => Vec::new(),
    };
    let services = load_principals(
        set_var(ENV_SERVICE_PRINCIPALS)?.as_deref(),
        set_var(ENV_SERVICE_PRINCIPALS_FILE)?.as_deref(),
    )?;
    let system_accounts = match std::env::var(ENV_SYSTEM_ACCOUNTS) {
        Ok(v) => parse_system_accounts(&v).map_err(|e| format!("{ENV_SYSTEM_ACCOUNTS}: {e}"))?,
        Err(_) => Vec::new(),
    };
    let rp_id = std::env::var(ENV_RP_ID)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty());
    let rp_origins = std::env::var(ENV_RP_ORIGINS)
        .map(|v| parse_origins(&v))
        .unwrap_or_default();
    Ok(Options {
        extra_tokens,
        max_legs,
        recipients,
        services,
        system_accounts,
        rp_id,
        rp_origins,
    })
}

async fn serve() -> Result<(), String> {
    let sidecar = Sidecar::start_with(assets()?, &options()?).await?;
    let listen = listen();
    let listener = tokio::net::TcpListener::bind(&listen)
        .await
        .map_err(|e| format!("{listen}: {e}"))?;
    let d = sidecar.describe();
    tracing::info!(
        %listen,
        operator = %d["operator"],
        issuance = %d["issuance"],
        assets = %d["assets"],
        max_legs = %d["max_legs"],
        extra_tokens = %d["extra_tokens"],
        recipients = %d["recipient_endpoints"],
        services = %d["service_principals"],
        system_accounts = %d["system_accounts"],
        passkeys = %d["passkeys"],
        "aledgent sidecar ready (book only, in memory, TEST CREDENTIALS)"
    );
    sidecar.serve(listener).await
}

async fn describe() -> Result<(), String> {
    let sidecar = Sidecar::start_with(assets()?, &options()?).await?;
    println!("{}", sidecar.describe());
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
        None | Some("serve") => serve().await,
        Some("health") => probe_address(&listen()).and_then(health),
        Some("describe") => describe().await,
        _ => Err("usage: aledgent-sidecar [serve|health|describe]".into()),
    };
    if let Err(e) = result {
        eprintln!("aledgent-sidecar: {e}");
        std::process::exit(1);
    }
}
