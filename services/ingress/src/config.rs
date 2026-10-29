// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The deployment's configuration: a JSON file for everything that is not a secret, and the
//! environment for every secret (`REQ-AU-8`).
//!
//! No secret has a default: a deployment without its database URL, sealing key or core root
//! refuses to start rather than fall back to one. The file holds only the SHA-256 of each bearer
//! token, never a token.

use std::collections::BTreeMap;
use std::time::Duration;

use ledger_core::account::Time;
use ledger_core::asset::{AssetId, AssetKind};
use ledger_core::authority::OperationKind;
use ledger_core::custody::{RecoveryConfig, RecoveryKey};
use ledger_core::id::{AccountId, CredentialId, IdentityId, OperationId};
use ledger_core::ledger::{AssetDecl, Founded, Genesis};
use ledger_core::lifecycle::LifecycleConfig;
use ledger_core::principal::Verifier;
use ledger_kernel::posting::OperationRef;
use serde::{Deserialize, Serialize};

use crate::auth::TokenEntry;
use crate::inprocess::CoreGenesis;
use crate::service::Config;
use crate::wire::OperationKindBody;

/// The environment variable holding the PostgreSQL URL.
pub const ENV_DATABASE_URL: &str = "ALEDGENT_INGRESS_DATABASE_URL";
/// The environment variable holding the recipients' sealing key, 32 bytes of hex.
pub const ENV_SEALING_KEY: &str = "ALEDGENT_INGRESS_SEALING_KEY";
/// The environment variable holding the core link's token (`F-035`): what ingress presents to
/// the core host. The host lists its SHA-256 in [`CoreLink::token_sha256`].
pub const ENV_CORE_LINK_TOKEN: &str = "ALEDGENT_CORE_LINK_TOKEN";
/// The environment variable holding the root of the in-process core's key ring, 32 bytes of hex.
/// Development only (`F-035`): the core host takes its keys from the signer service.
pub const ENV_CORE_ROOT: &str = "ALEDGENT_CORE_ROOT";
/// The environment variable holding the in-process core's clerk key (`F-017`), a canonical
/// scalar, 32 bytes of hex. Optional: without it the core issues no disclosure
/// (`ERR-DisclosureUnsupported`). `F-005`'s private-input path replaces it (issue #38).
pub const ENV_CLERK_KEY: &str = "ALEDGENT_CLERK_KEY";
/// The environment variable holding the key that MACs passkey challenges and sessions (`F-028`),
/// 32 bytes of hex. Required when the configuration accepts passkeys; it has no default.
pub const ENV_AUTHN_KEY: &str = "ALEDGENT_INGRESS_AUTHN_KEY";

/// One relying party whose passkeys the deployment accepts (`F-028`, `RISK-10`).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RelyingPartyConfig {
    /// The `rpId`: a lower-case DNS name, such as `api.example.com` for passkeys an operator
    /// registered as Kreivo `Pass` devices.
    pub rp_id: String,
    /// The origins that may use it, matched exactly, such as `https://api.example.com`.
    pub origins: Vec<String>,
    /// Whether new passkeys may be registered under it (default `true`). `false` keeps it for the
    /// passkeys it already has: sign-in and import only.
    #[serde(default = "yes")]
    pub registrations: bool,
}

/// The deployment's passkeys (`F-028`, `DEC-19`). Absent: no passkey is accepted, and no session.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PasskeysConfig {
    /// The relying parties accepted.
    pub relying_parties: Vec<RelyingPartyConfig>,
    /// Whether the user-verified flag is required (default `true`: Kreivo required it at
    /// registration).
    #[serde(default = "yes")]
    pub require_user_verification: bool,
    /// How long a challenge lives, milliseconds (default 120 000).
    #[serde(default = "default_challenge_ttl")]
    pub challenge_ttl_ms: Time,
    /// How long a session lives, milliseconds (default 900 000: Kreivo's own session bound).
    #[serde(default = "default_session_ttl")]
    pub session_ttl_ms: Time,
    /// Per-command approval (amendment 0037, `T-040-07`): `off` (the default), `report_only` or
    /// `enforce`. An approval lives as long as a challenge (`challenge_ttl_ms`).
    #[serde(default)]
    pub approvals: crate::approval::ApprovalMode,
    /// The name of this running environment (amendment 0037, F1 field 4), such as `prod-eu` or
    /// `test`: separates its approvals from another environment's, a restore drill's included.
    /// Required unless `approvals` is `off`. Never share the authentication key between
    /// environments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
}

fn yes() -> bool {
    true
}

fn default_challenge_ttl() -> Time {
    120_000
}

fn default_session_ttl() -> Time {
    900_000
}

impl PasskeysConfig {
    /// The authenticator's passkeys under `key`, with the per-command approval setting for the
    /// book `deployment` (genesis's, 16 bytes of hex; read only when approvals are on), or why the
    /// configuration is refused: an `rpId` that is not a lower-case DNS name, a party with no
    /// origin, an origin that is not `https` (or `http://localhost`), a zero time to live, or
    /// approvals on with no `environment` (amendment 0037).
    pub fn passkeys(
        &self,
        key: authn::token::AuthnKey,
        deployment: &str,
    ) -> Result<crate::auth::Passkeys, String> {
        let approvals = crate::approval::Approvals::new(
            self.approvals,
            self.environment.as_deref(),
            deployment,
        )?;
        if self.relying_parties.is_empty() {
            return Err("passkeys: at least one relying party".into());
        }
        let mut parties = Vec::new();
        for p in &self.relying_parties {
            if !ledger_core::scheme::valid_rp_id(p.rp_id.as_bytes()) {
                return Err(format!(
                    "passkeys: rp_id {} is not a lower-case DNS name",
                    p.rp_id
                ));
            }
            if p.origins.is_empty() {
                return Err(format!("passkeys: rp_id {} has no origin", p.rp_id));
            }
            for o in &p.origins {
                let ok = o.starts_with("https://")
                    || o == "http://localhost"
                    || o.starts_with("http://localhost:");
                if !ok {
                    return Err(format!("passkeys: origin {o} is not https"));
                }
            }
            if parties
                .iter()
                .any(|q: &authn::webauthn::RelyingParty| q.rp_id == p.rp_id)
            {
                return Err(format!("passkeys: rp_id {} is configured twice", p.rp_id));
            }
            parties.push(authn::webauthn::RelyingParty {
                rp_id: p.rp_id.clone(),
                origins: p.origins.clone(),
                registrations: p.registrations,
            });
        }
        if self.challenge_ttl_ms == 0 || self.session_ttl_ms == 0 {
            return Err("passkeys: times to live must not be zero".into());
        }
        Ok(crate::auth::Passkeys::new(
            key,
            parties,
            self.require_user_verification,
            self.challenge_ttl_ms,
            self.session_ttl_ms,
        )
        .with_approvals(approvals))
    }
}

/// What a deployment is, and so which start-up checks bind it (`T-009-14`, `T-016-13`; the
/// owner's ruling of 2026-10-02). The field has no default: a
/// deployment says which it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Profile {
    /// A deployment that holds real value. Ingress and the adapter
    /// refuse to start on a PostgreSQL connection string that does not require TLS
    /// (`sslmode=require`).
    Production,
    /// Tests, the zombienet suite and local runs: the connection string's `sslmode` is honoured
    /// as it stands, `prefer` by default.
    Development,
}

impl Profile {
    /// Whether this profile refuses a database connection that does not require TLS.
    pub fn requires_database_tls(self) -> bool {
        matches!(self, Profile::Production)
    }
}

/// What ingress requires of PostgreSQL before it replies to anyone (`T-009-02`, `REQ-AR-7`): the
/// configuration's `require_sync_standby`, written `true`, `false` or `"rds-multi-az"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncStandby {
    /// `true`: the server must name a synchronous standby (`synchronous_standby_names`).
    Required,
    /// `"rds-multi-az"`: the server is an Amazon RDS instance whose Multi-AZ deployment replicates
    /// every write to a standby in another zone at the storage level, before the commit returns.
    /// PostgreSQL reports no synchronous standby there, so the setting is trusted as declared
    /// (the owner's ruling of 2026-10-02). Ingress checks only
    /// that the server is an RDS instance; whether Multi-AZ is on is the infrastructure's to keep.
    RdsMultiAz,
    /// `false`: no standby. An acknowledged command can be lost with the one instance's storage;
    /// a deployment chooses it only where it accepts that loss.
    NotRequired,
}

impl Serialize for SyncStandby {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            SyncStandby::Required => s.serialize_bool(true),
            SyncStandby::NotRequired => s.serialize_bool(false),
            SyncStandby::RdsMultiAz => s.serialize_str("rds-multi-az"),
        }
    }
}

impl<'de> Deserialize<'de> for SyncStandby {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<SyncStandby, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Flag(bool),
            Named(String),
        }
        match Raw::deserialize(d)? {
            Raw::Flag(true) => Ok(SyncStandby::Required),
            Raw::Flag(false) => Ok(SyncStandby::NotRequired),
            Raw::Named(n) if n == "rds-multi-az" => Ok(SyncStandby::RdsMultiAz),
            Raw::Named(n) => Err(serde::de::Error::custom(format!(
                "require_sync_standby: true, false or \"rds-multi-az\", not {n:?}"
            ))),
        }
    }
}

/// One asset of the genesis registry.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AssetConfig {
    /// Its symbol.
    pub id: String,
    /// Its one precision.
    pub precision: u8,
    /// `fiat`, `stable`, `native` or `other`.
    pub kind: String,
    /// Registered deprecated.
    #[serde(default)]
    pub deprecated: bool,
}

/// The in-process core's genesis (`REQ-AS-2`).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GenesisConfig {
    /// The registry.
    pub assets: Vec<AssetConfig>,
    /// The operator's first verifier, hex (public material).
    pub operator_verifier: String,
    /// The deployment's identity, 16 bytes of hex.
    pub deployment: String,
    /// The decision bound of `REQ-CS-7`, milliseconds.
    pub decision_bound_ms: Time,
    /// The review service level, milliseconds.
    pub review_service_level_ms: Time,
    /// Default deadlines by operation kind, milliseconds from acceptance.
    #[serde(default)]
    pub default_deadlines_ms: BTreeMap<String, Time>,
    /// Genesis's operation reference, 16 bytes of hex.
    pub genesis_op: String,
    /// Genesis's time, milliseconds since the Unix epoch.
    pub genesis_at: Time,
    /// The recovery configuration (`F-012`, `DEC-13`). Absent: the deployment offers delegated
    /// custody only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<RecoveryDeploymentConfig>,
    /// Anchoring (`F-017`): the interval and the assurance declaration. Absent: the default
    /// interval, and no disclosure until a declaration is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchoring: Option<AnchoringDeploymentConfig>,
    /// The independent sources each asset's opening balances reconcile against (`F-014`,
    /// `REQ-RC-4`, `NFR-9`): asset symbol to source identifiers, 16 bytes of hex each. Absent: no
    /// opening set reconciles, so none loads.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub opening_sources: BTreeMap<String, Vec<String>>,
    /// The seed the book starts from (`F-036`, `REQ-RC-6`): what `migrate seed build` printed.
    /// Absent: the book starts empty, as before. Chosen once, with the rest of genesis.
    #[serde(default, alias = "seed", skip_serializing_if = "Option::is_none")]
    pub chainspec: Option<ChainSpecConfig>,
}

/// A seeded genesis's commitment, as the deployment file names it (`F-036`).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainSpecConfig {
    /// The seed digest, 32 bytes of hex.
    pub digest: String,
    /// Its chunks, the header included.
    pub chunks: u32,
}

