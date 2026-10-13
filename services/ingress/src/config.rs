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
    /// The authenticator's passkeys under `key`, or why the configuration is refused: an `rpId`
    /// that is not a lower-case DNS name, a party with no origin, an origin that is not `https`
    /// (or `http://localhost`), or a zero time to live.
    pub fn passkeys(&self, key: authn::token::AuthnKey) -> Result<crate::auth::Passkeys, String> {
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
        ))
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
            ..Config::default()
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
        assert!(p.passkeys(authn::token::AuthnKey::new([0; 32])).is_ok());
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
        let good: PasskeysConfig = serde_json::from_value(serde_json::json!({
            "relying_parties": [
                { "rp_id": "api.bloque.app", "origins": ["https://api.bloque.app"] }
            ]
        }))
        .unwrap();
        assert!(good.require_user_verification);
        assert_eq!(good.session_ttl_ms, 900_000);
        assert!(good.passkeys(key()).is_ok());
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
            assert!(c.passkeys(key()).is_err(), "{bad}");
        }
    }
}
