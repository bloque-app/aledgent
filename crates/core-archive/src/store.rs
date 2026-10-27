// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Write-once object stores: the sink and source the archive runs on (`T-005-15`).
//!
//! The semantics are those of VOS's checkpoint-archive store (`gap/5-checkpoint-archive` on the
//! fork, `vos::agent::checkpoint_archive`): publication is atomic and never replaces different
//! bytes under a name; an identical retry succeeds as not created; success means durable in the
//! backend; reads are bounded before they allocate, and return only what was published under the
//! name asked. A store is storage, not authority: what it returns is checked by whoever reads it
//! ([`crate::fetch()`]).
//!
//! Two backends: [`FsStore`], a directory (tests, the drill, a mounted volume), and
//! [`crate::s3::S3Store`], S3 with Object Lock (the deployment's). [`crate::seal::Sealed`] is a
//! layer over either that seals what it publishes and opens what it reads (`F-044`).

use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Why a store call failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreError {
    /// The name is not one this crate writes.
    InvalidName(String),
    /// Different bytes already own the name: another writer, or another history. Never retried.
    Conflict(String),
    /// A read would exceed its bound.
    TooLarge(String),
    /// What the backend holds is not what was published.
    Corrupt(String),
    /// The backend did not answer, or refused; nothing is implied about the object. Retried.
    Unavailable(String),
    /// The object is sealed and no key given opens it, or a key's service answered that it does
    /// not (`F-044`). Not retried.
    Sealed(String),
}

impl core::fmt::Display for StoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            StoreError::InvalidName(n) => write!(f, "invalid object name {n:?}"),
            StoreError::Conflict(n) => write!(f, "{n}: different bytes are already published"),
            StoreError::TooLarge(n) => write!(f, "{n}: larger than its bound"),
            StoreError::Corrupt(s) => write!(f, "corrupt: {s}"),
            StoreError::Unavailable(s) => write!(f, "unavailable: {s}"),
            StoreError::Sealed(s) => write!(f, "sealed: {s}"),
        }
    }
}

/// The outcome of a publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Put {
    /// False when identical bytes were already published under the name (an exact retry).
    pub created: bool,
}

/// What a reader with no key can know of an object: its length and SHA-256, and its leading
/// bytes. Of an unsealed object, all of it; of a sealed one, what its seal keeps in clear
/// ([`crate::seal::clear_len`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Head {
    /// The object's length.
    pub bytes: u64,
    /// Its SHA-256, hex.
    pub sha256: String,
    /// Its first bytes.
    pub clear: Vec<u8>,
}

impl Head {
    /// The head of an object read whole.
    pub fn of(bytes: Vec<u8>) -> Head {
        Head {
            bytes: bytes.len() as u64,
            sha256: crate::record::sha256_hex(&bytes),
            clear: bytes,
        }
    }

    /// Whether it holds the whole object.
    pub fn is_whole(&self) -> bool {
        self.clear.len() as u64 == self.bytes
    }
}

/// A write-once object store (module documentation).
pub trait ObjectStore: Send + Sync {
    /// Publish `bytes` under `name`, once.
    fn put_once(&self, name: &str, bytes: &[u8]) -> Result<Put, StoreError>;
    /// The bytes published under `name`, refusing more than `max` bytes.
    fn get(&self, name: &str, max: u64) -> Result<Option<Vec<u8>>, StoreError>;
    /// The [`Head`] of `name`, refusing an object of more than `max` bytes. A store that holds
    /// objects as they are reads the whole object.
    fn head(&self, name: &str, max: u64) -> Result<Option<Head>, StoreError> {
        Ok(self.get(name, max)?.map(Head::of))
    }
    /// Every name under `prefix` (which ends in `/`), sorted.
    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError>;
    /// Where it is, for logs and the status file: never a credential.
    fn describe(&self) -> String;
}

/// A name this crate writes: `/`-separated parts of ASCII letters, digits, `-`, `_` and `.`,
/// none empty, `.` or `..`.
pub fn check_name(name: &str) -> Result<(), StoreError> {
    let ok = !name.is_empty()
        && name.split('/').all(|p| {
            !p.is_empty()
                && p != "."
                && p != ".."
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        });
    if ok {
        Ok(())
    } else {
        Err(StoreError::InvalidName(name.to_owned()))
    }
}

/// Unpublished scratch files start with this; readers ignore them.
const TEMP_PREFIX: &str = ".tmp-";

/// A store in a directory: `root/<name>`. Publication is a temporary file written and synced, a
/// hard link that fails if the name exists, and the directory synced (VOS's
/// `FileCheckpointArchive`). Suitable for tests, the drill, and a mounted volume.
pub struct FsStore {
    root: PathBuf,
    counter: AtomicU64,
}

