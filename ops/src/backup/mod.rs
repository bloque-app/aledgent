// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Backup jobs (`T-018-01`, `NFR-4`, `NFR-10`, `REQ-RC-3`, `DEC-10`).
//!
//! **What must survive** (plan table), and what backs it up:
//!
//! | Unit | Backup | Module |
//! |---|---|---|
//! | Core | the `F-014` export on a cadence, with the in-process core's sidecar, the sealed openings, and its operations' pass-through data from ingress's received-log (`REQ-PO-5`, ruling `pq8-export` (a)) | [`core`] |
//! | Leaf store | none: rebuilt from the delta log (`DEC-4`, `F-006`) | [`crate::restore`] |
//! | Ingress | PostgreSQL base backup and WAL archive | [`pg`] |
//! | Adapter | PostgreSQL base backup and WAL archive | [`pg`] |
//! | Audit trail | a verified copy of its own retention store (`F-013`, `NFR-6`) | [`audit`] |
//! | Keys | the signer's, in Vault, and Vault's own backup (`F-035`): **never** in these backups | — |
//!
//! Every job writes its artifacts and a [`Manifest`] into a directory of its own, named by the
//! backup's identity: each artifact with its SHA-256 and size. [`verify()`] recomputes them; a
//! backup that does not verify is not a backup. The core Agent (`F-005`) has no backup job: its
//! host streams every decision to the off-node archive (`core_archive`, `T-005-15`), and
//! [`crate::agent`] restores from it.
//!
//! **The schedule** is `ops/backup/schedule.toml`, read by [`schedule::Schedule`]: a cadence and a
//! retention per store, and the list of what is excluded and why.

pub mod audit;
pub mod core;
pub mod pg;
pub mod schedule;

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::Error;
use crate::checksum::sha256_hex;

/// The manifest format this build writes and reads.
pub const MANIFEST_FORMAT: u32 = 1;

/// The name of a backup's manifest file.
pub const MANIFEST_FILE: &str = "manifest.json";

/// A durable store with a backup of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StoreKind {
    /// The core: roots, hot records, committed delta log, outbox, operation records.
    Core,
    /// Ingress: the received-log, input order, delivery attempts, recipients.
    Ingress,
    /// The settlement adapter: instructions, extrinsics, cursors, gaps.
    Adapter,
    /// The audit trail's append-only records.
    Audit,
}

impl StoreKind {
    /// Every store the plan's table gives a backup.
    pub const ALL: [StoreKind; 4] = [
        StoreKind::Core,
        StoreKind::Ingress,
        StoreKind::Adapter,
        StoreKind::Audit,
    ];

    /// The spelling in file names and the schedule.
    pub const fn as_str(self) -> &'static str {
        match self {
            StoreKind::Core => "core",
            StoreKind::Ingress => "ingress",
            StoreKind::Adapter => "adapter",
            StoreKind::Audit => "audit",
        }
    }

    /// Parse the spelling.
    pub fn parse(s: &str) -> Option<StoreKind> {
        StoreKind::ALL.into_iter().find(|k| k.as_str() == s)
    }
}

/// A store the plan's table gives no backup of its own, and why.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Excluded {
    /// The leaf store: a projection of the committed delta log, rebuilt from it (`DEC-4`).
    LeafStore,
    /// The signer, the clerk key and the core's key root: in Vault, and Vault's own backup
    /// (`NFR-8`, guardrail 13).
    Keys,
}

/// One artifact of a backup.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// Its file name in the backup's directory.
    pub name: String,
    /// Its SHA-256, hex.
    pub sha256: String,
    /// Its size in bytes.
    pub bytes: u64,
}

/// Where the core's backup stands: its point in the input sequence and the book it holds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorePoint {
    /// The input number the book stands at: the backup point `s_b` (plan, Restore step 1).
    pub inputs: u64,
    /// The posting sequence `S` of the export.
    pub sequence: u64,
    /// The kernel's composite root at `S`, hex.
    pub kernel_root: String,
    /// The service root at `S`, hex.
    pub service_root: String,
    /// The book root at `S`, hex: what an anchored checkpoint commits to.
    pub book_root: String,
}

