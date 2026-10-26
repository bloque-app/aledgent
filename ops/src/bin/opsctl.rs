// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `opsctl`: aledgent's operations tool (`F-018`).
//!
//! ```text
//! opsctl backup verify <dir>                      verify a backup's checksums
//! opsctl backup due --root <dir>                  which stores' backups are due (ops/backup/schedule.toml)
//! opsctl backup prune --root <dir>                remove verified backups past the schedule's retention
//! opsctl backup pg --store ingress|adapter --root <dir> --id <id> --user <role> --database <db>
//!                  --work <dir> --archive <dir> [--host <socket dir>]
//!                                                 a verified base backup and the WAL archive
//! opsctl backup audit --store-dir <dir> --root <dir> --id <id>
//!                                                 a verified copy of the audit store
//! opsctl restore audit <backup> <target>          restore the audit store into an empty directory
//! opsctl restore pg-script --pgdata <dir> --base <tar> --wal <tar> --wal-dir <dir> --start <cmd>
//!                                                 print the PostgreSQL restore script
//! opsctl restore core --config <ingress config> --backup <dir> --journal-out <file>
//!                     --audit-dir <dir> --book-dir <dir> --leaf-dir <dir>
//!                     --principal <did> --credential <did> [--report <dir>]
//!                     [--anchors-rpc <node URL>]
//!                                                 restore the in-process core from its backup and
//!                                                 the received-log; write its rebuilt journal
//! opsctl restore core --from <archive> --config <deployment config> --state <empty dir>
//!                     (--anchors-rpc <node URL> | --no-anchors) [--from-genesis] [--report <dir>]
//!                     [--accept-damaged-archive] [--fetch-concurrency <n>]
//!                     [--chainspec <file>] [--genesis-manifest <file>] [--unnamed-code]
//!                     [--core-host <aledgent-core-host> --runtime <pvm> --actor <vos> [--budget <bytes>]]
//!                                                 restore the Agent core from its off-node archive
//!                                                 (T-005-16): check it, re-derive the book and run
//!                                                 the drill's checks, lay it out on the fresh node,
//!                                                 and with --core-host bring the Agent to its point
//! opsctl restore core-tail --config <deployment config> (--after <input> | --state <dir>)
//!                                                 re-send through the restored core every input the
//!                                                 received-log shows decided after the archive's point
//! opsctl archive verify --from <archive> --config <deployment config> (--anchors-rpc <URL> | --no-anchors)
//!                       [--fetch-concurrency <n>] [--chainspec <file>]
//!                                                 check the archive and re-derive its book, laying
//!                                                 nothing out
//! opsctl archive status --state <dir>             the core host's archiver status
//! opsctl exporter --config <exporter.toml>        serve /metrics
//! opsctl health --config <exporter.toml>          exit 0 when the exporter at the configuration's
//!                                                 `listen` answers GET /metrics with 200: the
//!                                                 image's HEALTHCHECK
//! ```
//!
//! `restore core --from` and `archive verify` read the archive's segments `--fetch-concurrency` at
//! once (default 32, at most 256; `core_archive::fetch_with`), and check them strictly in order, as
//! one at a time would. A chainspec book (`F-036`) re-derives from its chainspec: `--chainspec
//! <file>`, or else the archive's copy (`<prefix>/chainspec/<digest>.bin`), fetched by the digest
//! the deployment's genesis commits and refused unless it is that chainspec. With `--core-host`,
//! the core host's `restore` gets `--archive <the same spec>` for such a book (it lays out the
//! chainspec and its kept verify-genesis report from there), and `--chainspec`,
//! `--genesis-manifest` and `--unnamed-code` as given here.
//!
//! `--core-host` runs the core host on this machine: both binaries, the Agent's programs and the
//! `--state` directory on one filesystem. The core image (`deploy/docker/core.Dockerfile`) carries
//! `opsctl` beside `aledgent-core-host` for this:
//!
//! ```text
//! podman run --rm --entrypoint /usr/local/bin/opsctl -v <state>:/var/lib/aledgent/core … <core image> \
//!   restore core --from … --state /var/lib/aledgent/core … --core-host aledgent-core-host \
//!   --runtime /usr/share/aledgent/agent_runtime.pvm --actor /usr/share/aledgent/core.vos
//! ```
//!
//! An archive is `fs:<dir>` or `s3://<bucket>/<prefix>?region=…` (`core_archive::open`); its
//! credentials come from the environment or the instance role, never from an argument.
//!
//! Secrets come from the environment only, never from an argument, and none has a default
//! (`REQ-AU-8`): the sealing key of backups (`ALEDGENT_OPS_SEALING_KEY`), the core's key-ring root
//! (`ALEDGENT_CORE_ROOT`), the received-log's database URL and sealing key (ingress's variables),
//! and the audit store's writer credential (`ALEDGENT_AUDIT_WRITER_CREDENTIAL`). Nothing here logs
//! any of them.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use ingress::clock::{Clock as _, SystemClock};
use ops::Error;
use ops::backup::{self, StoreKind};

