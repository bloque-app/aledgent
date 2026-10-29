// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The adapter's configuration. It names keys by where they are, never by their value (`NFR-8`,
//! `REQ-AU-8`): a key file, an environment variable's name, a key of the signer service, a key entry
//! on a signature engine (`F-042`).
//!
//! **Two forms of lane** (`F-045`), one per file: `keys` and `anchor_key`, the device keys of
//! community PASS accounts (`F-027`, secp256k1); or `accounts` and `anchor_account`, **native
//! accounts**: each lane, and the anchor key, is an Ed25519 key's own account, which the contract
//! lists, and which signs standard transactions. A native account's key may be on the sidecar or on
//! Vault Transit, where it is made and never leaves; a file only in development.

use std::path::PathBuf;

use ledger_core::account::Time;
use serde::{Deserialize, Serialize};

use crate::chain::ChainAsset;

/// One asset the adapter supports: the book's asset, and the settlement-layer location it maps
/// to (`DEC-20`). **No precision**: the adapter attests the location's own metadata to the core,
/// which registers the asset at those decimals and holds it if they ever differ from the
/// registry. A file that still carries a `precision` is refused.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetBinding {
    /// The book's asset (`USD`).
    pub asset: String,
    /// The settlement layer's asset location, in the textual form of the runtime's value
    /// (`Here(101)`).
    pub chain: ChainAsset,
}

/// Where a signing key is: the device key of the deployment's community PASS account (`F-027`),
/// and that account's hashed user id.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassKey {
    /// The PASS account's hashed user id, 32 bytes of hex: its address is
    /// `blake2_256([0; 32] ‖ user)`.
    pub user: String,
    /// Where its device key is.
    pub device: KeySource,
}

/// Where a lane's key is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeySource {
    /// A file holding the secret as hex: development only (`DevSigner`), refused unless
    /// [`ProcessConfig::development_keys`].
    File(PathBuf),
    /// An environment variable holding the secret as hex: development only, likewise.
    Env(String),
    /// A key of the signer service (`F-035`, `services/signer`), which reads it from Vault (or holds
    /// it in AWS KMS, opted in per key). The adapter holds its identifier and public
    /// half only; [`ProcessConfig::signer`] says where the signer is.
    Signer {
        /// The key's identifier in the signer's configuration.
        key: String,
    },
    /// A key entry of the file's `keys` (`F-042`): `{ "engine": "<name>", "key": { … } }`, on an engine
    /// declared in `engines`. A PASS device key may use the sidecar only; a native account's key the
    /// sidecar or Vault Transit (`F-045`, [`engine_key_need`]).
    Engine(String),
}

/// The adapter's configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdapterConfig {
    /// This instance's name, for the lease.
    pub holder: String,
    /// The settlement layer the deployment declares: its genesis hash, hex (`REQ-SA-2`).
    pub genesis: String,
    /// **Informational only, and optional** (`T-027-12`, amendment 0031): the runtime
    /// `spec_version`s a deployment was checked against. Since `REQ-SA-9` a `spec_version` gates
    /// nothing: the adapter signs for any runtime whose transaction surface it has a format for,
    /// once the layer confirms it. A file that still lists them is accepted, and the list is
    /// logged at start-up and otherwise ignored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub spec_versions: Vec<u32>,
    /// The operator's mapping from a runtime `spec_version` to the name of a known format
    /// (`kreivo-132`, `kreivo-133`), for a runtime whose surface would otherwise match none, or
    /// another (`T-027-12`, `REQ-SA-9`: "an operator MAY name in configuration the format to use
    /// for a set of rules"). The named format is used for that `spec_version` whatever its
    /// surface, and the layer's confirmation is still required before anything signed in it is
    /// broadcast. A name the adapter does not know is refused at start-up. Default: none.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub formats: std::collections::BTreeMap<u32, String>,
    /// The deployment's instance on the settlement layer.
    pub instance: u32,
    /// The assets it supports.
    pub assets: Vec<AssetBinding>,
    /// The lanes' keys, one per lane: each a device key of one of the deployment's community PASS
    /// accounts, which the contract lists as keys (`F-027` T-027-03). Exactly one of this and
    /// [`AdapterConfig::accounts`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<PassKey>,
    /// The lanes' keys as **native accounts** (`F-045`), one per lane: each an Ed25519 key whose own
    /// account the contract lists as a key, and which signs standard transactions. Exactly one of
    /// this and [`AdapterConfig::keys`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accounts: Vec<KeySource>,
    /// The native balance below which a lane does not sign (`REQ-SN-2`).
    pub fee_floor: u128,
    /// How long the store lease lasts unless renewed, in milliseconds.
    pub lease_ttl: Time,
    /// How long a status declaration to the core holds, in milliseconds (at most five minutes).
    pub status_lease: Time,
    /// Inbox positions read per page (VOS's source: 32).
    pub page: u32,
    /// The longest mortal era, in blocks; at most the runtime's `BlockHashCount`.
    pub max_era: u64,
    /// How many blocks after the finalized head a transaction may still be included (`F-027`):
    /// its PASS challenge's context is that head, and Kreivo accepts a context at most 30 minutes
    /// of its blocks old, so no era is signed to outlive it. Default 256.
    #[serde(default = "default_challenge_window")]
    pub challenge_window: u64,
    /// **Retired** (`T-043-05`, amendment 0040): the periodic re-scan is gone, since the adapter
    /// checks the chain's head at every sync. A file that still sets it is accepted, with a
    /// warning ([`AdapterConfig::retired`]), for this release; the next refuses it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rescan_every: Option<u64>,
    /// **Retired** with [`AdapterConfig::rescan_every`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rescan_window: Option<u64>,
    /// The key that publishes anchors (`F-017`), one of the instance's keys, used for nothing
    /// else. Without one, the adapter neither anchors nor reports positions for the settlement
    /// comparison.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_key: Option<PassKey>,
    /// The anchor key as a native account (`F-045`): with [`AdapterConfig::accounts`], in place of
    /// [`AdapterConfig::anchor_key`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor_account: Option<KeySource>,
    /// A lower cap on the gas any batch is given, if set; by default none. What always caps a
    /// batch is the runtime's own normal-extrinsic maximum, read at the block the batch is sized
    /// against (`F-027` NOTES, decision 41): Kreivo `0.17.0`'s moves with its cores. A batch holds
    /// only as many calls as its dry run, with [`AdapterConfig::batch_gas_margin_percent`], says fit
    /// under the lesser of the two.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch_gas_cap: Option<crate::chain::kreivo::Weight>,
    /// A batch's gas limit is its dry run's `gas_required` times this, in percent (default 150:
    /// half again), capped as [`AdapterConfig::batch_gas_cap`] says; its storage deposit limit is the
    /// dry run's deposit times this too. The margin covers a batch that
    /// runs a costlier path than its dry run priced, as when a deposit lands first in the same
    /// block and a withdrawal the dry run saw refused applies; the zombienet suite needed half
    /// again for that.
    #[serde(default = "default_batch_gas_margin_percent")]
    pub batch_gas_margin_percent: u64,
    /// After how many consecutive "not now" answers of the layer (`Stale`, `Future`, `Payment`,
    /// `ExhaustsResources`, `AncientBirthBlock`, an unknown validity) to the first transaction
    /// signed on a runtime not yet confirmed the adapter raises that runtime's
    /// `runtime:<spec_version>` alert, which pages (`F-027` NOTES, decision 52). Default 10: at
    /// least one answer a step while calls wait, and a step at least every `interval` (2 s in the
    /// deployment template), so about 20 s, well inside the four minutes or so a withdrawal can wait
    /// unsigned before it ends as never signed. 0 is taken as 1.
    #[serde(default = "default_not_now_alert_after")]
    pub not_now_alert_after: u32,
    /// Whether the adapter reports a reset of its instance (a new settlement sequence) to the core
    /// (amendment 0041, `T-043-15`). **Off by default**, and turned on only once the core runs a
    /// build that knows the input (`T-043-14`) at `SETTLEMENT_EPOCHS`: a core host built before it
    /// cannot decode the input, refuses the whole feed it arrives in, and the book's inputs wait
    /// behind it. Off, a reset stops the adapter as before amendment 0041 (`reset:` until
    /// `refound`; a first start moves to the new chain without telling the core), and the adapter
    /// is held while `reset:` is open: it declares that it does not serve.
    #[serde(default)]
    pub report_resets: bool,
}