/// A backup's manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// [`MANIFEST_FORMAT`].
    pub format: u32,
    /// The backup's identity.
    pub id: String,
    /// The store it backs up.
    pub store: StoreKind,
    /// When it was taken, in milliseconds since the Unix epoch.
    pub taken_at: u64,
    /// Its artifacts.
    pub artifacts: Vec<Artifact>,
    /// The core's point, for a core backup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub point: Option<CorePoint>,
}

impl Manifest {
    /// The artifact named `name`.
    pub fn artifact(&self, name: &str) -> Option<&Artifact> {
        self.artifacts.iter().find(|a| a.name == name)
    }
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let mut f = fs::File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

/// A file name that stays inside the backup's directory.
fn safe_name(name: &str) -> Result<&str, Error> {
    let ok = !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(name)
    } else {
        Err(Error::Config(format!("{name:?} is not an artifact name")))
    }
}

/// Write a backup: each artifact, synced, then its manifest, into `root/<store>/<id>/`. The
/// directory must not exist: a backup is never written over another. Returns the directory and
/// the manifest.
pub fn write(
    root: &Path,
    store: StoreKind,
    id: &str,
    taken_at: u64,
    artifacts: &[(&str, &[u8])],
    point: Option<CorePoint>,
) -> Result<(PathBuf, Manifest), Error> {
    let dir = root.join(store.as_str()).join(safe_name(id)?);
    if dir.exists() {
        return Err(Error::Config(format!(
            "backup {id} of {} exists already",
            store.as_str()
        )));
    }
    fs::create_dir_all(&dir)?;
    let mut entries = Vec::with_capacity(artifacts.len());
    for (name, bytes) in artifacts {
        let name = safe_name(name)?;
        if name == MANIFEST_FILE {
            return Err(Error::Config("an artifact cannot be the manifest".into()));
        }
        write_synced(&dir.join(name), bytes)?;
        entries.push(Artifact {
            name: name.to_owned(),
            sha256: sha256_hex(bytes),
            bytes: bytes.len() as u64,
        });
    }
    let manifest = Manifest {
        format: MANIFEST_FORMAT,
        id: id.to_owned(),
        store,
        taken_at,
        artifacts: entries,
        point,
    };
    let json = serde_json::to_vec_pretty(&manifest).map_err(|e| Error::Config(e.to_string()))?;
    write_synced(&dir.join(MANIFEST_FILE), &json)?;
    Ok((dir, manifest))
}

/// Read a backup's manifest and verify every artifact against it: present, the same size, the
/// same SHA-256.
pub fn verify(dir: &Path) -> Result<Manifest, Error> {
    let bytes = fs::read(dir.join(MANIFEST_FILE))?;
    let manifest: Manifest =
        serde_json::from_slice(&bytes).map_err(|e| Error::Config(e.to_string()))?;
    if manifest.format != MANIFEST_FORMAT {
        return Err(Error::Config(format!(
            "manifest format {} is not {MANIFEST_FORMAT}",
            manifest.format
        )));
    }
    for a in &manifest.artifacts {
        let data = fs::read(dir.join(safe_name(&a.name)?)).map_err(|_| Error::Checksum {
            artifact: a.name.clone(),
        })?;
        if data.len() as u64 != a.bytes || sha256_hex(&data) != a.sha256 {
            return Err(Error::Checksum {
                artifact: a.name.clone(),
            });
        }
    }
    Ok(manifest)
}

/// Read one verified artifact of a backup.
pub fn read_artifact(dir: &Path, manifest: &Manifest, name: &str) -> Result<Vec<u8>, Error> {
    let a = manifest
        .artifact(name)
        .ok_or_else(|| Error::Config(format!("the backup has no {name}")))?;
    let data = fs::read(dir.join(safe_name(&a.name)?))?;
    if data.len() as u64 != a.bytes || sha256_hex(&data) != a.sha256 {
        return Err(Error::Checksum {
            artifact: a.name.clone(),
        });
    }
    Ok(data)
}

