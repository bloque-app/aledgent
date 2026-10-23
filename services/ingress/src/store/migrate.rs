// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The ingress store's schema migrations (`T-009-26`; owner ruling `v0w-ingress-migrations`,
//! 2026-10-13: keep the in-house runner and harden it).
//!
//! - **Embedded, ordered, once each.** Every file under `services/ingress/migrations/` is built
//!   into the binary ([`MIGRATIONS`]) and applied in version order, one transaction per file,
//!   recorded in `ingress_schema` as `(version, applied_at, checksum)`.
//! - **An applied migration never changes.** `checksum` is the SHA-256 of the file's exact bytes,
//!   hex. At every start each applied version's recorded checksum is compared with this build's
//!   file, and a difference is a start-up error naming the version: an edited migration never
//!   passes silently. A version applied before checksums were kept (`checksum` NULL) is
//!   **backfilled** with this build's checksum the first time a migrator sees it, and logged; from
//!   then on it is compared like any other. Trade-off, stated plainly: a file edited before its
//!   backfill is recorded as edited, since there is nothing older to compare it with.
//! - **One migrator at a time.** Creating `ingress_schema` runs under a transaction-scoped advisory
//!   lock ([`BOOTSTRAP_LOCK`]), so migrators that start together on an empty database do not race
//!   on the catalogue. Each migration's transaction then takes `ingress_schema` in `EXCLUSIVE`
//!   mode, which only another migrator contends for (`serve` never touches the table). That wait
//!   has no bound of its own, since the migrator ahead of it is bounded by the lock timeout below;
//!   a migrator that has to wait says so in the log first.
//! - **A lock timeout on every migration.** Right after the migrator's lock, the transaction runs
//!   `SET LOCAL lock_timeout` ([`crate::config::MigrationsConfig::lock_timeout_ms`], 5 s by
//!   default), so a migration that needs a table some other transaction holds gives up after the
//!   timeout. While it waits, statements that need that table queue behind it, so the timeout is
//!   also the longest an append waits on a migration's lock request. It records nothing, the start
//!   fails with a message saying so, and the next start retries. A file may still set its own
//!   (`0006` does).
//! - **Nothing to do takes nothing.** When nothing is pending and every checksum is recorded,
//!   [`apply`] only reads: no lock and no DDL, so a role limited to rows passes it.
//! - **Apply or verify.** `serve` applies what is pending at start by default
//!   ([`crate::config::OnStart::Apply`]); with `"verify"` it changes nothing and refuses to start if
//!   a migration is pending or differs ([`verify`]). `aledgent-ingress migrate --status` prints
//!   [`status`]; `aledgent-ingress migrate --apply` runs [`apply`] and exits.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use sha2::{Digest, Sha256};
use tokio_postgres::Client;
use tokio_postgres::error::SqlState;

use super::StoreError;
use crate::config::{MigrationsConfig, OnStart, Profile};

/// One migration file, embedded.
#[derive(Clone, Copy, Debug)]
pub struct Migration {
    /// Its version: the order it applies in, and its key in `ingress_schema`.
    pub version: i32,
    /// Its file name under `services/ingress/migrations/`.
    pub name: &'static str,
    /// The file's exact contents.
    pub sql: &'static str,
}

impl Migration {
    /// The SHA-256 of the file's exact bytes, hex: what `ingress_schema.checksum` records.
    pub fn checksum(&self) -> String {
        hex::encode(Sha256::digest(self.sql.as_bytes()))
    }
}

macro_rules! migration {
    ($version:expr, $file:literal) => {
        Migration {
            version: $version,
            name: $file,
            sql: include_str!(concat!("../../migrations/", $file)),
        }
    };
}

/// The migrations, in order. A new one is appended; an applied one is never edited.
pub const MIGRATIONS: &[Migration] = &[
    migration!(1, "0001_received_log.sql"),
    migration!(2, "0002_passkeys.sql"),
    migration!(3, "0003_client_key_domain.sql"),
    migration!(4, "0004_answering.sql"),
    migration!(5, "0005_pass_through.sql"),
    migration!(6, "0006_latency_columns.sql"),
    migration!(7, "0007_ingress_fleet.sql"),
    migration!(8, "0008_ingress_roster.sql"),
    migration!(9, "0009_overtakes.sql"),
];

