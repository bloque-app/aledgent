// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The ingress store in PostgreSQL (accepted proposal of `F-009`): several replicas share one
//! received-log and one uniqueness constraint.
//!
//! - **Commit before reply.** Every connection runs with `synchronous_commit = on`: a commit
//!   returns only once it is flushed, and, where the server names synchronous standbys, once a
//!   standby has it. With [`PgOptions::require_sync_standby`] at [`SyncStandby::Required`], the
//!   store refuses to start unless the server names at least one, so no reply is ever sent before
//!   its row is on the synchronous replica (`T-009-02`, `REQ-AR-7`). At
//!   [`SyncStandby::RdsMultiAz`] it refuses to start unless the server is an Amazon RDS instance,
//!   whose Multi-AZ storage replication it trusts as declared (`T-009-14`).
//! - **TLS in production.** Under [`Profile::Production`] the store refuses a connection string
//!   that does not require TLS ([`check_tls`], `T-009-14`), before it opens any connection.
//! - **Leadership** is a session advisory lock held on a connection of this store's own: a leader
//!   that dies drops its connection, and the lock with it. Each replica connects its own store.
//! - **Secrets at rest.** A recipient's verification secret is sealed with ChaCha20-Poly1305
//!   under the deployment's sealing key, the principal as associated data, and is never stored
//!   in the clear (`REQ-EV-4`, `REQ-AU-8`).
//!
//! Migrations are embedded and, by default, applied in order on connect, each once, each checked
//! against the checksum recorded when it was applied ([`super::migrate`], `T-009-26`).

use std::sync::Arc;

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use ledger_core::account::Time;
use ledger_core::lifecycle::keys::Surface;
use tokio::sync::Mutex;
use tokio_postgres::{Client, Row as PgRow};
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::config::{MigrationsConfig, Profile, SyncStandby};

use super::{
    Answering, Appended, Attempt, Carried, DecidedRow, InputKind, NewRow, Recipient, Row,
    RowCaller, Status, Store, StoreError,
};
use crate::boundary::InputKey;
use crate::wire::PassThrough;

/// Amazon RDS's global certificate bundle, as <https://truststore.pki.rds.amazonaws.com/global/global-bundle.pem>
/// served it on 2026-09-30 (111 certificates; SHA-256 `fe45bbeb…a5395c`). RDS signs its servers'
/// certificates with these authorities, which no public root store holds.
const RDS_CA_BUNDLE: &str = include_str!("../../certs/rds-global-bundle.pem");