fn arg(args: &[String], name: &str) -> Result<String, Error> {
    args.windows(2)
        .find(|w| w[0] == name)
        .map(|w| w[1].clone())
        .ok_or_else(|| Error::Config(format!("missing {name}")))
}

fn opt(args: &[String], name: &str) -> Option<String> {
    arg(args, name).ok()
}

fn path(args: &[String], name: &str) -> Result<PathBuf, Error> {
    arg(args, name).map(PathBuf::from)
}

fn now() -> u64 {
    SystemClock.now()
}

fn backup_due(args: &[String]) -> Result<(), Error> {
    let root = path(args, "--root")?;
    let schedule = backup::schedule::Schedule::committed()?;
    let mut last = std::collections::BTreeMap::new();
    for s in StoreKind::ALL {
        if let Some((_, m)) = backup::latest(&root, s)? {
            last.insert(s, m.taken_at);
        }
    }
    for s in schedule.due(&last, now()) {
        println!("{}", s.as_str());
    }
    Ok(())
}

async fn backup_pg(args: &[String]) -> Result<(), Error> {
    let store = StoreKind::parse(&arg(args, "--store")?)
        .ok_or_else(|| Error::Config("--store is ingress or adapter".into()))?;
    let job = backup::pg::PgBackupJob {
        store,
        user: arg(args, "--user")?,
        host: opt(args, "--host"),
        database: arg(args, "--database")?,
        work: arg(args, "--work")?,
        archive: arg(args, "--archive")?,
        archive_wait: Duration::from_secs(300),
    };
    let (dir, m) = job
        .take(
            &backup::pg::LocalExec,
            &path(args, "--root")?,
            &arg(args, "--id")?,
            now(),
        )
        .await?;
    println!(
        "{}: {} artifacts, verified",
        dir.display(),
        m.artifacts.len()
    );
    Ok(())
}

fn restore_pg_script(args: &[String]) -> Result<(), Error> {
    print!(
        "{}",
        backup::pg::restore_script(
            &arg(args, "--pgdata")?,
            &arg(args, "--base")?,
            &arg(args, "--wal")?,
            &arg(args, "--wal-dir")?,
            &arg(args, "--start")?,
        )
    );
    Ok(())
}