/// The deployment's anchoring (`F-017`, `REQ-PR-1`, `REQ-AR-5`).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnchoringDeploymentConfig {
    /// The interval between checkpoints, milliseconds: one hour, `3600000` (`REQ-PR-1`).
    pub interval_ms: Time,
    /// The digest of the assurance declaration `F-015` generated for this deployment, 32 bytes
    /// of hex.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assurance_digest: Option<String>,
    /// Its summary, as disclosures show it after the signed-assertion label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assurance_summary: Option<String>,
}

/// The deployment's recovery authority and delays (`F-012`, `REQ-AU-9`, `REQ-AU-15`–`REQ-AU-19`).
/// Only the recovery authority's **public** key is here: its secret stays in `services/recovery`.
/// There is no guardian or threshold field (`REQ-AU-19`). The core checks it at start-up and
/// refuses to start on a bad one.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryDeploymentConfig {
    /// The recovery authority's public key, 32 bytes of hex.
    pub recovery_key: String,
    /// The recovery delay, milliseconds (`DEC-13`: 72 hours).
    pub recovery_delay_ms: Time,
    /// The succession delay, milliseconds, if succession is offered (`DEC-13`: 30 days).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub succession_delay_ms: Option<Time>,
    /// Whether a holder may register a holder-held recovery credential (`REQ-AU-18`).
    #[serde(default)]
    pub holder_held: bool,
}

impl RecoveryDeploymentConfig {
    /// The core's recovery configuration: owned custody offered, with this recovery authority.
    pub fn recovery(&self) -> Result<RecoveryConfig, String> {
        Ok(RecoveryConfig::owned_custody(RecoveryKey(hex_array::<32>(
            "recovery_key",
            &self.recovery_key,
        )?))
        .with_recovery_delay(self.recovery_delay_ms)
        .with_succession_delay(self.succession_delay_ms)
        .with_holder_held(self.holder_held))
    }
}

/// The deployment's configuration file.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentConfig {
    /// Where the surface listens, `host:port`.
    pub listen: String,
    /// This replica's name.
    pub replica: String,
    /// Production or development: which start-up checks bind this deployment ([`Profile`]).
    pub profile: Profile,
    /// What PostgreSQL must guarantee before a reply: `true` (a synchronous standby it names),
    /// `"rds-multi-az"` (an RDS Multi-AZ instance), or `false` ([`SyncStandby`]).
    pub require_sync_standby: SyncStandby,
    /// The credentials ingress recognises.
    pub tokens: Vec<TokenEntry>,
    /// The `DEP-7` switch: whether holder and agent credentials may call.
    #[serde(default)]
    pub allow_holder_credentials: bool,
    /// Passkeys (`F-028`): absent, none is accepted. The key comes from `ENV_AUTHN_KEY`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passkeys: Option<PasskeysConfig>,
    /// The ticker's interval and the leader lease, milliseconds.
    pub tick_interval_ms: u64,
    /// The decision bound of `REQ-CS-7` the oldest-undecided metric is held to, milliseconds.
    pub decision_bound_ms: u64,
    /// The in-process core's journal file.
    pub core_journal: String,
    /// The in-process core's leaf file (`T-031-14`), opt-in: rebuilt from the journal on every
    /// start. Absent (or `memory`): the leaves stay in memory, the default. On disk they cost
    /// about 26 KB a command less memory, but the stress suite measured the core no longer holding
    /// 10 × the reference hour on a 2-vCPU runner (about four in five offered commands decided,
    /// acknowledgement p99 2.2 s; run 37123728717 against 37123730023): redb's writes compete
    /// with PostgreSQL's syncs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core_book: Option<String>,
    /// The core's genesis: the in-process core's, or the core Agent's (`aledgent-core-host`
    /// reads the same file).
    pub genesis: GenesisConfig,
    /// The core in another process (`F-005`): `unix:<path>` or `tcp:<loopback:port>`, where
    /// `aledgent-core-host serve` listens. Absent: the in-process core, over `core_journal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core_endpoint: Option<String>,
    /// The most inputs the core takes in one step (`T-031-04`): the forwarder's batch, and the
    /// core Agent's inputs per invocation (`aledgent-core-host serve`, overridden by its
    /// `--batch`). Absent: [`crate::service::DEFAULT_CORE_BATCH`]. 1 is one input at a time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core_batch: Option<usize>,
    /// Whether ingress takes the settlement adapter's report of a new settlement sequence
    /// (`POST /v1/settlement/epochs`, amendment 0041). **Off by default**: a core host built before
    /// `T-043-14` cannot decode the input and refuses the whole feed it arrives in, so the book's
    /// inputs would wait behind it. Turned on only once the core runs a build that knows it; off,
    /// the route is refused `ERR-MalformedCommand` and nothing is logged.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub settlement_epochs: bool,
    /// How long ingress reuses what the core answered for an API key's credential, milliseconds
    /// (`T-031-13`, [`crate::credentials::VerifierCache`]). A revoked key stops authenticating
    /// within it. Absent: 500; 0 reads the core on every request; above 1 000 (`NFR-20`'s bound)
    /// ingress refuses to start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_cache_ms: Option<u64>,
    /// The audit store the core ships its audit trail to (`T-013-13`, `REQ-AT-2`, `NFR-6`): its
    /// own volume, written with the credential in `ALEDGENT_AUDIT_WRITER_CREDENTIAL`. Read by
    /// whichever process holds the core: `aledgent-core-host serve`, or ingress's in-process
    /// core. Absent: nothing ships, the outbox grows with the book, and `F-018` pages
    /// (`AledgentAuditNotShipped`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_store: Option<crate::audit_ship::AuditStoreSection>,
    /// The core link's credential (`F-035`): required with [`DeploymentConfig::core_endpoint`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core_link: Option<CoreLink>,
    /// The core's keys behind the signer service (`F-035`, guardrail 13): what
    /// `aledgent-core-host serve` signs with. Required by the core host, which takes no key from
    /// its environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core_keys: Option<CoreKeys>,
    /// The signature engines this deployment's components use (`F-042`), by name: each
    /// `{ "<kind>": { … } }`, with `sidecar`, `in_process` or `vault_transit` for a kind. One file
    /// serves several components, and each checks and builds only the engines its own keys name;
    /// the rest stay unparsed.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub engines: std::collections::BTreeMap<String, serde_json::Value>,
    /// The keys, each on an engine (`F-042`): ingress's `feed` here (`{ "engine": "<name>", "key":
    /// { … } }`, the key's locator in that engine). Another component's keys in the same file are
    /// neither parsed nor refused.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub keys: std::collections::BTreeMap<String, serde_json::Value>,
    /// The prior system's identifiers the initial state left out (`REQ-RC-7`, amendment 0024):
    /// the migration's `excluded.jsonl`, checked against the digest the book's chainspec commits
    /// (`T-036-29`). A request naming one is answered `ERR-IdentifierArchived` (410), for good
    /// ([`crate::archived`]). Absent: none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_identifiers: Option<ArchivedIdentifiers>,
    /// How the store's schema migrations run (`T-009-26`, owner ruling `v0w-ingress-migrations`):
    /// what `serve` does with them at start, and the lock timeout every migration runs under.
    /// Absent: apply at start, 5 s.
    #[serde(default, skip_serializing_if = "MigrationsConfig::is_default")]
    pub migrations: MigrationsConfig,
    /// The core stream (`F-038`, `DEC-27`): ingress's listener the core hosts dial
    /// (`stream.listen`), and what the core host dials (`stream.dial`). Absent: the socket link
    /// alone, as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream: Option<StreamSection>,
    /// Where ingress serves `GET /ready` and `GET /live` (`F-038` §10, [`crate::readiness`]),
    /// `host:port`, apart from the client surface. Absent: not served.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub readiness_listen: Option<String>,
}

/// Ingress's feed key as a component declares its needs (`F-042`): an Ed25519 key, on the signer
/// service, in a key file, or in Vault Transit. It holds no book.
pub const FEED_KEY: signer::engine::config::KeyNeed = signer::engine::config::KeyNeed {
    name: "feed",
    scheme: signer::proto::Scheme::Ed25519,
    kinds: &[
        signer::engine::EngineKind::Sidecar,
        signer::engine::EngineKind::ApiProvider,
        signer::engine::EngineKind::InProcess,
    ],
};

/// The kinds of engine the core host's keys may use (`F-042`): the sidecar only. The key ring and
/// the clerk, because only the sidecar and in-process offer their schemes; and the Agent's keys too,
/// because the core host cannot reach Vault or AWS credentials (the node's metadata service is locked
/// to it) and `NFR-8` bars key material in the book's process, so neither an API provider nor an
/// in-process key is for it.
const SIDECAR_ONLY: &[signer::engine::EngineKind] = &[signer::engine::EngineKind::Sidecar];

/// The core host's key ring.
pub const CORE_KEY_RING: signer::engine::config::KeyNeed = signer::engine::config::KeyNeed {
    name: "key_ring",
    scheme: signer::proto::Scheme::KeyRing,
    kinds: SIDECAR_ONLY,
};
/// The core host's clerk key, if disclosures are issued.
pub const CORE_CLERK: signer::engine::config::KeyNeed = signer::engine::config::KeyNeed {
    name: "clerk",
    scheme: signer::proto::Scheme::Clerk,
    kinds: SIDECAR_ONLY,
};
/// The Agent's authority key (VOS receipts).
pub const CORE_AGENT_AUTHORITY: signer::engine::config::KeyNeed = signer::engine::config::KeyNeed {
    name: "agent_authority",
    scheme: signer::proto::Scheme::Ed25519,
    kinds: SIDECAR_ONLY,
};
/// The runtime package key.
pub const CORE_AGENT_PACKAGE: signer::engine::config::KeyNeed = signer::engine::config::KeyNeed {
    name: "agent_package",
    scheme: signer::proto::Scheme::Ed25519,
    kinds: SIDECAR_ONLY,
};

/// The key ring and the clerk key, open: what a process that opens the book without running the Agent
/// needs (`opsctl restore-core`). The Agent's keys are not among them.
pub struct HostKeyHandles {
    /// The key ring.
    pub key_ring: std::sync::Arc<dyn signer::engine::KeyHandle>,
    /// The clerk key, if disclosures are issued.
    pub clerk: Option<std::sync::Arc<dyn signer::engine::KeyHandle>>,
}

/// The core's keys, open (`F-042`): where they are is the deployment's `core_keys` (the signer
/// service) or its `keys`, each on an engine.
pub struct CoreKeyHandles {
    /// The key ring.
    pub key_ring: std::sync::Arc<dyn signer::engine::KeyHandle>,
    /// The clerk key, if disclosures are issued.
    pub clerk: Option<std::sync::Arc<dyn signer::engine::KeyHandle>>,
    /// The Agent's authority key.
    pub agent_authority: std::sync::Arc<dyn signer::engine::KeyHandle>,
    /// The runtime package key.
    pub agent_package: std::sync::Arc<dyn signer::engine::KeyHandle>,
}

/// The variable holding the core host's token for a stream over a Unix socket (`F-038`): its
/// SHA-256 is what `stream.link.token_sha256` lists on ingress's side.
pub const ENV_CORE_STREAM_TOKEN: &str = "ALEDGENT_CORE_STREAM_TOKEN";

/// The feed key behind the signer service (`F-038` §5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeedKeyConfig {
    /// Where the signer is, and ingress's token file.
    pub signer: signer::client::SignerEndpoint,
    /// The Ed25519 key's identifier.
    pub key: String,
}

