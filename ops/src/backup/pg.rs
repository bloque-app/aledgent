// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! PostgreSQL backups of ingress and the adapter (`T-018-01`, `DEC-10`): a base backup and the
//! WAL archive, restorable to the latest archived point.
//!
//! **The server** archives WAL continuously: `wal_level = replica`, `archive_mode = on`, an
//! `archive_command` that copies each completed segment to the archive and never overwrites one,
//! and `archive_timeout` bounding how long a quiet server holds a segment open (the data-loss
//! window of a restore from the archive; ingress's synchronous standby, `F-009` decision 11, is
//! what keeps an acknowledged row when only the primary is lost).
//!
//! **The job** ([`PgBackupJob`]) runs PostgreSQL's own tools where the server's files are:
//!
//! 1. `pg_basebackup` in plain format, streaming the WAL it needs, with a backup manifest of
//!    SHA-256 checksums;
//! 2. `pg_verifybackup` against that manifest (every file's checksum, and the WAL parsed);
//! 3. a tarball of the verified backup, fetched and written with its own SHA-256 as an artifact;
//! 4. for the WAL archive: `pg_switch_wal()`, a wait until the archiver has archived that segment
//!    and has not failed since, then a tarball of the archive, fetched the same way.
//!
//! Commands run through [`Exec`]: [`LocalExec`] on the database host, or a test's container.
//! No password is ever on a command line or in an error (`REQ-AU-8`): the tools authenticate by
//! the host's own means (a local socket, a `.pgpass` file named by `PGPASSFILE`).
//!
//! **Restore** ([`restore_script`]): unpack the base backup into an empty data directory, the
//! archive beside it, `restore_command` pointing at it and `recovery.signal`; the server replays
//! every archived segment and promotes at the end of the archive, the latest point.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use super::{Manifest, StoreKind};
use crate::Error;

/// A boxed future.
/// Not `Send`: a container runtime's copy is not, and a backup job runs on one task.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// What a command printed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecOutput {
    /// Its exit code.
    pub code: i64,
    /// Its standard output.
    pub stdout: String,
    /// Its standard error.
    pub stderr: String,
}

/// Where the tools run.
pub trait Exec {
    /// Run `argv`.
    fn run<'a>(&'a self, argv: Vec<String>) -> BoxFuture<'a, Result<ExecOutput, String>>;
    /// Read the file at `path` there.
    fn fetch<'a>(&'a self, path: String) -> BoxFuture<'a, Result<Vec<u8>, String>>;
}

/// Commands on this host.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalExec;

impl Exec for LocalExec {
    fn run<'a>(&'a self, argv: Vec<String>) -> BoxFuture<'a, Result<ExecOutput, String>> {
        Box::pin(async move {
            let (cmd, args) = argv.split_first().ok_or("no command")?;
            let out = tokio::process::Command::new(cmd)
                .args(args)
                .output()
                .await
                .map_err(|e| format!("{cmd}: {e}"))?;
            Ok(ExecOutput {
                code: i64::from(out.status.code().unwrap_or(-1)),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            })
        })
    }

    fn fetch<'a>(&'a self, path: String) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async move {
            tokio::fs::read(&path)
                .await
                .map_err(|e| format!("{path}: {e}"))
        })
    }
}

/// A PostgreSQL store's backup job. Paths are on the host [`Exec`] runs on.
#[derive(Clone, Debug)]
pub struct PgBackupJob {
    /// Which store: ingress or the adapter.
    pub store: StoreKind,
    /// The role the tools connect as.
    pub user: String,
    /// The host or socket directory, if not the tools' default.
    pub host: Option<String>,
    /// The database the WAL checks connect to.
    pub database: String,
    /// A scratch directory for the backup before it is fetched. Emptied afterwards.
    pub work: String,
    /// The WAL archive the server's `archive_command` writes to.
    pub archive: String,
    /// How long to wait for the archiver.
    pub archive_wait: Duration,
}

/// The base backup's artifact.
pub const BASE: &str = "base.tar.gz";
/// The WAL archive's artifact.
pub const WAL: &str = "wal.tar.gz";

fn s(v: &str) -> String {
    v.to_owned()
}

impl PgBackupJob {
    fn conn(&self) -> Vec<String> {
        let mut v = vec![s("-U"), self.user.clone()];
        if let Some(h) = &self.host {
            v.push(s("-h"));
            v.push(h.clone());
        }
        v
    }

    async fn ok(&self, exec: &dyn Exec, argv: Vec<String>) -> Result<ExecOutput, Error> {
        let tool = argv.first().cloned().unwrap_or_default();
        let out = exec.run(argv).await.map_err(Error::Postgres)?;
        if out.code != 0 {
            return Err(Error::Postgres(format!(
                "{tool} exited {}: {}",
                out.code,
                out.stderr.trim()
            )));
        }
        Ok(out)
    }

    async fn sql(&self, exec: &dyn Exec, query: &str) -> Result<String, Error> {
        let mut argv = vec![s("psql")];
        argv.extend(self.conn());
        argv.extend([
            s("-d"),
            self.database.clone(),
            s("-v"),
            s("ON_ERROR_STOP=1"),
            s("-Atc"),
            s(query),
        ]);
        Ok(self.ok(exec, argv).await?.stdout.trim().to_owned())
    }

