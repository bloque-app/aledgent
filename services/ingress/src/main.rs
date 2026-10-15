// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `aledgent-ingress`: the one process every command and query reaches aledgent through
//! (`F-009`).
//!
//! ```text
//! aledgent-ingress serve --config <file>       run a replica
//! aledgent-ingress openapi                     print the OpenAPI 3.1 document
//! aledgent-ingress founded --config <file>     print what genesis founds (the operator's
//!                                              identity and credential, the issuance account):
//!                                              what the operator's entry in `tokens` names
//! aledgent-ingress clerk-public                the public half of ALEDGENT_CLERK_KEY: the
//!                                              settlement contract's attestation key
//! aledgent-ingress account-ref <account>       the account reference a deposit to that ledger
//!                                              account names on the settlement layer (REQ-SL-7)
//! aledgent-ingress recipient --config <file> --principal <did> --endpoint <url> --secret-env <VAR>
//!                                              register a recipient; the secret is read from VAR
//! aledgent-ingress migrate --config <file> --status
//!                                              every migration: applied/ok, applied/backfillable,
//!                                              pending, mismatch or applied/unknown (T-009-26);
//!                                              exits non-zero on a mismatch
//! aledgent-ingress migrate --config <file> --apply
//!                                              apply the pending migrations and exit
//! aledgent-ingress health --config <file>      exit 0 when the replica at the configuration's
//!                                              `listen` answers HTTP: the image's HEALTHCHECK
//! ```
//!
//! `serve` applies pending migrations at start unless the configuration's `migrations.on_start`
//! is `"verify"`, under which it refuses to start while one is pending or differs.
//!
//! Secrets come from the environment only, and none has a default: the PostgreSQL URL
//! (`ALEDGENT_INGRESS_DATABASE_URL`), the recipients' sealing key (`ALEDGENT_INGRESS_SEALING_KEY`)
//! and the in-process core's key-ring root (`ALEDGENT_CORE_ROOT`). A missing one stops the
//! process before it serves anything (`REQ-AU-8`).
//!
//! With `core_endpoint` set, the core is the core Agent's host (`F-005`, `aledgent-core-host
//! serve`), reached over a local socket ([`ingress::remote`]); the core's root and clerk key are
//! then that process's, and ingress reads neither.
//!
//! With `stream.listen` set (`F-038`, `DEC-27`), ingress runs apart from every core: it listens
//! for the core hosts that dial out to it ([`ingress::stream`]), acknowledges with no core
//! connected, and signs every forwarded command with `stream.feed_key` (the signer service). On
//! `SIGTERM` (or `SIGINT`) it drains: `GET /ready` turns 503, the feed in flight finishes, the
//! feed lease is released, every core gets a go-away, and clients are answered for
//! `stream.drain_ms` before the process exits. `readiness_listen` serves `/ready` and `/live` on
//! a port of its own ([`ingress::readiness`]).

use std::sync::Arc;

use ingress::auth::Authenticator;
use ingress::boundary::Core;
use ingress::clock::{SharedClock, SystemClock};
use ingress::config::{
    DeploymentConfig, ENV_AUTHN_KEY, ENV_CLERK_KEY, ENV_CORE_LINK_TOKEN, ENV_CORE_ROOT,
    ENV_DATABASE_URL, ENV_SEALING_KEY, secret_hex32,
};
use ingress::inprocess::{BOOK_CACHE_BYTES, FileJournal, InProcessCore};
use ingress::remote::{Endpoint, LinkToken, RemoteCore};
use ingress::service::SharedFeedSigner;
use ingress::store::Store;
use ingress::store::pg::{PgOptions, PgStore};
use ingress::stream::StreamCore;
use ingress::{delivery, openapi, readiness, service, store, surface};

fn arg(args: &[String], name: &str) -> Result<String, String> {
    args.windows(2)
        .find(|w| w[0] == name)
        .map(|w| w[1].clone())
        .ok_or_else(|| format!("missing {name}"))
}

fn config(args: &[String]) -> Result<DeploymentConfig, String> {
    let path = arg(args, "--config")?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))
}

fn database_url() -> Result<String, String> {
    std::env::var(ENV_DATABASE_URL)
        .map_err(|_| format!("{ENV_DATABASE_URL} is not set: refusing to start"))
}