/// What the core host dials (`F-038` §11).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DialSection {
    /// Every address: `tls:<host:port>` or `unix:<path>`. One stream each.
    pub connect: Vec<String>,
    /// The name ingress's certificate carries, for `tls:` addresses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_name: Option<String>,
    /// The core's name in ingress's logs. Absent: `core`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub core: Option<String>,
    /// The partitions it writes. Absent: the book ([`crate::feed::BOOK`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub partitions: Option<Vec<String>>,
    /// The user ids ingress may run as, for `unix:` addresses: the core host checks the socket's
    /// peer credentials against them before it sends its token. Required with a `unix:` address.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub peer_uids: Vec<u32>,
}

/// The core stream's section (`F-038` §11).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamSection {
    /// Ingress's listener for cores: `tls:<host:port>` (with `tls`) or `unix:<path>` (with
    /// `link`). With it, ingress serves the stream instead of dialing `core_endpoint`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listen: Option<String>,
    /// This side's TLS files, for a `tls:` listener or `tls:` dial addresses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls: Option<crate::stream::tls::TlsFiles>,
    /// Who a `unix:` listener lets in: the SHA-256 of the core host's token and its user ids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<CoreLink>,
    /// Ingress's feed key behind the signer service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feed_key: Option<FeedKeyConfig>,
    /// A file holding the feed key's seed, hex: the development profile only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub development_feed_key: Option<String>,
    /// What the core host dials.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dial: Option<DialSection>,
    /// The feed keys' public halves the core host accepts, hex (two while one rotates). Required
    /// with `dial`, and for the core host in production on every link
    /// ([`DeploymentConfig::core_feed_keys`]); with them, the host takes no unsigned input.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ingress_keys: Vec<String>,
    /// How often each side pings, milliseconds. Absent: 1 000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keepalive_ms: Option<u64>,
    /// How long a side waits on silence before it drops the stream, milliseconds. Absent: 3 000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dead_after_ms: Option<u64>,
    /// How long a draining ingress keeps answering clients after its go-away, milliseconds.
    /// Absent: 5 000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub drain_ms: Option<u64>,
    /// The address this instance's region's cores reach it at, advertised in the fleet's roster
    /// (`T-038-21`): `tls:<host:port>` or `unix:<path>`. Absent: detected from `listen`
    /// ([`StreamSection::advertised_address`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advertise: Option<String>,
    /// How often an instance renews its roster entry, and a core host reads the roster again,
    /// milliseconds. Absent: 2 000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roster_ms: Option<u64>,
}

/// The address of the interface this host routes from: a UDP socket "connected" to a
/// documentation address (RFC 5737, RFC 3849), which sends nothing, and its local address read
/// back.
fn routed_ip(v6: bool) -> Option<std::net::IpAddr> {
    let (bind, probe) = if v6 {
        ("[::]:0", "[2001:db8::1]:9")
    } else {
        ("0.0.0.0:0", "192.0.2.1:9")
    };
    let s = std::net::UdpSocket::bind(bind).ok()?;
    s.connect(probe).ok()?;
    let ip = s.local_addr().ok()?.ip();
    (!ip.is_unspecified()).then_some(ip)
}

/// How long a draining ingress keeps answering, by default.
pub const DEFAULT_DRAIN: Duration = Duration::from_secs(5);

impl StreamSection {
    /// Check the section for `profile` (`T-038-14`): every file a transport needs, a feed key for
    /// a production listener, a development key only in development, the keys a dialer checks.
    pub fn check(&self, profile: Profile) -> Result<(), String> {
        self.check_with(profile, false)
    }

    /// [`StreamSection::check`], when the feed key may instead be the deployment's `keys.feed`
    /// (`F-042`): `feed_key_in_keys` is whether the file gives it.
    pub fn check_with(&self, profile: Profile, feed_key_in_keys: bool) -> Result<(), String> {
        if let Some(l) = &self.listen {
            match crate::stream::Address::parse(l)? {
                crate::stream::Address::Tls(_) if self.tls.is_none() => {
                    return Err("stream.listen is tls: and stream.tls is missing".into());
                }
                crate::stream::Address::Unix(_) if self.link.is_none() => {
                    return Err("stream.listen is unix: and stream.link is missing".into());
                }
                _ => {}
            }
            if profile == Profile::Production && self.feed_key.is_none() && !feed_key_in_keys {
                return Err(
                    "stream.listen in production needs stream.feed_key or keys.feed: every \
                     forwarded command is signed (F-038)"
                        .into(),
                );
            }
        }
        if let Some(a) = &self.advertise {
            crate::stream::Address::parse(a).map_err(|e| format!("stream.advertise: {e}"))?;
        }
        if self.development_feed_key.is_some() && profile != Profile::Development {
            return Err(
                "stream.development_feed_key outside the development profile: production keys \
                 live in the signer (guardrail 13)"
                    .into(),
            );
        }
        if self.feed_key.is_some() && self.development_feed_key.is_some() {
            return Err("stream.feed_key and stream.development_feed_key: one of them".into());
        }
        if let Some(d) = &self.dial {
            if d.connect.is_empty() {
                return Err("stream.dial.connect is empty".into());
            }
            for a in &d.connect {
                match crate::stream::Address::parse(a)? {
                    crate::stream::Address::Tls(_) => {
                        if self.tls.is_none() {
                            return Err(format!("stream.dial: {a} needs stream.tls"));
                        }
                        if d.server_name.is_none() {
                            return Err(format!("stream.dial: {a} needs stream.dial.server_name"));
                        }
                    }
                    crate::stream::Address::Unix(_) if d.peer_uids.is_empty() => {
                        return Err(format!(
                            "stream.dial: {a} needs stream.dial.peer_uids: the core host sends \
                             its token only to ingress's user id"
                        ));
                    }
                    crate::stream::Address::Unix(_) => {}
                }
            }
            crate::feed::FeedKeys::parse(&self.ingress_keys)?;
        }
        Ok(())
    }

    /// The stream's timings.
    pub fn timings(&self) -> crate::stream::Timings {
        let d = crate::stream::Timings::default();
        crate::stream::Timings {
            keepalive: self.keepalive_ms.map_or(d.keepalive, Duration::from_millis),
            dead_after: self
                .dead_after_ms
                .map_or(d.dead_after, Duration::from_millis),
            roster: self.roster_ms.map_or(d.roster, Duration::from_millis),
            ..d
        }
    }

    /// The address this instance advertises in the fleet's roster (`T-038-21`): `advertise` as
    /// configured; or `listen` itself when it names a Unix socket or a specific host; or, for a
    /// listener on every interface (`0.0.0.0`, `[::]`), the address of the interface this host
    /// routes from (a task's or pod's own address on ECS or Kubernetes), with `listen`'s port.
    pub fn advertised_address(&self) -> Result<String, String> {
        if let Some(a) = &self.advertise {
            crate::stream::Address::parse(a)?;
            return Ok(a.clone());
        }
        let listen = self.listen.as_deref().ok_or("stream.listen is missing")?;
        let crate::stream::Address::Tls(addr) = crate::stream::Address::parse(listen)? else {
            return Ok(listen.to_owned());
        };
        let bound: std::net::SocketAddr = match addr.parse() {
            Ok(a) => a,
            // A host name: the cores resolve it as this instance does.
            Err(_) => return Ok(listen.to_owned()),
        };
        if !bound.ip().is_unspecified() {
            return Ok(listen.to_owned());
        }
        let ip = routed_ip(bound.is_ipv6()).ok_or(
            "stream.advertise: this instance's address could not be detected (no route); set it",
        )?;
        Ok(format!(
            "tls:{}",
            std::net::SocketAddr::new(ip, bound.port())
        ))
    }

    /// How long a draining ingress keeps answering.
    pub fn drain(&self) -> Duration {
        self.drain_ms.map_or(DEFAULT_DRAIN, Duration::from_millis)
    }

    /// The keys the core host checks feeds with, if any are listed.
    pub fn ingress_keys(&self) -> Result<Option<crate::feed::FeedKeys>, String> {
        if self.ingress_keys.is_empty() {
            return Ok(None);
        }
        crate::feed::FeedKeys::parse(&self.ingress_keys).map(Some)
    }

    /// Ingress's feed signer: the signer service's key, a development file, or none.
    pub fn feed_signer(
        &self,
        profile: Profile,
    ) -> Result<Option<crate::service::SharedFeedSigner>, String> {
        self.check(profile)?;
        if let Some(k) = &self.feed_key {
            let key = crate::feed::ServiceFeedKey::connect(&k.signer, &k.key)?;
            return Ok(Some(crate::service::SharedFeedSigner(std::sync::Arc::new(
                key,
            ))));
        }
        if let Some(path) = &self.development_feed_key {
            let key = crate::feed::LocalFeedKey::from_file(std::path::Path::new(path))?;
            return Ok(Some(crate::service::SharedFeedSigner(std::sync::Arc::new(
                key,
            ))));
        }
        Ok(None)
    }

    /// Bind ingress's listener for cores, if `listen` names one.
    pub async fn bind(&self) -> Result<Option<crate::stream::Listen>, String> {
        let Some(l) = &self.listen else {
            return Ok(None);
        };
        Ok(Some(match crate::stream::Address::parse(l)? {
            crate::stream::Address::Tls(addr) => {
                let tls = self.tls.as_ref().ok_or("stream.tls is missing")?;
                crate::stream::Listen::Tls {
                    acceptor: tls.acceptor()?,
                    listener: tokio::net::TcpListener::bind(&addr)
                        .await
                        .map_err(|e| format!("stream.listen {addr}: {e}"))?,
                }
            }
            crate::stream::Address::Unix(path) => {
                let link = self.link.as_ref().ok_or("stream.link is missing")?;
                let _ = std::fs::remove_file(&path);
                crate::stream::Listen::Unix {
                    auth: link.auth()?,
                    listener: tokio::net::UnixListener::bind(&path)
                        .map_err(|e| format!("stream.listen {}: {e}", path.display()))?,
                }
            }
        }))
    }

    /// What the core host dials, if `dial` is set; a `unix:` address takes its token from
    /// `token` (the environment's [`ENV_CORE_STREAM_TOKEN`] in a deployment).
    pub fn dial_config(
        &self,
        token: impl Fn() -> Option<String>,
    ) -> Result<Option<crate::stream::DialConfig>, String> {
        let Some(d) = &self.dial else {
            return Ok(None);
        };
        let mut targets = Vec::with_capacity(d.connect.len());
        for a in &d.connect {
            targets.push(match crate::stream::Address::parse(a)? {
                crate::stream::Address::Tls(addr) => {
                    let tls = self.tls.as_ref().ok_or("stream.tls is missing")?;
                    let name = d
                        .server_name
                        .clone()
                        .ok_or("stream.dial.server_name is missing")?;
                    crate::stream::Target::Tls {
                        addr,
                        server_name: rustls_pki_types::ServerName::try_from(name)
                            .map_err(|e| format!("stream.dial.server_name: {e}"))?,
                        connector: tls.connector()?,
                    }
                }
                crate::stream::Address::Unix(path) => crate::stream::Target::Unix {
                    path,
                    peer_uids: d.peer_uids.clone(),
                    token: crate::remote::LinkToken::new(&token().ok_or_else(|| {
                        format!("{ENV_CORE_STREAM_TOKEN} is not set: refusing to start")
                    })?)?,
                },
            });
        }
        let partitions = d
            .partitions
            .clone()
            .unwrap_or_else(|| vec![crate::feed::BOOK.to_owned()]);
        Ok(Some(crate::stream::DialConfig {
            targets,
            core: d.core.clone().unwrap_or_else(|| "core".into()),
            principals: partitions.iter().any(|p| p == crate::feed::BOOK),
            partitions,
            timings: self.timings(),
        }))
    }
}