/// Where one version stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    /// Applied, and its recorded checksum is this build's file's.
    Ok,
    /// Applied before checksums were kept: the next migrator records this build's checksum.
    Backfillable,
    /// In this build, not applied.
    Pending,
    /// Applied with a different checksum than this build's file: the file was edited.
    Mismatch {
        /// The checksum recorded when it was applied (or backfilled).
        recorded: String,
    },
    /// Applied, and not in this build: a later build applied it (this one is older than the
    /// schema, as after a rollback). Reported, never refused.
    Unknown,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            State::Ok => "applied/ok",
            State::Backfillable => "applied/backfillable",
            State::Pending => "pending",
            State::Mismatch { .. } => "mismatch",
            State::Unknown => "applied/unknown",
        })
    }
}

/// One version's line in [`Status`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The version.
    pub version: i32,
    /// Its file in this build, if it has one.
    pub name: Option<&'static str>,
    /// When it was applied (UTC, to the second), if it was.
    pub applied_at: Option<String>,
    /// This build's file's checksum, if this build has the file.
    pub checksum: Option<String>,
    /// Where it stands.
    pub state: State,
}

/// Every version this build embeds or the database records, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    /// One entry per version.
    pub entries: Vec<Entry>,
}

impl Status {
    fn versions(&self, keep: impl Fn(&State) -> bool) -> Vec<i32> {
        self.entries
            .iter()
            .filter(|e| keep(&e.state))
            .map(|e| e.version)
            .collect()
    }

    /// The versions not yet applied.
    pub fn pending(&self) -> Vec<i32> {
        self.versions(|s| *s == State::Pending)
    }

    /// The applied versions whose file differs from what was applied.
    pub fn mismatched(&self) -> Vec<i32> {
        self.versions(|s| matches!(s, State::Mismatch { .. }))
    }

    /// The applied versions with no checksum recorded yet.
    pub fn backfillable(&self) -> Vec<i32> {
        self.versions(|s| *s == State::Backfillable)
    }

    /// The applied versions this build does not have.
    pub fn unknown(&self) -> Vec<i32> {
        self.versions(|s| *s == State::Unknown)
    }

    /// An error naming every mismatched version, if there is one.
    fn refuse_mismatch(&self) -> Result<(), StoreError> {
        let bad: Vec<String> = self
            .entries
            .iter()
            .filter_map(|e| match &e.state {
                State::Mismatch { recorded } => Some(format!(
                    "migration {} ({}) was applied with checksum {recorded}, and this build's file \
                     has {}",
                    e.version,
                    e.name.unwrap_or("?"),
                    e.checksum.as_deref().unwrap_or("?"),
                )),
                _ => None,
            })
            .collect();
        if bad.is_empty() {
            return Ok(());
        }
        Err(StoreError(format!(
            "{}: an applied migration was edited. Refusing to start: restore the file as it was \
             applied and put the change in a new migration",
            bad.join("; ")
        )))
    }

    fn warn(&self) {
        let unknown = self.unknown();
        if !unknown.is_empty() {
            tracing::warn!(
                ?unknown,
                "the database has migrations this build does not: a later build applied them"
            );
        }
    }
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "{:<8} {:<30} {:<21} {:<21} checksum",
            "version", "file", "state", "applied_at"
        )?;
        for e in &self.entries {
            let checksum = match &e.state {
                State::Mismatch { recorded } => format!(
                    "recorded {recorded}, file {}",
                    e.checksum.as_deref().unwrap_or("-")
                ),
                _ => e.checksum.clone().unwrap_or_else(|| "-".into()),
            };
            writeln!(
                f,
                "{:<8} {:<30} {:<21} {:<21} {checksum}",
                e.version,
                e.name.unwrap_or("-"),
                e.state.to_string(),
                e.applied_at.as_deref().unwrap_or("-"),
            )?;
        }
        let count =
            |s: &dyn Fn(&State) -> bool| self.entries.iter().filter(|e| s(&e.state)).count();
        writeln!(
            f,
            "{} applied/ok, {} applied/backfillable, {} pending, {} mismatch, {} applied/unknown",
            count(&|s| *s == State::Ok),
            count(&|s| *s == State::Backfillable),
            count(&|s| *s == State::Pending),
            count(&|s| matches!(s, State::Mismatch { .. })),
            count(&|s| *s == State::Unknown),
        )
    }
}