#[cfg(feature = "postgres")]
async fn restore_core(args: &[String]) -> Result<(), Error> {
    use audit_store::{AuditStoreConfig, FileAuditStore, WriterCredential};
    use ingress::config::{DeploymentConfig, ENV_CORE_ROOT, ENV_DATABASE_URL, ENV_SEALING_KEY};
    use ingress::inprocess::FileJournal;
    use ingress::store::Store as _;
    use ingress::store::pg::{PgOptions, PgStore};
    use ledger_core::id::{CredentialId, IdentityId, OperationId};
    use ledger_kernel::keys::KeyRing;
    use ledger_kernel::store::MemStore;
    use ledger_kernel::vault::MemVault;

    let started = std::time::Instant::now();
    let config_path = arg(args, "--config")?;
    let text = std::fs::read_to_string(&config_path)?;
    let config: DeploymentConfig =
        serde_json::from_str(&text).map_err(|e| Error::Config(format!("{config_path}: {e}")))?;
    let root = ingress::config::secret_hex32(ENV_CORE_ROOT).map_err(Error::Config)?;
    let genesis = config.genesis.core_genesis(root).map_err(Error::Config)?;
    let sealing = ops::seal::SealingKey::from_env(ops::seal::ENV_SEALING_KEY)?;
    let backup_dir = path(args, "--backup")?;
    let bundle = backup::core::read(&backup_dir, &sealing)?;
    let url = std::env::var(ENV_DATABASE_URL)
        .map_err(|_| Error::Config(format!("{ENV_DATABASE_URL} is not set")))?;
    let log = PgStore::connect(PgOptions {
        url,
        sealing_key: ingress::config::secret_hex32(ENV_SEALING_KEY).map_err(Error::Config)?,
        // A restore reads the log; it never replies to a command, so no standby is needed.
        require_sync_standby: ingress::config::SyncStandby::NotRequired,
        profile: config.profile,
        pool_size: 4,
    })
    .await
    .map_err(|e| Error::Postgres(e.to_string()))?;
    let rows = log
        .rows()
        .await
        .map_err(|e| Error::Postgres(e.to_string()))?;
    let writer = std::env::var("ALEDGENT_AUDIT_WRITER_CREDENTIAL")
        .map_err(|_| Error::Config("ALEDGENT_AUDIT_WRITER_CREDENTIAL is not set".into()))?;
    let mut audit = FileAuditStore::open(
        &AuditStoreConfig {
            dir: path(args, "--audit-dir")?,
            book_dir: path(args, "--book-dir")?,
            segment_records: 4096,
        },
        &WriterCredential::new(writer.into_bytes()),
    )
    .map_err(|e| Error::Audit(format!("{e:?}")))?;
    let journal_out = path(args, "--journal-out")?;
    if journal_out.exists() {
        return Err(Error::Config(format!(
            "{} exists: the rebuilt journal is never written over another",
            journal_out.display()
        )));
    }
    let journal = FileJournal::open(&journal_out).map_err(Error::Io)?;
    let mut op = [0u8; 16];
    getrandom::fill(&mut op).map_err(|e| Error::Config(e.to_string()))?;
    let by = ops::restore::ImportBy {
        ctx: ledger_core::ledger::Ctx {
            caller: ledger_core::principal::Caller {
                principal: IdentityId::parse(&arg(args, "--principal")?)
                    .map_err(|_| Error::Config("--principal".into()))?,
                credential: CredentialId::parse(&arg(args, "--credential")?)
                    .map_err(|_| Error::Config("--credential".into()))?,
                on_behalf_of: None,
            },
            op: OperationId::from_ref(ledger_kernel::posting::OperationRef(op)),
            now: now(),
        },
        justification: b"opsctl restore core",
    };
    let export_bytes = bundle
        .manifest
        .artifact(backup::core::EXPORT)
        .map_or(0, |a| a.bytes);
    let backup_id = bundle.manifest.id.clone();
    let carried = bundle.pass_through.clone();
    // F-017: the clerk key, read as the ingress binary reads it; never logged.
    let clerk: Option<ingress::inprocess::SharedClerk> =
        match std::env::var_os(ingress::config::ENV_CLERK_KEY) {
            None => None,
            Some(_) => Some(std::sync::Arc::new(
                ledger_core::anchor::clerk::InProcessClerk::from_secret(
                    ingress::config::secret_hex32(ingress::config::ENV_CLERK_KEY)
                        .map_err(Error::Config)?,
                )
                .ok_or_else(|| Error::Config("the clerk key is not a canonical scalar".into()))?,
            )),
        };
    let (core, replay) = ops::restore::restore_core(
        bundle,
        genesis.clone(),
        MemStore::new(),
        MemVault::new(),
        move || {
            ledger_kernel::book::Book::bootstrap(
                MemStore::new(),
                MemVault::new(),
                KeyRing::from_root(root),
            )
        },
        journal,
        clerk,
        &rows,
        &by,
        &mut audit,
    )
    .await?;
    // REQ-PO-5 (ruling pq8-export (a)): the pass-through data the export carried, back on the log.
    let restored = ops::restore::restore_pass_through(&log, &carried).await?;
    println!(
        "pass-through data: {} carried, {} written back to the received-log",
        restored.carried, restored.written
    );
    let recovery_time_ms = started.elapsed().as_millis() as u64;
    let (acks, _) = ops::drill::acknowledgements(&rows)?;
    // INV-19 reads the settlement layer's anchors at its finalized head, from a node.
    let anchors = opt(args, "--anchors-rpc").map(|url| verify::rpc::RpcAnchors::new(&url));
    let leaf_dir = path(args, "--leaf-dir")?;
    let checked = core
        .with_ledger(|l| -> Result<_, Error> {
            let rebuild = ops::restore::rebuild_leaf_store(l.book().store(), Some(&leaf_dir))?;
            let checks = ops::drill::checks(
                l,
                &acks,
                None,
                &rebuild,
                anchors
                    .as_ref()
                    .map(|a| (a as &dyn verify::AnchorSource, replay.import_commits)),
            );
            Ok((
                rebuild,
                checks,
                ops::drill::size_of(l, rows.len(), export_bytes),
            ))
        })
        .ok_or_else(|| Error::Core("the restored core is not running".into()))??;
    let (rebuild, checks, size) = checked;
    let report = ops::drill::DrillReport {
        id: format!("restore-{}", now()),
        host: opt(args, "--host-name").unwrap_or_else(|| "opsctl".into()),
        backup: backup_id,
        started_at: now(),
        recovery_time_ms,
        data_loss: ops::drill::DataLoss {
            inputs_before: None,
            inputs_restored: replay.last_input,
            inputs_lost: 0,
            wal_window_s: None,
        },
        replay,
        rebuild,
        rescan: None,
        size,
        checks,
    };
    if let Some(dir) = opt(args, "--report") {
        let p = report.record(Path::new(&dir))?;
        println!("recorded {}", p.display());
    }
    for c in &report.checks {
        println!("{}: {:?} ({})", c.name, c.verdict, c.detail);
    }
    println!(
        "restored to input {} ({} replayed after the backup point {}); journal {}",
        report.replay.last_input,
        report.replay.replayed,
        report.replay.backup_point,
        journal_out.display()
    );
    if report.passed() {
        Ok(())
    } else if report.undecided() {
        Err(Error::Restore(UNDECIDED.into()))
    } else {
        Err(Error::Restore("a check failed: see above".into()))
    }
}

