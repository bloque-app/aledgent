// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The restore drill with PostgreSQL (`T-018-01`, `T-018-02`, `T-018-03`, `NFR-10`, `AC-K5.1`), in
//! containers (testcontainers; Docker on the CI runner).
//!
//! The source deployment's received-log and adapter store live in one PostgreSQL cluster with WAL
//! archiving on. A base backup is taken (verified with `pg_verifybackup`) beside the core's backup;
//! the book moves on; a command is acknowledged at `Received` while the core is down; the WAL
//! archive is taken at the failure, and the cluster is destroyed. On an isolated host — a new
//! container with nothing but the two tarballs, and a directory of its own — the cluster is
//! restored from the base backup and the archive to the latest point, and the drill runs over it:
//! the core from its export, the ordered replay, the leaf store rebuilt, the adapter re-scanning
//! the fake layer. The pre-failure composite root and book root must come back.
//!
//! Without Docker these tests say so and pass, except under CI (`CI` set), where a missing Docker
//! fails them. Generated data is a few megabytes, deleted with the test's directory.

use std::time::Duration;

use ingress::store::pg::{PgOptions, PgStore as IngressPg};
use ops::backup::pg::{
    BASE, BoxFuture, Exec, ExecOutput, PgBackupJob, WAL, archiving_settings, restore_script,
};
use ops::backup::{self, StoreKind};
use ops::drill::{Expected, Verdict};
use ops::kit::{DrillPlan, OpsKit, run_drill};
use serde_json::json;
use settle::store::pg::PgStore as AdapterPg;
use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::core::{
    CmdWaitFor, ExecCommand, IntoContainerPort, WaitFor,
};
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, GenericImage, ImageExt};

const TAG: &str = "16-alpine";
const PGDATA: &str = "/var/lib/postgresql/data";
/// Inside the image's own directory, which belongs to the server's user.
const ARCHIVE: &str = "/var/lib/postgresql/wal-archive";
const WAL_WINDOW_S: u32 = 1;

/// Commands in a container.
struct DockerExec<'a, I: testcontainers_modules::testcontainers::Image>(&'a ContainerAsync<I>);

impl<I: testcontainers_modules::testcontainers::Image> Exec for DockerExec<'_, I> {
    fn run<'a>(&'a self, argv: Vec<String>) -> BoxFuture<'a, Result<ExecOutput, String>> {
        Box::pin(async move {
            let mut r = self
                .0
                .exec(ExecCommand::new(argv).with_cmd_ready_condition(CmdWaitFor::exit()))
                .await
                .map_err(|e| e.to_string())?;
            let stdout = r.stdout_to_vec().await.map_err(|e| e.to_string())?;
            let stderr = r.stderr_to_vec().await.map_err(|e| e.to_string())?;
            let code = r
                .exit_code()
                .await
                .map_err(|e| e.to_string())?
                .unwrap_or(-1);
            Ok(ExecOutput {
                code,
                stdout: String::from_utf8_lossy(&stdout).into_owned(),
                stderr: String::from_utf8_lossy(&stderr).into_owned(),
            })
        })
    }

    fn fetch<'a>(&'a self, path: String) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async move {
            self.0
                .copy_file_from(path, Vec::<u8>::new())
                .await
                .map_err(|e| e.to_string())
        })
    }
}

fn docker_missing(e: impl std::fmt::Display) -> bool {
    assert!(
        std::env::var_os("CI").is_none(),
        "Docker is required under CI: {e}"
    );
    eprintln!("skipped: no Docker ({e})");
    true
}

fn job() -> PgBackupJob {
    PgBackupJob {
        store: StoreKind::Ingress,
        user: "postgres".into(),
        host: Some("/var/run/postgresql".into()),
        database: "postgres".into(),
        work: "/tmp/ops".into(),
        archive: ARCHIVE.into(),
        archive_wait: ops::kit::PATIENCE,
    }
}

async fn url(
    c: &ContainerAsync<impl testcontainers_modules::testcontainers::Image>,
    db: &str,
) -> String {
    let port = c.get_host_port_ipv4(5432).await.expect("a port");
    format!("postgres://postgres:postgres@127.0.0.1:{port}/{db}")
}

async fn stores(base: &str) -> (IngressPg, AdapterPg) {
    let ingress = IngressPg::connect(PgOptions {
        url: format!("{base}ingress"),
        sealing_key: [7; 32],
        require_sync_standby: ingress::config::SyncStandby::NotRequired,
        profile: ingress::config::Profile::Development,
        pool_size: 8,
    })
    .await
    .expect("ingress's store connects");
    let adapter = AdapterPg::connect(
        &format!("{base}settle"),
        8,
        ingress::config::Profile::Development,
    )
    .await
    .expect("the adapter's store connects");
    (ingress, adapter)
}