async fn connect(c: &DeploymentConfig) -> Result<PgStore, String> {
    PgStore::connect_with(
        PgOptions {
            url: database_url()?,
            sealing_key: secret_hex32(ENV_SEALING_KEY)?,
            require_sync_standby: c.require_sync_standby,
            profile: c.profile,
            pool_size: 16,
        },
        &c.migrations,
    )
    .await
    .map_err(|e| e.to_string())
}

/// T-009-26: `migrate --status` or `migrate --apply`, with `serve`'s configuration and database
/// URL. Needs no sealing key: it reads and changes the schema only.
async fn migrate(args: &[String]) -> Result<(), String> {
    use ingress::store::migrate::{self, MIGRATIONS};
    let c = config(args)?;
    let lock_timeout = c.migrations.lock_timeout()?;
    let flag = |f: &str| args.iter().any(|a| a == f);
    let (status, apply) = (flag("--status"), flag("--apply"));
    if status == apply {
        return Err("usage: aledgent-ingress migrate --config <file> --status|--apply".into());
    }
    let mut client = migrate::connect(&database_url()?, c.profile)
        .await
        .map_err(|e| e.to_string())?;
    if apply {
        let done = migrate::apply(&mut client, MIGRATIONS, lock_timeout)
            .await
            .map_err(|e| e.to_string())?;
        println!(
            "applied {:?}, checksums backfilled {:?}",
            done.applied, done.backfilled
        );
    }
    let s = migrate::status(&client, MIGRATIONS)
        .await
        .map_err(|e| e.to_string())?;
    print!("{s}");
    // A script reading the exit status sees an edited migration without parsing the table.
    let mismatched = s.mismatched();
    if !mismatched.is_empty() {
        return Err(format!(
            "migrations {mismatched:?} differ from what was applied (state mismatch)"
        ));
    }
    Ok(())
}

/// The image's health check (deploy/docker/services.Dockerfile): the replica at `listen` answers
/// `GET /openapi.json` with anything below 500. It binds its listener only once the store is
/// connected, the core started and the replica running, so an answer means ready; with no
/// credential the document is refused (401), which is an answer all the same. Needs no secret.
fn health(args: &[String]) -> Result<(), String> {
    let c = config(args)?;
    let addr = ingress::probe::address(&c.listen)?;
    match ingress::probe::status(addr, "/openapi.json", std::time::Duration::from_secs(5))? {
        code if code < 500 => Ok(()),
        code => Err(format!("{addr}: GET /openapi.json answered {code}")),
    }
}

/// How long ingress waits for the core in another process to answer one request. A decision
/// through the core Agent takes seconds today (`notes/MEASUREMENTS-F005.md`).
const REMOTE_CORE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

async fn serve(args: &[String]) -> Result<(), String> {
    let c = config(args)?;
    if let Some(s) = &c.stream {
        s.check(c.profile)?;
    }
    // F-038 §5, §12: in production, what goes to a core in another process is signed.
    c.check_forward_signing()?;
    let store = connect(&c).await?;
    // F-038: the stream, when ingress listens for cores; the socket link otherwise.
    if let Some(s) = c.stream.clone().filter(|s| s.listen.is_some()) {
        let signer = s.feed_signer(c.profile)?;
        let listen = s.bind().await?.ok_or("stream.listen")?;
        let core = StreamCore::new(&c.replica, s.timings());
        let hub = core.clone();
        tokio::spawn(async move {
            if let Err(e) = hub.serve(listen).await {
                tracing::error!(error = %e, "the core stream's listener stopped");
            }
        });
        let drain = s.drain();
        return run(c, store, core.clone(), signer, Some((core, drain))).await;
    }
    // The socket link signs too once a feed key is configured: the transition (`F-038` §12).
    let signer = match &c.stream {
        Some(s) => s.feed_signer(c.profile)?,
        None => None,
    };
    if let Some(e) = &c.core_endpoint {
        let endpoint = Endpoint::parse(e)
            .ok_or_else(|| format!("core_endpoint {e}: unix:<path> or tcp:<addr>"))?;
        // F-035: the core link is authenticated; no token, no start.
        if c.core_link.is_none() {
            return Err("core_endpoint needs core_link (F-035): refusing to start".into());
        }
        let token = std::env::var(ENV_CORE_LINK_TOKEN)
            .map_err(|_| format!("{ENV_CORE_LINK_TOKEN} is not set: refusing to start"))
            .and_then(|t| LinkToken::new(&t))?;
        return run(
            c,
            store,
            RemoteCore::new(endpoint, REMOTE_CORE_TIMEOUT, token),
            signer,
            None,
        )
        .await;
    }
    // F-035: the in-process core holds the key-ring root in this process, which guardrail 13
    // rules out for production: development, the tests and the stress suite only.
    tracing::warn!(
        "the in-process core holds the key-ring root: development only (NFR-8); production runs \
         the core host over the signer service"
    );
    let genesis = c.genesis.core_genesis(secret_hex32(ENV_CORE_ROOT)?)?;
    let journal = FileJournal::open(&c.core_journal)?;
    // T-031-14: the book's leaves in memory, unless `core_book` names a file (opt-in).
    match c.core_book_path() {
        Some(path) => {
            let core = InProcessCore::on_disk(genesis, journal, path, BOOK_CACHE_BYTES);
            let core = with_audit(with_clerk(core)?, &c)?;
            core.start()?;
            run(c, store, core, None, None).await
        }
        None => {
            let core = with_audit(with_clerk(InProcessCore::in_memory(genesis, journal))?, &c)?;
            core.start()?;
            run(c, store, core, None, None).await
        }
    }
}