/// The latest verified backup of `store` under `root`, by `taken_at`: its directory and manifest.
/// Backups that do not verify are skipped, never used.
pub fn latest(root: &Path, store: StoreKind) -> Result<Option<(PathBuf, Manifest)>, Error> {
    let dir = root.join(store.as_str());
    if !dir.exists() {
        return Ok(None);
    }
    let mut best: Option<(PathBuf, Manifest)> = None;
    for entry in fs::read_dir(&dir)? {
        let path = entry?.path();
        let Ok(m) = verify(&path) else {
            tracing::warn!(backup = %path.display(), "a backup does not verify; skipped");
            continue;
        };
        if best.as_ref().is_none_or(|(_, b)| m.taken_at > b.taken_at) {
            best = Some((path, m));
        }
    }
    Ok(best)
}

/// Remove the verified backups of `store` under `root` beyond the newest `retain` (the schedule's
/// retention). Backups that do not verify are left where they are and never counted: they are not
/// backups, and an operator looks at them. Returns what was removed.
pub fn prune(root: &Path, store: StoreKind, retain: u32) -> Result<Vec<PathBuf>, Error> {
    let dir = root.join(store.as_str());
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut verified = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let path = entry?.path();
        if let Ok(m) = verify(&path) {
            verified.push((m.taken_at, path));
        }
    }
    verified.sort_by_key(|v| std::cmp::Reverse(v.0));
    let mut removed = Vec::new();
    for (_, path) in verified.into_iter().skip(retain.max(1) as usize) {
        fs::remove_dir_all(&path)?;
        removed.push(path);
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t_018_01_a_backup_verifies_and_a_changed_byte_does_not() {
        let tmp = tempfile::tempdir().unwrap();
        let (dir, m) = write(
            tmp.path(),
            StoreKind::Audit,
            "b1",
            7,
            &[("a.bin", b"alpha"), ("b.bin", b"beta")],
            None,
        )
        .unwrap();
        assert_eq!(verify(&dir).unwrap(), m);
        assert_eq!(read_artifact(&dir, &m, "a.bin").unwrap(), b"alpha");
        // Never written over.
        assert!(write(tmp.path(), StoreKind::Audit, "b1", 8, &[], None).is_err());
        // One byte changed, or one artifact gone, fails verification.
        fs::write(dir.join("b.bin"), b"betA").unwrap();
        assert_eq!(
            verify(&dir),
            Err(Error::Checksum {
                artifact: "b.bin".into()
            })
        );
        fs::remove_file(dir.join("b.bin")).unwrap();
        assert!(verify(&dir).is_err());
        // A broken backup is never the latest.
        write(
            tmp.path(),
            StoreKind::Audit,
            "b0",
            1,
            &[("a.bin", b"x")],
            None,
        )
        .unwrap();
        let (_, latest) = latest(tmp.path(), StoreKind::Audit).unwrap().unwrap();
        assert_eq!(latest.id, "b0");
        // Pruning keeps the newest verified backups, and never a broken one.
        for (id, at) in [("p1", 10), ("p2", 20), ("p3", 30)] {
            write(
                tmp.path(),
                StoreKind::Core,
                id,
                at,
                &[("a.bin", b"x")],
                None,
            )
            .unwrap();
        }
        let removed = prune(tmp.path(), StoreKind::Core, 2).unwrap();
        assert_eq!(removed.len(), 1);
        assert!(removed[0].ends_with("p1"));
        assert_eq!(
            super::latest(tmp.path(), StoreKind::Core)
                .unwrap()
                .unwrap()
                .1
                .id,
            "p3"
        );
        let removed = prune(tmp.path(), StoreKind::Audit, 1).unwrap();
        assert!(
            removed.is_empty(),
            "b0 is the only verified audit backup; b1 is broken"
        );
        assert!(dir.exists());
        // Names stay inside the directory.
        assert!(write(tmp.path(), StoreKind::Audit, "../x", 1, &[], None).is_err());
        assert!(
            write(
                tmp.path(),
                StoreKind::Audit,
                "b2",
                1,
                &[("../x", b"")],
                None
            )
            .is_err()
        );
    }
}