/// What a run that found nothing wrong but could not decide a check says (`Verdict::Unknown`):
/// not a pass, and not a fault of the book.
const UNDECIDED: &str = "no check failed, but an anchor could not be read (a check above is \
    Unknown, needs an archive node): not a pass. Run it again with --anchors-rpc naming a node \
    that keeps every block's state";

/// The most segments `--fetch-concurrency` reads at once.
const MAX_FETCH_CONCURRENCY: usize = 256;

/// `w` as one word of a POSIX shell command, for a command printed to be run by hand (an archive
/// spec carries `?` and `&`).
fn shell_word(w: &str) -> String {
    let plain = !w.is_empty()
        && w.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-./:=,+@%".contains(&b));
    if plain {
        w.to_owned()
    } else {
        format!("'{}'", w.replace('\'', "'\\''"))
    }
}

fn flag(args: &[String], name: &str) -> bool {
    args.iter().any(|a| a == name)
}

/// What the book is founded on, from the deployment's configuration. Its keys are the signer
/// service's when the configuration names `core_keys` (`F-035`: production, where no key is in any
/// process but the signer's); otherwise the key-ring root and the clerk key come from the
/// environment (development only). Secrets are never logged.
///
/// A chainspec book (`F-036`) re-derives from its chainspec: `--chainspec <file>`, or else the
/// archive's own copy (`<prefix>/chainspec/<digest>.bin`, kept there by the core host), fetched by
/// the digest this genesis commits. Either must be the chainspec the genesis commits.
fn founding(
    args: &[String],
    archive: Option<&core_archive::Archive>,
) -> Result<core_binding::core::Founding, Error> {
    use ingress::config::{DeploymentConfig, ENV_CLERK_KEY, ENV_CORE_ROOT};
    let config_path = arg(args, "--config")?;
    let text = std::fs::read_to_string(&config_path)?;
    let config: DeploymentConfig =
        serde_json::from_str(&text).map_err(|e| Error::Config(format!("{config_path}: {e}")))?;
    let (mut genesis, op, at) = config.genesis.agent_genesis().map_err(Error::Config)?;
    if let Some(c) = genesis.chainspec {
        let digest = hex::encode(c.digest);
        let (from, bytes) = match (opt(args, "--chainspec"), archive) {
            (Some(path), _) => {
                let bytes = std::fs::read(&path)?;
                (path, bytes)
            }
            (None, Some(a)) => {
                let bytes = a
                    .get_chainspec(&digest)
                    .map_err(|e| Error::Restore(format!("the archive's chainspec: {e}")))?
                    .ok_or_else(|| {
                        Error::Config(format!(
                            "this genesis commits chainspec {digest}, and the archive keeps none: \
                             --chainspec <file> is required"
                        ))
                    })?;
                (
                    format!(
                        "{}: {}",
                        a.describe(),
                        core_archive::archive::chainspec_name(&digest)
                    ),
                    bytes,
                )
            }
            (None, None) => {
                return Err(Error::Config(format!(
                    "this genesis commits chainspec {digest}: --chainspec <file> is required"
                )));
            }
        };
        let spec = ledger_core::chainspec::ChainSpec::from_bytes(&bytes)
            .map_err(|e| Error::Config(format!("{from}: {e:?}")))?;
        if spec.commitment() != c {
            return Err(Error::Config(format!(
                "{from}: not the chainspec this genesis commits"
            )));
        }
        genesis.spec = Some(std::sync::Arc::new(spec));
    }
    let keys = match config
        .core_key_handles()
        .map_err(|e| Error::Config(e.to_string()))?
    {
        Some(h) => {
            use signer::client::{Patience, RemoteClerk, RemoteCustody};
            let custody = RemoteCustody::from_key(h.key_ring, Patience::PRODUCTION)
                .map_err(|e| Error::Config(format!("core_keys.key_ring: {e}")))?;
            let clerk = match h.clerk {
                Some(c) => Some(std::sync::Arc::new(
                    RemoteClerk::from_key(c, Patience::PRODUCTION)
                        .map_err(|e| Error::Config(format!("core_keys.clerk: {e}")))?,
                ) as core_binding::host::SharedClerk),
                None => None,
            };
            core_binding::host::HostKeys::new(std::sync::Arc::new(custody), clerk)
        }
        None => {
            let root = ingress::config::secret_hex32(ENV_CORE_ROOT).map_err(Error::Config)?;
            let clerk = match std::env::var_os(ENV_CLERK_KEY) {
                Some(_) => {
                    Some(ingress::config::secret_hex32(ENV_CLERK_KEY).map_err(Error::Config)?)
                }
                None => None,
            };
            core_binding::host::HostKeys::local(root, clerk)
        }
    };
    Ok(core_binding::core::Founding {
        genesis,
        op,
        at,
        keys,
        limits: core_binding::hot::Limits::DEFAULT,
        genesis_roots: None,
    })
}