/// The lock timeout a migration runs under when the configuration names none: 5 s.
pub const DEFAULT_MIGRATION_LOCK_TIMEOUT_MS: u64 = 5_000;

fn default_migration_lock_timeout_ms() -> u64 {
    DEFAULT_MIGRATION_LOCK_TIMEOUT_MS
}

/// The configuration's `migrations` section (`T-009-26`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationsConfig {
    /// What `serve` does with the migrations at start ([`OnStart`]). Default `"apply"`.
    #[serde(default)]
    pub on_start: OnStart,
    /// How long a migration waits for a lock on a table it changes before it gives up,
    /// milliseconds: `SET LOCAL lock_timeout` at the start of every migration's transaction. A
    /// migration that gives up records nothing and fails the start; the next start retries.
    /// Default 5 000; 0 is refused (PostgreSQL reads it as no bound).
    #[serde(default = "default_migration_lock_timeout_ms")]
    pub lock_timeout_ms: u64,
}

impl Default for MigrationsConfig {
    fn default() -> MigrationsConfig {
        MigrationsConfig {
            on_start: OnStart::Apply,
            lock_timeout_ms: DEFAULT_MIGRATION_LOCK_TIMEOUT_MS,
        }
    }
}

impl MigrationsConfig {
    fn is_default(&self) -> bool {
        *self == MigrationsConfig::default()
    }

    /// The lock timeout, checked: 0 would let a migration wait forever.
    pub fn lock_timeout(&self) -> Result<Duration, String> {
        if self.lock_timeout_ms == 0 {
            return Err(
                "migrations.lock_timeout_ms is 0, which PostgreSQL reads as no bound: refusing to \
                 start"
                    .into(),
            );
        }
        Ok(Duration::from_millis(self.lock_timeout_ms))
    }
}

/// What `serve` does with the store's migrations at start (`migrations.on_start`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnStart {
    /// `"apply"`: apply every pending migration, then serve. The default, as before `T-009-26`.
    #[default]
    Apply,
    /// `"verify"`: change nothing; refuse to start if a migration is pending or an applied one
    /// differs from this build's file. For replicas started after `aledgent-ingress migrate
    /// --apply` has run, with a role that may not change the schema.
    Verify,
}

/// Where the archived identifiers are, and the chainspec that commits their digest (`REQ-RC-7`,
/// `T-036-29`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchivedIdentifiers {
    /// The file (`excluded.jsonl`, format `aledgent/excluded/v1`).
    pub path: String,
    /// The book's chainspec (`chainspec.bin`): the one `genesis.chainspec` commits, whose header's
    /// `excluded_sha256` the file must hash to.
    pub chainspec: String,
}

impl DeploymentConfig {
    /// The keys the core host checks ingress's feeds against (`F-038` §5, `DEC-27` (f)), on every
    /// link it serves. A host with keys takes no unsigned or unsequenced input from the moment it
    /// starts ([`crate::feed::Sequencer`]). In production they are required once the host dials
    /// the new link (`stream.dial`), and it refuses to start without them. A production host
    /// without `stream.dial` and without keys serves the legacy socket link alone (`F-038` §12
    /// step 0): it takes the unsequenced inputs of an ingress of an earlier release, exactly as a
    /// host of that release does, so the next core host build still runs against a legacy ingress
    /// during the transition.
    pub fn core_feed_keys(&self) -> Result<Option<crate::feed::FeedKeys>, String> {
        let keys = match &self.stream {
            Some(s) => s.ingress_keys()?,
            None => None,
        };
        let dials = self.stream.as_ref().is_some_and(|s| s.dial.is_some());
        if keys.is_none() && self.profile == Profile::Production && dials {
            return Err(
                "stream.dial in production needs stream.ingress_keys: the core host takes no \
                 unsigned input on the new link (F-038 §5); configure them with ingress's \
                 stream.feed_key"
                    .into(),
            );
        }
        Ok(keys)
    }

    /// Whether the file gives a feed key that goes through the signer service or an engine:
    /// `stream.feed_key` (the form written before `F-042`) or `keys.feed`.
    pub fn has_feed_key(&self) -> bool {
        self.stream.as_ref().is_some_and(|s| s.feed_key.is_some())
            || self.keys.contains_key(FEED_KEY.name)
    }

    fn engine_section(&self) -> signer::engine::config::Section<'_> {
        signer::engine::config::Section {
            engines: &self.engines,
            keys: &self.keys,
        }
    }

    /// Check the `stream` section and the feed key (`F-042`): the section as it was, and `keys.feed`
    /// on its engine (a scheme the engine offers, a kind the key may use, a locator it accepts),
    /// not given together with `stream.feed_key` or `stream.development_feed_key`. Only the keys
    /// ingress declares are read; the rest of `engines` and `keys` is another component's.
    pub fn check_stream(&self) -> Result<(), String> {
        let in_keys = self.keys.contains_key(FEED_KEY.name);
        if let Some(s) = &self.stream {
            s.check_with(self.profile, in_keys)?;
            if in_keys && s.feed_key.is_some() {
                return Err(
                    "keys.feed and stream.feed_key: one of them (the engines' form and the form \
                     written before F-042)"
                        .into(),
                );
            }
            if in_keys && s.development_feed_key.is_some() {
                return Err("keys.feed and stream.development_feed_key: one of them".into());
            }
        }
        if in_keys && self.stream.is_some() {
            signer::engine::config::check(
                &signer::engine::config::Registry::builtin(),
                &self.engine_section(),
                &[FEED_KEY],
                self.profile == Profile::Development,
            )?;
        }
        Ok(())
    }

    /// The core's keys as the engines' form gives them: `keys.key_ring`, `keys.clerk` (optional),
    /// `keys.agent_authority`, `keys.agent_package`. They are the sidecar's only
    /// ([`CORE_KEY_RING`]).
    fn core_needs(&self) -> Vec<signer::engine::config::KeyNeed<'static>> {
        let mut needs = vec![CORE_KEY_RING, CORE_AGENT_AUTHORITY, CORE_AGENT_PACKAGE];
        if self.keys.contains_key(CORE_CLERK.name) {
            needs.push(CORE_CLERK);
        }
        needs
    }

    /// The keys a process that opens the book without running the Agent needs: the key ring, and the
    /// clerk key when the file gives one. A deployment whose grants for that process are narrower than
    /// the core host's (no Agent keys) must keep working.
    pub fn host_needs(&self) -> Vec<signer::engine::config::KeyNeed<'static>> {
        let mut needs = vec![CORE_KEY_RING];
        if self.keys.contains_key(CORE_CLERK.name) {
            needs.push(CORE_CLERK);
        }
        needs
    }

    /// Whether the file gives the core's keys in the engines' form.
    fn core_keys_in_keys(&self) -> bool {
        self.keys.contains_key(CORE_KEY_RING.name)
    }

    /// Check the core's keys (`F-042`): `core_keys` or `keys`, not both; in the engines' form, each
    /// key on an engine that offers its scheme and is of the sidecar's kind. Only the keys the core
    /// host declares are read; ingress's `feed` in the same file is not.
    pub fn check_core_keys(&self) -> Result<(), String> {
        if self.core_keys.is_some() && self.core_keys_in_keys() {
            return Err(
                "core_keys and keys.key_ring: one of them (the form written before F-042, and the \
                 engines' form)"
                    .into(),
            );
        }
        if self.core_keys_in_keys() {
            signer::engine::config::check(
                &signer::engine::config::Registry::builtin(),
                &self.engine_section(),
                &self.core_needs(),
                self.profile == Profile::Development,
            )?;
        }
        Ok(())
    }

    /// The core's keys, open: connect to the signer (or the engines the file names) and open each
    /// key. `None` when the file gives neither form. No key is read here: the handles hold
    /// identifiers and public halves.
    pub fn core_key_handles(&self) -> Result<Option<CoreKeyHandles>, String> {
        use signer::proto::Scheme;
        self.check_core_keys()?;
        if let Some(k) = &self.core_keys {
            let engine = signer::engine::sidecar::SidecarEngine::connect(&k.signer)
                .map_err(|e| e.to_string())?;
            let open = |id: &str, scheme: Scheme, what: &str| {
                engine
                    .handle(id, scheme)
                    .map(|h| h as std::sync::Arc<dyn signer::engine::KeyHandle>)
                    .map_err(|e| format!("core_keys.{what}: {e}"))
            };
            let clerk = match &k.clerk {
                Some(id) => Some(open(id, Scheme::Clerk, "clerk")?),
                None => None,
            };
            return Ok(Some(CoreKeyHandles {
                key_ring: open(&k.key_ring, Scheme::KeyRing, "key_ring")?,
                clerk,
                agent_authority: open(&k.agent_authority, Scheme::Ed25519, "agent_authority")?,
                agent_package: open(&k.agent_package, Scheme::Ed25519, "agent_package")?,
            }));
        }
        if !self.core_keys_in_keys() {
            return Ok(None);
        }
        let needs = self.core_needs();
        let opened = signer::engine::config::open(
            &signer::engine::config::Registry::builtin(),
            &self.engine_section(),
            &needs,
            self.profile == Profile::Development,
        )?;
        let get = |n: &str| {
            opened
                .handle(n)
                .ok_or_else(|| format!("keys.{n}: not opened"))
        };
        Ok(Some(CoreKeyHandles {
            key_ring: get(CORE_KEY_RING.name)?,
            clerk: opened.handle(CORE_CLERK.name),
            agent_authority: get(CORE_AGENT_AUTHORITY.name)?,
            agent_package: get(CORE_AGENT_PACKAGE.name)?,
        }))
    }

    /// The key ring and the clerk key only, open (`opsctl restore-core`): the same two keys it opened
    /// before `F-042` and no others, whichever form the file is in. `None` when the file gives neither
    /// form. In the form written before `F-042` it opens the key ring and, if named, the clerk, and
    /// does not touch `agent_authority` or `agent_package`; in the engines' form only the keys of
    /// [`DeploymentConfig::host_needs`] are checked and opened.
    pub fn host_key_handles(&self) -> Result<Option<HostKeyHandles>, String> {
        use signer::proto::Scheme;
        if self.core_keys.is_some() && self.core_keys_in_keys() {
            return Err("core_keys and keys.key_ring: one of them".into());
        }
        if let Some(k) = &self.core_keys {
            let engine = signer::engine::sidecar::SidecarEngine::connect(&k.signer)
                .map_err(|e| e.to_string())?;
            let open = |id: &str, scheme: Scheme, what: &str| {
                engine
                    .handle(id, scheme)
                    .map(|h| h as std::sync::Arc<dyn signer::engine::KeyHandle>)
                    .map_err(|e| format!("core_keys.{what}: {e}"))
            };
            let clerk = match &k.clerk {
                Some(id) => Some(open(id, Scheme::Clerk, "clerk")?),
                None => None,
            };
            return Ok(Some(HostKeyHandles {
                key_ring: open(&k.key_ring, Scheme::KeyRing, "key_ring")?,
                clerk,
            }));
        }
        if !self.core_keys_in_keys() {
            return Ok(None);
        }
        let needs = self.host_needs();
        let opened = signer::engine::config::open(
            &signer::engine::config::Registry::builtin(),
            &self.engine_section(),
            &needs,
            self.profile == Profile::Development,
        )?;
        Ok(Some(HostKeyHandles {
            key_ring: opened
                .handle(CORE_KEY_RING.name)
                .ok_or("keys.key_ring: not opened")?,
            clerk: opened.handle(CORE_CLERK.name),
        }))
    }

    /// Ingress's feed signer (`F-038` §5): `keys.feed` opened on its engine, or the form written
    /// before `F-042`, or a development file, or none (the legacy link, which signs nothing).
    pub fn feed_signer(&self) -> Result<Option<crate::service::SharedFeedSigner>, String> {
        let Some(stream) = &self.stream else {
            return Ok(None);
        };
        self.check_stream()?;
        if self.keys.contains_key(FEED_KEY.name) {
            let opened = signer::engine::config::open(
                &signer::engine::config::Registry::builtin(),
                &self.engine_section(),
                &[FEED_KEY],
                self.profile == Profile::Development,
            )?;
            let handle = opened
                .handle(FEED_KEY.name)
                .ok_or("keys.feed: not opened")?;
            let key = crate::feed::EngineFeedKey::new(handle, opened)?;
            return Ok(Some(crate::service::SharedFeedSigner(std::sync::Arc::new(
                key,
            ))));
        }
        stream.feed_signer(self.profile)
    }

    /// Check that ingress signs what it forwards to a core in another process (`F-038` §5, §12):
    /// in production, an ingress on `core_endpoint` with the new link configured (a `stream`
    /// section, [`DeploymentConfig::forward_link`]) needs `stream.feed_key`, as one with
    /// `stream.listen` does ([`StreamSection::check`]), since the core host with keys refuses
    /// unsigned inputs. Without a `stream` section ingress forwards on the legacy link, which
    /// carries no feed and no signature, and needs no feed key (`F-038` §12 step 0).
    pub fn check_forward_signing(&self) -> Result<(), String> {
        let signs = self.has_feed_key();
        let sequenced = self.forward_link() == crate::service::ForwardLink::Sequenced;
        if self.profile == Profile::Production
            && self.core_endpoint.is_some()
            && sequenced
            && !signs
        {
            return Err(
                "core_endpoint in production with a stream section needs stream.feed_key: every \
                 command forwarded on the new link is signed, and the core host refuses unsigned \
                 ones (F-038 §5)"
                    .into(),
            );
        }
        Ok(())
    }

    /// The archived identifiers this deployment names, read and checked against the digest its
    /// genesis's chainspec commits (`T-036-29`); none when it names none.
    pub fn archived(&self) -> Result<crate::archived::ArchivedIds, String> {
        let Some(a) = &self.archived_identifiers else {
            return Ok(crate::archived::ArchivedIds::default());
        };
        let c = self.genesis.chainspec.as_ref().ok_or(
            "archived_identifiers: the genesis commits no chainspec, so it commits no archived \
             identifiers (REQ-RC-7)",
        )?;
        let committed = ledger_core::chainspec::SpecCommitment {
            digest: hex_array::<32>("seed.digest", &c.digest)?,
            chunks: c.chunks,
        };
        let spec_bytes =
            std::fs::read(&a.chainspec).map_err(|e| format!("{}: {e}", a.chainspec))?;
        let spec = ledger_core::chainspec::ChainSpec::from_bytes(&spec_bytes)
            .map_err(|e| format!("{}: {e:?}", a.chainspec))?;
        if spec.commitment() != committed {
            return Err(format!(
                "{}: not the chainspec this genesis commits",
                a.chainspec
            ));
        }
        let digest = spec.header().excluded_sha256.ok_or_else(|| {
            format!(
                "{}: the chainspec commits no exclusion file (format 3's excluded_sha256)",
                a.chainspec
            )
        })?;
        let bytes = std::fs::read(&a.path).map_err(|e| format!("{}: {e}", a.path))?;
        crate::archived::ArchivedIds::parse(&bytes, &digest).map_err(|e| format!("{}: {e}", a.path))
    }
}