/// T-013-13: ship the audit trail to the configured audit store, which must live apart from the
/// core's journal. Without one, nothing ships, and `F-018` pages (`AledgentAuditNotShipped`).
fn with_audit<S, V>(
    core: InProcessCore<S, V>,
    c: &DeploymentConfig,
) -> Result<InProcessCore<S, V>, String>
where
    S: ledger_kernel::store::LeafStore + Send + Sync + 'static,
    V: ledger_kernel::vault::VaultBackend + Send + Sync + 'static,
{
    let Some(section) = &c.audit_store else {
        eprintln!(
            "aledgent-ingress: WARNING: no audit_store is configured; the audit trail is not shipped \
             (REQ-AT-2, NFR-6)"
        );
        return Ok(core);
    };
    let journal = std::path::Path::new(&c.core_journal);
    let book_dir = journal
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let sink = section.open(book_dir)?;
    core.with_audit(Box::new(sink), section.max_unshipped())
}

/// F-017: the clerk key, a secret like the core's root: read once, never logged.
fn with_clerk<S, V>(core: InProcessCore<S, V>) -> Result<InProcessCore<S, V>, String>
where
    S: ledger_kernel::store::LeafStore + Send + Sync + 'static,
    V: ledger_kernel::vault::VaultBackend + Send + Sync + 'static,
{
    if std::env::var_os(ENV_CLERK_KEY).is_none() {
        return Ok(core);
    }
    let clerk =
        ledger_core::anchor::clerk::InProcessClerk::from_secret(secret_hex32(ENV_CLERK_KEY)?)
            .ok_or_else(|| format!("{ENV_CLERK_KEY} is not a canonical scalar"))?;
    Ok(core.with_clerk(Arc::new(clerk)))
}

/// The signal a replica drains on: `SIGTERM` (an orchestrator's stop) or `SIGINT`.
async fn stop_signal() {
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = term => {}
        _ = tokio::signal::ctrl_c() => {}
    }
}