/// What [`apply`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// The versions it applied.
    pub applied: Vec<i32>,
    /// The versions whose checksum it recorded for the first time.
    pub backfilled: Vec<i32>,
}

/// The advisory lock that serialises creating `ingress_schema`: "ingr_sch" in ASCII. Distinct
/// from the leader's lock.
pub const BOOTSTRAP_LOCK: i64 = 0x696e_6772_5f73_6368;

/// A PostgreSQL error with the server's message and SQLSTATE: `tokio_postgres` prints a server
/// error as `db error` alone. The server's `detail` is left out; the message never carries the
/// connection string (`REQ-AU-8`).
fn describe(e: &tokio_postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => format!(
            "{}: {} (SQLSTATE {})",
            db.severity(),
            db.message(),
            db.code().code()
        ),
        None => e.to_string(),
    }
}

fn err(e: tokio_postgres::Error) -> StoreError {
    StoreError(describe(&e))
}

/// Whether `ingress_schema` has its checksum column, looked up on the very table
/// `to_regclass` resolves, whichever schema of the search path holds it.
const HAS_CHECKSUM: &str = "SELECT EXISTS (SELECT 1 FROM pg_attribute WHERE attrelid = \
     to_regclass('ingress_schema') AND attname = 'checksum' AND NOT attisdropped)";

/// One connection for `aledgent-ingress migrate`, made as the store's are: the profile's TLS
/// check first, then rustls, with synchronous commit.
pub async fn connect(url: &str, profile: Profile) -> Result<Client, StoreError> {
    let mut config: tokio_postgres::Config = url.parse().map_err(err)?;
    super::pg::check_tls(&config, profile).map_err(StoreError)?;
    config.options("-c synchronous_commit=on");
    let (client, connection) = config
        .connect(super::pg::tls().map_err(StoreError)?)
        .await
        .map_err(err)?;
    tokio::spawn(async move {
        if let Err(e) = connection.await {
            tracing::warn!(error = %e, "the migration connection closed");
        }
    });
    Ok(client)
}

/// The versions the database records: `version → (applied_at, checksum)`. Reads only; works on
/// a database with no `ingress_schema`, or one from before checksums.
async fn recorded(c: &Client) -> Result<BTreeMap<i32, (String, Option<String>)>, StoreError> {
    let exists: bool = c
        .query_one("SELECT to_regclass('ingress_schema') IS NOT NULL", &[])
        .await
        .map_err(err)?
        .get(0);
    if !exists {
        return Ok(BTreeMap::new());
    }
    let with_checksum: bool = c.query_one(HAS_CHECKSUM, &[]).await.map_err(err)?.get(0);
    let checksum = if with_checksum {
        "checksum"
    } else {
        "NULL::text"
    };
    let rows = c
        .query(
            &format!(
                "SELECT version, to_char(applied_at AT TIME ZONE 'UTC', \
                 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"'), {checksum} FROM ingress_schema"
            ),
            &[],
        )
        .await
        .map_err(err)?;
    Ok(rows
        .iter()
        .map(|r| (r.get(0), (r.get(1), r.get(2))))
        .collect())
}

/// Where every version stands, read without a lock and without writing.
pub async fn status(c: &Client, set: &[Migration]) -> Result<Status, StoreError> {
    let mut recorded = recorded(c).await?;
    let mut entries: Vec<Entry> = set
        .iter()
        .map(|m| {
            let checksum = m.checksum();
            let (applied_at, state) = match recorded.remove(&m.version) {
                None => (None, State::Pending),
                Some((at, None)) => (Some(at), State::Backfillable),
                Some((at, Some(r))) if r == checksum => (Some(at), State::Ok),
                Some((at, Some(recorded))) => (Some(at), State::Mismatch { recorded }),
            };
            Entry {
                version: m.version,
                name: Some(m.name),
                applied_at,
                checksum: Some(checksum),
                state,
            }
        })
        .collect();
    entries.extend(recorded.into_iter().map(|(version, (at, _))| Entry {
        version,
        name: None,
        applied_at: Some(at),
        checksum: None,
        state: State::Unknown,
    }));
    entries.sort_by_key(|e| e.version);
    Ok(Status { entries })
}