/// [`ProcessConfig::wake_poll`]'s default, in milliseconds.
pub fn default_wake_poll() -> Time {
    250
}

/// [`AdapterConfig::challenge_window`]'s default.
pub fn default_challenge_window() -> u64 {
    256
}

/// [`AdapterConfig::not_now_alert_after`]'s default.
pub fn default_not_now_alert_after() -> u32 {
    10
}

/// [`AdapterConfig::batch_gas_margin_percent`]'s default: half again.
pub fn default_batch_gas_margin_percent() -> u64 {
    150
}

impl AdapterConfig {
    /// The declared genesis hash.
    pub fn genesis_hash(&self) -> Option<[u8; 32]> {
        hex::decode(self.genesis.trim_start_matches("0x"))
            .ok()
            .and_then(|v| v.try_into().ok())
    }

    /// The format names in [`AdapterConfig::formats`] the adapter does not know (`T-027-12`): a
    /// configuration naming one is refused at start-up, since nothing could be signed in it.
    pub fn unknown_formats(&self) -> Vec<String> {
        self.formats
            .values()
            .filter(|name| crate::chain::kreivo::TxFormat::named(name).is_none())
            .cloned()
            .collect()
    }

    /// Whether the lanes are native accounts (`F-045`), after checking that the file gives one form
    /// ([`AdapterConfig::keys`] and [`AdapterConfig::anchor_key`], or [`AdapterConfig::accounts`] and
    /// [`AdapterConfig::anchor_account`]) and at least one lane. Mixing the two is refused: a
    /// transaction format is confirmed on the layer for one form at a time.
    pub fn native(&self) -> Result<bool, String> {
        let pass = !self.keys.is_empty() || self.anchor_key.is_some();
        let native = !self.accounts.is_empty() || self.anchor_account.is_some();
        match (pass, native) {
            (true, true) => Err(
                "PASS keys (\"keys\", \"anchor_key\") and native accounts (\"accounts\",                  \"anchor_account\"): one form, not both"
                    .into(),
            ),
            (_, true) if self.accounts.is_empty() => {
                Err("no lane: \"anchor_account\" needs \"accounts\"".into())
            }
            (true, _) if self.keys.is_empty() => {
                Err("no lane: \"anchor_key\" needs \"keys\"".into())
            }
            (false, false) => Err("no lane: configure \"accounts\" (or \"keys\")".into()),
            (_, native) => Ok(native),
        }
    }

    /// Where every lane's key is, then the anchor key's, whatever the form: `(source, native)`.
    pub fn key_sources(&self) -> Vec<(&KeySource, bool)> {
        self.keys
            .iter()
            .chain(self.anchor_key.as_ref())
            .map(|k| (&k.device, false))
            .chain(
                self.accounts
                    .iter()
                    .chain(self.anchor_account.as_ref())
                    .map(|k| (k, true)),
            )
            .collect()
    }

    /// The retired fields the file still sets (`T-043-05`): accepted with a warning for one
    /// release, then refused.
    pub fn retired(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.rescan_every.is_some() {
            out.push("rescan_every");
        }
        if self.rescan_window.is_some() {
            out.push("rescan_window");
        }
        out
    }

    /// The binding of the book's `asset`.
    pub fn binding(&self, asset: &str) -> Option<&AssetBinding> {
        self.assets.iter().find(|a| a.asset == asset)
    }

    /// The binding of the chain's `asset`.
    pub fn binding_of_chain(&self, asset: &ChainAsset) -> Option<&AssetBinding> {
        self.assets.iter().find(|a| &a.chain == asset)
    }
}