/// The core link's credential (`F-035`, `crate::remote`): the SHA-256 of each token the core host
/// accepts (two while one rotates), and the user ids ingress may run as.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoreLink {
    /// SHA-256 of each accepted token, hex. Ingress's own is in [`ENV_CORE_LINK_TOKEN`].
    pub token_sha256: Vec<String>,
    /// The user ids a peer on the Unix socket may run as (its peer credentials). Empty: any user
    /// the socket's permissions let connect, which a deployment should not rely on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub peer_uids: Vec<u32>,
}

impl CoreLink {
    /// The host's side of the link.
    pub fn auth(&self) -> Result<crate::remote::LinkAuth, String> {
        crate::remote::LinkAuth::new(&self.token_sha256, &self.peer_uids)
    }
}

/// The core's keys, by their identifiers in the signer service (`F-035`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoreKeys {
    /// Where the signer is, and the core host's token file.
    pub signer: signer::client::SignerEndpoint,
    /// The key ring (the root of every kernel key, blinding and nonce).
    pub key_ring: String,
    /// The clerk key, if disclosures are issued.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clerk: Option<String>,
    /// The Agent's authority key (VOS receipts).
    pub agent_authority: String,
    /// The runtime package key.
    pub agent_package: String,
}

fn hex_array<const N: usize>(what: &str, s: &str) -> Result<[u8; N], String> {
    hex::decode(s)
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or_else(|| format!("{what} must be {N} bytes of hex"))
}

fn kind_of(name: &str) -> Result<OperationKind, String> {
    serde_json::from_value::<OperationKindBody>(serde_json::Value::String(name.into()))
        .map(crate::command::op_kind)
        .map_err(|_| format!("unknown operation kind {name}"))
}

impl GenesisConfig {
    /// What genesis founds: the operator's organisation identity, its first credential and the
    /// issuance account. They are minted from genesis's operation reference alone, so they are
    /// known from this file before the first start, with no secret: the operator names them in
    /// `tokens` (`aledgent-ingress founded`), since every credential ingress recognises is
    /// configured, and the first one cannot be issued through the surface it would open.
    pub fn founded(&self) -> Result<Founded, String> {
        let op = OperationId::from_ref(OperationRef(hex_array::<16>(
            "genesis_op",
            &self.genesis_op,
        )?));
        Ok(Founded {
            operator: IdentityId::mint(&op, 0),
            operator_credential: CredentialId::mint(&op, 1),
            issuance: AccountId::mint(&op, 2),
        })
    }

    /// The core's genesis, with the key ring's root from the environment.
    pub fn core_genesis(&self, root: [u8; 32]) -> Result<CoreGenesis, String> {
        let (genesis, op, at) = self.agent_genesis()?;
        Ok(CoreGenesis {
            genesis,
            op,
            at,
            root,
        })
    }

    /// The core's genesis without a key: what the core host founds its Agent on, its keys being
    /// the signer's (`F-035`).
    pub fn agent_genesis(&self) -> Result<(Genesis, OperationId, Time), String> {
        let mut assets = Vec::new();
        for a in &self.assets {
            assets.push(AssetDecl {
                id: AssetId::parse(&a.id).map_err(|r| format!("asset {}: {}", a.id, r.code()))?,
                // A deployment's configuration registers book-only assets only: each is bound
                // later by its issuance authority (amendment 0011, `REQ-AS-7`, `REQ-AS-8`).
                location: None,
                precision: a.precision,
                kind: match a.kind.as_str() {
                    "fiat" => AssetKind::Fiat,
                    "stable" => AssetKind::Stable,
                    "native" => AssetKind::Native,
                    "other" => AssetKind::Other,
                    k => return Err(format!("unknown asset kind {k}")),
                },
                deprecated: a.deprecated,
            });
        }
        let mut lifecycle = LifecycleConfig::new(
            hex_array::<16>("deployment", &self.deployment)?,
            self.decision_bound_ms,
            self.review_service_level_ms,
        );
        for (k, after) in &self.default_deadlines_ms {
            lifecycle = lifecycle.with_default_deadline(kind_of(k)?, *after);
        }
        if let Some(r) = &self.recovery {
            lifecycle = lifecycle.with_recovery(r.recovery()?);
        }
        for (asset, sources) in &self.opening_sources {
            let id = ledger_core::asset::AssetId::parse(asset)
                .map_err(|_| format!("opening_sources: {asset} is not an asset symbol"))?;
            let mut set = std::collections::BTreeSet::new();
            for s in sources {
                set.insert(ledger_core::opening::OpeningSource(hex_array::<16>(
                    "opening_sources",
                    s,
                )?));
            }
            let set = ledger_core::set::NonEmptySet::new(set)
                .ok_or_else(|| format!("opening_sources: {asset} names no source"))?;
            lifecycle = lifecycle.with_opening_sources(id, set);
        }
        let chainspec = match &self.chainspec {
            None => None,
            Some(s) => Some(ledger_core::chainspec::SpecCommitment {
                digest: hex_array::<32>("seed.digest", &s.digest)?,
                chunks: s.chunks,
            }),
        };
        if let Some(a) = &self.anchoring {
            let assurance = match (&a.assurance_digest, &a.assurance_summary) {
                (Some(d), Some(s)) => Some(ledger_core::anchor::AssuranceDeclaration {
                    digest: hex_array::<32>("assurance_digest", d)?,
                    summary: s.clone(),
                }),
                (None, None) => None,
                _ => return Err("the assurance digest and summary go together".into()),
            };
            lifecycle = lifecycle.with_anchoring(ledger_core::anchor::AnchoringConfig {
                interval: a.interval_ms,
                assurance,
            });
        }
        Ok((
            Genesis {
                assets,
                operator_verifier: Verifier::new(
                    hex::decode(&self.operator_verifier)
                        .map_err(|_| "operator_verifier must be hex".to_owned())?,
                ),
                lifecycle,
                chainspec,
                spec: None,
            },
            OperationId::from_ref(OperationRef(hex_array::<16>(
                "genesis_op",
                &self.genesis_op,
            )?)),
            self.genesis_at,
        ))
    }
}

impl DeploymentConfig {
    /// Where the in-process core keeps its leaves: a file if `core_book` names one, or `None`
    /// for memory, the default (`T-031-14`).
    pub fn core_book_path(&self) -> Option<std::path::PathBuf> {
        match self.core_book.as_deref() {
            None | Some("memory") => None,
            Some(p) => Some(p.into()),
        }
    }

    /// The replica's running configuration.
    pub fn service(&self) -> Config {
        let tick = Duration::from_millis(self.tick_interval_ms.max(1));
        Config {
            replica: self.replica.clone(),
            tick_interval: tick,
            lease: tick,
            decision_bound: self.decision_bound_ms,
            core_batch: self
                .core_batch
                .unwrap_or(crate::service::DEFAULT_CORE_BATCH)
                .max(1),
            // T-036-25: a chainspec book takes commands only once its core says so.
            genesis_gate: self.genesis.chainspec.is_some(),
            settlement_epochs: self.settlement_epochs,
            link: self.forward_link(),
            ..Config::default()
        }
    }

    /// How ingress forwards to its core (`F-038` §12): the sequenced feed once the new link is
    /// configured (a `stream` section: a listener, a feed key, or the keys the core host checks),
    /// and otherwise the legacy link, exactly as before `F-038`, which a core host of an earlier
    /// release speaks.
    pub fn forward_link(&self) -> crate::service::ForwardLink {
        if self.stream.is_some() {
            crate::service::ForwardLink::Sequenced
        } else {
            crate::service::ForwardLink::Legacy
        }
    }
}