/// Take the migrator's lock, then bound every later lock wait in this transaction. If another
/// migrator holds it, say so before waiting: that wait has no bound of its own.
async fn begin(
    tx: &mut tokio_postgres::Transaction<'_>,
    lock_timeout: Duration,
) -> Result<(), StoreError> {
    let probe = tx.savepoint("migrator_lock").await.map_err(err)?;
    match probe
        .execute("LOCK TABLE ingress_schema IN EXCLUSIVE MODE NOWAIT", &[])
        .await
    {
        Ok(_) => probe.commit().await.map_err(err)?,
        Err(e) if e.code() == Some(&SqlState::LOCK_NOT_AVAILABLE) => {
            probe.rollback().await.map_err(err)?;
            tracing::info!("another migrator holds ingress_schema: waiting for it to finish");
            tx.execute("LOCK TABLE ingress_schema IN EXCLUSIVE MODE", &[])
                .await
                .map_err(err)?;
        }
        Err(e) => return Err(err(e)),
    }
    tx.execute(
        "SELECT set_config('lock_timeout', $1, true)",
        &[&format!("{}ms", lock_timeout.as_millis())],
    )
    .await
    .map_err(err)?;
    Ok(())
}

/// A failed statement of `what`, saying plainly when it was the lock timeout.
fn failed(what: &str, e: tokio_postgres::Error, lock_timeout: Duration) -> StoreError {
    if e.code() == Some(&SqlState::LOCK_NOT_AVAILABLE) {
        StoreError(format!(
            "{what} gave up waiting for a lock (lock_timeout: {} ms, migrations.lock_timeout_ms, \
             unless the file sets its own): another session holds a table it changes. Nothing of \
             it was recorded; the next start retries",
            lock_timeout.as_millis()
        ))
    } else {
        StoreError(format!("{what} failed: {}", describe(&e)))
    }
}