    /// Steps 1–3: a verified base backup, as the bytes of its tarball.
    pub async fn base_backup(&self, exec: &dyn Exec, id: &str) -> Result<Vec<u8>, Error> {
        let dir = format!("{}/base-{id}", self.work);
        let tar = format!("{dir}.tar.gz");
        let mut argv = vec![s("pg_basebackup")];
        argv.extend(self.conn());
        argv.extend([
            s("-D"),
            dir.clone(),
            s("-Fp"),
            s("-X"),
            s("stream"),
            s("--checkpoint=fast"),
            s("--manifest-checksums=SHA256"),
            s("-l"),
            format!("aledgent-{id}"),
        ]);
        self.ok(exec, argv).await?;
        self.ok(exec, vec![s("pg_verifybackup"), dir.clone()])
            .await?;
        self.ok(
            exec,
            vec![
                s("tar"),
                s("-czf"),
                tar.clone(),
                s("-C"),
                dir.clone(),
                s("."),
            ],
        )
        .await?;
        let bytes = exec.fetch(tar.clone()).await.map_err(Error::Postgres)?;
        self.ok(exec, vec![s("rm"), s("-rf"), dir, tar]).await?;
        Ok(bytes)
    }

    /// Step 4: close the current WAL segment, wait until the archiver has it, and return the
    /// archive's tarball. Fails if the archiver has failed since it last succeeded.
    pub async fn wal_archive(&self, exec: &dyn Exec, id: &str) -> Result<Vec<u8>, Error> {
        let segment = self
            .sql(exec, "SELECT pg_walfile_name(pg_switch_wal())")
            .await?;
        let deadline = tokio::time::Instant::now() + self.archive_wait;
        loop {
            let row = self
                .sql(
                    exec,
                    "SELECT coalesce(last_archived_wal, ''), \
                     coalesce(last_failed_time > last_archived_time, false) \
                     FROM pg_stat_archiver",
                )
                .await?;
            let (archived, failing) = row.split_once('|').unwrap_or((row.as_str(), "f"));
            if failing == "t" {
                return Err(Error::Postgres(
                    "the WAL archiver is failing: its last attempt failed".into(),
                ));
            }
            // WAL file names of one timeline sort as their positions.
            if !archived.is_empty() && archived >= segment.as_str() {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(Error::Postgres(format!(
                    "segment {segment} was not archived in time"
                )));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let tar = format!("{}/wal-{id}.tar.gz", self.work);
        self.ok(
            exec,
            vec![
                s("tar"),
                s("-czf"),
                tar.clone(),
                s("-C"),
                self.archive.clone(),
                s("."),
            ],
        )
        .await?;
        let bytes = exec.fetch(tar.clone()).await.map_err(Error::Postgres)?;
        self.ok(exec, vec![s("rm"), s("-f"), tar]).await?;
        Ok(bytes)
    }

    /// The whole job: a verified base backup and the archive up to now, written with their
    /// checksums into `root/<store>/<id>/`.
    pub async fn take(
        &self,
        exec: &dyn Exec,
        root: &std::path::Path,
        id: &str,
        taken_at: u64,
    ) -> Result<(std::path::PathBuf, Manifest), Error> {
        if !matches!(self.store, StoreKind::Ingress | StoreKind::Adapter) {
            return Err(Error::Config(
                "a PostgreSQL backup is ingress's or the adapter's".into(),
            ));
        }
        let base = self.base_backup(exec, id).await?;
        let wal = self.wal_archive(exec, id).await?;
        super::write(
            root,
            self.store,
            id,
            taken_at,
            &[(BASE, &base), (WAL, &wal)],
            None,
        )
    }
}

/// The server settings WAL archiving needs, as `postgres -c` arguments, archiving into `archive`.
pub fn archiving_settings(archive: &str, timeout_s: u32) -> Vec<String> {
    [
        "wal_level=replica".to_owned(),
        "archive_mode=on".to_owned(),
        format!("archive_command=test ! -f {archive}/%f && cp %p {archive}/%f"),
        format!("archive_timeout={timeout_s}"),
    ]
    .into_iter()
    .flat_map(|c| ["-c".to_owned(), c])
    .collect()
}

/// A shell script that restores a data directory from a base backup's tarball and a WAL
/// archive's tarball to the latest archived point, then starts the server with `start`. The
/// data directory must be empty. Run as the operating-system user that owns it, or as root where
/// the image's entrypoint drops privileges (the official image's does).
pub fn restore_script(
    pgdata: &str,
    base_tar: &str,
    wal_tar: &str,
    wal_dir: &str,
    start: &str,
) -> String {
    format!(
        "set -eu\n\
         test -z \"$(ls -A {pgdata} 2>/dev/null)\" || {{ echo 'the data directory is not empty' >&2; exit 1; }}\n\
         mkdir -p {pgdata} {wal_dir}\n\
         tar -xzf {base_tar} -C {pgdata}\n\
         tar -xzf {wal_tar} -C {wal_dir}\n\
         rm -f {pgdata}/postmaster.pid\n\
         echo \"restore_command = 'cp {wal_dir}/%f %p'\" >> {pgdata}/postgresql.auto.conf\n\
         touch {pgdata}/recovery.signal\n\
         chown -R postgres:postgres {pgdata} {wal_dir} 2>/dev/null || true\n\
         chmod 700 {pgdata}\n\
         exec {start}\n"
    )
}