/// How every connection to PostgreSQL is made, ingress's and the adapter's (`services/settle`
/// uses it too): through rustls, which PostgreSQL uses as the connection string's `sslmode` says:
/// `prefer` (the default) when the server offers TLS, `require` always, `disable` never. Once TLS
/// is used, the server's certificate must chain to the Mozilla roots or to Amazon RDS's global
/// bundle, and name the host connected to. An RDS instance with `rds.force_ssl`
/// takes nothing else, and its connection string carries `sslmode=require`.
pub fn tls() -> Result<MakeRustlsConnect, String> {
    use rustls::pki_types::CertificateDer;
    use rustls::pki_types::pem::PemObject as _;
    let mut roots = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    for cert in CertificateDer::pem_slice_iter(RDS_CA_BUNDLE.as_bytes()) {
        let cert = cert.map_err(|e| format!("the RDS certificate bundle: {e}"))?;
        roots
            .add(cert)
            .map_err(|e| format!("the RDS certificate bundle: {e}"))?;
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_root_certificates(roots)
    .with_no_client_auth();
    Ok(MakeRustlsConnect::new(config))
}

/// Refuse, under a profile that requires it, a connection that does not require TLS: the
/// connection string must say `sslmode=require` (`T-009-14`, the owner's ruling of 2026-10-02). With [`tls`] a required connection also checks the server's
/// certificate chain and host name, so `require` here is as strong as libpq's `verify-full`. The
/// error names the mode found, never the string, which may carry a password (`REQ-AU-8`).
pub fn check_tls(config: &tokio_postgres::Config, profile: Profile) -> Result<(), String> {
    if !profile.requires_database_tls() {
        return Ok(());
    }
    match config.get_ssl_mode() {
        tokio_postgres::config::SslMode::Require => Ok(()),
        mode => Err(format!(
            "the production profile requires TLS to PostgreSQL: the connection string says \
             sslmode={}, and must say sslmode=require",
            match mode {
                tokio_postgres::config::SslMode::Disable => "disable",
                tokio_postgres::config::SslMode::Prefer => "prefer (the default)",
                _ => "an unknown mode",
            }
        )),
    }
}

/// The advisory lock the leader holds: "aledgent" in ASCII.
const LEADER_LOCK: i64 = 0x616c_6564_6765_6e74;

const COLUMNS: &str = "seq, op_ref, input_kind, principal, credential, on_behalf_of, surface, \
     client_key, key_derived, body_digest, body, reserves, synchronous, received_at, status, \
     forwarded_at, core_input_seq, core_reply, decided_at, client_key_domain, pass_through, \
     arrived_at, replied_at, completed_at, submitted_at, feed_seq";

/// How the store connects.
#[derive(Clone)]
pub struct PgOptions {
    /// The connection string. It may carry a password: never logged.
    pub url: String,
    /// The key recipients' secrets are sealed with. Never logged.
    pub sealing_key: [u8; 32],
    /// What the server must guarantee before a reply ([`SyncStandby`]).
    pub require_sync_standby: SyncStandby,
    /// Which start-up checks bind the store: TLS is required in production ([`check_tls`]).
    pub profile: Profile,
    /// The pool's size.
    pub pool_size: usize,
}

struct Leader {
    client: Client,
    held: bool,
}

/// The ingress store in PostgreSQL. Clones share the pool and the leader connection; each
/// replica connects its own.
#[derive(Clone)]
pub struct PgStore {
    pool: Pool,
    config: tokio_postgres::Config,
    tls: MakeRustlsConnect,
    leader: Arc<Mutex<Option<Leader>>>,
    cipher: Arc<ChaCha20Poly1305>,
}

fn err(e: impl std::fmt::Display) -> StoreError {
    StoreError(e.to_string())
}

fn i64_of(v: u64) -> Result<i64, StoreError> {
    i64::try_from(v).map_err(|_| StoreError(format!("{v} does not fit the column")))
}

fn u64_of(v: i64) -> Result<u64, StoreError> {
    u64::try_from(v).map_err(|_| StoreError(format!("{v} is negative")))
}

fn surface_of(code: i16) -> Result<Surface, StoreError> {
    match code {
        1 => Ok(Surface::Command),
        2 => Ok(Surface::Operator),
        _ => Err(StoreError(format!("unknown surface {code}"))),
    }
}

/// A row's pass-through data, checked again on the way out: the column holds only what ingress
/// checked on the way in (`REQ-PO-5`).
fn pass_through_of(r: &PgRow) -> Result<Option<PassThrough>, StoreError> {
    r.try_get::<_, Option<String>>("pass_through")
        .map_err(err)?
        .map(|t| {
            PassThrough::parse(&t).ok_or_else(|| StoreError("pass_through is not one value".into()))
        })
        .transpose()
}

fn row_of(r: &PgRow) -> Result<Row, StoreError> {
    let op_ref: Vec<u8> = r.try_get("op_ref").map_err(err)?;
    let digest: Vec<u8> = r.try_get("body_digest").map_err(err)?;
    let kind: String = r.try_get("input_kind").map_err(err)?;
    let status: String = r.try_get("status").map_err(err)?;
    let principal: Option<String> = r.try_get("principal").map_err(err)?;
    let credential: Option<String> = r.try_get("credential").map_err(err)?;
    let opt = |name: &str| -> Result<Option<u64>, StoreError> {
        r.try_get::<_, Option<i64>>(name)
            .map_err(err)?
            .map(u64_of)
            .transpose()
    };
    Ok(Row {
        seq: u64_of(r.try_get("seq").map_err(err)?)?,
        new: NewRow {
            op_ref: InputKey(
                op_ref
                    .try_into()
                    .map_err(|_| StoreError("op_ref is not 16 bytes".into()))?,
            ),
            kind: InputKind::parse(&kind).ok_or_else(|| StoreError(format!("kind {kind}")))?,
            caller: match (principal, credential) {
                (Some(principal), Some(credential)) => Some(RowCaller {
                    principal,
                    credential,
                    on_behalf_of: r.try_get("on_behalf_of").map_err(err)?,
                }),
                _ => None,
            },
            surface: surface_of(r.try_get("surface").map_err(err)?)?,
            client_key: r.try_get("client_key").map_err(err)?,
            key_domain: r.try_get("client_key_domain").map_err(err)?,
            key_derived: r.try_get("key_derived").map_err(err)?,
            body_digest: digest
                .try_into()
                .map_err(|_| StoreError("body_digest is not 32 bytes".into()))?,
            body: r.try_get("body").map_err(err)?,
            pass_through: pass_through_of(r)?,
            reserves: r.try_get("reserves").map_err(err)?,
            synchronous: r.try_get("synchronous").map_err(err)?,
            received_at: u64_of(r.try_get("received_at").map_err(err)?)?,
            arrived_at: opt("arrived_at")?,
        },
        status: Status::parse(&status).ok_or_else(|| StoreError(format!("status {status}")))?,
        forwarded_at: opt("forwarded_at")?,
        core_input_seq: opt("core_input_seq")?,
        core_reply: r.try_get("core_reply").map_err(err)?,
        decided_at: opt("decided_at")?,
        replied_at: opt("replied_at")?,
        completed_at: opt("completed_at")?,
        submitted_at: opt("submitted_at")?,
        feed_seq: opt("feed_seq")?,
    })
}

impl PgStore {
    /// Connect, apply the migrations, and check the commit guarantees.
    pub async fn connect(options: PgOptions) -> Result<PgStore, StoreError> {
        PgStore::connect_with(options, &MigrationsConfig::default()).await
    }

    /// Connect, apply or verify the migrations as `migrations` says ([`super::migrate`]), and
    /// check the commit guarantees.
    pub async fn connect_with(
        options: PgOptions,
        migrations: &MigrationsConfig,
    ) -> Result<PgStore, StoreError> {
        let mut config: tokio_postgres::Config = options.url.parse().map_err(err)?;
        check_tls(&config, options.profile).map_err(StoreError)?;
        config.options("-c synchronous_commit=on");
        let tls = tls().map_err(StoreError)?;
        let manager = Manager::from_config(
            config.clone(),
            tls.clone(),
            ManagerConfig {
                recycling_method: RecyclingMethod::Fast,
            },
        );
        let pool = Pool::builder(manager)
            .max_size(options.pool_size.max(1))
            .build()
            .map_err(err)?;
        let store = PgStore {
            pool,
            config,
            tls,
            leader: Arc::new(Mutex::new(None)),
            cipher: Arc::new(ChaCha20Poly1305::new(Key::from_slice(&options.sealing_key))),
        };
        {
            let mut c = store.client().await?;
            super::migrate::on_start(&mut c, migrations).await?;
        }
        store.check_commit(options.require_sync_standby).await?;
        Ok(store)
    }

    async fn client(&self) -> Result<deadpool_postgres::Object, StoreError> {
        self.pool.get().await.map_err(err)
    }

    /// The server's `synchronous_commit` and `synchronous_standby_names` as this store's
    /// connections see them.
    pub async fn commit_settings(&self) -> Result<(String, String), StoreError> {
        let c = self.client().await?;
        let commit: String = c
            .query_one("SHOW synchronous_commit", &[])
            .await
            .map_err(err)?
            .get(0);
        let standbys: String = c
            .query_one("SHOW synchronous_standby_names", &[])
            .await
            .map_err(err)?
            .get(0);
        Ok((commit, standbys))
    }

    async fn check_commit(&self, standby: SyncStandby) -> Result<(), StoreError> {
        let (commit, standbys) = self.commit_settings().await?;
        if !matches!(commit.as_str(), "on" | "remote_write" | "remote_apply") {
            return Err(StoreError(format!(
                "synchronous_commit is {commit}: a reply could precede the commit"
            )));
        }
        match standby {
            SyncStandby::Required if standbys.trim().is_empty() => Err(StoreError(
                "no synchronous standby is named: refusing to start (T-009-02)".into(),
            )),
            SyncStandby::RdsMultiAz if !self.is_rds().await? => Err(StoreError(
                "require_sync_standby is rds-multi-az, and this server is not an Amazon RDS \
                 instance: refusing to start (T-009-14)"
                    .into(),
            )),
            SyncStandby::RdsMultiAz => {
                tracing::info!(
                    "durability rests on the RDS instance's Multi-AZ replication, as configured; \
                     PostgreSQL cannot show it (T-009-14)"
                );
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Whether the server is an Amazon RDS instance: RDS, and nothing else, defines the `rds.*`
    /// parameters (`rds.force_ssl`, `rds.extensions`, …).
    async fn is_rds(&self) -> Result<bool, StoreError> {
        self.exists(
            "SELECT EXISTS (SELECT 1 FROM pg_settings WHERE name LIKE 'rds.%')",
            None,
        )
        .await
    }

    fn seal(&self, principal: &str, secret: &[u8]) -> Result<(Vec<u8>, Vec<u8>), StoreError> {
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce).map_err(err)?;
        let sealed = self
            .cipher
            .encrypt(
                Nonce::from_slice(&nonce),
                Payload {
                    msg: secret,
                    aad: principal.as_bytes(),
                },
            )
            .map_err(|_| StoreError("sealing failed".into()))?;
        Ok((nonce.to_vec(), sealed))
    }

    fn open(&self, principal: &str, nonce: &[u8], sealed: &[u8]) -> Result<Vec<u8>, StoreError> {
        if nonce.len() != 12 {
            return Err(StoreError("a sealed secret's nonce is 12 bytes".into()));
        }
        self.cipher
            .decrypt(
                Nonce::from_slice(nonce),
                Payload {
                    msg: sealed,
                    aad: principal.as_bytes(),
                },
            )
            .map_err(|_| StoreError("a sealed secret did not open".into()))
    }

    async fn exists(&self, sql: &str, key: Option<&str>) -> Result<bool, StoreError> {
        let c = self.client().await?;
        let row = match key {
            Some(k) => c.query_one(sql, &[&k]).await,
            None => c.query_one(sql, &[]).await,
        }
        .map_err(err)?;
        Ok(row.get(0))
    }
}

impl Store for PgStore {
    async fn append(&self, row: NewRow) -> Result<Appended, StoreError> {
        let c = self.client().await?;
        let (principal, credential, obo) = match &row.caller {
            Some(x) => (
                Some(x.principal.clone()),
                Some(x.credential.clone()),
                x.on_behalf_of.clone(),
            ),
            None => (None, None, None),
        };
        let surface = i16::from(row.surface.code());
        let received_at = i64_of(row.received_at)?;
        let arrived_at = row.arrived_at.map(i64_of).transpose()?;
        let sql = format!(
            "INSERT INTO received_log (op_ref, input_kind, principal, credential, on_behalf_of, \
             surface, client_key, key_derived, body_digest, body, reserves, synchronous, \
             received_at, status, client_key_domain, pass_through, arrived_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, 'Received', $14, $15, \
             $16) \
             ON CONFLICT (client_key_domain, surface, client_key) WHERE input_kind = 'command' \
             DO NOTHING RETURNING {COLUMNS}"
        );
        // Prepared once per connection (`T-032-13`): one round trip on the acknowledgement's
        // path instead of two.
        let insert = c.prepare_cached(&sql).await.map_err(err)?;
        let inserted = c
            .query_opt(
                &insert,
                &[
                    &row.op_ref.0.as_slice(),
                    &row.kind.as_str(),
                    &principal,
                    &credential,
                    &obo,
                    &surface,
                    &row.client_key,
                    &row.key_derived,
                    &row.body_digest.as_slice(),
                    &row.body,
                    &row.reserves,
                    &row.synchronous,
                    &received_at,
                    &row.key_domain,
                    &row.pass_through.as_ref().map(PassThrough::get),
                    &arrived_at,
                ],
            )
            .await
            .map_err(err)?;
        if let Some(r) = inserted {
            return Ok(Appended::New(row_of(&r)?));
        }
        let existing = c
            .query_one(
                &format!(
                    "SELECT {COLUMNS} FROM received_log WHERE input_kind = 'command' \
                     AND client_key_domain = $1 AND surface = $2 AND client_key = $3"
                ),
                &[&row.key_domain, &surface, &row.client_key],
            )
            .await
            .map_err(err)?;
        Ok(Appended::Existing(row_of(&existing)?))
    }

    async fn sign_count(&self, credential: &str) -> Result<Option<u32>, StoreError> {
        let c = self.client().await?;
        c.query_opt(
            "SELECT count FROM passkey_counters WHERE credential = $1",
            &[&credential],
        )
        .await
        .map_err(err)?
        .map(|r| {
            let v: i64 = r.get(0);
            u32::try_from(v).map_err(|_| StoreError(format!("{v} is not a sign count")))
        })
        .transpose()
    }

    async fn raise_sign_count(&self, credential: &str, count: u32) -> Result<bool, StoreError> {
        let c = self.client().await?;
        // One statement: two replicas racing on one assertion cannot both raise it.
        let raised = c
            .execute(
                "INSERT INTO passkey_counters (credential, count) VALUES ($1, $2) \
                 ON CONFLICT (credential) DO UPDATE SET count = EXCLUDED.count \
                 WHERE passkey_counters.count < EXCLUDED.count",
                &[&credential, &i64::from(count)],
            )
            .await
            .map_err(err)?;
        Ok(raised == 1)
    }

    async fn use_challenge(
        &self,
        nonce: [u8; 16],
        expires_at: Time,
        now: Time,
    ) -> Result<bool, StoreError> {
        let c = self.client().await?;
        c.execute(
            "DELETE FROM used_challenges WHERE expires_at <= $1",
            &[&i64_of(now)?],
        )
        .await
        .map_err(err)?;
        let fresh = c
            .execute(
                "INSERT INTO used_challenges (nonce, expires_at) VALUES ($1, $2) \
                 ON CONFLICT (nonce) DO NOTHING",
                &[&nonce.as_slice(), &i64_of(expires_at)?],
            )
            .await
            .map_err(err)?;
        Ok(fresh == 1)
    }

    async fn find(
        &self,
        domain: &str,
        surface: Surface,
        client_key: &str,
    ) -> Result<Option<Row>, StoreError> {
        let c = self.client().await?;
        c.query_opt(
            &format!(
                "SELECT {COLUMNS} FROM received_log WHERE input_kind = 'command' \
                 AND client_key_domain = $1 AND surface = $2 AND client_key = $3"
            ),
            &[&domain, &i16::from(surface.code()), &client_key],
        )
        .await
        .map_err(err)?
        .as_ref()
        .map(row_of)
        .transpose()
    }

    async fn tie_of(&self, principal: &str) -> Result<Option<String>, StoreError> {
        let c = self.client().await?;
        let tie = c
            .prepare_cached("SELECT party FROM party_ties WHERE principal = $1")
            .await
            .map_err(err)?;
        Ok(c.query_opt(&tie, &[&principal])
            .await
            .map_err(err)?
            .map(|r| r.get(0)))
    }

    async fn restore_pass_through(&self, entries: &[Carried]) -> Result<u64, StoreError> {
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(err)?;
        let mut written = 0;
        for e in entries {
            let row = tx
                .query_opt(
                    "SELECT input_kind, principal, pass_through FROM received_log \
                     WHERE op_ref = $1 FOR UPDATE",
                    &[&e.op_ref.0.as_slice()],
                )
                .await
                .map_err(err)?
                .ok_or_else(|| {
                    StoreError("the received-log has no row for an exported operation".into())
                })?;
            let kind: String = row.try_get("input_kind").map_err(err)?;
            let sender: Option<String> = row.try_get("principal").map_err(err)?;
            if kind != InputKind::Command.as_str() || sender.as_deref() != Some(&e.principal) {
                return Err(StoreError(
                    "an exported operation's row was sent by another principal".into(),
                ));
            }
            match pass_through_of(&row)? {
                Some(p) if p == e.pass_through => {}
                Some(_) => {
                    return Err(StoreError(
                        "a row holds other pass-through data than the export".into(),
                    ));
                }
                None => {
                    tx.execute(
                        "UPDATE received_log SET pass_through = $2 \
                         WHERE op_ref = $1 AND pass_through IS NULL",
                        &[&e.op_ref.0.as_slice(), &e.pass_through.get()],
                    )
                    .await
                    .map_err(err)?;
                    written += 1;
                }
            }
        }
        tx.commit().await.map_err(err)?;
        Ok(written)
    }

    async fn put_tie(&self, principal: &str, party: &str) -> Result<(), StoreError> {
        let c = self.client().await?;
        c.execute(
            "INSERT INTO party_ties (principal, party) VALUES ($1, $2) \
             ON CONFLICT (principal) DO NOTHING",
            &[&principal, &party],
        )
        .await
        .map_err(err)?;
        Ok(())
    }

    async fn get(&self, op_ref: InputKey) -> Result<Option<Row>, StoreError> {
        let c = self.client().await?;
        let get = c
            .prepare_cached(&format!(
                "SELECT {COLUMNS} FROM received_log WHERE op_ref = $1"
            ))
            .await
            .map_err(err)?;
        c.query_opt(&get, &[&op_ref.0.as_slice()])
            .await
            .map_err(err)?
            .as_ref()
            .map(row_of)
            .transpose()
    }

    async fn pass_through_of(&self, op_refs: &[InputKey]) -> Result<Vec<Carried>, StoreError> {
        if op_refs.is_empty() {
            return Ok(Vec::new());
        }
        let c = self.client().await?;
        let select = c
            .prepare_cached(
                "SELECT op_ref, principal, pass_through FROM received_log \
                 WHERE op_ref = ANY($1) AND input_kind = 'command' AND pass_through IS NOT NULL",
            )
            .await
            .map_err(err)?;
        let keys: Vec<&[u8]> = op_refs.iter().map(|k| k.0.as_slice()).collect();
        c.query(&select, &[&keys])
            .await
            .map_err(err)?
            .iter()
            .map(|r| {
                let op_ref: Vec<u8> = r.try_get("op_ref").map_err(err)?;
                Ok(Carried {
                    op_ref: InputKey(
                        op_ref
                            .try_into()
                            .map_err(|_| StoreError("op_ref is not 16 bytes".into()))?,
                    ),
                    principal: r.try_get("principal").map_err(err)?,
                    pass_through: pass_through_of(r)?
                        .ok_or_else(|| StoreError("pass_through vanished".into()))?,
                })
            })
            .collect()
    }

    async fn next_undecided(&self) -> Result<Option<Row>, StoreError> {
        Ok(self.next_undecided_batch(1).await?.into_iter().next())
    }

    async fn next_undecided_batch(&self, n: usize) -> Result<Vec<Row>, StoreError> {
        let c = self.client().await?;
        let limit = i64::try_from(n).unwrap_or(i64::MAX);
        // F-038: rows in flight first, by their feed sequence; both reads use the undecided index.
        let mut rows = c
            .query(
                &format!(
                    "SELECT {COLUMNS} FROM received_log WHERE status <> 'Decided' \
                     AND feed_seq IS NOT NULL ORDER BY feed_seq LIMIT $1"
                ),
                &[&limit],
            )
            .await
            .map_err(err)?
            .iter()
            .map(row_of)
            .collect::<Result<Vec<_>, _>>()?;
        let left = limit - i64::try_from(rows.len()).unwrap_or(limit);
        if left > 0 {
            let fresh = c
                .query(
                    &format!(
                        "SELECT {COLUMNS} FROM received_log WHERE status <> 'Decided' \
                         AND feed_seq IS NULL ORDER BY reserves DESC, seq LIMIT $1"
                    ),
                    &[&left],
                )
                .await
                .map_err(err)?;
            for r in &fresh {
                rows.push(row_of(r)?);
            }
        }
        Ok(rows)
    }

    async fn mark_forwarded(&self, op_ref: InputKey, at: Time) -> Result<Row, StoreError> {
        let c = self.client().await?;
        c.execute(
            "UPDATE received_log SET status = 'Forwarded', forwarded_at = $2 \
             WHERE op_ref = $1 AND status = 'Received'",
            &[&op_ref.0.as_slice(), &i64_of(at)?],
        )
        .await
        .map_err(err)?;
        let r = c
            .query_one(
                &format!("SELECT {COLUMNS} FROM received_log WHERE op_ref = $1"),
                &[&op_ref.0.as_slice()],
            )
            .await
            .map_err(err)?;
        row_of(&r)
    }

    async fn mark_decided(
        &self,
        op_ref: InputKey,
        core_input_seq: u64,
        reply: Vec<u8>,
        at: Time,
    ) -> Result<(), StoreError> {
        let c = self.client().await?;
        c.execute(
            "UPDATE received_log SET status = 'Decided', core_input_seq = $2, core_reply = $3, \
             decided_at = $4 WHERE op_ref = $1 AND status <> 'Decided'",
            &[
                &op_ref.0.as_slice(),
                &i64_of(core_input_seq)?,
                &reply,
                &i64_of(at)?,
            ],
        )
        .await
        .map_err(err)?;
        Ok(())
    }

    async fn mark_forwarded_many(
        &self,
        op_refs: &[InputKey],
        at: Time,
    ) -> Result<Vec<Row>, StoreError> {
        if op_refs.is_empty() {
            return Ok(Vec::new());
        }
        let c = self.client().await?;
        let keys: Vec<&[u8]> = op_refs.iter().map(|r| r.0.as_slice()).collect();
        c.execute(
            "UPDATE received_log SET status = 'Forwarded', forwarded_at = $2 \
             WHERE op_ref = ANY($1) AND status = 'Received'",
            &[&keys, &i64_of(at)?],
        )
        .await
        .map_err(err)?;
        let rows = c
            .query(
                &format!("SELECT {COLUMNS} FROM received_log WHERE op_ref = ANY($1)"),
                &[&keys],
            )
            .await
            .map_err(err)?;
        let mut by_key = std::collections::HashMap::with_capacity(rows.len());
        for r in &rows {
            let row = row_of(r)?;
            by_key.insert(row.new.op_ref, row);
        }
        op_refs
            .iter()
            .map(|k| {
                by_key
                    .remove(k)
                    .ok_or_else(|| StoreError("a forwarded row vanished".into()))
            })
            .collect()
    }

    async fn mark_decided_many(&self, items: &[DecidedRow], at: Time) -> Result<(), StoreError> {
        if items.is_empty() {
            return Ok(());
        }
        let keys: Vec<&[u8]> = items.iter().map(|d| d.op_ref.0.as_slice()).collect();
        let seqs: Vec<i64> = items
            .iter()
            .map(|d| i64_of(d.input_seq))
            .collect::<Result<_, _>>()?;
        let replies: Vec<&[u8]> = items.iter().map(|d| d.reply.as_slice()).collect();
        let completes: Vec<bool> = items.iter().map(|d| d.completes).collect();
        let c = self.client().await?;
        // `completed_at` rides in the same statement (`T-009-25`): no write of its own. The
        // reply stamper never waits on a lock (`mark_replied_many`), so this statement waits at
        // most for the stamper's own short commit, and the two never deadlock.
        c.execute(
            "UPDATE received_log AS r SET status = 'Decided', core_input_seq = v.seq, \
             core_reply = v.reply, decided_at = $4, \
             completed_at = CASE WHEN v.completes THEN $4 ELSE r.completed_at END \
             FROM unnest($1::bytea[], $2::bigint[], $3::bytea[], $5::bool[]) \
             AS v(op_ref, seq, reply, completes) \
             WHERE r.op_ref = v.op_ref AND r.status <> 'Decided'",
            &[&keys, &seqs, &replies, &i64_of(at)?, &completes],
        )
        .await
        .map_err(err)?;
        Ok(())
    }

    async fn mark_replied_many(
        &self,
        stamps: &[(InputKey, Time)],
    ) -> Result<Vec<(InputKey, Time)>, StoreError> {
        if stamps.is_empty() {
            return Ok(Vec::new());
        }
        let keys: Vec<&[u8]> = stamps.iter().map(|(r, _)| r.0.as_slice()).collect();
        let ats: Vec<i64> = stamps
            .iter()
            .map(|(_, t)| i64_of(*t))
            .collect::<Result<_, _>>()?;
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(err)?;
        // A stamp is best-effort (`T-009-25`): its commit does not wait for the synchronous
        // standby, which every reply's commit does. It never waits on a row lock either: a row
        // the forwarder holds is skipped, so the forwarder never queues behind a stamp beyond this
        // short commit (stress run on PR #235).
        tx.batch_execute("SET LOCAL synchronous_commit = off")
            .await
            .map_err(err)?;
        let update = tx
            .prepare_cached(
                "WITH v AS (SELECT * FROM unnest($1::bytea[], $2::bigint[]) AS v(op_ref, at)), \
                 l AS (SELECT r.op_ref FROM received_log r JOIN v ON r.op_ref = v.op_ref \
                 WHERE r.replied_at IS NULL FOR UPDATE OF r SKIP LOCKED) \
                 UPDATE received_log AS r SET replied_at = v.at \
                 FROM l JOIN v ON v.op_ref = l.op_ref \
                 WHERE r.op_ref = l.op_ref RETURNING r.op_ref",
            )
            .await
            .map_err(err)?;
        let written: std::collections::HashSet<Vec<u8>> = tx
            .query(&update, &[&keys, &ats])
            .await
            .map_err(err)?
            .iter()
            .map(|r| r.get::<_, Vec<u8>>(0))
            .collect();
        let rest: Vec<&[u8]> = keys
            .iter()
            .copied()
            .filter(|k| !written.contains(*k))
            .collect();
        // Of the rows not written, the ones still unstamped were held by another writer. A plain
        // read takes no lock and waits on none.
        let held: std::collections::HashSet<Vec<u8>> = if rest.is_empty() {
            std::collections::HashSet::new()
        } else {
            tx.query(
                "SELECT op_ref FROM received_log WHERE op_ref = ANY($1) AND replied_at IS NULL",
                &[&rest],
            )
            .await
            .map_err(err)?
            .iter()
            .map(|r| r.get::<_, Vec<u8>>(0))
            .collect()
        };
        tx.commit().await.map_err(err)?;
        Ok(stamps
            .iter()
            .filter(|(k, _)| held.contains(k.0.as_slice()))
            .copied()
            .collect())
    }

    async fn mark_completed(
        &self,
        op_ref: InputKey,
        at: Time,
        principal: Option<&str>,
    ) -> Result<Option<Time>, StoreError> {
        let c = self.client().await?;
        let r = c
            .query_opt(
                "UPDATE received_log SET completed_at = $2 \
                 WHERE op_ref = $1 AND input_kind = 'command' AND completed_at IS NULL \
                 AND ($3::text IS NULL OR principal = $3) \
                 RETURNING COALESCE(submitted_at, received_at)",
                &[&op_ref.0.as_slice(), &i64_of(at)?, &principal],
            )
            .await
            .map_err(err)?;
        r.map(|r| u64_of(r.get(0))).transpose()
    }

    async fn mark_submitted(&self, op_ref: InputKey, at: Time) -> Result<bool, StoreError> {
        let c = self.client().await?;
        let n = c
            .execute(
                "UPDATE received_log SET submitted_at = $2 \
                 WHERE op_ref = $1 AND input_kind = 'command' AND submitted_at IS NULL \
                 AND convert_from(body, 'UTF8')::jsonb ->> 'command' = 'withdraw'",
                &[&op_ref.0.as_slice(), &i64_of(at)?],
            )
            .await
            .map_err(err)?;
        Ok(n == 1)
    }

    async fn oldest_undecided(&self) -> Result<Option<Time>, StoreError> {
        let c = self.client().await?;
        let v: Option<i64> = c
            .query_one(
                "SELECT min(received_at) FROM received_log WHERE status <> 'Decided'",
                &[],
            )
            .await
            .map_err(err)?
            .get(0);
        v.map(u64_of).transpose()
    }

    async fn rows(&self) -> Result<Vec<Row>, StoreError> {
        let c = self.client().await?;
        c.query(
            &format!("SELECT {COLUMNS} FROM received_log ORDER BY seq"),
            &[],
        )
        .await
        .map_err(err)?
        .iter()
        .map(row_of)
        .collect()
    }

    async fn delivery_pending(&self, notification: &str) -> Result<bool, StoreError> {
        self.exists(
            "SELECT EXISTS (SELECT 1 FROM received_log WHERE input_kind = 'delivery' \
             AND status <> 'Decided' AND client_key = $1)",
            Some(notification),
        )
        .await
    }

    async fn tick_pending(&self) -> Result<bool, StoreError> {
        self.exists(
            "SELECT EXISTS (SELECT 1 FROM received_log WHERE input_kind = 'tick' \
             AND status <> 'Decided')",
            None,
        )
        .await
    }

    async fn record_attempt(&self, a: Attempt) -> Result<(), StoreError> {
        let c = self.client().await?;
        c.execute(
            "INSERT INTO delivery_attempt (notification, at, outcome, next_at) \
             VALUES ($1, $2, $3, $4)",
            &[
                &a.notification,
                &i64_of(a.at)?,
                &a.outcome,
                &i64_of(a.next_at)?,
            ],
        )
        .await
        .map_err(err)?;
        Ok(())
    }

    async fn last_attempt(&self, notification: &str) -> Result<Option<Attempt>, StoreError> {
        let c = self.client().await?;
        let r = c
            .query_opt(
                "SELECT at, outcome, next_at FROM delivery_attempt WHERE notification = $1 \
                 ORDER BY id DESC LIMIT 1",
                &[&notification],
            )
            .await
            .map_err(err)?;
        r.map(|r| {
            Ok(Attempt {
                notification: notification.to_owned(),
                at: u64_of(r.get(0))?,
                outcome: r.get(1),
                next_at: u64_of(r.get(2))?,
            })
        })
        .transpose()
    }

    async fn attempts(&self, notification: &str) -> Result<u64, StoreError> {
        let c = self.client().await?;
        let n: i64 = c
            .query_one(
                "SELECT count(*) FROM delivery_attempt WHERE notification = $1",
                &[&notification],
            )
            .await
            .map_err(err)?
            .get(0);
        u64_of(n)
    }

    async fn recipient(&self, principal: &str) -> Result<Option<Recipient>, StoreError> {
        let c = self.client().await?;
        let r = c
            .query_opt(
                "SELECT endpoint, secret_nonce, secret_sealed FROM recipient WHERE principal = $1",
                &[&principal],
            )
            .await
            .map_err(err)?;
        let Some(r) = r else { return Ok(None) };
        let nonce: Option<Vec<u8>> = r.get(1);
        let sealed: Option<Vec<u8>> = r.get(2);
        let secret = match (nonce, sealed) {
            (Some(n), Some(s)) => Some(self.open(principal, &n, &s)?),
            _ => None,
        };
        Ok(Some(Recipient {
            principal: principal.to_owned(),
            endpoint: r.get(0),
            secret,
        }))
    }

    async fn put_recipient(&self, r: Recipient) -> Result<(), StoreError> {
        let (nonce, sealed) = match &r.secret {
            Some(s) => {
                let (n, s) = self.seal(&r.principal, s)?;
                (Some(n), Some(s))
            }
            None => (None, None),
        };
        let c = self.client().await?;
        c.execute(
            "INSERT INTO recipient (principal, endpoint, secret_nonce, secret_sealed) \
             VALUES ($1, $2, $3, $4) ON CONFLICT (principal) DO UPDATE SET \
             endpoint = EXCLUDED.endpoint, secret_nonce = EXCLUDED.secret_nonce, \
             secret_sealed = EXCLUDED.secret_sealed",
            &[&r.principal, &r.endpoint, &nonce, &sealed],
        )
        .await
        .map_err(err)?;
        Ok(())
    }

    async fn answering(&self, principal: &str) -> Result<Option<Answering>, StoreError> {
        let c = self.client().await?;
        let answering = c
            .prepare_cached(
                "SELECT decided, wait_ms, set_by, set_at FROM principal_answering \
                 WHERE principal = $1",
            )
            .await
            .map_err(err)?;
        let r = c.query_opt(&answering, &[&principal]).await.map_err(err)?;
        let Some(r) = r else { return Ok(None) };
        let wait_ms: i64 = r.get(1);
        let set_at: i64 = r.get(3);
        Ok(Some(Answering {
            principal: principal.to_owned(),
            decided: r.get(0),
            wait_ms: u64_of(wait_ms)?,
            set_by: r.get(2),
            set_at: u64_of(set_at)?,
        }))
    }

    async fn put_answering(&self, a: Answering) -> Result<(), StoreError> {
        let wait_ms = i64::try_from(a.wait_ms).map_err(|e| StoreError(e.to_string()))?;
        let set_at = i64::try_from(a.set_at).map_err(|e| StoreError(e.to_string()))?;
        let c = self.client().await?;
        c.execute(
            "INSERT INTO principal_answering (principal, decided, wait_ms, set_by, set_at) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (principal) DO UPDATE SET \
             decided = EXCLUDED.decided, wait_ms = EXCLUDED.wait_ms, \
             set_by = EXCLUDED.set_by, set_at = EXCLUDED.set_at",
            &[&a.principal, &a.decided, &wait_ms, &a.set_by, &set_at],
        )
        .await
        .map_err(err)?;
        Ok(())
    }

    async fn try_lead(&self, _replica: &str) -> Result<bool, StoreError> {
        let mut leader = self.leader.lock().await;
        if let Some(l) = leader.as_ref() {
            let alive = l.client.simple_query("SELECT 1").await.is_ok();
            if !alive {
                *leader = None;
            } else if l.held {
                return Ok(true);
            }
        }
        if leader.is_none() {
            let (client, connection) = self.config.connect(self.tls.clone()).await.map_err(err)?;
            tokio::spawn(async move {
                let _ = connection.await;
            });
            *leader = Some(Leader {
                client,
                held: false,
            });
        }
        let Some(l) = leader.as_mut() else {
            return Ok(false);
        };
        let got: bool = l
            .client
            .query_one("SELECT pg_try_advisory_lock($1)", &[&LEADER_LOCK])
            .await
            .map_err(err)?
            .get(0);
        l.held = got;
        Ok(got)
    }

    async fn resign(&self, _replica: &str) -> Result<(), StoreError> {
        let mut leader = self.leader.lock().await;
        if let Some(l) = leader.take()
            && l.held
        {
            let _ = l
                .client
                .execute("SELECT pg_advisory_unlock($1)", &[&LEADER_LOCK])
                .await;
        }
        Ok(())
    }

    async fn try_feed(
        &self,
        partition: &str,
        holder: &str,
        lease: std::time::Duration,
    ) -> Result<Option<u64>, StoreError> {
        let c = self.client().await?;
        let ms = i64::try_from(lease.as_millis()).unwrap_or(i64::MAX);
        // The database's clock, so the instances' clocks never decide who holds it. A renewal by
        // the live holder keeps the epoch; a change of holder, or a lapse, raises it.
        let row = c
            .query_opt(
                "INSERT INTO feed_lease AS l (partition, holder, epoch, expires_at, next_seq) \
                 VALUES ($1, $2, 1, clock_timestamp() + $3::bigint * interval '1 millisecond', 1) \
                 ON CONFLICT (partition) DO UPDATE SET \
                   epoch = CASE WHEN l.holder = EXCLUDED.holder \
                                 AND l.expires_at > clock_timestamp() \
                            THEN l.epoch ELSE l.epoch + 1 END, \
                   holder = EXCLUDED.holder, \
                   expires_at = EXCLUDED.expires_at \
                 WHERE l.holder = EXCLUDED.holder OR l.expires_at <= clock_timestamp() \
                 RETURNING epoch",
                &[&partition, &holder, &ms],
            )
            .await
            .map_err(err)?;
        row.map(|r| u64_of(r.get(0))).transpose()
    }

    async fn release_feed(&self, partition: &str, holder: &str) -> Result<(), StoreError> {
        let c = self.client().await?;
        c.execute(
            "UPDATE feed_lease SET expires_at = clock_timestamp() - interval '1 millisecond' \
             WHERE partition = $1 AND holder = $2",
            &[&partition, &holder],
        )
        .await
        .map_err(err)?;
        Ok(())
    }

    async fn feed_live(&self, partition: &str) -> Result<bool, StoreError> {
        let c = self.client().await?;
        Ok(c.query_one(
            "SELECT EXISTS (SELECT 1 FROM feed_lease WHERE partition = $1 \
             AND expires_at > clock_timestamp())",
            &[&partition],
        )
        .await
        .map_err(err)?
        .get(0))
    }

    async fn mark_fed(
        &self,
        partition: &str,
        holder: &str,
        epoch: u64,
        op_refs: &[InputKey],
        at: Time,
    ) -> Result<Vec<Row>, StoreError> {
        if op_refs.is_empty() {
            return Ok(Vec::new());
        }
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(err)?;
        // The lease row, locked: every assignment of the partition's sequences queues here, so
        // they are gapless and unique without an index on the log.
        let lease = tx
            .query_opt(
                "SELECT next_seq FROM feed_lease WHERE partition = $1 AND holder = $2 \
                 AND epoch = $3 AND expires_at > clock_timestamp() FOR UPDATE",
                &[&partition, &holder, &i64_of(epoch)?],
            )
            .await
            .map_err(err)?;
        let Some(lease) = lease else {
            return Err(StoreError::fenced());
        };
        let mut next: i64 = lease.get(0);
        let keys: Vec<&[u8]> = op_refs.iter().map(|r| r.0.as_slice()).collect();
        let current = tx
            .query(
                "SELECT op_ref, feed_seq, status FROM received_log WHERE op_ref = ANY($1) \
                 FOR UPDATE",
                &[&keys],
            )
            .await
            .map_err(err)?;
        let mut state = std::collections::HashMap::with_capacity(current.len());
        for r in &current {
            let op: Vec<u8> = r.get(0);
            let seq: Option<i64> = r.get(1);
            let status: String = r.get(2);
            state.insert(op, (seq, status));
        }
        let mut assign: Vec<&[u8]> = Vec::new();
        let mut seqs: Vec<i64> = Vec::new();
        for k in &keys {
            if let Some((None, status)) = state.get(*k)
                && status != "Decided"
            {
                assign.push(k);
                seqs.push(next);
                next += 1;
            }
        }
        if !assign.is_empty() {
            tx.execute(
                "UPDATE received_log r SET feed_seq = v.s, feed_epoch = $3 \
                 FROM unnest($1::bytea[], $2::bigint[]) AS v(op, s) WHERE r.op_ref = v.op",
                &[&assign, &seqs, &i64_of(epoch)?],
            )
            .await
            .map_err(err)?;
            tx.execute(
                "UPDATE feed_lease SET next_seq = $2 WHERE partition = $1",
                &[&partition, &next],
            )
            .await
            .map_err(err)?;
        }
        tx.execute(
            "UPDATE received_log SET status = 'Forwarded', forwarded_at = $2 \
             WHERE op_ref = ANY($1) AND status = 'Received'",
            &[&keys, &i64_of(at)?],
        )
        .await
        .map_err(err)?;
        let rows = tx
            .query(
                &format!("SELECT {COLUMNS} FROM received_log WHERE op_ref = ANY($1)"),
                &[&keys],
            )
            .await
            .map_err(err)?;
        tx.commit().await.map_err(err)?;
        let mut by_key = std::collections::HashMap::with_capacity(rows.len());
        for r in &rows {
            let row = row_of(r)?;
            by_key.insert(row.new.op_ref, row);
        }
        op_refs
            .iter()
            .map(|k| {
                by_key
                    .remove(k)
                    .ok_or_else(|| StoreError("a forwarded row vanished".into()))
            })
            .collect()
    }

    async fn unfeed(
        &self,
        partition: &str,
        holder: &str,
        epoch: u64,
        from: u64,
    ) -> Result<(), StoreError> {
        let mut c = self.client().await?;
        let tx = c.transaction().await.map_err(err)?;
        let held = tx
            .query_opt(
                "SELECT next_seq FROM feed_lease WHERE partition = $1 AND holder = $2 \
                 AND epoch = $3 AND expires_at > clock_timestamp() FOR UPDATE",
                &[&partition, &holder, &i64_of(epoch)?],
            )
            .await
            .map_err(err)?;
        if held.is_none() {
            return Err(StoreError::fenced());
        }
        let from = i64_of(from)?;
        // The tail is undecided by contract (the caller gives back only what certainly did not
        // run), so it is found through the undecided index, never by a scan of the log.
        tx.execute(
            "UPDATE received_log SET feed_seq = NULL, feed_epoch = NULL \
             WHERE status <> 'Decided' AND feed_seq >= $1",
            &[&from],
        )
        .await
        .map_err(err)?;
        tx.execute(
            "UPDATE feed_lease SET next_seq = LEAST(next_seq, $2) WHERE partition = $1",
            &[&partition, &from],
        )
        .await
        .map_err(err)?;
        tx.commit().await.map_err(err)
    }

    async fn copy_verifier(
        &self,
        credential: &str,
        found: Option<&crate::boundary::CredentialBody>,
        at: Time,
    ) -> Result<(), StoreError> {
        let c = self.client().await?;
        match found {
            Some(f) => c
                .execute(
                    "INSERT INTO verifier_copy (credential, identity, kind, scheme, verifier, \
                     read_at) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (credential) DO UPDATE \
                     SET identity = EXCLUDED.identity, kind = EXCLUDED.kind, \
                     scheme = EXCLUDED.scheme, verifier = EXCLUDED.verifier, \
                     read_at = EXCLUDED.read_at",
                    &[
                        &credential,
                        &f.identity,
                        &f.kind,
                        &f.scheme,
                        &f.verifier,
                        &i64_of(at)?,
                    ],
                )
                .await
                .map_err(err)?,
            None => c
                .execute(
                    "DELETE FROM verifier_copy WHERE credential = $1",
                    &[&credential],
                )
                .await
                .map_err(err)?,
        };
        Ok(())
    }

    async fn copied_verifier(
        &self,
        credential: &str,
    ) -> Result<Option<crate::boundary::CredentialBody>, StoreError> {
        let c = self.client().await?;
        Ok(c.query_opt(
            "SELECT identity, kind, scheme, verifier FROM verifier_copy WHERE credential = $1",
            &[&credential],
        )
        .await
        .map_err(err)?
        .map(|r| crate::boundary::CredentialBody {
            identity: r.get(0),
            kind: r.get(1),
            scheme: r.get(2),
            verifier: r.get(3),
        }))
    }

    async fn advertise(
        &self,
        instance: &str,
        address: &str,
        ttl: std::time::Duration,
    ) -> Result<Vec<crate::stream::RosterEntry>, StoreError> {
        let c = self.client().await?;
        let ms = i64::try_from(ttl.as_millis()).unwrap_or(i64::MAX);
        // The database's clock, as the feed lease's: the instances' clocks never decide who is
        // live.
        c.execute(
            "INSERT INTO ingress_roster (instance, address, expires_at) \
             VALUES ($1, $2, clock_timestamp() + $3::bigint * interval '1 millisecond') \
             ON CONFLICT (instance) DO UPDATE SET \
               address = EXCLUDED.address, expires_at = EXCLUDED.expires_at",
            &[&instance, &address, &ms],
        )
        .await
        .map_err(err)?;
        Ok(c.query(
            "SELECT instance, address FROM ingress_roster \
             WHERE expires_at > clock_timestamp() ORDER BY instance",
            &[],
        )
        .await
        .map_err(err)?
        .into_iter()
        .map(|r| crate::stream::RosterEntry {
            instance: r.get(0),
            address: r.get(1),
        })
        .collect())
    }

    async fn withdraw(&self, instance: &str) -> Result<(), StoreError> {
        let c = self.client().await?;
        c.execute(
            "DELETE FROM ingress_roster WHERE instance = $1",
            &[&instance],
        )
        .await
        .map_err(err)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every certificate of the embedded RDS bundle is read and trusted beside the Mozilla roots:
    /// A server that forces TLS (`rds.force_ssl`) presents a certificate that chains to one of
    /// them.
    #[test]
    fn the_tls_connector_trusts_the_rds_bundle() {
        use rustls::pki_types::CertificateDer;
        use rustls::pki_types::pem::PemObject as _;
        let certs = CertificateDer::pem_slice_iter(RDS_CA_BUNDLE.as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(certs.len(), 111);
        assert!(tls().is_ok());
    }

    /// `T-009-14`: in production, a connection string that does not require TLS is refused
    /// before any connection is made, and the refusal never carries the string; in development,
    /// every mode is taken as written.
    #[test]
    fn t_009_14_production_refuses_a_database_connection_without_tls() {
        let parse = |s: &str| s.parse::<tokio_postgres::Config>().unwrap();
        for refused in [
            "postgres://u:hunter2@db.example:5432/ingress",
            "postgres://u:hunter2@db.example:5432/ingress?sslmode=prefer",
            "postgres://u:hunter2@db.example:5432/ingress?sslmode=disable",
            "host=db.example user=u password=hunter2 dbname=ingress",
        ] {
            let e = check_tls(&parse(refused), Profile::Production).unwrap_err();
            assert!(e.contains("sslmode=require"), "{e}");
            assert!(!e.contains("hunter2") && !e.contains("db.example"), "{e}");
            assert!(check_tls(&parse(refused), Profile::Development).is_ok());
        }
        for taken in [
            "postgres://u:p@db.example:5432/ingress?sslmode=require",
            "host=db.example user=u sslmode=require",
        ] {
            assert!(check_tls(&parse(taken), Profile::Production).is_ok());
        }
    }

    /// `T-009-14`: the store applies the check before it opens a connection, so a production
    /// replica with a plain-text string never reaches the server.
    #[tokio::test]
    async fn t_009_14_a_production_store_without_tls_refuses_to_start() {
        let refused = PgStore::connect(PgOptions {
            // Nothing listens on port 1: reaching it would be a different error.
            url: "postgres://u:hunter2@127.0.0.1:1/ingress".into(),
            sealing_key: [7; 32],
            require_sync_standby: SyncStandby::Required,
            profile: Profile::Production,
            pool_size: 1,
        })
        .await;
        let Err(e) = refused else {
            panic!("a production store without TLS started")
        };
        assert!(e.0.contains("sslmode=require"), "{}", e.0);
    }
}