/// A secret from the environment, or an error naming the variable: never a default.
pub fn secret_hex32(var: &str) -> Result<[u8; 32], String> {
    let v = std::env::var(var).map_err(|_| format!("{var} is not set: refusing to start"))?;
    hex_array::<32>(var, v.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The deployment template with its numbers filled, as JSON.
    fn template() -> serde_json::Value {
        serde_json::from_str(&fill_numbers(include_str!(
            "../../../deploy/templates/ingress.json"
        )))
        .unwrap()
    }

    fn deployment(v: serde_json::Value) -> DeploymentConfig {
        serde_json::from_value(v).unwrap()
    }

    /// Amendment 0041 (`T-043-14`): the epoch route is closed unless the deployment opens it. A
    /// file that does not name `settlement_epochs` (the template) leaves it off, and so does the
    /// running configuration's default; naming it `true` opens it.
    #[test]
    fn t043_14_settlement_epochs_is_off_by_default() {
        assert!(!crate::service::Config::default().settlement_epochs);
        let v = template();
        assert!(v.get("settlement_epochs").is_none());
        assert!(!deployment(v.clone()).settlement_epochs);
        assert!(!deployment(v.clone()).service().settlement_epochs);
        let mut on = v;
        on["settlement_epochs"] = serde_json::Value::Bool(true);
        assert!(deployment(on).service().settlement_epochs);
    }

    /// `F-042`: the feed key on an engine. The example (`ingress-feed-key-transit.json`) put in the
    /// template's place checks in production, with `stream.feed_key` taken out; the kind and the
    /// scheme are checked; `keys.feed` and `stream.feed_key` are one or the other; and an in-process
    /// key is a file, never the environment.
    #[test]
    fn f_042_the_feed_key_is_on_an_engine_and_the_check_reads_only_ingresss_keys() {
        let example: serde_json::Value = serde_json::from_str(include_str!(
            "../../../deploy/templates/ingress-feed-key-transit.json"
        ))
        .unwrap();
        let with = |edit: &dyn Fn(&mut serde_json::Value)| {
            let mut v = template();
            v["stream"].as_object_mut().unwrap().remove("feed_key");
            v["engines"] = example["engines"].clone();
            v["keys"] = example["keys"].clone();
            edit(&mut v);
            deployment(v)
        };
        // The example, in production.
        assert_eq!(with(&|_| {}).check_stream(), Ok(()));
        assert!(with(&|_| {}).has_feed_key());
        // Another component's keys in the same file are neither parsed nor refused.
        let c = with(&|v| {
            v["keys"]["key_ring"] = serde_json::json!({ "engine": "ghost", "key": 7 });
            v["engines"]["elsewhere"] = serde_json::json!({ "no_such_kind": {} });
        });
        assert_eq!(c.check_stream(), Ok(()));
        // Both forms of the feed key: refused.
        let both = with(&|v| {
            v["stream"]["feed_key"] = serde_json::json!({
                "signer": { "socket": "/run/signer.sock", "client": "ingress", "token_file": "/t" },
                "key": "ingress-feed",
            });
        });
        assert!(both.check_stream().unwrap_err().contains("one of them"));
        // An engine that is not declared, an http:// Vault in production, a version not pinned.
        let e = with(&|v| v["keys"]["feed"]["engine"] = "ghost".into())
            .check_stream()
            .unwrap_err();
        assert!(e.contains("keys.feed") && e.contains("not declared"), "{e}");
        let e = with(&|v| {
            v["engines"]["vault"]["vault_transit"]["vault"]["address"] = "http://v".into()
        })
        .check_stream()
        .unwrap_err();
        assert!(e.contains("https"), "{e}");
        let e = with(&|v| {
            v["keys"]["feed"]["key"]
                .as_object_mut()
                .unwrap()
                .remove("version");
        })
        .check_stream()
        .unwrap_err();
        assert!(e.contains("pin the key version"), "{e}");
        // In process: a file is accepted in production; the environment is not an option anywhere.
        let file = |v: &mut serde_json::Value| {
            v["engines"] = serde_json::json!({ "files": { "in_process": {} } });
            v["keys"] = serde_json::json!({ "feed": { "engine": "files", "key": { "file": "/etc/feed.key" } } });
        };
        assert_eq!(with(&file).check_stream(), Ok(()));
        let env = with(&|v| {
            file(v);
            v["keys"]["feed"]["key"] = serde_json::json!({ "env": "FEED_KEY" });
        });
        assert!(env.check_stream().unwrap_err().contains("env"));
        // The old form, alone, still checks.
        let mut old = template();
        old["keys"] = serde_json::Value::Null;
        old.as_object_mut().unwrap().remove("keys");
        assert_eq!(deployment(old).check_stream(), Ok(()));
    }

    /// `F-042`: the core host's keys may use the sidecar only. Transit for the key ring is refused for
    /// its scheme, Transit for an Agent key and an in-process key for the key ring are refused for their
    /// kind, the two forms are one or the other, and the core host loads the file in which ingress's
    /// feed key is on Transit without reading that entry.
    #[test]
    fn f_042_the_core_hosts_keys_may_use_the_sidecar_only() {
        let sidecar = serde_json::json!({ "sidecar": {
            "socket": "/run/aledgent-signer/signer.sock", "client": "core",
            "token_file": "/etc/aledgent/core-signer.token" } });
        let transit = serde_json::json!({ "vault_transit": {
            "vault": { "address": "https://vault.example",
                       "auth": { "aws_iam": { "mount": "aws", "role": "r", "region": "us-east-1" } } },
            "mount": "transit" } });
        let base = || {
            let mut v = template();
            v.as_object_mut().unwrap().remove("core_keys");
            v["engines"] = serde_json::json!({ "local": sidecar, "vault": transit, "files": { "in_process": {} } });
            v["keys"] = serde_json::json!({
                "key_ring": { "engine": "local", "key": { "id": "keyring" } },
                "clerk": { "engine": "local", "key": { "id": "clerk" } },
                "agent_authority": { "engine": "local", "key": { "id": "agent-authority" } },
                "agent_package": { "engine": "local", "key": { "id": "agent-package" } },
                // Ingress's, on Transit, in the same file.
                "feed": { "engine": "vault", "key": { "name": "lane-1", "version": 1 } },
            });
            v
        };
        let check = |edit: &dyn Fn(&mut serde_json::Value)| {
            let mut v = base();
            edit(&mut v);
            deployment(v).check_core_keys()
        };
        // The core host loads a file whose feed key is on Transit, and ingress's engines are not its own.
        assert_eq!(check(&|_| {}), Ok(()));
        // Transit lacks the key ring's scheme.
        let e = check(&|v| v["keys"]["key_ring"]["engine"] = "vault".into()).unwrap_err();
        assert!(
            e.contains("keys.key_ring") && e.contains("does not offer"),
            "{e}"
        );
        // Transit has Ed25519, but not for the Agent's keys.
        let e = check(&|v| v["keys"]["agent_authority"]["engine"] = "vault".into()).unwrap_err();
        assert!(
            e.contains("keys.agent_authority")
                && e.contains("kind api_provider")
                && e.contains("may use: sidecar"),
            "{e}"
        );
        let e = check(&|v| v["keys"]["agent_package"]["engine"] = "files".into()).unwrap_err();
        assert!(
            e.contains("keys.agent_package") && e.contains("kind in_process"),
            "{e}"
        );
        let e = check(&|v| v["keys"]["key_ring"]["engine"] = "files".into()).unwrap_err();
        assert!(
            e.contains("keys.key_ring") && e.contains("kind in_process"),
            "{e}"
        );
        // An engine of a kind this build lacks, named by one of the core host's own keys.
        let e = check(&|v| {
            v["engines"]["local"] = serde_json::json!({ "no_such_kind": {} });
        })
        .unwrap_err();
        assert!(
            e.contains("keys.key_ring") && e.contains("no_such_kind"),
            "{e}"
        );
        // A key it needs and the file does not give.
        let e = check(&|v| {
            v["keys"].as_object_mut().unwrap().remove("agent_package");
        })
        .unwrap_err();
        assert!(e.contains("keys.agent_package: not given"), "{e}");
        // The form written before F-042 and the engines' form: one or the other.
        let e = check(&|v| {
            v["core_keys"] = serde_json::json!({
                "signer": { "socket": "/s", "client": "core", "token_file": "/t" },
                "key_ring": "keyring", "agent_authority": "a", "agent_package": "p",
            });
        })
        .unwrap_err();
        assert!(e.contains("one of them"), "{e}");
        // The form written before F-042, alone, still checks.
        assert_eq!(deployment(template()).check_core_keys(), Ok(()));
        assert!(deployment(template()).core_keys.is_some());
    }

    /// `F-042`: `opsctl restore-core` opens the key ring and the clerk key and no other. The keys it
    /// needs are pinned here, and a file that gives it no Agent keys (narrower grants than the core
    /// host's) still serves it, in the engines' form.
    #[test]
    fn f_042_opsctl_needs_the_key_ring_and_the_clerk_key_and_no_others() {
        let sidecar = serde_json::json!({ "sidecar": {
            "socket": "/run/aledgent-signer/signer.sock", "client": "ops", "token_file": "/t" } });
        let file = |with_clerk: bool, with_agent_keys: bool| {
            let mut v = template();
            v.as_object_mut().unwrap().remove("core_keys");
            v["engines"] = serde_json::json!({ "local": sidecar });
            let mut keys = serde_json::Map::new();
            let entry = |id: &str| serde_json::json!({ "engine": "local", "key": { "id": id } });
            keys.insert("key_ring".into(), entry("keyring"));
            if with_clerk {
                keys.insert("clerk".into(), entry("clerk"));
            }
            if with_agent_keys {
                keys.insert("agent_authority".into(), entry("agent-authority"));
                keys.insert("agent_package".into(), entry("agent-package"));
            }
            v["keys"] = serde_json::Value::Object(keys);
            deployment(v)
        };
        let names =
            |c: &DeploymentConfig| c.host_needs().iter().map(|n| n.name).collect::<Vec<_>>();
        assert_eq!(names(&file(false, true)), ["key_ring"]);
        assert_eq!(names(&file(true, true)), ["key_ring", "clerk"]);
        // The core host's four are a different, larger set.
        let core = file(true, true).core_needs();
        assert_eq!(
            core.iter().map(|n| n.name).collect::<Vec<_>>(),
            ["key_ring", "agent_authority", "agent_package", "clerk"]
        );
        // The checks of the two sets agree on a file with all four, and the narrow one passes where the
        // core host's refuses: grants without the Agent keys.
        let narrow = file(true, false);
        assert!(
            narrow
                .check_core_keys()
                .unwrap_err()
                .contains("agent_authority: not given")
        );
        assert_eq!(
            signer::engine::config::check(
                &signer::engine::config::Registry::builtin(),
                &narrow.engine_section(),
                &narrow.host_needs(),
                false,
            ),
            Ok(())
        );
    }

    /// `T-038-14` (`REQ-AU-8`, `NFR-7`): a stream section missing what its transport needs, a
    /// production listener without the signer's feed key, a file key outside development, and a
    /// dialer without the keys it checks feeds with are each refused at load.
    #[test]
    fn t_038_14_an_incomplete_stream_section_is_refused_at_load() {
        use Profile::{Development, Production};
        let s = |v: serde_json::Value| serde_json::from_value::<StreamSection>(v).unwrap();
        let tls = serde_json::json!({ "ca": "ca.pem", "cert": "c.pem", "key": "c.key" });
        use crate::feed::FeedSigner as _;
        let key = hex::encode(crate::feed::LocalFeedKey::from_seed([1; 32]).public());
        let feed_key = serde_json::json!({
            "signer": { "socket": "/run/signer.sock", "client": "ingress", "token_file": "/t" },
            "key": "ingress-feed",
        });
        let refused = [
            (
                serde_json::json!({ "listen": "tls:0.0.0.0:7443" }),
                Development,
            ),
            (
                serde_json::json!({ "listen": "unix:/run/s.sock" }),
                Development,
            ),
            (
                serde_json::json!({ "listen": "tcp:0.0.0.0:7443", "tls": tls }),
                Development,
            ),
            (
                serde_json::json!({ "listen": "tls:0.0.0.0:7443", "tls": tls }),
                Production,
            ),
            (
                serde_json::json!({ "development_feed_key": "k.hex" }),
                Production,
            ),
            (
                serde_json::json!({ "feed_key": feed_key, "development_feed_key": "k.hex" }),
                Development,
            ),
            (
                serde_json::json!({ "dial": { "connect": ["tls:i:7443"], "server_name": "i" }, "ingress_keys": [key] }),
                Development,
            ),
            (
                serde_json::json!({ "dial": { "connect": ["tls:i:7443"] }, "tls": tls, "ingress_keys": [key] }),
                Development,
            ),
            (
                serde_json::json!({ "dial": { "connect": ["unix:/s"] } }),
                Development,
            ),
            (
                serde_json::json!({ "dial": { "connect": [] }, "ingress_keys": [key] }),
                Development,
            ),
            (
                serde_json::json!({ "dial": { "connect": ["unix:/s"] }, "ingress_keys": [key] }),
                Development,
            ),
        ];
        for (v, p) in refused {
            assert!(s(v.clone()).check(p).is_err(), "{v} under {p:?}");
        }
        let accepted = [
            (
                serde_json::json!({ "listen": "tls:0.0.0.0:7443", "tls": tls }),
                Development,
            ),
            (
                serde_json::json!({ "listen": "tls:0.0.0.0:7443", "tls": tls, "feed_key": feed_key }),
                Production,
            ),
            (
                serde_json::json!({ "dial": { "connect": ["tls:i:7443"], "server_name": "i" }, "tls": tls, "ingress_keys": [key] }),
                Production,
            ),
            (
                serde_json::json!({ "dial": { "connect": ["unix:/s"], "peer_uids": [1000] }, "ingress_keys": [key] }),
                Production,
            ),
        ];
        for (v, p) in accepted {
            assert!(s(v.clone()).check(p).is_ok(), "{v} under {p:?}");
        }
    }

    /// `T-038-21`: the advertised address is the configured one; else `listen` when it names a
    /// host or a socket; else, on every interface, this host's routed address with `listen`'s
    /// port. A malformed one is refused at load.
    #[test]
    fn t_038_21_the_advertised_address() {
        let s = |v: serde_json::Value| serde_json::from_value::<StreamSection>(v).unwrap();
        let tls = serde_json::json!({ "ca": "ca.pem", "cert": "c.pem", "key": "c.key" });
        assert_eq!(
            s(serde_json::json!({ "listen": "tls:0.0.0.0:7443", "advertise": "tls:10.0.0.7:7443" }))
                .advertised_address(),
            Ok("tls:10.0.0.7:7443".into())
        );
        for listen in [
            "tls:10.1.2.3:7443",
            "tls:ingress-a.internal:7443",
            "unix:/run/i.sock",
        ] {
            assert_eq!(
                s(serde_json::json!({ "listen": listen })).advertised_address(),
                Ok(listen.into())
            );
        }
        if let Ok(a) = s(serde_json::json!({ "listen": "tls:0.0.0.0:7443" })).advertised_address() {
            assert!(a.starts_with("tls:") && a.ends_with(":7443") && !a.contains("0.0.0.0"));
        }
        let bad = s(
            serde_json::json!({ "listen": "tls:0.0.0.0:7443", "tls": tls, "advertise": "10.0.0.7" }),
        );
        assert!(bad.check(Profile::Development).is_err());
    }

    /// `T-038-14`, `DEC-27` (f), `F-038` §12: in production, once the new link is configured, the
    /// core host refuses to dial it without `stream.ingress_keys`, and an ingress forwarding over
    /// `core_endpoint` refuses to start without `stream.feed_key`; with no `stream` section both
    /// run the legacy socket link, as before `F-038`; the development profile may run without
    /// either.
    #[test]
    fn t_038_14_production_requires_the_feed_keys_on_both_sides() {
        use crate::feed::FeedSigner as _;
        let template: serde_json::Value = serde_json::from_str(&fill_numbers(include_str!(
            "../../../deploy/templates/ingress.json"
        )))
        .unwrap();
        let key = hex::encode(crate::feed::LocalFeedKey::from_seed([1; 32]).public());
        let with = |profile: &str, stream: Option<serde_json::Value>| {
            let mut t = template.clone();
            t["profile"] = profile.into();
            match stream {
                Some(s) => t["stream"] = s,
                None => {
                    t.as_object_mut().unwrap().remove("stream");
                }
            }
            serde_json::from_value::<DeploymentConfig>(t).unwrap()
        };
        let feed_key = serde_json::json!({
            "signer": { "socket": "/run/signer.sock", "client": "ingress", "token_file": "/t" },
            "key": "ingress-feed",
        });
        // No stream section: the legacy socket link on both sides (`F-038` §12 step 0), which
        // neither signs nor checks, as before `F-038`.
        let none = with("production", None);
        assert!(
            none.core_feed_keys().unwrap().is_none(),
            "a legacy host starts"
        );
        assert!(
            none.check_forward_signing().is_ok(),
            "a legacy ingress starts"
        );
        assert_eq!(none.forward_link(), crate::service::ForwardLink::Legacy);
        // The new link configured: the keys on both sides.
        let unsigned = with(
            "production",
            Some(serde_json::json!({ "ingress_keys": [key] })),
        );
        assert_eq!(
            unsigned.forward_link(),
            crate::service::ForwardLink::Sequenced
        );
        assert!(
            unsigned.check_forward_signing().is_err(),
            "the new link without a feed key, no ingress"
        );
        let dialing = with(
            "production",
            Some(serde_json::json!({ "dial": { "connect": ["unix:/s"], "peer_uids": [1000] } })),
        );
        assert!(
            dialing.core_feed_keys().is_err(),
            "a dialer without keys, no host"
        );
        let both = with(
            "production",
            Some(serde_json::json!({ "feed_key": feed_key, "ingress_keys": [key] })),
        );
        assert!(both.core_feed_keys().unwrap().is_some());
        assert!(both.check_forward_signing().is_ok());
        let dev = with("development", None);
        assert!(dev.core_feed_keys().unwrap().is_none());
        assert!(dev.check_forward_signing().is_ok());
    }

    #[test]
    fn a_genesis_config_builds_the_core_genesis() {
        let g = GenesisConfig {
            assets: vec![AssetConfig {
                id: "USD".into(),
                precision: 2,
                kind: "fiat".into(),
                deprecated: false,
            }],
            operator_verifier: "0f".into(),
            deployment: "d9".repeat(16),
            decision_bound_ms: 60_000,
            review_service_level_ms: 3_600_000,
            default_deadlines_ms: [("reserve".to_owned(), 600_000)].into_iter().collect(),
            genesis_op: "e9".repeat(16),
            genesis_at: 1,
            recovery: None,
            anchoring: None,
            opening_sources: BTreeMap::new(),
            chainspec: None,
        };
        let c = g.core_genesis([1; 32]).unwrap();
        assert_eq!(c.genesis.assets.len(), 1);
        // Anchoring (`F-017`): the interval, and a declaration only with both halves.
        let mut a = g.clone();
        a.anchoring = Some(AnchoringDeploymentConfig {
            interval_ms: 900_000,
            assurance_digest: Some("a5".repeat(32)),
            assurance_summary: Some("m2 declaration".into()),
        });
        let anchoring = a.core_genesis([1; 32]).unwrap().genesis.lifecycle.anchoring;
        assert_eq!(anchoring.interval, 900_000);
        assert_eq!(anchoring.assurance.map(|d| d.digest), Some([0xA5; 32]));
        a.anchoring = Some(AnchoringDeploymentConfig {
            interval_ms: 1,
            assurance_digest: Some("a5".repeat(32)),
            assurance_summary: None,
        });
        assert!(a.core_genesis([1; 32]).is_err());
        assert_eq!(
            c.genesis
                .lifecycle
                .default_deadlines
                .get(&OperationKind::Reserve),
            Some(&600_000)
        );
        let mut bad = g.clone();
        bad.default_deadlines_ms.insert("nonsense".into(), 1);
        assert!(bad.core_genesis([1; 32]).is_err());
        // Owned custody only with a recovery authority (`F-012`).
        assert!(!c.genesis.lifecycle.recovery.offers_owned());
        let mut owned = g.clone();
        owned.recovery = Some(RecoveryDeploymentConfig {
            recovery_key: "e2".repeat(32),
            recovery_delay_ms: 259_200_000,
            succession_delay_ms: Some(2_592_000_000),
            holder_held: false,
        });
        let c = owned.core_genesis([1; 32]).unwrap();
        assert!(c.genesis.lifecycle.recovery.offers_owned());
        assert_eq!(c.genesis.lifecycle.recovery.recovery_delay, 259_200_000);
        let mut short = owned.clone();
        if let Some(r) = short.recovery.as_mut() {
            r.recovery_key = "e2".into();
        }
        assert!(short.core_genesis([1; 32]).is_err());
    }

    #[test]
    fn the_founded_identifiers_are_known_from_the_file_and_are_the_cores() {
        let g = GenesisConfig {
            assets: vec![AssetConfig {
                id: "USD".into(),
                precision: 2,
                kind: "fiat".into(),
                deprecated: false,
            }],
            operator_verifier: "0f".into(),
            deployment: "d9".repeat(16),
            decision_bound_ms: 60_000,
            review_service_level_ms: 3_600_000,
            default_deadlines_ms: BTreeMap::new(),
            genesis_op: "e9".repeat(16),
            genesis_at: 1,
            recovery: None,
            anchoring: None,
            opening_sources: BTreeMap::new(),
            chainspec: None,
        };
        let from_file = g.founded().unwrap();
        let core = crate::inprocess::InProcessCore::in_memory(
            g.core_genesis([7; 32]).unwrap(),
            crate::inprocess::MemJournal::default(),
        );
        core.start().unwrap();
        assert_eq!(core.founded(), Some(from_file));
        assert!(from_file.operator.to_did().starts_with("did:aledgent:id:"));
        let mut bad = g.clone();
        bad.genesis_op = "e9".into();
        assert!(bad.founded().is_err());
    }

    /// `text` with every `"<FILL-NUMBER …>"` placeholder made a number, so that a template parses;
    /// `"<FILL …>"` strings stay as they are.
    fn fill_numbers(text: &str) -> String {
        let mut out = String::new();
        let mut rest = text;
        while let Some(at) = rest.find("\"<FILL-NUMBER") {
            out.push_str(&rest[..at]);
            let end = rest[at + 1..].find('"').map(|e| at + e + 2).unwrap();
            out.push('1');
            rest = &rest[end..];
        }
        out.push_str(rest);
        out
    }

    /// `T-036-29` (`REQ-RC-7`): the archived identifiers are the list the genesis's chainspec
    /// commits by its header's `excluded_sha256`; another file, a chainspec of another genesis, or
    /// one that commits no list stops the start.
    #[test]
    fn t_036_29_the_archived_list_is_the_one_the_chainspec_commits() {
        use ledger_core::chainspec::{ChainSpec, ChunkLimits};
        use sha2::{Digest as _, Sha256};
        let dir = std::env::temp_dir().join(format!("ingress-archived-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let list = "{\"legacy\":\"3f0c\",\"kind\":\"ledger_account_id\",\"reason\":\"test-identity-zero-balance\"}\n";
        let list_path = dir.join("excluded.jsonl");
        std::fs::write(&list_path, list).unwrap();
        let op = OperationId::from_ref(ledger_kernel::posting::OperationRef([7; 16]));
        let bare =
            ChainSpec::build(op, 0, vec![], vec![], None, None, ChunkLimits::DEFAULT).unwrap();
        let mut c: DeploymentConfig = serde_json::from_str(&fill_numbers(include_str!(
            "../../../deploy/templates/ingress.json"
        )))
        .unwrap();
        let with = |spec: &ChainSpec, name: &str, c: &mut DeploymentConfig| {
            let path = dir.join(name);
            std::fs::write(&path, spec.to_bytes()).unwrap();
            let k = spec.commitment();
            c.genesis.chainspec = Some(ChainSpecConfig {
                digest: hex::encode(k.digest),
                chunks: k.chunks,
            });
            c.archived_identifiers = Some(ArchivedIdentifiers {
                path: list_path.display().to_string(),
                chainspec: path.display().to_string(),
            });
        };
        let committed = bare.clone().with_excluded(Sha256::digest(list).into());
        with(&committed, "committed.bin", &mut c);
        let a = c.archived().unwrap();
        assert!(a.contains("3f0c") && a.len() == 1);
        // Another list than the one committed.
        let other = bare.clone().with_excluded([0xEE; 32]);
        with(&other, "other.bin", &mut c);
        assert!(
            c.archived()
                .unwrap_err()
                .contains("chainspec's genesis commits")
        );
        // A chainspec that commits none.
        with(&bare, "bare.bin", &mut c);
        assert!(
            c.archived()
                .unwrap_err()
                .contains("commits no exclusion file")
        );
        // A chainspec of another genesis than the one the deployment commits.
        with(&committed, "committed.bin", &mut c);
        c.genesis.chainspec = Some(ChainSpecConfig {
            digest: hex::encode([1u8; 32]),
            chunks: 1,
        });
        assert!(c.archived().unwrap_err().contains("not the chainspec"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `T-009-26`: the `migrations` section. Absent, `serve` applies at start under a 5 s lock
    /// timeout, as before, and the section is not written back; `"verify"` and a timeout parse;
    /// a timeout of 0 (no bound, to PostgreSQL) and an unknown mode are refused.
    #[test]
    fn t_009_26_the_migrations_section() {
        let template: serde_json::Value = serde_json::from_str(&fill_numbers(include_str!(
            "../../../deploy/templates/ingress.json"
        )))
        .unwrap();
        let c: DeploymentConfig = serde_json::from_value(template.clone()).unwrap();
        assert_eq!(c.migrations, MigrationsConfig::default());
        assert_eq!(c.migrations.on_start, OnStart::Apply);
        assert_eq!(
            c.migrations.lock_timeout().unwrap(),
            Duration::from_millis(5_000)
        );
        assert!(
            serde_json::to_value(&c)
                .unwrap()
                .get("migrations")
                .is_none()
        );
        let with = |section: serde_json::Value| {
            let mut t = template.clone();
            t["migrations"] = section;
            serde_json::from_value::<DeploymentConfig>(t)
        };
        let c = with(serde_json::json!({"on_start": "verify"})).unwrap();
        assert_eq!(c.migrations.on_start, OnStart::Verify);
        assert_eq!(c.migrations.lock_timeout_ms, 5_000);
        let c = with(serde_json::json!({"lock_timeout_ms": 1500})).unwrap();
        assert_eq!(
            (c.migrations.on_start, c.migrations.lock_timeout_ms),
            (OnStart::Apply, 1_500)
        );
        let c = with(serde_json::json!({"lock_timeout_ms": 0})).unwrap();
        assert!(
            c.migrations
                .lock_timeout()
                .unwrap_err()
                .contains("no bound")
        );
        assert!(with(serde_json::json!({"on_start": "skip"})).is_err());
        assert!(with(serde_json::json!({"lock_timeout": 5})).is_err());
    }

    /// The deployment template (`deploy/templates/ingress.json`): it parses; it does not require a
    /// synchronous standby, which a single-zone database does not have; it runs the core as a VOS Agent
    /// (`DEC-21`); and its book-only assets are the ones the adapter's template maps.
    #[test]
    fn the_canary_template_parses_and_agrees_with_the_adapters() {
        let c: DeploymentConfig = serde_json::from_str(&fill_numbers(include_str!(
            "../../../deploy/templates/ingress.json"
        )))
        .unwrap();
        assert_eq!(c.require_sync_standby, SyncStandby::NotRequired);
        // It holds real value: TLS to PostgreSQL is required (`T-016-13`).
        assert_eq!(c.profile, Profile::Production);
        // DEC-21 (the owner's `d-core-vos`): the core runs as a VOS Agent, in `aledgent-core-host`,
        // over a socket both containers mount; never a routable address (the link has no
        // credential).
        assert_eq!(
            c.core_endpoint.as_deref(),
            Some("unix:/run/aledgent/core.sock")
        );
        // F-035: the link is authenticated, and the core host's keys are the signer's.
        let link = c.core_link.as_ref().expect("the core link's credential");
        assert_eq!(link.token_sha256.len(), 1);
        assert!(!link.peer_uids.is_empty());
        // F-038 §5, §12: in production both sides carry the feed keys: ingress signs through the
        // signer, and the core host checks against ingress's public key.
        assert!(c.check_forward_signing().is_ok());
        let stream = c.stream.as_ref().expect("the feed keys");
        assert!(stream.feed_key.is_some());
        assert_eq!(stream.ingress_keys.len(), 1);
        let keys = c
            .core_keys
            .as_ref()
            .expect("the core's keys behind the signer");
        assert_eq!(keys.key_ring, "keyring");
        assert!(keys.signer.socket.starts_with("/run/aledgent-signer"));
        // OQ-12, closed by amendment 0009: holder and agent credentials reach ingress (DEP-7).
        assert!(c.allow_holder_credentials);
        // F-028: the accepted relying parties, as DEC-19 describes them. The first takes new
        // passkeys; the second keeps imported Kreivo Pass ones, for sign-in and import only.
        let p = c.passkeys.as_ref().expect("the canary accepts passkeys");
        let parties: Vec<(&str, bool)> = p
            .relying_parties
            .iter()
            .map(|r| (r.rp_id.as_str(), r.registrations))
            .collect();
        assert_eq!(parties, [("example.com", true), ("api.example.com", false)]);
        assert!(
            p.passkeys(authn::token::AuthnKey::new([0; 32]), &c.genesis.deployment)
                .is_ok()
        );
        let settle: serde_json::Value =
            serde_json::from_str(include_str!("../../../deploy/templates/settle.json")).unwrap();
        // Amendment 0011: the template's assets are registered book-only, COPM/2, TSTA/2, TSTB/2
        // and TSTC/6; the adapter maps each to its Kreivo location,
        // with no precision, for its binding later (`REQ-AS-8`).
        let ids: Vec<&str> = c.genesis.assets.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, ["COPM", "TSTA", "TSTB", "TSTC"]);
        let precisions: Vec<u8> = c.genesis.assets.iter().map(|a| a.precision).collect();
        assert_eq!(precisions, [2, 2, 2, 6]);
        let kinds: Vec<&str> = c.genesis.assets.iter().map(|a| a.kind.as_str()).collect();
        assert_eq!(kinds, ["stable", "stable", "stable", "stable"]);
        let mapped: Vec<&str> = settle["adapter"]["assets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| {
                assert!(b.get("precision").is_none(), "{b}");
                b["asset"].as_str().unwrap()
            })
            .collect();
        assert_eq!(mapped, ids);
        for kind in c.genesis.default_deadlines_ms.keys() {
            assert!(kind_of(kind).is_ok(), "{kind}");
        }
    }

    /// `T-009-14`: `require_sync_standby` reads `true`, `false` and `"rds-multi-az"`, and nothing
    /// else; the profile has no default.
    #[test]
    fn t_009_14_the_standby_setting_and_the_profile_parse_and_have_no_default() {
        let template: serde_json::Value = serde_json::from_str(&fill_numbers(include_str!(
            "../../../deploy/templates/ingress.json"
        )))
        .unwrap();
        let with = |field: &str, v: serde_json::Value| {
            let mut t = template.clone();
            t[field] = v;
            serde_json::from_value::<DeploymentConfig>(t)
        };
        let standby = |v| with("require_sync_standby", v).map(|c| c.require_sync_standby);
        assert_eq!(standby(true.into()).unwrap(), SyncStandby::Required);
        assert_eq!(standby(false.into()).unwrap(), SyncStandby::NotRequired);
        assert_eq!(
            standby("rds-multi-az".into()).unwrap(),
            SyncStandby::RdsMultiAz
        );
        assert!(standby("multi-az".into()).is_err());
        assert!(standby(serde_json::Value::Null).is_err());
        let profile = |v| with("profile", v).map(|c| c.profile);
        assert_eq!(profile("development".into()).unwrap(), Profile::Development);
        assert!(profile("staging".into()).is_err());
        let mut none = template.clone();
        none.as_object_mut().unwrap().remove("profile");
        assert!(serde_json::from_value::<DeploymentConfig>(none).is_err());
        // Both round-trip as written.
        for s in [
            SyncStandby::Required,
            SyncStandby::NotRequired,
            SyncStandby::RdsMultiAz,
        ] {
            let v = serde_json::to_value(s).unwrap();
            assert_eq!(serde_json::from_value::<SyncStandby>(v).unwrap(), s);
        }
        assert!(Profile::Production.requires_database_tls());
        assert!(!Profile::Development.requires_database_tls());
    }

    #[test]
    fn req_au_8_a_missing_secret_refuses_to_start() {
        assert!(secret_hex32("ALEDGENT_TEST_SECRET_THAT_IS_NEVER_SET").is_err());
    }

    #[test]
    fn dec_19_a_passkey_configuration_is_refused_unless_well_formed() {
        let key = || authn::token::AuthnKey::new([1; 32]);
        const DEP: &str = "d9d9d9d9d9d9d9d9d9d9d9d9d9d9d9d9";
        let good: PasskeysConfig = serde_json::from_value(serde_json::json!({
            "relying_parties": [
                { "rp_id": "api.bloque.app", "origins": ["https://api.bloque.app"] }
            ]
        }))
        .unwrap();
        assert!(good.require_user_verification);
        assert_eq!(good.session_ttl_ms, 900_000);
        assert!(good.passkeys(key(), DEP).is_ok());
        // Approvals on need an environment, and the setting is part of the passkeys built.
        let mut on = good.clone();
        on.approvals = crate::approval::ApprovalMode::Enforce;
        assert!(on.passkeys(key(), DEP).is_err());
        on.environment = Some("test".into());
        let built = on.passkeys(key(), DEP).unwrap();
        assert_eq!(
            built.approvals().mode(),
            crate::approval::ApprovalMode::Enforce
        );
        for bad in [
            serde_json::json!({ "relying_parties": [] }),
            serde_json::json!({ "relying_parties": [
                { "rp_id": "API.bloque.app", "origins": ["https://api.bloque.app"] } ] }),
            serde_json::json!({ "relying_parties": [ { "rp_id": "api.bloque.app", "origins": [] } ] }),
            serde_json::json!({ "relying_parties": [
                { "rp_id": "api.bloque.app", "origins": ["http://api.bloque.app"] } ] }),
            serde_json::json!({ "relying_parties": [
                { "rp_id": "api.bloque.app", "origins": ["https://api.bloque.app"] } ],
                "session_ttl_ms": 0 }),
        ] {
            let c: PasskeysConfig = serde_json::from_value(bad.clone()).unwrap();
            assert!(c.passkeys(key(), DEP).is_err(), "{bad}");
        }
    }
}