/// How the adapter reaches the settlement layer.
///
/// The light client is the default: the adapter checks what it reads against the chain's own
/// consensus instead of trusting one node's answers. A node's RPC is an explicit opt-in, and the
/// adapter never falls back from one to the other: a light client that cannot sync leaves the
/// adapter not serving, as any unavailable settlement layer does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Connection {
    /// subxt's embedded smoldot light client, syncing [`ProcessConfig::chain_spec`].
    #[default]
    LightClient,
    /// A node's RPC at [`ProcessConfig::endpoint`].
    Rpc,
}

impl Connection {
    /// Its name in the configuration.
    pub fn name(self) -> &'static str {
        match self {
            Connection::LightClient => "light-client",
            Connection::Rpc => "rpc",
        }
    }
}

/// Where the adapter reaches the settlement layer, once the configuration is checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainSource {
    /// Through the light client.
    LightClient {
        /// The settlement layer's chain spec.
        chain_spec: PathBuf,
        /// Its relay chain's spec, for a parachain.
        relay_chain_spec: Option<PathBuf>,
    },
    /// Over a node's RPC.
    Rpc {
        /// The node's endpoint.
        endpoint: String,
    },
}

/// A configuration whose connection fields do not agree with its [`Connection`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectionError {
    /// A field the connection needs is absent.
    Missing {
        /// The connection.
        connection: Connection,
        /// The field.
        field: &'static str,
    },
    /// A field of the other connection is present: refused rather than ignored.
    Foreign {
        /// The connection.
        connection: Connection,
        /// The field.
        field: &'static str,
    },
}

impl std::fmt::Display for ConnectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectionError::Missing { connection, field } => write!(
                f,
                "\"connection\": \"{}\" needs \"{field}\"",
                connection.name()
            ),
            ConnectionError::Foreign { connection, field } => {
                write!(
                    f,
                    "\"{field}\" is not used with \"connection\": \"{}\"",
                    connection.name()
                )?;
                if *connection == Connection::LightClient {
                    f.write_str(
                        " (the default); to use a node's RPC, say \"connection\": \"rpc\"",
                    )?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for ConnectionError {}

/// Where the binary finds everything else (feature `light-client`, `http`, `postgres`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessConfig {
    /// Production or development (no default): in production the adapter refuses a database
    /// connection string that does not require TLS (`T-016-13`; the owner's ruling of 2026-10-02).
    pub profile: ingress::config::Profile,
    /// The adapter.
    pub adapter: AdapterConfig,
    /// How the adapter reaches the settlement layer: `"light-client"` (the default) or `"rpc"`.
    #[serde(default)]
    pub connection: Connection,
    /// The settlement layer's chain spec, a JSON file with its boot nodes: what the light client
    /// syncs. Required with `"light-client"`; refused with `"rpc"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain_spec: Option<PathBuf>,
    /// The relay chain's spec, when the settlement layer is a parachain (Kreivo, on Kusama). Only
    /// with `"light-client"`; refused with `"rpc"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay_chain_spec: Option<PathBuf>,
    /// A node's RPC endpoint (`ws://…`, `wss://…`). Required with `"rpc"`; refused with
    /// `"light-client"`, so that a configuration never names a node it does not use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// The venue: the settlement contract of each deployment (`F-026`, `F-027`). The deployment's
    /// own ([`AdapterConfig::instance`]) must be listed; so must every deployment a withdrawal may
    /// name (`REQ-SL-6`).
    pub deployments: Vec<DeploymentContract>,
    /// `pallet-contracts`' name in the runtime (`Contracts`).
    #[serde(default = "default_contracts_pallet")]
    pub contracts_pallet: String,
    /// The assets pallet's name in the runtime.
    pub assets_pallet: String,
    /// A node's RPC endpoint (`ws://…`, `wss://…`) that batches are dry-run through, and nothing
    /// else (`F-027` NOTES, decisions 36, 42 and 43). Optional, and absent from the canary: over
    /// the light client the adapter dry-runs batches itself, now that its smoldot no longer
    /// recalculates, once a call has finished, the roots of the child tries the call wrote (the
    /// workspace's `[patch.crates-io]`). A dry run only sizes and prices a batch; every read that
    /// decides anything stays on `connection`. Absent: dry runs go through `connection` too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dry_run_endpoint: Option<String>,
    /// A node's RPC that keeps every block's events (an archive node), `ws://…` or `wss://…`, used
    /// only to catch up (`T-043-04`, amendment 0040): a full node, and so the light client's peers,
    /// keep about 256 blocks of events, and an adapter further behind reads the older blocks'
    /// events here. Nothing it serves is trusted: its entries are folded against the head read
    /// through `connection`. Absent: an adapter behind the node's history stops with an alert until
    /// one is configured or the operator records a gap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_endpoint: Option<String>,
    /// Ingress's base URL.
    pub ingress: String,
    /// The file holding the adapter's ingress token. Exactly one of this and
    /// [`ProcessConfig::ingress_token_env`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress_token_file: Option<PathBuf>,
    /// The environment variable holding the adapter's ingress token, where a deployment hands its
    /// secrets over as environment variables (from the operator's secret store). Exactly one of this
    /// and [`ProcessConfig::ingress_token_file`]. The file names the variable, never the token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ingress_token_env: Option<String>,
    /// The adapter's store: a PostgreSQL connection string, its own database and role. Exactly
    /// one of this and [`ProcessConfig::database_env`]; a string with a password in it belongs in
    /// the environment, not here (`REQ-AU-8`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    /// The environment variable holding the adapter's PostgreSQL connection string. Exactly one of
    /// this and [`ProcessConfig::database`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database_env: Option<String>,
    /// Milliseconds between steps: the longest the adapter goes without one.
    pub interval: Time,
    /// Milliseconds between checks for news within the interval (`NFR-15`): a moved finalized or
    /// best head, or a call new in the outbox, starts a step at once rather than at the interval's
    /// end. Each check reads two heads from the layer's client and the outbox; nothing is proved
    /// or dry-run. Zero: steps only every `interval`, as before. Default 250.
    #[serde(default = "default_wake_poll")]
    pub wake_poll: Time,
    /// Where to write the adapter's signals after every step, as JSON, for `F-018`'s exporter
    /// (`settle::signals`). Absent: not written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signals_file: Option<PathBuf>,
    /// The signer service (`F-035`): required when a key's source is [`KeySource::Signer`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signer: Option<signer::client::SignerEndpoint>,
    /// The signature engines the keys use (`F-042`), by name: `{ "<kind>": { … } }`. Read only for the
    /// keys a source names with [`KeySource::Engine`].
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub engines: std::collections::BTreeMap<String, serde_json::Value>,
    /// The key entries ([`KeySource::Engine`] names them): `{ "engine": "<name>", "key": { … } }`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub keys: std::collections::BTreeMap<String, serde_json::Value>,
    /// Whether a key may come from a file or the environment (`KeySource::File`,
    /// `KeySource::Env`): development and the canary only. Without it, such a key fails the
    /// start (`NFR-8`: settlement-layer key material is never readable from a process
    /// environment).
    #[serde(default, skip_serializing_if = "core::ops::Not::not")]
    pub development_keys: bool,
}

/// A deployment's settlement contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentContract {
    /// The deployment's instance: the number the book's coordinates carry.
    pub instance: u32,
    /// Its contract's account, 32 bytes of hex.
    pub contract: String,
}