/// `NFR-10`, `AC-K5.1`, `RISK-5`: the drill over PostgreSQL restored from a base backup and its
/// WAL archive to the latest point (module documentation).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nfr_10_the_drill_restores_postgres_from_base_backup_and_wal_to_the_pre_failure_root() {
    let tmp = tempfile::tempdir().unwrap();
    let mut archiving = vec!["-c".to_owned(), "fsync=off".to_owned()];
    archiving.extend(
        archiving_settings(ARCHIVE, WAL_WINDOW_S)
            .into_iter()
            .map(|a| {
                a.replace(
                    "archive_command=",
                    &format!("archive_command=mkdir -p {ARCHIVE} && "),
                )
            }),
    );
    let pg = match Postgres::default()
        .with_tag(TAG)
        .with_cmd(archiving)
        .start()
        .await
    {
        Ok(c) => c,
        Err(e) if docker_missing(&e) => return,
        Err(_) => unreachable!(),
    };
    let exec = DockerExec(&pg);
    for db in ["ingress", "settle"] {
        let out = exec
            .run(
                ["psql", "-U", "postgres", "-h", "/var/run/postgresql", "-c"]
                    .into_iter()
                    .map(str::to_owned)
                    .chain([format!("CREATE DATABASE {db}")])
                    .collect(),
            )
            .await
            .unwrap();
        assert_eq!(out.code, 0, "{}", out.stderr);
    }
    exec.run(vec!["mkdir".into(), "-p".into(), "/tmp/ops".into()])
        .await
        .unwrap();
    let base = url(&pg, "").await;
    let (ingress, adapter) = stores(&base).await;
    let mut k = OpsKit::start(
        ingress,
        adapter,
        &tmp.path().join("audit"),
        &tmp.path().join("book"),
    )
    .await;
    for _ in 0..2 {
        k.round().await;
        k.anchor_round(12).await;
    }
    // The backups: the core, the audit trail, the cluster's base backup.
    let root = tmp.path().join("backups");
    let (core_backup, m) = k.backup_core(&root, "c1").await.unwrap();
    let (audit_backup, _, _) = backup::audit::take(&k.audit_dir, &root, "a1", 1).unwrap();
    let base_tar = job().base_backup(&exec, "p1").await.unwrap();
    for _ in 0..3 {
        k.round().await;
        k.anchor_round(12).await;
    }
    k.quiet().await;
    let r = k.roots().unwrap();
    let want = Expected {
        kernel_root: r.kernel,
        book_root: r.book,
        inputs: r.inputs,
    };
    assert!(want.inputs > m.point.unwrap().inputs);
    // The failure: the core dies; a transfer is acknowledged at Received while it is down.
    k.fail();
    let (a, b) = (k.parties[0].clone(), k.parties[1].clone());
    let (s, v) = k
        .d
        .call(
            "POST",
            "/v1/transfers",
            Some(&a.token),
            &[],
            Some(json!({ "legs": [{ "from": a.account, "to": b.account, "asset": "USD", "amount": "3" }] })),
        )
        .await;
    assert_eq!(s.as_u16(), 202, "{v}");
    // The archive as it stands at the failure, then the cluster is gone.
    let wal_tar = job().wal_archive(&exec, "p1").await.unwrap();
    let (pg_backup, _) = backup::write(
        &root,
        StoreKind::Ingress,
        "p1",
        2,
        &[(BASE, &base_tar), (WAL, &wal_tar)],
        None,
    )
    .unwrap();
    let outside = k.outside();
    let OpsKit { d, .. } = k;
    drop(d);
    drop(pg);
    // The isolated host: a new container with the two tarballs, restored to the latest point.
    let pm = backup::verify(&pg_backup).unwrap();
    let script = restore_script(
        PGDATA,
        "/restore/base.tar.gz",
        "/restore/wal.tar.gz",
        "/var/lib/postgresql/wal-restore",
        "docker-entrypoint.sh postgres -c fsync=off",
    );
    let restored = GenericImage::new("postgres", TAG)
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_PASSWORD", "postgres")
        .with_copy_to(
            "/restore/base.tar.gz",
            backup::read_artifact(&pg_backup, &pm, BASE).unwrap(),
        )
        .with_copy_to(
            "/restore/wal.tar.gz",
            backup::read_artifact(&pg_backup, &pm, WAL).unwrap(),
        )
        .with_cmd(["sh", "-c", script.as_str()])
        .with_startup_timeout(Duration::from_secs(120))
        .start()
        .await
        .expect("the restored cluster starts");
    let base = url(&restored, "").await;
    let (ingress, adapter) = stores(&base).await;
    let host = tmp.path().join("isolated");
    let drilled = run_drill(
        &DrillPlan {
            id: "pg",
            host: "container: postgres restored from base backup and WAL",
            workdir: &host,
            core_backup: &core_backup,
            audit_backup: &audit_backup,
            expected: Some(want),
            wal_window_s: Some(WAL_WINDOW_S),
        },
        &outside,
        ingress,
        adapter,
    )
    .await
    .unwrap();
    let report = &drilled.report;
    for c in &report.checks {
        assert_eq!(c.verdict, Verdict::Passed, "{}: {}", c.name, c.detail);
    }
    assert!(report.passed());
    assert_eq!(report.data_loss.inputs_lost, 0);
    assert!(report.replay.replayed > 0 && report.replay.undecided > 0);
    let path = report.record(&tmp.path().join("results")).unwrap();
    // The record, for NOTES.md's measurements.
    println!("{}", std::fs::read_to_string(path).unwrap());
    let artifacts: u64 = pm.artifacts.iter().map(|a| a.bytes).sum();
    println!("postgres backup: {artifacts} bytes");
}
