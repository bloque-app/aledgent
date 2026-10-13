// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The audit trail's backup (`T-018-01`, `F-013`, `REQ-AT-2`, `NFR-6`).
//!
//! The audit store is its own retention store, on media apart from the book's. Its backup is a
//! copy of its files — the stored hash of the writer credential, `HEAD`, and every append-only
//! segment — taken only once the store verifies (every frame re-hashed, the chain walked), and
//! verified again as a copy: the copy must open as a store with the same head. A restore copies
//! the files back; the store re-verifies itself on open.

use std::fs;
use std::path::{Path, PathBuf};

use audit_store::FileAuditStore;

use super::{Manifest, StoreKind};
use crate::Error;

/// The head of an audit store: its last record's sequence number and hash.
pub type Head = Option<(u64, [u8; 32])>;

fn head_of(dir: &Path) -> Result<Head, Error> {
    let store = FileAuditStore::read(dir).map_err(|e| Error::Audit(format!("{e:?}")))?;
    store
        .verify()
        .map_err(|e| Error::Audit(format!("the chain does not verify: {e:?}")))
}

/// Back up the audit store in `store_dir` into `root/audit/<id>/`. Returns the backup's
/// directory, its manifest and the head it holds.
pub fn take(
    store_dir: &Path,
    root: &Path,
    id: &str,
    taken_at: u64,
) -> Result<(PathBuf, Manifest, Head), Error> {
    let head = head_of(store_dir)?;
    let mut names: Vec<String> = Vec::new();
    for entry in fs::read_dir(store_dir)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    let mut files = Vec::with_capacity(names.len());
    for n in &names {
        files.push((n.as_str(), fs::read(store_dir.join(n))?));
    }
    // A segment appended between the head's read and the copy is copied too; the copy's own head
    // is what the backup holds.
    let refs: Vec<(&str, &[u8])> = files.iter().map(|(n, b)| (*n, b.as_slice())).collect();
    let (dir, manifest) = super::write(root, StoreKind::Audit, id, taken_at, &refs, None)?;
    let copied = head_of(&dir)?;
    if copied.map(|(s, _)| s) < head.map(|(s, _)| s) {
        return Err(Error::Audit("the copy is behind the store".into()));
    }
    Ok((dir, manifest, copied))
}

/// Restore the audit store backed up in `backup` into `target`, an empty directory, and verify
/// it. Returns its head.
pub fn restore(backup: &Path, target: &Path) -> Result<Head, Error> {
    let manifest = super::verify(backup)?;
    if manifest.store != StoreKind::Audit {
        return Err(Error::Config("not an audit backup".into()));
    }
    fs::create_dir_all(target)?;
    if fs::read_dir(target)?.next().is_some() {
        return Err(Error::Audit("the target is not empty".into()));
    }
    for a in &manifest.artifacts {
        fs::write(
            target.join(&a.name),
            super::read_artifact(backup, &manifest, &a.name)?,
        )?;
    }
    head_of(target)
}
