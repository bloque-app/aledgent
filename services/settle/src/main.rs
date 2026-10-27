// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `aledgent-settle`: the settlement adapter's binary (`F-016`).
//!
//! `aledgent-settle <config.json>` (or `ALEDGENT_SETTLE_CONFIG`): a [`settle::config::ProcessConfig`]
//! naming the settlement layer (a chain spec for the light client, by default), ingress, the store
//! and where each lane's key is. It runs one
//! [`settle::Adapter::step`] every `interval` milliseconds until interrupted, and sooner when there
//! is news for it (`wake_poll`, `NFR-15`). Only one instance per
//! set of keys acts at a time: the others wait for the lease.
//!
//! Two operator commands run once, holding the lease, and exit (amendment 0040, `T-043-04`):
//!
//! - `aledgent-settle <config.json> gap <from> <to>`: **the operator's gap** (ruling a40-q3). When
//!   the adapter stopped at position `from` (an `unreadable:` or `chain:` alert) and no reader can
//!   serve the range, it records `[from, to)` as a gap through its own source and accepts the
//!   layer's head at `to`, unverified for that range. Any other range is refused.
//! - `aledgent-settle <config.json> refound`: the book was re-founded with its instance's reset (a
//!   `reset:` alert): the cursor and the accepted head of the chain before are reset, and the next
//!   run starts on the current chain from its start.
//!
//! Nothing here logs a key, a token or a connection string (`NFR-8`, `REQ-AU-8`).

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use ingress::clock::SystemClock;
use settle::adapter::{Adapter, AdapterError};
use settle::chain::rpc::{Names, RpcChain};
use settle::config::{ChainSource, KeySource, PassKey, ProcessConfig};
use settle::link::HttpTransport;
use settle::signer::{DevSigner, PassAccount, ServiceSigner, Signer};
use settle::store::pg::PgStore;

/// How long every step may find the settlement layer unreachable before the process ends, for its
/// supervisor to start a new one.
const BLIND_RESTART: Duration = Duration::from_secs(180);

fn lanes(config: &ProcessConfig, keys: &Keys) -> Result<Vec<Arc<dyn Signer>>, String> {
    let mut out: Vec<Arc<dyn Signer>> = Vec::new();
    for k in &config.adapter.keys {
        out.push(pass(k, keys)?);
    }
    if out.is_empty() {
        return Err("no lane: configure at least one key".into());
    }
    Ok(out)
}

/// Where the keys come from: the signer service, connected once, and whether development keys
/// are allowed (`F-035`).
struct Keys {
    signer: Option<Arc<signer::client::SignerClient>>,
    /// The keys on engines (`F-042`), open.
    engines: Option<signer::engine::config::OpenKeys>,
    development: bool,
}

impl Keys {
    fn of(config: &ProcessConfig) -> Result<Keys, String> {
        let wants_signer = config.check_key_sources()?;
        let signer = match (&config.signer, wants_signer) {
            (Some(e), true) => Some(Arc::new(
                signer::client::SignerClient::connect(e).map_err(|e| e.to_string())?,
            )),
            _ => None,
        };
        let names = config.engine_key_names();
        let engines = if names.is_empty() {
            None
        } else {
            let needs = names
                .iter()
                .map(|n| settle::config::engine_key_need(n))
                .collect::<Vec<_>>();
            Some(signer::engine::config::open(
                &signer::engine::config::Registry::builtin(),
                &config.engine_section(),
                &needs,
                config.profile == ingress::config::Profile::Development,
            )?)
        };
        Ok(Keys {
            signer,
            engines,
            development: config.development_keys,
        })
    }
}

/// A community PASS account (`F-027` T-027-03): its hashed user id, and its device key behind the
/// signer boundary.
fn pass(k: &PassKey, keys: &Keys) -> Result<Arc<dyn Signer>, String> {
    let user: [u8; 32] = hex::decode(k.user.trim_start_matches("0x"))
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or("a PASS account's user id is 32 bytes of hex")?;
    Ok(Arc::new(PassAccount::new(signer(&k.device, keys)?, user)))
}

