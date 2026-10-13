// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The signal exporter (`T-018-04`, `NFR-11`, `REQ-PV-5`): `opsctl exporter` serves `/metrics`.
//!
//! On every scrape it collects:
//!
//! - the core's signals **through ingress**, `GET /v1/ops/signals`, as the operations principal
//!   with the `read_signals` operator kind: ingress authenticates it and its access log attributes
//!   the read to that principal and a request id (`REQ-PV-5`), and the core refuses anyone else.
//!   The reply is counts and ages, never a ledger fact. A core that does not answer within the
//!   scrape's timeout is `aledgent_signals_up{source="core"} 0`, which pages;
//! - the adapter's signals from the file the adapter writes after every step
//!   (`settle::signals`): only the adapter touches the settlement layer (guardrail 8);
//! - the reconciliation's records, the backups' manifests and the last drill's record, from their
//!   directories.
//!
//! VOS itself exports only counters (`notes/FIT-VOS-CIPHERCLERK.md`): each unit exposes its own signals, and this
//! exporter joins them.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use axum::Router;
use ingress::query::SignalsBody;
use serde::Deserialize;
use settle::signals::AdapterSignals;

use crate::Error;
use crate::backup::StoreKind;
use crate::drill::DrillReport;
use crate::signals::{Snapshot, render};

/// The route the exporter reads the core's signals from.
pub const SIGNALS_ROUTE: &str = "/v1/ops/signals";

/// Where the core's signals come from.
pub trait CoreSource: Send + Sync {
    /// The signals, or why they could not be read.
    fn signals(&self) -> impl std::future::Future<Output = Result<SignalsBody, String>> + Send;
}

/// Where the adapter's signals come from.
pub trait AdapterSource: Send + Sync {
    /// The signals, or why they could not be read.
    fn signals(&self) -> impl std::future::Future<Output = Result<AdapterSignals, String>> + Send;
}

/// Ingress's surface in process, with the operations principal's token (tests, and a host that
/// runs the exporter beside ingress).
#[derive(Clone)]
pub struct RouterSource {
    router: Router,
    token: String,
}

impl RouterSource {
    /// Read through `router` with `token`.
    pub fn new(router: Router, token: &str) -> RouterSource {
        RouterSource {
            router,
            token: token.to_owned(),
        }
    }
}

impl CoreSource for RouterSource {
    async fn signals(&self) -> Result<SignalsBody, String> {
        let (s, v) = ingress::testkit::call(
            &self.router,
            "GET",
            SIGNALS_ROUTE,
            Some(&self.token),
            &[],
            None,
        )
        .await;
        if !s.is_success() {
            return Err(format!("{s}: {}", v["error"]));
        }
        serde_json::from_value(v).map_err(|e| e.to_string())
    }
}

/// Ingress over HTTP, with the operations principal's token read from a file (never an argument,
/// never logged: `REQ-AU-8`).
pub struct HttpSource {
    url: String,
    token: zeroize::Zeroizing<String>,
    client: reqwest::Client,
}

impl HttpSource {
    /// Read `base` + [`SIGNALS_ROUTE`] with the token in `token_file`, giving up after `timeout`.
    pub fn new(base: &str, token_file: &Path, timeout: Duration) -> Result<HttpSource, Error> {
        let token = zeroize::Zeroizing::new(
            std::fs::read_to_string(token_file)
                .map_err(|e| Error::Config(format!("the token file: {e}")))?
                .trim()
                .to_owned(),
        );
        HttpSource::with_token(base, token, timeout)
    }

    /// Read `base` + [`SIGNALS_ROUTE`] with `token` ([`ExporterConfig::token`]).
    pub fn with_token(
        base: &str,
        token: zeroize::Zeroizing<String>,
        timeout: Duration,
    ) -> Result<HttpSource, Error> {
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| Error::Config(e.to_string()))?;
        Ok(HttpSource {
            url: format!("{}{SIGNALS_ROUTE}", base.trim_end_matches('/')),
            token,
            client,
        })
    }
}

impl CoreSource for HttpSource {
    async fn signals(&self) -> Result<SignalsBody, String> {
        let r = self
            .client
            .get(&self.url)
            .bearer_auth(self.token.as_str())
            .send()
            .await
            .map_err(|e| e.without_url().to_string())?;
        if !r.status().is_success() {
            return Err(r.status().to_string());
        }
        r.json().await.map_err(|e| e.without_url().to_string())
    }
}