impl FsStore {
    /// The store rooted at `root`, created if absent.
    pub fn open(root: impl Into<PathBuf>) -> Result<FsStore, StoreError> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| io(&root, e))?;
        Ok(FsStore {
            root,
            counter: AtomicU64::new(0),
        })
    }

    /// Its root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, name: &str) -> Result<PathBuf, StoreError> {
        check_name(name)?;
        Ok(self.root.join(name))
    }
}

fn io(p: &Path, e: std::io::Error) -> StoreError {
    StoreError::Unavailable(format!("{}: {e}", p.display()))
}

fn read_bounded(path: &Path, name: &str, max: u64) -> Result<Option<Vec<u8>>, StoreError> {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io(path, e)),
    };
    let meta = f.metadata().map_err(|e| io(path, e))?;
    if !meta.is_file() {
        return Err(StoreError::Corrupt(format!("{name} is not a file")));
    }
    if meta.len() > max {
        return Err(StoreError::TooLarge(name.to_owned()));
    }
    let mut out = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    f.read_to_end(&mut out).map_err(|e| io(path, e))?;
    if out.len() as u64 != meta.len() {
        return Err(StoreError::Corrupt(format!("{name} changed while read")));
    }
    Ok(Some(out))
}

impl ObjectStore for FsStore {
    fn put_once(&self, name: &str, bytes: &[u8]) -> Result<Put, StoreError> {
        let path = self.path(name)?;
        let dir = path
            .parent()
            .ok_or_else(|| StoreError::InvalidName(name.to_owned()))?;
        fs::create_dir_all(dir).map_err(|e| io(dir, e))?;
        let tmp = dir.join(format!(
            "{TEMP_PREFIX}{}-{}",
            std::process::id(),
            self.counter.fetch_add(1, Ordering::Relaxed)
        ));
        let written = (|| {
            let mut f = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()
        })();
        if let Err(e) = written {
            let _ = fs::remove_file(&tmp);
            return Err(io(&tmp, e));
        }
        let linked = fs::hard_link(&tmp, &path);
        let _ = fs::remove_file(&tmp);
        match linked {
            Ok(()) => {
                File::open(dir)
                    .and_then(|d| d.sync_all())
                    .map_err(|e| io(dir, e))?;
                Ok(Put { created: true })
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                match read_bounded(&path, name, bytes.len() as u64)? {
                    Some(old) if old == bytes => Ok(Put { created: false }),
                    _ => Err(StoreError::Conflict(name.to_owned())),
                }
            }
            Err(e) => Err(io(&path, e)),
        }
    }

    fn get(&self, name: &str, max: u64) -> Result<Option<Vec<u8>>, StoreError> {
        read_bounded(&self.path(name)?, name, max)
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        let rel = prefix.trim_end_matches('/');
        if !rel.is_empty() {
            check_name(rel)?;
        }
        let dir = self.root.join(rel);
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io(&dir, e)),
        };
        let mut out = Vec::new();
        for e in entries {
            let e = e.map_err(|e| io(&dir, e))?;
            let Some(n) = e.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if n.starts_with(TEMP_PREFIX) || !e.file_type().map_err(|x| io(&dir, x))?.is_file() {
                continue;
            }
            out.push(if rel.is_empty() {
                n
            } else {
                format!("{rel}/{n}")
            });
        }
        out.sort();
        Ok(out)
    }

    fn describe(&self) -> String {
        format!("fs:{}", self.root.display())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t_005_15_a_name_is_published_once_and_an_exact_retry_is_not_a_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let s = FsStore::open(tmp.path()).unwrap();
        assert_eq!(s.put_once("a/b/1.seg", b"one"), Ok(Put { created: true }));
        assert_eq!(s.put_once("a/b/1.seg", b"one"), Ok(Put { created: false }));
        assert_eq!(
            s.put_once("a/b/1.seg", b"two"),
            Err(StoreError::Conflict("a/b/1.seg".into()))
        );
        assert_eq!(s.get("a/b/1.seg", 3).unwrap().as_deref(), Some(&b"one"[..]));
        assert!(matches!(
            s.get("a/b/1.seg", 2),
            Err(StoreError::TooLarge(_))
        ));
        assert_eq!(s.get("a/b/2.seg", 3).unwrap(), None);
        s.put_once("a/b/0.seg", b"zero").unwrap();
        // A crash-left scratch file is never listed.
        fs::write(tmp.path().join("a/b/.tmp-1-1"), b"x").unwrap();
        assert_eq!(s.list("a/b/").unwrap(), vec!["a/b/0.seg", "a/b/1.seg"]);
        assert_eq!(s.list("a/c/").unwrap(), Vec::<String>::new());
        for bad in ["", "/a", "a//b", "../a", "a/./b", "a b"] {
            assert!(s.put_once(bad, b"").is_err(), "{bad:?}");
        }
    }
}