/// The value of the environment variable `var`, which the field `field` names: an error names the
/// field and the variable, never a value.
fn from_env(field: &str, var: &str) -> Result<zeroize::Zeroizing<String>, String> {
    match std::env::var(var) {
        Ok(v) if !v.trim().is_empty() => Ok(zeroize::Zeroizing::new(v)),
        Ok(_) => Err(format!("{field}: {var} is empty")),
        Err(_) => Err(format!("{field}: {var} is not set")),
    }
}

/// [`ProcessConfig::contracts_pallet`]'s default.
pub fn default_contracts_pallet() -> String {
    "Contracts".into()
}

/// What the adapter needs of each key on an engine (`F-042`, `F-045`). `NFR-8` says settlement-layer key
/// material is held in a boundary that is separate from the book and can be audited separately: the
/// sidecar, or Vault Transit (whose audit log is Vault's), and never in the adapter's own process (an
/// in-process key has no audit apart from the adapter's).
///
/// - A PASS device key (`native` false): `secp256k1`, on the sidecar only (S1 of `F-042`, ruled), since
///   nothing else offers the scheme.
/// - A native account's key (`native` true): `ed25519`, on the sidecar or Vault Transit (S1: Transit
///   opens for the adapter's keys with native accounts).
pub fn engine_key_need(name: &str, native: bool) -> signer::engine::config::KeyNeed<'_> {
    use signer::engine::EngineKind;
    if native {
        signer::engine::config::KeyNeed {
            name,
            scheme: signer::proto::Scheme::Ed25519,
            kinds: &[EngineKind::Sidecar, EngineKind::ApiProvider],
        }
    } else {
        signer::engine::config::KeyNeed {
            name,
            scheme: signer::proto::Scheme::Secp256k1,
            kinds: &[EngineKind::Sidecar],
        }
    }
}

impl ProcessConfig {
    /// Check where the keys come from (`F-035`, `NFR-8`): a file or an environment variable only
    /// with [`ProcessConfig::development_keys`], a signer key only with [`ProcessConfig::signer`].
    /// `Ok(true)` when a key is the signer service's.
    pub fn check_key_sources(&self) -> Result<bool, String> {
        let native = self.adapter.native()?;
        let mut wants_signer = false;
        for (s, _) in self.adapter.key_sources() {
            match s {
                KeySource::Signer { .. } => wants_signer = true,
                KeySource::Engine(_) => {}
                // NFR-8, T-042-09: a native account's key is never read from the environment.
                KeySource::Env(_) if native => {
                    return Err(
                        "a native account's key is never read from the environment (NFR-8): \
                         name a key entry (\"engine\"), the signer service, or, in development, \
                         a file"
                            .into(),
                    );
                }
                KeySource::File(_) | KeySource::Env(_) if !self.development_keys => {
                    return Err(
                        "a key from a file or the environment is for development only (NFR-8): \
                         use the signer service, or set \"development_keys\": true"
                            .into(),
                    );
                }
                KeySource::File(_) | KeySource::Env(_) => {}
            }
        }
        if wants_signer && self.signer.is_none() {
            return Err("a key of the signer service needs \"signer\"".into());
        }
        let engine_keys = self.engine_key_names();
        if !engine_keys.is_empty() {
            if wants_signer {
                return Err(
                    "keys of the signer service (\"signer\") and keys on engines (\"engine\"): one \
                     form (the form written before F-042, and the engines' form)"
                        .into(),
                );
            }
            let needs = engine_keys
                .iter()
                .map(|name| engine_key_need(name, native))
                .collect::<Vec<_>>();
            signer::engine::config::check(
                &signer::engine::config::Registry::builtin(),
                &self.engine_section(),
                &needs,
                self.profile == ingress::config::Profile::Development,
            )?;
        }
        Ok(wants_signer)
    }

    /// The key entries the lanes and the anchor name ([`KeySource::Engine`]).
    pub fn engine_key_names(&self) -> Vec<String> {
        self.adapter
            .key_sources()
            .into_iter()
            .filter_map(|(k, _)| match k {
                KeySource::Engine(name) => Some(name.clone()),
                _ => None,
            })
            .collect()
    }