async fn run<C: Core + Clone>(
    c: DeploymentConfig,
    store: PgStore,
    core: C,
    feed_signer: Option<SharedFeedSigner>,
    stream: Option<(StreamCore, std::time::Duration)>,
) -> Result<(), String> {
    let auth = Authenticator::new();
    auth.allow_holders(c.allow_holder_credentials);
    // T-031-13: the API-key cache's TTL, never above NFR-20's bound.
    if let Some(ms) = c.api_key_cache_ms {
        auth.verifiers()
            .set_ttl(std::time::Duration::from_millis(ms))?;
    }
    // F-028: passkeys, with their key from the environment; no key, no start (REQ-AU-8).
    if let Some(p) = &c.passkeys {
        let key =
            std::env::var(ENV_AUTHN_KEY).map_err(|_| format!("{ENV_AUTHN_KEY} is not set"))?;
        auth.set_passkeys(p.passkeys(authn::token::AuthnKey::from_hex(&key)?)?);
    }
    for t in &c.tokens {
        auth.add(t)?;
    }
    let transport = delivery::HttpTransport::new(std::time::Duration::from_secs(10))?;
    let clock: SharedClock = Arc::new(SystemClock);
    // REQ-RC-7: the archived identifiers, checked against their digest before anything serves.
    let mut service = c.service();
    service.archived = Arc::new(c.archived()?);
    if !service.archived.is_empty() {
        tracing::info!(
            archived = service.archived.len(),
            "archived identifiers answer ERR-IdentifierArchived"
        );
    }
    // F-038: every forwarded command signed, on either link, once a feed key is configured.
    service.feed_signer = feed_signer;
    let replica = service::Replica::start(store, core, clock, auth, Arc::new(transport), service);
    let app = surface::router(Arc::clone(&replica.shared));
    // F-038 §10: readiness on a port of its own, apart from the client surface.
    if let Some(addr) = &c.readiness_listen {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .map_err(|e| format!("readiness_listen {addr}: {e}"))?;
        let router = readiness::router(Arc::clone(&replica.shared));
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, router).await {
                tracing::error!(error = %e, "the readiness listener stopped");
            }
        });
    }
    let listener = tokio::net::TcpListener::bind(&c.listen)
        .await
        .map_err(|e| format!("{}: {e}", c.listen))?;
    tracing::info!(listen = %c.listen, replica = %c.replica, "serving");
    let shared = Arc::clone(&replica.shared);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            stop_signal().await;
            // F-038 §9: not ready, the feed in flight finished, the lease released, every core
            // sent away; clients answered meanwhile, then for the drain period.
            let drain = stream
                .as_ref()
                .map_or(ingress::config::DEFAULT_DRAIN, |(_, d)| *d);
            tracing::info!(?drain, "draining");
            service::drain(&shared, drain).await;
            if let Some((hub, _)) = &stream {
                hub.go_away("draining");
                tokio::time::sleep(drain).await;
            }
        })
        .await
        .map_err(|e| e.to_string())
}

async fn recipient(args: &[String]) -> Result<(), String> {
    let c = config(args)?;
    let s = connect(&c).await?;
    let recipient = store::Recipient::with_secret_env(
        arg(args, "--principal")?,
        arg(args, "--endpoint")?,
        &arg(args, "--secret-env")?,
        |v| std::env::var(v).ok(),
    )?;
    s.put_recipient(recipient).await.map_err(|e| e.to_string())
}

/// What genesis founds, as JSON: public identifiers only, read from the file (no secret).
fn founded(args: &[String]) -> Result<(), String> {
    let f = config(args)?.genesis.founded()?;
    println!(
        "{}",
        serde_json::json!({
            "operator": f.operator.to_did(),
            "operator_credential": f.operator_credential.to_did(),
            "issuance": f.issuance.to_did(),
        })
    );
    Ok(())
}

/// The clerk key's public half, hex (`F-017`): what the settlement contract checks anchors
/// against (its attestation key). The secret is read from the environment and never printed.
fn clerk_public() -> Result<(), String> {
    use ledger_core::anchor::clerk::ClerkSigner as _;
    let clerk =
        ledger_core::anchor::clerk::InProcessClerk::from_secret(secret_hex32(ENV_CLERK_KEY)?)
            .ok_or_else(|| format!("{ENV_CLERK_KEY} is not a canonical scalar"))?;
    println!("{}", hex::encode(clerk.public_key()));
    Ok(())
}

/// The opaque account reference of a ledger account (`REQ-SL-7`), hex with `0x`: what a
/// depositor passes to the settlement contract's `deposit`. Public, derived from the account's
/// identifier alone.
fn account_ref(args: &[String]) -> Result<(), String> {
    let account = args
        .get(2)
        .ok_or("usage: aledgent-ingress account-ref <did:aledgent:acct:…>")?;
    let id = ledger_core::id::AccountId::parse(account)
        .map_err(|e| format!("{account}: not a ledger account ({e:?})"))?;
    println!("0x{}", hex::encode(ledger_core::id::account_reference(&id)));
    Ok(())
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args: Vec<String> = std::env::args().collect();
    let result = match args.get(1).map(String::as_str) {
        Some("serve") => serve(&args).await,
        Some("openapi") => {
            print!("{}", openapi::document_text());
            Ok(())
        }
        Some("recipient") => recipient(&args).await,
        Some("migrate") => migrate(&args).await,
        Some("founded") => founded(&args),
        Some("clerk-public") => clerk_public(),
        Some("account-ref") => account_ref(&args),
        Some("health") => health(&args),
        _ => Err(
            "usage: aledgent-ingress serve|openapi|recipient|migrate|founded|clerk-public|\
             account-ref|health (see the crate docs)"
                .into(),
        ),
    };
    if let Err(e) = result {
        eprintln!("aledgent-ingress: {e}");
        std::process::exit(1);
    }
}