/// Apply every pending migration of `set` in order, one transaction each, and backfill the
/// checksum of every applied one that has none. Refuses before changing anything if an applied
/// migration differs from `set`, and stops at the first that fails, recording nothing of it.
pub async fn apply(
    c: &mut Client,
    set: &[Migration],
    lock_timeout: Duration,
) -> Result<Applied, StoreError> {
    let before = status(c, set).await?;
    before.refuse_mismatch()?;
    // Nothing to apply or record: no lock, no DDL, so a role that may only read and write rows
    // passes here too (`opsctl`, a replica left at "apply" on an up-to-date schema).
    if before.pending().is_empty() && before.backfillable().is_empty() {
        before.warn();
        return Ok(Applied::default());
    }
    // Created under an advisory lock, so two migrators never create it at once. A migrator that
    // waited on the lock may still find it missing: this session's catalogue cache is refreshed
    // only when it next locks a relation. Its `CREATE` then fails as a duplicate, which means the
    // table is there.
    {
        let mut tx = c.transaction().await.map_err(err)?;
        tx.execute("SELECT pg_advisory_xact_lock($1)", &[&BOOTSTRAP_LOCK])
            .await
            .map_err(err)?;
        // Only when missing: `CREATE TABLE IF NOT EXISTS` needs `CREATE` on the schema even when
        // the table is there.
        let exists: bool = tx
            .query_one("SELECT to_regclass('ingress_schema') IS NOT NULL", &[])
            .await
            .map_err(err)?
            .get(0);
        if !exists {
            let create = tx.savepoint("create_ingress_schema").await.map_err(err)?;
            match create
                .batch_execute(
                    "CREATE TABLE ingress_schema (version INTEGER PRIMARY KEY, \
                     applied_at TIMESTAMPTZ NOT NULL DEFAULT now(), checksum TEXT)",
                )
                .await
            {
                Ok(()) => create.commit().await.map_err(err)?,
                Err(e)
                    if e.code() == Some(&SqlState::DUPLICATE_TABLE)
                        || e.code() == Some(&SqlState::UNIQUE_VIOLATION) =>
                {
                    create.rollback().await.map_err(err)?
                }
                Err(e) => return Err(err(e)),
            }
        }
        tx.commit().await.map_err(err)?;
    }
    // A table from before T-009-26 gains its checksum column: once, under the migrator's lock.
    {
        let mut tx = c.transaction().await.map_err(err)?;
        begin(&mut tx, lock_timeout).await?;
        let present: bool = tx.query_one(HAS_CHECKSUM, &[]).await.map_err(err)?.get(0);
        if !present {
            tx.batch_execute("ALTER TABLE ingress_schema ADD COLUMN IF NOT EXISTS checksum TEXT")
                .await
                .map_err(|e| failed("adding ingress_schema.checksum", e, lock_timeout))?;
            tracing::info!("ingress_schema gained its checksum column");
        }
        tx.commit().await.map_err(err)?;
    }
    let mut done = Applied::default();
    for m in set {
        let what = format!("migration {} ({})", m.version, m.name);
        let checksum = m.checksum();
        let mut tx = c.transaction().await.map_err(err)?;
        begin(&mut tx, lock_timeout).await?;
        let row = tx
            .query_opt(
                "SELECT checksum FROM ingress_schema WHERE version = $1",
                &[&m.version],
            )
            .await
            .map_err(err)?
            .map(|r| r.get::<_, Option<String>>(0));
        match row {
            Some(Some(recorded)) if recorded == checksum => {}
            Some(Some(recorded)) => {
                return Err(StoreError(format!(
                    "{what} was applied with checksum {recorded}, and this build's file has \
                     {checksum}: an applied migration was edited. Refusing to start: restore the \
                     file as it was applied and put the change in a new migration"
                )));
            }
            Some(None) => {
                tx.execute(
                    "UPDATE ingress_schema SET checksum = $2 WHERE version = $1",
                    &[&m.version, &checksum],
                )
                .await
                .map_err(err)?;
                tx.commit().await.map_err(err)?;
                tracing::warn!(
                    version = m.version,
                    file = m.name,
                    %checksum,
                    "backfilled the checksum of a migration applied before checksums were kept: \
                     this build's file is taken as the one applied"
                );
                done.backfilled.push(m.version);
                continue;
            }
            None => {
                tx.batch_execute(m.sql)
                    .await
                    .map_err(|e| failed(&what, e, lock_timeout))?;
                tx.execute(
                    "INSERT INTO ingress_schema (version, checksum) VALUES ($1, $2)",
                    &[&m.version, &checksum],
                )
                .await
                .map_err(err)?;
                tx.commit()
                    .await
                    .map_err(|e| failed(&what, e, lock_timeout))?;
                tracing::info!(version = m.version, file = m.name, %checksum, "applied");
                done.applied.push(m.version);
                continue;
            }
        }
        tx.commit().await.map_err(err)?;
    }
    status(c, set).await?.warn();
    Ok(done)
}

/// Change nothing; refuse if a migration of `set` is pending or differs from what was applied.
/// A version with no checksum recorded yet passes with a warning: the next `migrate --apply`
/// records it.
pub async fn verify(c: &Client, set: &[Migration]) -> Result<Status, StoreError> {
    let s = status(c, set).await?;
    s.refuse_mismatch()?;
    let pending = s.pending();
    if !pending.is_empty() {
        return Err(StoreError(format!(
            "migrations.on_start is \"verify\" and migrations {pending:?} are pending: run \
             `aledgent-ingress migrate --apply` first. Refusing to start"
        )));
    }
    let backfillable = s.backfillable();
    if !backfillable.is_empty() {
        tracing::warn!(
            ?backfillable,
            "migrations applied before checksums were kept are not checked until \
             `aledgent-ingress migrate --apply` records their checksums"
        );
    }
    s.warn();
    Ok(s)
}

/// What `serve` does at start, as the configuration says ([`OnStart`]).
pub async fn on_start(c: &mut Client, config: &MigrationsConfig) -> Result<(), StoreError> {
    let lock_timeout = config.lock_timeout().map_err(StoreError)?;
    match config.on_start {
        OnStart::Apply => apply(c, MIGRATIONS, lock_timeout).await.map(|_| ()),
        OnStart::Verify => verify(c, MIGRATIONS).await.map(|_| ()),
    }
}