/// The settlement layer's anchors, from a node (`--anchors-rpc`), or none for an isolated drill
/// (`--no-anchors`: `INV-19` is then `Deferred`, never passed). One of the two must be named.
fn anchors(args: &[String]) -> Result<Option<verify::rpc::RpcAnchors>, Error> {
    match (opt(args, "--anchors-rpc"), flag(args, "--no-anchors")) {
        (Some(url), false) => Ok(Some(verify::rpc::RpcAnchors::new(&url))),
        (None, true) => Ok(None),
        _ => Err(Error::Config(
            "name --anchors-rpc <node URL> (the archive's roots are checked against the settlement layer's anchors), or --no-anchors for an isolated drill".into(),
        )),
    }
}

/// Read and check the archive `--from`, and re-derive its book (`ops::agent`). The last item:
/// whether its genesis commits a chainspec (`F-036`).
fn archive_verified(
    args: &[String],
) -> Result<
    (
        core_archive::Archive,
        core_archive::Fetched,
        ops::agent::Verified,
        bool,
    ),
    Error,
> {
    let spec = arg(args, "--from")?;
    let archive = core_archive::open(&spec, core_archive::Access::Read).map_err(Error::Config)?;
    let founding = founding(args, Some(&archive))?;
    let concurrency = match opt(args, "--fetch-concurrency") {
        Some(n) => n
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=MAX_FETCH_CONCURRENCY).contains(n))
            .ok_or_else(|| {
                Error::Config(format!(
                    "--fetch-concurrency: segments read at once, 1 to {MAX_FETCH_CONCURRENCY}"
                ))
            })?,
        None => core_archive::DEFAULT_CONCURRENCY,
    };
    let book = core_archive::book_of(&founding.op);
    let read = std::time::Instant::now();
    let fetched =
        core_archive::fetch_with(&archive, Some(&book), concurrency).map_err(Error::Restore)?;
    println!(
        "read and checked {} segments in {} ms, {concurrency} at once",
        fetched.segments,
        read.elapsed().as_millis()
    );
    for n in &fetched.notes {
        println!("note: {n}");
    }
    if fetched.damaged && !flag(args, "--accept-damaged-archive") {
        return Err(Error::Restore(
            "the archive is damaged (above): what the damaged segments held is not recoverable from it. \
             Refused; --accept-damaged-archive restores to the point before the damage, and the \
             received-log's tail replay must then cover the rest"
                .into(),
        ));
    }
    let layer = anchors(args)?;
    let chainspec_book = founding.genesis.chainspec.is_some();
    let verified = ops::agent::verify(
        &fetched,
        &founding,
        layer.as_ref().map(|a| a as &dyn verify::AnchorSource),
    )?;
    for c in &verified.checks {
        println!("{}: {:?} ({})", c.name, c.verdict, c.detail);
    }
    let p = &fetched.point;
    println!(
        "{}: {} segments, point at input {} (Agent input {}), checkpoint {}",
        archive.describe(),
        fetched.segments,
        p.inputs,
        p.entries,
        fetched
            .checkpoint
            .as_ref()
            .map_or("none".to_owned(), |c| format!(
                "at Agent input {}",
                c.header.point.entries
            ))
    );
    Ok((archive, fetched, verified, chainspec_book))
}