fn signer(k: &KeySource, keys: &Keys) -> Result<Arc<dyn Signer>, String> {
    match k {
        KeySource::Signer { key } => {
            let client = keys
                .signer
                .as_ref()
                .ok_or("a key of the signer service needs \"signer\"")?;
            Ok(Arc::new(
                ServiceSigner::new(Arc::clone(client), key).map_err(|e| e.to_string())?,
            ))
        }
        KeySource::Engine(name) => {
            let handle = keys
                .engines
                .as_ref()
                .and_then(|e| e.handle(name))
                .ok_or_else(|| format!("keys.{name}: not opened"))?;
            Ok(Arc::new(
                ServiceSigner::from_key(handle).map_err(|e| e.to_string())?,
            ))
        }
        KeySource::File(path) if keys.development => {
            tracing::warn!("a lane's key is read from a file: development only (NFR-8)");
            Ok(Arc::new(
                DevSigner::from_file(path).map_err(|e| e.to_string())?,
            ))
        }
        KeySource::Env(var) if keys.development => {
            tracing::warn!("a lane's key is read from the environment: development only (NFR-8)");
            Ok(Arc::new(
                DevSigner::from_env(var).map_err(|e| e.to_string())?,
            ))
        }
        KeySource::File(_) | KeySource::Env(_) => {
            Err("a key from a file or the environment is for development only (NFR-8)".into())
        }
    }
}

/// What the process is asked to do.
enum Command {
    /// Run step after step.
    Run,
    /// Record the operator's gap `[from, to)` and exit.
    Gap { from: u64, to: u64 },
    /// Reset the cursor and the accepted head with the book's re-founding, and exit.
    Refound,
}

const USAGE: &str = "usage: aledgent-settle <config.json> [gap <from> <to> | refound]";