/// The adapter's signals file.
pub struct FileSource(pub PathBuf);

impl AdapterSource for FileSource {
    async fn signals(&self) -> Result<AdapterSignals, String> {
        let bytes = tokio::fs::read(&self.0).await.map_err(|e| e.to_string())?;
        serde_json::from_slice(&bytes).map_err(|e| e.to_string())
    }
}

/// Signals given as they are (tests).
pub struct Fixed<T>(pub Mutex<Option<T>>);

impl AdapterSource for Fixed<AdapterSignals> {
    async fn signals(&self) -> Result<AdapterSignals, String> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .ok_or_else(|| "no signals".to_owned())
    }
}

impl CoreSource for Fixed<SignalsBody> {
    async fn signals(&self) -> Result<SignalsBody, String> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
            .ok_or_else(|| "no signals".to_owned())
    }
}

/// Where the exporter finds the records it reads.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Records {
    /// The reconciliation's records (`crate::reconcile`).
    pub reconciliation: Option<PathBuf>,
    /// The backups' root.
    pub backups: Option<PathBuf>,
    /// The drills' records.
    pub drills: Option<PathBuf>,
    /// The Agent core's archiver status file (`<state>/archive.status.json`, `T-005-15`). When
    /// set, the core's backup is the archive: its age is how far behind the archive is.
    #[serde(default)]
    pub core_archive: Option<PathBuf>,
}

/// `opsctl exporter`'s configuration (`ops/exporter.example.toml`).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExporterConfig {
    /// Where `/metrics` is served.
    pub listen: String,
    /// Ingress's base URL.
    pub ingress: String,
    /// The file holding the operations principal's token. Exactly one of this and
    /// [`ExporterConfig::token_env`].
    #[serde(default)]
    pub token_file: Option<PathBuf>,
    /// The environment variable holding that token, where a deployment hands its secrets over as
    /// environment variables. Exactly one of this and [`ExporterConfig::token_file`]; the file
    /// names the variable, never the token.
    #[serde(default)]
    pub token_env: Option<String>,
    /// The adapter's signals file.
    pub adapter_signals: PathBuf,
    /// The review queue's owner.
    pub review_owner: String,
    /// How long a source may take, in milliseconds.
    pub timeout_ms: u64,
    /// The records' directories.
    #[serde(default)]
    pub records: Records,
}

impl ExporterConfig {
    /// Parse the TOML.
    pub fn parse(text: &str) -> Result<ExporterConfig, Error> {
        toml::from_str(text).map_err(|e| Error::Config(e.to_string()))
    }

    /// The operations principal's token: from [`ExporterConfig::token_file`] or the variable
    /// [`ExporterConfig::token_env`] names, exactly one of them. No error carries its value
    /// (`REQ-AU-8`).
    pub fn token(&self) -> Result<zeroize::Zeroizing<String>, Error> {
        let raw = match (&self.token_file, &self.token_env) {
            (Some(path), None) => zeroize::Zeroizing::new(
                std::fs::read_to_string(path)
                    .map_err(|e| Error::Config(format!("the token file: {e}")))?,
            ),
            (None, Some(var)) => zeroize::Zeroizing::new(
                std::env::var(var)
                    .map_err(|_| Error::Config(format!("token_env: {var} is not set")))?,
            ),
            (Some(_), Some(_)) => {
                return Err(Error::Config(
                    "token_file and token_env: name one, not both".into(),
                ));
            }
            (None, None) => return Err(Error::Config("name token_file or token_env".into())),
        };
        let token = zeroize::Zeroizing::new(raw.trim().to_owned());
        if token.is_empty() {
            return Err(Error::Config("the operations token is empty".into()));
        }
        Ok(token)
    }
}

/// The exporter. See the module documentation.
pub struct Exporter<C: CoreSource, A: AdapterSource> {
    core: C,
    adapter: A,
    records: Records,
    review_owner: String,
    timeout: Duration,
    /// The finalized height last seen, and when it last moved.
    finalized: Mutex<Option<(u64, u64)>>,
}

impl<C: CoreSource, A: AdapterSource> Exporter<C, A> {
    /// An exporter of `core` and `adapter`, reading `records`, labelling the review queue with
    /// its owner, giving up on a source after `timeout`.
    pub fn new(
        core: C,
        adapter: A,
        records: Records,
        review_owner: &str,
        timeout: Duration,
    ) -> Exporter<C, A> {
        Exporter {
            core,
            adapter,
            records,
            review_owner: review_owner.to_owned(),
            timeout,
            finalized: Mutex::new(None),
        }
    }