/// `restore core --from` (module documentation; `ops::agent`).
fn restore_core_from(args: &[String]) -> Result<(), Error> {
    let started = std::time::Instant::now();
    let started_at = now();
    let state = path(args, "--state")?;
    let (archive, fetched, verified, chainspec_book) = archive_verified(args)?;
    let mut report = ops::drill::DrillReport {
        id: format!("restore-agent-{started_at}"),
        host: opt(args, "--host-name").unwrap_or_else(|| "opsctl".into()),
        backup: format!("{} segment {}", archive.describe(), fetched.segments),
        started_at,
        recovery_time_ms: 0,
        data_loss: ops::drill::DataLoss {
            inputs_before: None,
            inputs_restored: fetched.point.inputs,
            inputs_lost: 0,
            wal_window_s: None,
        },
        replay: ops::restore::ReplayReport {
            backup_point: fetched.point.inputs,
            last_input: fetched.point.inputs,
            ..ops::restore::ReplayReport::default()
        },
        rebuild: verified.rebuild.clone(),
        rescan: None,
        size: ops::drill::Size {
            postings: verified.postings,
            kernel_accounts: 0,
            rows: fetched.replies_parsed.len() as u64,
            export_bytes: fetched.archive.len() as u64,
        },
        checks: verified.checks.clone(),
    };
    let record = |r: &ops::drill::DrillReport| -> Result<(), Error> {
        if let Some(dir) = opt(args, "--report") {
            println!("recorded {}", r.record(Path::new(&dir))?.display());
        }
        Ok(())
    };
    if !verified.passed() {
        record(&report)?;
        return Err(Error::Restore(if verified.undecided() {
            format!("refused: {UNDECIDED}; nothing was laid out")
        } else {
            "refused: a check failed (above); nothing was laid out".into()
        }));
    }
    let plan = core_archive::lay_out(
        &state,
        &fetched,
        &archive.describe(),
        "agent.state",
        !flag(args, "--from-genesis"),
        now(),
    )
    .map_err(Error::Restore)?;
    println!(
        "laid out in {}: {} from {}",
        state.display(),
        match plan.checkpoint {
            Some((_, e)) => format!("the Agent starts at its checkpoint (Agent input {e})"),
            None => "the Agent is founded afresh".to_owned(),
        },
        plan.source
    );
    let host_args = |bin: &str| -> Result<Vec<String>, Error> {
        let mut v = vec![
            bin.to_owned(),
            "restore".into(),
            "--config".into(),
            arg(args, "--config")?,
            "--runtime".into(),
            arg(args, "--runtime")?,
            "--actor".into(),
            arg(args, "--actor")?,
            "--state".into(),
            state.display().to_string(),
        ];
        if let Some(b) = opt(args, "--budget") {
            v.extend(["--budget".into(), b]);
        }
        // F-036: a chainspec book's core host needs its chainspec too, and the verify-genesis
        // report kept beside it, or the restored book takes no command. It reads both from the
        // archive (`aledgent-core-host restore --archive`), by the digest genesis commits, unless
        // a chainspec was given here; and it takes the genesis flags its own serve took.
        if chainspec_book {
            v.extend(["--archive".into(), arg(args, "--from")?]);
        }
        for name in ["--chainspec", "--genesis-manifest"] {
            if let Some(x) = opt(args, name) {
                v.extend([name.to_owned(), x]);
            }
        }
        if flag(args, "--unnamed-code") {
            v.push("--unnamed-code".into());
        }
        Ok(v)
    };
    match opt(args, "--core-host") {
        Some(bin) => {
            let cmd = host_args(&bin)?;
            let out = std::process::Command::new(&cmd[0])
                .args(&cmd[1..])
                .stderr(std::process::Stdio::inherit())
                .output()?;
            let stdout = String::from_utf8_lossy(&out.stdout);
            print!("{stdout}");
            if !out.status.success() {
                report.checks.push(ops::drill::Check {
                    name: "roll-forward".into(),
                    verdict: ops::drill::Verdict::Failed,
                    detail: format!("aledgent-core-host restore exited {}", out.status),
                });
                record(&report)?;
                return Err(Error::Restore(
                    "the Agent was not brought to the archive's point: do not serve".into(),
                ));
            }
            let done: serde_json::Value = stdout
                .lines()
                .rev()
                .find_map(|l| serde_json::from_str(l).ok())
                .ok_or_else(|| Error::Restore("the core host's restore said nothing".into()))?;
            let to = done["rolled"]["to"].as_u64().unwrap_or(u64::MAX);
            let from = done["rolled"]["from"].as_u64().unwrap_or(0);
            let ok = to == fetched.point.entries
                && (fetched.point.entries == 0
                    || done["rolled"]["kernel_root"].as_str()
                        == Some(fetched.point.kernel_root.as_str()));
            report.replay.replayed = to.saturating_sub(from);
            report.checks.push(ops::drill::Check {
                name: "roll-forward".into(),
                verdict: if ok {
                    ops::drill::Verdict::Passed
                } else {
                    ops::drill::Verdict::Failed
                },
                detail: format!("the Agent re-sent inputs {}..={to}", from + 1),
            });
            report.recovery_time_ms = started.elapsed().as_millis() as u64;
            record(&report)?;
            if !ok {
                return Err(Error::Restore(
                    "the Agent does not stand at the archive's point: do not serve".into(),
                ));
            }
            println!(
                "restored to input {} in {} ms; next: start the core host on {}, then `opsctl restore core-tail`",
                fetched.point.inputs,
                report.recovery_time_ms,
                state.display()
            );
        }
        None => {
            report.recovery_time_ms = started.elapsed().as_millis() as u64;
            record(&report)?;
            println!(
                "next: {} (with the core host's secrets in its environment)",
                host_args("aledgent-core-host")?
                    .iter()
                    .map(|a| shell_word(a.as_str()))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
    }
    Ok(())
}

#[cfg(feature = "postgres")]
async fn received_log(
    profile: ingress::config::Profile,
) -> Result<Vec<ingress::store::Row>, Error> {
    use ingress::config::{ENV_DATABASE_URL, ENV_SEALING_KEY};
    use ingress::store::Store as _;
    use ingress::store::pg::{PgOptions, PgStore};
    let url = std::env::var(ENV_DATABASE_URL)
        .map_err(|_| Error::Config(format!("{ENV_DATABASE_URL} is not set")))?;
    let log = PgStore::connect(PgOptions {
        url,
        sealing_key: ingress::config::secret_hex32(ENV_SEALING_KEY).map_err(Error::Config)?,
        // Reading the log replies to no command, so no standby is needed.
        require_sync_standby: ingress::config::SyncStandby::NotRequired,
        profile,
        pool_size: 4,
    })
    .await
    .map_err(|e| Error::Postgres(e.to_string()))?;
    log.rows().await.map_err(|e| Error::Postgres(e.to_string()))
}

/// `restore core-tail` (`ops::agent::replay_tail`).
#[cfg(feature = "postgres")]
async fn restore_core_tail(args: &[String]) -> Result<(), Error> {
    use ingress::config::DeploymentConfig;
    use ingress::remote::{Endpoint, RemoteCore};
    let config_path = arg(args, "--config")?;
    let config: DeploymentConfig = serde_json::from_str(&std::fs::read_to_string(&config_path)?)
        .map_err(|e| Error::Config(format!("{config_path}: {e}")))?;
    let endpoint = config
        .core_endpoint
        .as_deref()
        .and_then(Endpoint::parse)
        .ok_or_else(|| Error::Config(format!("{config_path}: core_endpoint is required")))?;
    let after = match opt(args, "--after") {
        Some(n) => n
            .parse::<u64>()
            .map_err(|_| Error::Config("--after: an input number".into()))?,
        None => {
            let done = path(args, "--state")?.join("restore.done.json");
            let v: serde_json::Value = serde_json::from_slice(&std::fs::read(&done)?)
                .map_err(|e| Error::Config(format!("{}: {e}", done.display())))?;
            v["plan"]["point"]["inputs"]
                .as_u64()
                .ok_or_else(|| Error::Config(format!("{}: no point", done.display())))?
        }
    };
    let rows = received_log(config.profile).await?;
    // F-035: the core link is authenticated: opsctl presents ingress's link token.
    let token = std::env::var(ingress::config::ENV_CORE_LINK_TOKEN)
        .map_err(|_| {
            Error::Config(format!(
                "{} is not set: the core link needs its token",
                ingress::config::ENV_CORE_LINK_TOKEN
            ))
        })
        .and_then(|t| ingress::remote::LinkToken::new(&t).map_err(Error::Config))?;
    let core = RemoteCore::new(endpoint, Duration::from_secs(120), token);
    let t = ops::agent::replay_tail(&core, &rows, after).await?;
    println!(
        "re-sent {} inputs after {}; the received-log's last decided input is {}",
        t.replayed, t.after, t.last
    );
    Ok(())
}

async fn exporter(args: &[String]) -> Result<(), Error> {
    use std::sync::Arc;

    use axum::routing::get;

    let p = arg(args, "--config")?;
    let c = ops::exporter::ExporterConfig::parse(&std::fs::read_to_string(&p)?)?;
    let timeout = Duration::from_millis(c.timeout_ms.max(100));
    let exporter = Arc::new(ops::exporter::Exporter::new(
        ops::exporter::HttpSource::with_token(&c.ingress, c.token()?, timeout)?,
        ops::exporter::FileSource(c.adapter_signals),
        c.records,
        &c.review_owner,
        timeout,
    ));
    let app = axum::Router::new().route(
        "/metrics",
        get(move || {
            let e = Arc::clone(&exporter);
            async move { e.scrape(now()).await }
        }),
    );
    let listener = tokio::net::TcpListener::bind(&c.listen)
        .await
        .map_err(|e| Error::Config(format!("{}: {e}", c.listen)))?;
    tracing::info!(listen = %c.listen, "serving /metrics");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
        .map_err(|e| Error::Io(e.to_string()))
}

/// The image's health check (deploy/docker/services.Dockerfile): the exporter at `listen` answers
/// `GET /metrics` with 200. A scrape always answers, ingress reachable or not (an unreachable
/// ingress is itself a signal), so this is the exporter's own readiness. Reads no token.
fn health(args: &[String]) -> Result<(), Error> {
    let p = arg(args, "--config")?;
    let c = ops::exporter::ExporterConfig::parse(&std::fs::read_to_string(&p)?)?;
    let addr = ingress::probe::address(&c.listen).map_err(Error::Config)?;
    // Above the scrape's own wait on ingress (`timeout_ms`), so a slow ingress is not a failure.
    let wait = Duration::from_millis(c.timeout_ms.max(100)) + Duration::from_secs(5);
    match ingress::probe::status(addr, "/metrics", wait).map_err(Error::Io)? {
        200 => Ok(()),
        code => Err(Error::Io(format!("{addr}: GET /metrics answered {code}"))),
    }
}

async fn run(args: &[String]) -> Result<(), Error> {
    let words: Vec<&str> = args.iter().take(2).map(String::as_str).collect();
    match words.as_slice() {
        ["backup", "verify"] => {
            let dir = args
                .get(2)
                .ok_or_else(|| Error::Config("backup verify <dir>".into()))?;
            let m = backup::verify(Path::new(dir))?;
            println!(
                "{} backup {} of {}: {} artifacts verified",
                m.store.as_str(),
                m.id,
                m.taken_at,
                m.artifacts.len()
            );
            Ok(())
        }
        ["backup", "due"] => backup_due(args),
        ["backup", "prune"] => {
            let root = path(args, "--root")?;
            let schedule = backup::schedule::Schedule::committed()?;
            for j in &schedule.jobs {
                for p in backup::prune(&root, j.store, j.retain)? {
                    println!("removed {}", p.display());
                }
            }
            Ok(())
        }
        ["backup", "pg"] => backup_pg(args).await,
        ["backup", "audit"] => {
            let (dir, m, head) = backup::audit::take(
                &path(args, "--store-dir")?,
                &path(args, "--root")?,
                &arg(args, "--id")?,
                now(),
            )?;
            println!(
                "{}: {} files, head {:?}",
                dir.display(),
                m.artifacts.len(),
                head.map(|(s, _)| s)
            );
            Ok(())
        }
        ["restore", "audit"] => {
            let (b, t) = (args.get(2), args.get(3));
            let (Some(b), Some(t)) = (b, t) else {
                return Err(Error::Config("restore audit <backup> <target>".into()));
            };
            let head = backup::audit::restore(Path::new(b), Path::new(t))?;
            println!("restored; head {:?}", head.map(|(s, _)| s));
            Ok(())
        }
        ["restore", "pg-script"] => restore_pg_script(args),
        ["restore", "core"] if flag(args, "--from") => restore_core_from(args),
        #[cfg(feature = "postgres")]
        ["restore", "core"] => restore_core(args).await,
        #[cfg(feature = "postgres")]
        ["restore", "core-tail"] => restore_core_tail(args).await,
        ["archive", "verify"] => {
            let (_, _, verified, _) = archive_verified(args)?;
            if verified.passed() {
                Ok(())
            } else if verified.undecided() {
                Err(Error::Restore(UNDECIDED.into()))
            } else {
                Err(Error::Restore("a check failed: see above".into()))
            }
        }
        ["archive", "status"] => {
            let p = path(args, "--state")?.join(core_archive::STATUS_FILE);
            let s = core_archive::ArchiveStatus::read(&p).map_err(Error::Config)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&s).map_err(|e| Error::Config(e.to_string()))?
            );
            Ok(())
        }
        ["exporter"] | ["exporter", ..] => exporter(args).await,
        ["health"] | ["health", ..] => health(args),
        _ => Err(Error::Config(
            "usage: opsctl backup|restore|exporter|health …  (see the crate documentation)".into(),
        )),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("opsctl: {e}");
            ExitCode::FAILURE
        }
    }
}