async fn run() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let path = args
        .next()
        .or_else(|| std::env::var("ALEDGENT_SETTLE_CONFIG").ok())
        .ok_or(USAGE)?;
    let command = match args.next().as_deref() {
        None => Command::Run,
        Some("gap") => {
            let mut n = || args.next().and_then(|a| a.parse::<u64>().ok()).ok_or(USAGE);
            Command::Gap {
                from: n()?,
                to: n()?,
            }
        }
        Some("refound") => Command::Refound,
        Some(_) => return Err(USAGE.into()),
    };
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
    let config: ProcessConfig = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
    // F-027 T-027-12 (REQ-SA-9): a format the operator names must be one the adapter can sign in.
    let unknown = config.adapter.unknown_formats();
    if !unknown.is_empty() {
        return Err(format!(
            "{path}: adapter.formats names {unknown:?}, which the adapter does not know; it signs \
             in {:?}",
            settle::chain::kreivo::FORMATS
                .iter()
                .map(|f| f.name)
                .collect::<Vec<_>>()
        ));
    }
    // A runtime's spec_version gates nothing since T-027-12: a list from an older file is kept
    // for the record only.
    if !config.adapter.spec_versions.is_empty() {
        tracing::info!(
            spec_versions = ?config.adapter.spec_versions,
            "adapter.spec_versions is informational: the format follows the runtime's transaction \
             surface, confirmed by the layer (T-027-12); the list gates nothing"
        );
    }
    // T-043-05: the periodic re-scan is gone; its settings are accepted for this release only.
    let retired = config.adapter.retired();
    if !retired.is_empty() {
        tracing::warn!(
            fields = ?retired,
            "adapter.rescan_every and adapter.rescan_window are ignored: the adapter checks the \
             settlement layer's chain of entries at every sync (amendment 0040); remove them, the \
             next release refuses them"
        );
    }
    // F-035: the keys behind the signer service; development keys only if the file says so.
    let keys = Keys::of(&config)?;
    let lanes = lanes(&config, &keys)?;
    let assets: Vec<_> = config
        .adapter
        .assets
        .iter()
        .map(|a| a.chain.clone())
        .collect();
    let names = Names {
        contracts: config.contracts_pallet.clone(),
        assets: config.assets_pallet.clone(),
        deployments: config.contracts().map_err(|e| format!("{path}: {e}"))?,
    };
    // Exactly the connection the configuration names; never a fallback to the other.
    let chain = match config.chain_source().map_err(|e| format!("{path}: {e}"))? {
        ChainSource::LightClient {
            chain_spec,
            relay_chain_spec,
        } => {
            let read = |p: &std::path::Path| {
                std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))
            };
            let chain_spec = read(&chain_spec)?;
            let relay_chain_spec = relay_chain_spec.as_deref().map(read).transpose()?;
            tracing::info!(
                parachain = relay_chain_spec.is_some(),
                "connecting through the light client"
            );
            RpcChain::light(&chain_spec, relay_chain_spec.as_deref(), names, &assets).await
        }
        ChainSource::Rpc { endpoint } => {
            tracing::warn!(
                "connecting over a node's RPC (\"connection\": \"rpc\"), not the light client: \
                 what the adapter reads is that node's word"
            );
            RpcChain::connect(&endpoint, names, &assets).await
        }
    }
    .map_err(|e| e.to_string())?;
    // Dry runs only, through a node's RPC, if one is named (F-027 NOTES, decisions 36, 42 and 43).
    // Optional: the light client dry-runs batches itself.
    let chain = match &config.dry_run_endpoint {
        Some(url) => {
            tracing::info!("batches are dry-run through a node's RPC; nothing else is read there");
            chain
                .with_dry_runs_over(url)
                .await
                .map_err(|e| format!("dry_run_endpoint: {e}"))?
        }
        None => chain,
    };
    // T-043-04: older blocks' events, to catch up only, from a node that keeps them all.
    let chain = match &config.history_endpoint {
        Some(url) => {
            tracing::info!("blocks the client no longer serves are read from a history endpoint");
            chain
                .with_history_over(url)
                .await
                .map_err(|e| format!("history_endpoint: {e}"))?
        }
        None => chain,
    };
    let database = config.database_url().map_err(|e| format!("{path}: {e}"))?;
    let store = PgStore::connect(&database, 8, config.profile)
        .await
        .map_err(|e| e.to_string())?;
    drop(database);
    let token = config.ingress_token().map_err(|e| format!("{path}: {e}"))?;
    let transport = HttpTransport::with_token(&config.ingress, token, Duration::from_secs(30))
        .map_err(|e| e.to_string())?;
    let mut adapter = Adapter::new(
        config.adapter.clone(),
        chain,
        store,
        transport,
        lanes,
        Arc::new(SystemClock),
    );
    // F-017: anchors are published with a key of their own, used for nothing else.
    if let Some(k) = &config.adapter.anchor_key {
        adapter = adapter.with_anchor_signer(pass(k, &keys)?);
    }
    // Feature `fault-injection` only (the zombienet suite's build, never a release): die the first
    // time the step reaches the point `ALEDGENT_SETTLE_CRASH_AT` names.
    #[cfg(feature = "fault-injection")]
    if let Ok(point) = std::env::var("ALEDGENT_SETTLE_CRASH_AT") {
        use settle::adapter::CrashPoint;
        let point = match point.as_str() {
            "after-sign" => CrashPoint::AfterSign,
            "after-persist" => CrashPoint::AfterPersist,
            "after-broadcast" => CrashPoint::AfterBroadcast,
            other => return Err(format!("ALEDGENT_SETTLE_CRASH_AT: unknown point {other}")),
        };
        tracing::warn!(?point, "fault injection: the process dies at this point");
        adapter.crash_at(point);
    }
    match command {
        Command::Run => {}
        Command::Gap { from, to } => {
            adapter
                .record_operator_gap(from, to)
                .await
                .map_err(|e| e.to_string())?;
            tracing::warn!(from, to, "the operator's gap is recorded");
            return Ok(());
        }
        Command::Refound => {
            adapter.refound().await.map_err(|e| e.to_string())?;
            return Ok(());
        }
    }
    let interval = Duration::from_millis(config.interval.max(100));
    // `NFR-15`: within the interval, a check for news every `wake_poll` starts a step as soon as
    // there is something for it (`Adapter::news`); zero turns it off. Never more often than the
    // interval itself.
    let tick = match config.wake_poll {
        0 => interval,
        ms => Duration::from_millis(ms.max(50)).min(interval),
    };
    let mut last_step: Option<std::time::Instant> = None;
    // Since when every step has found the settlement layer unreachable (F-027 NOTES, decision 38):
    // past `BLIND_RESTART` the process ends, and its supervisor starts it with a new light client.
    // smoldot did not always reconnect after its peers were cut for a minute and a half.
    let mut blind_since: Option<std::time::Instant> = None;
    let mut served = false;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("stopping");
                return Ok(());
            }
            _ = tokio::time::sleep(tick) => {}
        }
        let due = last_step.is_none_or(|t| t.elapsed() + tick / 2 >= interval);
        if !due && !adapter.news().await {
            continue;
        }
        last_step = Some(std::time::Instant::now());
        let stepped = adapter.step().await;
        if let Some(path) = &config.signals_file {
            write_signals(&adapter, path).await;
        }
        // Blind: the layer does not answer; or, in a process that has never served, it answers
        // from before the deployment existed (a light client that has not caught up: seen on the
        // fleet after a restart, stuck for minutes).
        let (serving, refusing) = adapter.status();
        served |= serving;
        let blind = matches!(
            stepped,
            Err(AdapterError::Chain(settle::chain::ChainError::Unavailable(
                _
            )))
        ) || refusing.contains(&settle::adapter::Refusing::Unreachable)
            || (!served && refusing.contains(&settle::adapter::Refusing::NoInstance));
        if blind {
            let since = *blind_since.get_or_insert_with(std::time::Instant::now);
            if since.elapsed() > BLIND_RESTART {
                return Err(format!(
                    "the settlement layer has been unreachable for {}s; stopping so that a new \
                     process connects afresh",
                    since.elapsed().as_secs()
                ));
            }
        } else {
            blind_since = None;
        }
        match stepped {
            Ok(r) => tracing::info!(
                serving = r.serving,
                acknowledged = r.acknowledged,
                signed = r.signed,
                parked = r.parked,
                observed = r.observed,
                gaps = r.gaps,
                finalized = r.finalized,
                best = r.best,
                "step"
            ),
            Err(AdapterError::LeaseLost) => {
                tracing::warn!("another instance holds the lease; waiting");
            }
            // The zombienet suite's "kill the adapter between signing and submitting" (T-027-06):
            // the process dies at once, as a SIGKILL would leave it, with nothing flushed or undone.
            #[cfg(feature = "fault-injection")]
            Err(AdapterError::Crashed(point)) => {
                tracing::error!(?point, "fault injection: dying here");
                std::process::abort();
            }
            Err(e) => tracing::warn!(error = %e, "the step stopped; the next one resumes"),
        }
    }
}