    /// Collect a snapshot at `now`.
    pub async fn collect(&self, now: u64) -> Snapshot {
        let core = match tokio::time::timeout(self.timeout, self.core.signals()).await {
            Ok(Ok(s)) => Some(s),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "the core's signals were not read");
                None
            }
            Err(_) => {
                tracing::warn!("the core's signals timed out");
                None
            }
        };
        let adapter = match tokio::time::timeout(self.timeout, self.adapter.signals()).await {
            Ok(Ok(s)) => Some(s),
            _ => None,
        };
        let finalized_stall = adapter.as_ref().map(|a| {
            let mut f = self.finalized.lock().unwrap_or_else(|p| p.into_inner());
            match *f {
                Some((h, since)) if h == a.finalized => now.saturating_sub(since),
                _ => {
                    *f = Some((a.finalized, now));
                    0
                }
            }
        });
        let suspense = self
            .records
            .reconciliation
            .as_deref()
            .and_then(|d| crate::reconcile::latest_run(d).ok().flatten())
            .map(|r| r.suspense_signal(now));
        let reconciliation = self
            .records
            .reconciliation
            .as_deref()
            .map(|d| crate::reconcile::latest_axes(d).unwrap_or_default())
            .unwrap_or_default();
        let mut backups = std::collections::BTreeMap::new();
        if let Some(root) = &self.records.backups {
            for store in StoreKind::ALL {
                if let Ok(Some((_, m))) = crate::backup::latest(root, store) {
                    backups.insert(store, m.taken_at);
                }
            }
        }
        // The Agent core's backup is its archive: everything committed before `at - lag` is in it.
        let core_archive = self.records.core_archive.as_deref().map(|p| {
            core_archive::ArchiveStatus::read(p)
                .inspect_err(|e| tracing::warn!(error = %e, "the archive's status was not read"))
                .ok()
        });
        if let Some(Some(a)) = &core_archive
            && a.conflict.is_none()
            && a.point.is_some()
        {
            backups.insert(StoreKind::Core, a.at.saturating_sub(a.lag_ms));
        }
        let drill = self
            .records
            .drills
            .as_deref()
            .and_then(|d| DrillReport::latest(d).ok().flatten())
            .map(|r| (r.started_at, r.passed()));
        Snapshot {
            at: now,
            core,
            adapter,
            finalized_stall,
            reconciliation,
            suspense,
            backups,
            drill,
            core_archive,
            review_owner: self.review_owner.clone(),
        }
    }

    /// Collect and render.
    pub async fn scrape(&self, now: u64) -> String {
        render(&self.collect(now).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_example_configuration_parses() {
        let c = ExporterConfig::parse(include_str!("../exporter.example.toml")).unwrap();
        assert!(c.records.reconciliation.is_some() && c.timeout_ms > 0);
        assert!(c.token_file.is_some() && c.token_env.is_none());
    }

    #[test]
    fn the_canary_configuration_parses_and_names_its_token_by_variable() {
        let c =
            ExporterConfig::parse(include_str!("../../deploy/templates/exporter.toml")).unwrap();
        assert!(c.token_file.is_none());
        assert_eq!(c.token_env.as_deref(), Some("ALEDGENT_OPS_TOKEN"));
    }

    #[test]
    fn req_au_8_the_token_comes_from_exactly_one_named_place() {
        let base = include_str!("../exporter.example.toml");
        let both = format!("token_env = \"PATH\"\n{base}");
        assert!(ExporterConfig::parse(&both).unwrap().token().is_err());
        let env_only = base.replace(
            "token_file = \"/run/secrets/aledgent-ops-token\"",
            "token_env = \"ALEDGENT_OPS_TEST_VARIABLE_THAT_IS_NEVER_SET\"",
        );
        let e = ExporterConfig::parse(&env_only)
            .unwrap()
            .token()
            .unwrap_err();
        assert!(e.to_string().contains("is not set"), "{e}");
        let neither = base.replace("token_file = \"/run/secrets/aledgent-ops-token\"", "");
        assert!(ExporterConfig::parse(&neither).unwrap().token().is_err());
    }
}