    /// The two maps of key entries.
    pub fn engine_section(&self) -> signer::engine::config::Section<'_> {
        signer::engine::config::Section {
            engines: &self.engines,
            keys: &self.keys,
        }
    }

    /// Every deployment's contract, as `(instance, account)`; an error names one that is not 32
    /// bytes of hex, or a deployment's own contract that is missing.
    pub fn contracts(&self) -> Result<Vec<(u32, [u8; 32])>, String> {
        let out = self
            .deployments
            .iter()
            .map(|d| {
                hex::decode(d.contract.trim_start_matches("0x"))
                    .ok()
                    .and_then(|v| <[u8; 32]>::try_from(v).ok())
                    .map(|c| (d.instance, c))
                    .ok_or_else(|| {
                        format!("instance {}: a contract is 32 bytes of hex", d.instance)
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if !out.iter().any(|(i, _)| *i == self.adapter.instance) {
            return Err(format!(
                "no contract is configured for the deployment's own instance {}",
                self.adapter.instance
            ));
        }
        Ok(out)
    }

    /// The adapter's ingress token: from [`ProcessConfig::ingress_token_file`] or
    /// [`ProcessConfig::ingress_token_env`], exactly one of them. Neither the token nor its value
    /// ever reaches an error message (`REQ-AU-8`).
    pub fn ingress_token(&self) -> Result<zeroize::Zeroizing<String>, String> {
        let token = match (&self.ingress_token_file, &self.ingress_token_env) {
            (Some(path), None) => zeroize::Zeroizing::new(
                std::fs::read_to_string(path)
                    .map_err(|e| format!("the ingress token file could not be read: {e}"))?,
            ),
            (None, Some(var)) => from_env("ingress_token_env", var)?,
            (Some(_), Some(_)) => {
                return Err("ingress_token_file and ingress_token_env: name one, not both".into());
            }
            (None, None) => return Err("name ingress_token_file or ingress_token_env".into()),
        };
        let trimmed = zeroize::Zeroizing::new(token.trim().to_owned());
        if trimmed.is_empty() {
            return Err("the adapter's ingress token is empty".into());
        }
        Ok(trimmed)
    }

    /// The adapter's store's connection string: [`ProcessConfig::database`] or the variable
    /// [`ProcessConfig::database_env`] names, exactly one of them.
    pub fn database_url(&self) -> Result<zeroize::Zeroizing<String>, String> {
        match (&self.database, &self.database_env) {
            (Some(url), None) => Ok(zeroize::Zeroizing::new(url.clone())),
            (None, Some(var)) => from_env("database_env", var),
            (Some(_), Some(_)) => Err("database and database_env: name one, not both".into()),
            (None, None) => Err("name database or database_env".into()),
        }
    }

    /// Where the adapter reaches the settlement layer: exactly what [`ProcessConfig::connection`]
    /// names, or an error. A field of the other connection is refused, never ignored and never
    /// used as a fallback.
    pub fn chain_source(&self) -> Result<ChainSource, ConnectionError> {
        let connection = self.connection;
        let refuse = |present: bool, field| {
            if present {
                Err(ConnectionError::Foreign { connection, field })
            } else {
                Ok(())
            }
        };
        let need = |field| ConnectionError::Missing { connection, field };
        match connection {
            Connection::LightClient => {
                refuse(self.endpoint.is_some(), "endpoint")?;
                Ok(ChainSource::LightClient {
                    chain_spec: self.chain_spec.clone().ok_or(need("chain_spec"))?,
                    relay_chain_spec: self.relay_chain_spec.clone(),
                })
            }
            Connection::Rpc => {
                refuse(self.chain_spec.is_some(), "chain_spec")?;
                refuse(self.relay_chain_spec.is_some(), "relay_chain_spec")?;
                Ok(ChainSource::Rpc {
                    endpoint: self.endpoint.clone().ok_or(need("endpoint"))?,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Object = serde_json::Map<String, serde_json::Value>;

    /// The README's example configuration, parsed.
    fn readme_example() -> serde_json::Value {
        let readme = include_str!("../README.md");
        let start = readme
            .find("```json\n")
            .expect("the README has a JSON example")
            + 8;
        let end = start + readme[start..].find("```").unwrap();
        serde_json::from_str(&readme[start..end]).expect("the README's example is JSON")
    }

    fn with(edit: impl FnOnce(&mut Object)) -> ProcessConfig {
        let mut v = readme_example();
        edit(v.as_object_mut().unwrap());
        serde_json::from_value(v).unwrap()
    }

    fn rpc(m: &mut Object) {
        m.insert("connection".into(), "rpc".into());
        m.remove("chain_spec");
        m.remove("relay_chain_spec");
        m.insert("endpoint".into(), "ws://127.0.0.1:9944".into());
    }

    /// Amendment 0041 (`T-043-15`): reporting a reset is off unless the file turns it on. The
    /// README's example does not name it.
    #[test]
    fn t043_15_report_resets_is_off_by_default() {
        let v = readme_example();
        assert!(v["adapter"].get("report_resets").is_none());
        assert!(!with(|_| {}).adapter.report_resets);
        let on = with(|m| {
            m.get_mut("adapter").unwrap()["report_resets"] = true.into();
        });
        assert!(on.adapter.report_resets);
    }

    #[test]
    fn nfr_15_wake_poll_defaults_to_a_quarter_second_and_zero_turns_it_off() {
        assert_eq!(with(|_| {}).wake_poll, 250);
        let c = with(|m| {
            m.remove("wake_poll");
        });
        assert_eq!(c.wake_poll, default_wake_poll());
        let c = with(|m| {
            m.insert("wake_poll".into(), 0.into());
        });
        assert_eq!(c.wake_poll, 0);
    }

    #[test]
    fn t_027_02_the_readme_example_names_its_own_contract() {
        let c: ProcessConfig = serde_json::from_value(readme_example()).unwrap();
        let contracts = c.contracts().unwrap();
        assert!(contracts.iter().any(|(i, _)| *i == c.adapter.instance));
        assert_eq!(c.contracts_pallet, "Contracts");
        assert!(c.adapter.keys.iter().all(|k| k.user.len() == 64));
        let c = with(|m| {
            m.insert("deployments".into(), serde_json::json!([]));
        });
        assert!(
            c.contracts().is_err(),
            "the deployment's own contract is required"
        );
        let c = with(|m| {
            m.insert(
                "deployments".into(),
                serde_json::json!([{ "instance": 1, "contract": "00" }]),
            );
        });
        assert!(c.contracts().is_err(), "a contract is 32 bytes");
    }

    /// `T-027-12`, `REQ-SA-9`: no `spec_version` list is needed; one from an older file still
    /// parses, gating nothing; the operator's `formats` names known formats only.
    #[test]
    fn t_027_12_spec_versions_are_optional_and_formats_name_known_ones() {
        let c: ProcessConfig = serde_json::from_value(readme_example()).unwrap();
        assert!(c.adapter.spec_versions.is_empty() && c.adapter.formats.is_empty());
        assert!(c.adapter.unknown_formats().is_empty());
        // An older file's list, numbers the adapter has no format for included.
        let c = with(|m| {
            m["adapter"]["spec_versions"] = serde_json::json!([132, 135, 133, 1]);
        });
        assert_eq!(c.adapter.spec_versions, vec![132, 135, 133, 1]);
        assert!(c.adapter.unknown_formats().is_empty());
        // The operator's mapping: JSON's keys are strings.
        let c = with(|m| {
            m["adapter"]["formats"] = serde_json::json!({ "135": "kreivo-133", "136": "kreivo-9" });
        });
        assert_eq!(
            c.adapter.formats.get(&135).map(String::as_str),
            Some("kreivo-133")
        );
        assert_eq!(c.adapter.unknown_formats(), vec!["kreivo-9".to_owned()]);
    }

    #[test]
    fn req_au_8_the_token_and_the_database_come_from_exactly_one_named_place() {
        // The README's example names a token file and an inline connection string.
        let c: ProcessConfig = serde_json::from_value(readme_example()).unwrap();
        assert!(c.ingress_token_file.is_some() && c.ingress_token_env.is_none());
        assert_eq!(
            c.database_url().unwrap().as_str(),
            "postgres://settle@db/settle?sslmode=require"
        );
        // From the environment: the variable is named, and an error names it, never a value.
        // (Edition 2024 makes setting a variable unsafe, so the test reads one that is always set.)
        let c = with(|m| {
            m.remove("ingress_token_file");
            m.remove("database");
            m.insert("ingress_token_env".into(), "PATH".into());
            m.insert("database_env".into(), "PATH".into());
        });
        let path = std::env::var("PATH").unwrap();
        assert_eq!(c.ingress_token().unwrap().as_str(), path.trim());
        assert_eq!(c.database_url().unwrap().as_str(), path);
        let unset = "ALEDGENT_SETTLE_TEST_VARIABLE_THAT_IS_NEVER_SET";
        let c = with(|m| {
            m.remove("ingress_token_file");
            m.remove("database");
            m.insert("ingress_token_env".into(), unset.into());
            m.insert("database_env".into(), unset.into());
        });
        let e = c.ingress_token().unwrap_err();
        assert!(e.contains(unset) && e.contains("not set"), "{e}");
        assert!(c.database_url().unwrap_err().contains(unset));
        // A token file, trimmed.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("token");
        std::fs::write(&file, "  a-token\n").unwrap();
        let c = with(|m| {
            m.insert("ingress_token_file".into(), file.to_str().unwrap().into());
        });
        assert_eq!(c.ingress_token().unwrap().as_str(), "a-token");
        // Both, or neither: refused, never one chosen silently.
        let both = with(|m| {
            m.insert("ingress_token_env".into(), "PATH".into());
            m.insert("database_env".into(), "PATH".into());
        });
        assert!(both.ingress_token().is_err());
        assert!(both.database_url().is_err());
        let neither = with(|m| {
            m.remove("ingress_token_file");
            m.remove("database");
        });
        assert!(neither.ingress_token().is_err());
        assert!(neither.database_url().is_err());
    }

    /// `F-042`: the adapter's lane and anchor keys on an engine. A key entry on the sidecar checks in
    /// production; an engine of another kind is refused for its kind (`NFR-8`: the adapter's keys are
    /// held in a boundary that can be audited separately); a kind this build lacks is refused; and the
    /// two forms are one or the other.
    #[test]
    fn f_042_the_adapters_keys_on_an_engine_may_use_the_sidecar_only() {
        let mut c: ProcessConfig =
            serde_json::from_str(include_str!("../../../deploy/templates/settle.json")).unwrap();
        c.development_keys = false;
        c.profile = ingress::config::Profile::Production;
        c.adapter.anchor_key = None;
        for k in c.adapter.keys.iter_mut() {
            k.device = KeySource::Engine("lane-1".into());
        }
        let engines = |kind: serde_json::Value| {
            [("e".to_owned(), kind)]
                .into_iter()
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        c.keys = [(
            "lane-1".to_owned(),
            serde_json::json!({ "engine": "e", "key": { "id": "lane-1" } }),
        )]
        .into_iter()
        .collect();
        c.engines = engines(serde_json::json!({ "sidecar": {
            "socket": "/run/aledgent-signer/signer.sock", "client": "settle",
            "token_file": "/etc/aledgent/signer_token" } }));
        assert_eq!(c.check_key_sources(), Ok(false));
        assert_eq!(c.engine_key_names().len(), c.adapter.keys.len());
        // In process: the adapter's keys are not for it.
        c.engines = engines(serde_json::json!({ "in_process": {} }));
        c.keys.insert(
            "lane-1".into(),
            serde_json::json!({ "engine": "e", "key": { "file": "/k" } }),
        );
        let e = c.check_key_sources().unwrap_err();
        assert!(
            e.contains("keys.lane-1")
                && e.contains("kind in_process")
                && e.contains("may use: sidecar"),
            "{e}"
        );
        // Transit, for a PASS device key: Transit does not offer secp256k1 (F-045 opens Transit for
        // native accounts' Ed25519 keys only).
        c.engines = engines(serde_json::json!({ "vault_transit": {} }));
        let e = c.check_key_sources().unwrap_err();
        assert!(
            e.contains("vault_transit")
                && (e.contains("does not have") || e.contains("does not offer")),
            "{e}"
        );
        // The two forms.
        c.engines = engines(serde_json::json!({ "sidecar": {
            "socket": "/s", "client": "settle", "token_file": "/t" } }));
        c.keys.insert(
            "lane-1".into(),
            serde_json::json!({ "engine": "e", "key": { "id": "lane-1" } }),
        );
        c.signer = Some(signer::client::SignerEndpoint {
            socket: "/s".into(),
            client: "settle".into(),
            token_file: "/t".into(),
            timeout_ms: None,
        });
        let mut other = c.adapter.keys[0].clone();
        other.device = KeySource::Signer {
            key: "other".into(),
        };
        c.adapter.keys.push(other);
        assert!(c.check_key_sources().unwrap_err().contains("one form"));
    }

    /// A file whose lanes are native accounts (`F-045`) on Vault Transit, as the deployments' are.
    fn native_on_transit() -> ProcessConfig {
        let mut c: ProcessConfig =
            serde_json::from_str(include_str!("../../../deploy/templates/settle.json")).unwrap();
        c.development_keys = false;
        c.profile = ingress::config::Profile::Production;
        c.adapter.keys.clear();
        c.adapter.anchor_key = None;
        c.adapter.accounts = vec![KeySource::Engine("lane-1".into())];
        c.adapter.anchor_account = Some(KeySource::Engine("anchor".into()));
        c.engines = [(
            "vault".to_owned(),
            serde_json::json!({ "vault_transit": {
                "vault": { "address": "https://vault.example",
                           "auth": { "aws_iam": { "mount": "aledgent-signer", "role": "aledgent-signer-settle-iam",
                                                  "region": "us-east-2", "server_id": "vault.example" } } },
                "mount": "transit-aledgent-signer" } }),
        )]
        .into_iter()
        .collect();
        c.keys = [
            (
                "lane-1".to_owned(),
                serde_json::json!({ "engine": "vault", "key": { "name": "lane-1", "version": 1 } }),
            ),
            (
                "anchor".to_owned(),
                serde_json::json!({ "engine": "vault", "key": { "name": "anchor", "version": 1 } }),
            ),
        ]
        .into_iter()
        .collect();
        c
    }

    /// `F-045`: native accounts' keys are Ed25519, on Vault Transit or the sidecar; never in process,
    /// never from the environment; a file in development only. One form per file.
    #[test]
    fn f_045_native_accounts_keys_may_use_transit_or_the_sidecar() {
        let c = native_on_transit();
        assert_eq!(c.adapter.native(), Ok(true));
        assert_eq!(c.check_key_sources(), Ok(false));
        assert_eq!(c.engine_key_names(), ["lane-1", "anchor"]);
        // A version is required: a new version is a new account.
        let mut v = c.clone();
        v.keys.insert(
            "anchor".into(),
            serde_json::json!({ "engine": "vault", "key": { "name": "anchor" } }),
        );
        assert!(
            v.check_key_sources()
                .unwrap_err()
                .contains("pin the key version")
        );
        // The sidecar, Ed25519.
        let mut sc = c.clone();
        sc.engines.insert(
            "vault".into(),
            serde_json::json!({ "sidecar": { "socket": "/s", "client": "settle", "token_file": "/t" } }),
        );
        sc.keys.insert(
            "lane-1".into(),
            serde_json::json!({ "engine": "vault", "key": { "id": "lane-1" } }),
        );
        sc.keys.insert(
            "anchor".into(),
            serde_json::json!({ "engine": "vault", "key": { "id": "anchor" } }),
        );
        assert_eq!(sc.check_key_sources(), Ok(false));
        // In process: refused for its kind, in production as in development.
        let mut ip = c.clone();
        ip.engines
            .insert("vault".into(), serde_json::json!({ "in_process": {} }));
        for k in ["lane-1", "anchor"] {
            ip.keys.insert(
                k.into(),
                serde_json::json!({ "engine": "vault", "key": { "file": "/k" } }),
            );
        }
        let e = ip.check_key_sources().unwrap_err();
        assert!(
            e.contains("kind in_process") && e.contains("may use: sidecar, api_provider"),
            "{e}"
        );
        // The environment: never, even in development.
        let mut env = c.clone();
        env.development_keys = true;
        env.adapter.accounts = vec![KeySource::Env("LANE".into())];
        assert!(
            env.check_key_sources()
                .unwrap_err()
                .contains("never read from the environment")
        );
        // A file: development only.
        let mut file = c.clone();
        file.adapter.accounts = vec![KeySource::File("/k".into())];
        assert!(
            file.check_key_sources()
                .unwrap_err()
                .contains("development only")
        );
        file.development_keys = true;
        assert_eq!(file.check_key_sources(), Ok(false));
        // One form: PASS keys beside native accounts are refused, and so is an anchor without lanes.
        let mut mixed = c.clone();
        mixed.adapter.anchor_key = Some(PassKey {
            user: "00".repeat(32),
            device: KeySource::Engine("anchor".into()),
        });
        assert!(mixed.check_key_sources().unwrap_err().contains("one form"));
        let mut alone = c.clone();
        alone.adapter.accounts.clear();
        assert!(
            alone
                .check_key_sources()
                .unwrap_err()
                .contains("needs \"accounts\"")
        );
        // The JSON form: `accounts` and `anchor_account` hold key sources, and a file without
        // `keys` parses.
        let text = serde_json::to_value(&c).unwrap();
        assert!(text["adapter"].get("keys").is_none());
        assert_eq!(
            text["adapter"]["accounts"],
            serde_json::json!([{ "engine": "lane-1" }])
        );
        assert_eq!(
            text["adapter"]["anchor_account"],
            serde_json::json!({ "engine": "anchor" })
        );
        let back: ProcessConfig = serde_json::from_value(text).unwrap();
        assert_eq!(back, c);
    }

    /// The canary's template (`deploy/templates/settle.json`), its `<FILL …>` strings left as they
    /// are: it parses, it connects through the light client and dry-runs there too, it takes
    /// every secret from the environment, it names no runtime list (T-027-12), and it declares
    /// the genesis its Kreivo chain spec (`deploy/templates/chain-specs/kreivo.json`) describes.
    #[test]
    fn the_canary_template_parses_names_its_secrets_and_declares_its_chain_specs_genesis() {
        use blake2::Digest as _;
        let c: ProcessConfig =
            serde_json::from_str(include_str!("../../../deploy/templates/settle.json")).unwrap();
        assert_eq!(c.connection, Connection::LightClient);
        assert!(c.dry_run_endpoint.is_none(), "F-027 NOTES, decision 43");
        // Real value: TLS to PostgreSQL is required (T-016-13).
        assert_eq!(c.profile, ingress::config::Profile::Production);
        assert!(c.ingress_token_file.is_none() && c.ingress_token_env.is_some());
        assert!(c.database.is_none() && c.database_env.is_some());
        let keys = c.adapter.keys.iter().chain(c.adapter.anchor_key.as_ref());
        for k in keys {
            assert!(matches!(k.device, KeySource::Env(_)), "{k:?}");
        }
        // F-035: the canary's keys are development keys, and it says so; production's are the
        // signer service's (`F-035`).
        assert!(c.development_keys && c.signer.is_none());
        assert_eq!(c.check_key_sources(), Ok(false));
        let mut production = c.clone();
        production.development_keys = false;
        assert!(
            production
                .check_key_sources()
                .unwrap_err()
                .contains("NFR-8")
        );
        for k in production.adapter.keys.iter_mut() {
            k.device = KeySource::Signer {
                key: "lane-1".into(),
            };
        }
        production.adapter.anchor_key = None;
        assert!(
            production
                .check_key_sources()
                .unwrap_err()
                .contains("\"signer\"")
        );
        production.signer = Some(signer::client::SignerEndpoint {
            socket: "/run/aledgent-signer/signer.sock".into(),
            client: "settle".into(),
            token_file: "/etc/aledgent/signer_token".into(),
            timeout_ms: None,
        });
        assert_eq!(production.check_key_sources(), Ok(true));
        // T-027-12: no runtime list; the format follows the runtime's surface.
        assert!(c.adapter.spec_versions.is_empty() && c.adapter.formats.is_empty());
        // The chain spec: Kreivo's verified light spec, 12 boot nodes (the single boot node is
        // lifted), and a genesis state root whose genesis block is the one the
        // adapter declares (REQ-SA-2).
        let spec: serde_json::Value = serde_json::from_str(include_str!(
            "../../../deploy/templates/chain-specs/kreivo.json"
        ))
        .unwrap();
        assert_eq!(spec["bootNodes"].as_array().map(Vec::len), Some(12));
        assert_eq!(spec["relay_chain"], "ksmcc3");
        let root = hex::decode(
            spec["genesis"]["stateRootHash"]
                .as_str()
                .unwrap()
                .trim_start_matches("0x"),
        )
        .unwrap();
        let blake2_256 = |bytes: &[u8]| -> [u8; 32] {
            blake2::Blake2b::<blake2::digest::consts::U32>::digest(bytes).into()
        };
        // A genesis header: no parent, number 0, the state root, the empty trie's root as the
        // extrinsics root, and no digest items.
        let mut header = vec![0u8; 32];
        header.push(0);
        header.extend_from_slice(&root);
        header.extend_from_slice(&blake2_256(&[0]));
        header.push(0);
        assert_eq!(Some(blake2_256(&header)), c.adapter.genesis_hash());
    }

    /// `T-016-13`: the profile has no default; the README's example is a production one, and its
    /// connection string requires TLS, as production demands.
    #[cfg(feature = "postgres")]
    #[test]
    fn t_016_13_the_profile_has_no_default_and_the_example_requires_tls() {
        let c: ProcessConfig = serde_json::from_value(readme_example()).unwrap();
        assert_eq!(c.profile, ingress::config::Profile::Production);
        let url: tokio_postgres::Config = c.database.as_deref().unwrap().parse().unwrap();
        assert!(ingress::store::pg::check_tls(&url, c.profile).is_ok());
        let mut v = readme_example();
        v.as_object_mut().unwrap().remove("profile");
        assert!(serde_json::from_value::<ProcessConfig>(v).is_err());
    }

    #[test]
    fn the_readme_example_connects_through_the_light_client() {
        let c: ProcessConfig = serde_json::from_value(readme_example()).unwrap();
        assert_eq!(c.connection, Connection::LightClient);
        assert!(matches!(
            c.chain_source(),
            Ok(ChainSource::LightClient {
                relay_chain_spec: Some(_),
                ..
            })
        ));
    }

    #[test]
    fn the_light_client_is_the_default_connection() {
        let c = with(|m| {
            m.remove("connection");
            m.remove("relay_chain_spec");
        });
        assert_eq!(c.connection, Connection::LightClient);
        assert_eq!(
            c.chain_source(),
            Ok(ChainSource::LightClient {
                chain_spec: c.chain_spec.clone().unwrap(),
                relay_chain_spec: None,
            })
        );
        let c = with(|m| {
            m.remove("chain_spec");
        });
        assert_eq!(
            c.chain_source(),
            Err(ConnectionError::Missing {
                connection: Connection::LightClient,
                field: "chain_spec",
            })
        );
    }

    #[test]
    fn an_endpoint_without_an_explicit_rpc_connection_is_refused_never_used() {
        // A configuration of the earlier form: an endpoint and no connection. The light client
        // is the default, and the endpoint is refused rather than taken as a fallback.
        let c = with(|m| {
            rpc(m);
            m.remove("connection");
        });
        let e = c.chain_source().unwrap_err();
        assert_eq!(
            e,
            ConnectionError::Foreign {
                connection: Connection::LightClient,
                field: "endpoint",
            }
        );
        assert!(e.to_string().contains("\"connection\": \"rpc\""), "{e}");
        // Nor beside a chain spec.
        let c = with(|m| {
            m.insert("endpoint".into(), "ws://127.0.0.1:9944".into());
        });
        assert!(matches!(
            c.chain_source(),
            Err(ConnectionError::Foreign {
                field: "endpoint",
                ..
            })
        ));
    }

    #[test]
    fn rpc_is_used_only_when_named_and_alone() {
        assert_eq!(
            with(rpc).chain_source(),
            Ok(ChainSource::Rpc {
                endpoint: "ws://127.0.0.1:9944".into()
            })
        );
        for field in ["chain_spec", "relay_chain_spec"] {
            let c = with(|m| {
                rpc(m);
                m.insert(field.into(), "/etc/aledgent/spec.json".into());
            });
            assert_eq!(
                c.chain_source(),
                Err(ConnectionError::Foreign {
                    connection: Connection::Rpc,
                    field,
                })
            );
        }
        let c = with(|m| {
            rpc(m);
            m.remove("endpoint");
        });
        assert_eq!(
            c.chain_source(),
            Err(ConnectionError::Missing {
                connection: Connection::Rpc,
                field: "endpoint",
            })
        );
        // No other connection parses.
        let mut v = readme_example();
        v["connection"] = "http".into();
        assert!(serde_json::from_value::<ProcessConfig>(v).is_err());
    }
}