/// Write the adapter's signals for `F-018`'s exporter: to a temporary file, then renamed, so the
/// exporter never reads half a file. A failure is a warning: the signals are observation, and
/// their staleness is what the exporter pages on.
async fn write_signals<C, S, T>(adapter: &Adapter<C, S, T>, path: &std::path::Path)
where
    C: settle::chain::Chain,
    S: settle::store::Store,
    T: settle::link::Transport,
{
    use ingress::clock::Clock as _;
    let now = SystemClock.now();
    let written = match adapter.signals(now).await {
        Ok(s) => serde_json::to_vec(&s)
            .map_err(|e| e.to_string())
            .and_then(|bytes| {
                let tmp = path.with_extension("tmp");
                std::fs::write(&tmp, bytes)
                    .and_then(|()| std::fs::rename(&tmp, path))
                    .map_err(|e| e.to_string())
            }),
        Err(e) => Err(e.to_string()),
    };
    if let Err(e) = written {
        tracing::warn!(error = %e, "the signals were not written");
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    // Any panic ends the process, so that its supervisor starts a new one over the same store,
    // which resumes where this one stopped (REQ-SA-5, REQ-SA-6). The embedded smoldot panics in
    // its own threads and leaves the light client dead in a process that runs on, serving
    // nothing: seen on the zombienet fleet (F-026 NOTES, finding 5; F-027 NOTES, decision 37).
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default(info);
        tracing::error!(panic = %info, "aledgent-settle stops: a thread panicked");
        std::process::abort();
    }));
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!(error = %e, "aledgent-settle stopped");
            ExitCode::FAILURE
        }
    }
}
