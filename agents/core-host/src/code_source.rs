// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Where the core host fetches code from (`T-037-06`; `REQ-RT-17`, `REQ-AU-8`; amendment 0023
//! "Retrieval: a pluggable `CodeSource`").
//!
//! Staging an `Upgrade` ([`core_binding::staging`]) asks one [`CodeFetch`] for bytes and checks
//! all of them. This module is that fetcher for a real node:
//!
//! - **[`CodeSource`]**: one place bytes can come from. Untrusted: whatever it returns is checked.
//! - **[`FsSource`]**: a local or mounted directory, one regular file per digest, named by the
//!   digest's 64 lowercase hex digits and by nothing else.
//! - **[`Fetcher`]**: the cache first, then the sources in order. Every answer is checked by length
//!   and `blake2b-256` before it is returned or cached. A mismatch is discarded, counted
//!   (`aledgent_code_source_mismatch_total`, one label: the source's name), logged, and the next
//!   source is tried. The cache is written only after the check, by rename, and every read of it is
//!   checked again.
//!
//! The fetcher checks so that a bad source does not hide a good one; staging checks again, because
//! that is the check the specification relies on ([`core_binding::staging::fetch`]), and
//! `StagedVersions::insert` checks a third time.
//!
//! The configuration is a file of its own, named by `serve --code-config` (see [`CodeConfig`]):
//! the deployment file is shared with ingress, which refuses keys it does not know.

use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use core_binding::staging::CodeFetch;
use ledger_core::code::artefact_digest;
use serde::Deserialize;

/// The most a manifest is read for: its length is not known to the `Upgrade`, which names only
/// its digest. Today's manifests are about 0.7 to 2 KB.
pub const MANIFEST_READ_CAP: u64 = 64 * 1024;
/// The most an artefact may be: one of a manifest naming more is refused without being read.
/// `core.vos` is 2.9 to 4.6 MB, the runtime 1.1 MB.
pub const ARTEFACT_CAP: u64 = 64 * 1024 * 1024;
/// The name the cache's mismatches are counted under.
pub const CACHE_NAME: &str = "cache";

/// Why a source could not answer. Not a mismatch: no bytes came back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceError {
    /// The source failed (an I/O error). Nothing is known about the bytes it holds.
    Failed(String),
    /// What stands where bytes belong is a link, a directory or a special file. That is not an I/O
    /// error: it is counted as a wrong answer, and in the cache it is cleared away.
    NotRegular(String),
    /// The artefact is longer than [`ARTEFACT_CAP`]: it is not read.
    TooLarge(u64),
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Failed(why) | Self::NotRegular(why) => f.write_str(why),
            Self::TooLarge(len) => write!(f, "{len} bytes is over the {ARTEFACT_CAP} byte cap"),
        }
    }
}

/// One place code can come from (amendment 0023). Untrusted: whatever it returns is checked by
/// the caller.
///
/// `len` is `None` for a manifest, whose length the `Upgrade` does not name; the amendment's
/// `fetch` takes a `u64`. A source that needs the length (`kreivo-preimage`, `T-037-08`) must be
/// given it some other way.
pub trait CodeSource: Send + Sync {
    /// A name for logs and metrics (no credential, no path): the kind and the position in the
    /// configured list, as `fs:0`.
    fn name(&self) -> &str;
    /// Bytes this source holds for `digest`; `Ok(None)` if it holds none. At most `len + 1` bytes
    /// when `len` is known, [`MANIFEST_READ_CAP`] + 1 when not, so a longer answer reads as a
    /// mismatch without being read whole.
    fn fetch(&self, digest: &[u8; 32], len: Option<u64>) -> Result<Option<Vec<u8>>, SourceError>;
}

/// How many bytes may be read for an answer of `len`, plus one so that a longer file is seen to
/// be longer.
fn read_limit(len: Option<u64>) -> Result<u64, SourceError> {
    match len {
        Some(l) if l > ARTEFACT_CAP => Err(SourceError::TooLarge(l)),
        Some(l) => Ok(l + 1),
        None => Ok(MANIFEST_READ_CAP + 1),
    }
}

/// The file `digest` is kept in, under `root`: its 64 lowercase hex digits and nothing else. No
/// string from a request or a manifest reaches a path.
fn file_of(root: &Path, digest: &[u8; 32]) -> PathBuf {
    root.join(hex::encode(digest))
}

/// Read `path` if it is a regular file, at most `limit` bytes. It is opened without following a
/// symbolic link and without blocking, and then must be a regular file: a symbolic link, a
/// directory, a device or a FIFO is refused, never read. `Ok(None)`: there is no such file.
fn read_regular(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, SourceError> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        // `O_NOFOLLOW` on a symbolic link fails the open with `ELOOP`: a link, not an I/O error.
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            return Err(SourceError::NotRegular("it is a symbolic link".into()));
        }
        Err(e) => return Err(SourceError::Failed(format!("not readable: {e}"))),
    };
    let meta = file
        .metadata()
        .map_err(|e| SourceError::Failed(format!("not readable: {e}")))?;
    if !meta.is_file() {
        return Err(SourceError::NotRegular(
            "it is not a regular file (a link, a directory or a special file is never read)".into(),
        ));
    }
    let mut bytes = Vec::new();
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(|e| SourceError::Failed(format!("not readable: {e}")))?;
    Ok(Some(bytes))
}

/// The `fs` source: a directory holding one regular file for each digest, named by its hex.
/// Network mounts and offline copies alike; what it returns is as untrusted as any other source's.
#[derive(Debug)]
pub struct FsSource {
    root: PathBuf,
    name: String,
}

impl FsSource {
    /// The source over directory `root`, the `index`-th of the configured sources.
    #[must_use]
    pub fn new(root: PathBuf, index: usize) -> Self {
        Self {
            root,
            name: format!("fs:{index}"),
        }
    }
}

impl CodeSource for FsSource {
    fn name(&self) -> &str {
        &self.name
    }

    fn fetch(&self, digest: &[u8; 32], len: Option<u64>) -> Result<Option<Vec<u8>>, SourceError> {
        read_regular(&file_of(&self.root, digest), read_limit(len)?)
    }
}

/// What the code-source file holds (`serve --code-config <file>`): amendment 0023's `[code]`
/// table, in JSON, with the keys it has.
///
/// ```json
/// { "cache": "/srv/aledgent/code-cache",
///   "sources": [ { "kind": "fs", "path": "/srv/aledgent/code-src" } ] }
/// ```
///
/// Unknown keys and kinds fail the start: a source the host does not know is never skipped.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CodeConfig {
    /// The content-addressed cache's directory: absolute, outside every source and outside the
    /// state directory.
    pub cache: PathBuf,
    /// The sources, tried in order after the cache.
    #[serde(default)]
    pub sources: Vec<SourceConfig>,
}

/// One configured source.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceConfig {
    /// A directory (`fs`).
    Fs {
        /// The directory: absolute.
        path: PathBuf,
    },
}

/// The fetcher: the cache first, then the sources in order, every answer checked.
pub struct Fetcher {
    cache: PathBuf,
    sources: Vec<Box<dyn CodeSource>>,
    /// The mismatches discarded: the cache's, then each source's, in that order.
    mismatches: Vec<(String, AtomicU64)>,
    /// Makes the names of temporary files unique within this process.
    temporary: AtomicU64,
}

/// Make the cache directory (and any parent it lacks) readable by its owner alone. A directory
/// that exists is left as the operator made it: it belongs to the operator.
fn make_dir(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

/// Print a line about the fetcher to standard error, as the host's other lines.
fn note(what: &str) {
    eprintln!("aledgent-core-host: code source: {what}");
}

/// The digest, short enough for a log.
fn short(digest: &[u8; 32]) -> String {
    hex::encode(&digest[..6])
}

/// Whether `bytes` are what `digest` and `len` name.
fn is_what_it_names(bytes: &[u8], digest: &[u8; 32], len: Option<u64>) -> bool {
    len.is_none_or(|l| bytes.len() as u64 == l) && &artefact_digest(bytes) == digest
}

impl Fetcher {
    /// The fetcher over the cache directory `cache` (made if absent; any temporary file a torn
    /// write left in it is removed) and `sources`, tried in order.
    ///
    /// # Errors
    ///
    /// The cache directory cannot be made or listed.
    pub fn new(cache: PathBuf, sources: Vec<Box<dyn CodeSource>>) -> Result<Self, String> {
        make_dir(&cache).map_err(|e| format!("{}: {e}", cache.display()))?;
        for entry in fs::read_dir(&cache).map_err(|e| format!("{}: {e}", cache.display()))? {
            let entry = entry.map_err(|e| format!("{}: {e}", cache.display()))?;
            if entry.file_name().to_string_lossy().starts_with(TEMPORARY) {
                let _ = fs::remove_file(entry.path());
            }
        }
        let mismatches = std::iter::once(CACHE_NAME.to_owned())
            .chain(sources.iter().map(|s| s.name().to_owned()))
            .map(|name| (name, AtomicU64::new(0)))
            .collect();
        Ok(Self {
            cache,
            sources,
            mismatches,
            temporary: AtomicU64::new(0),
        })
    }

    /// The fetcher a code-source file describes (`state`: the core host's state directory, which
    /// the cache must not be inside: its `replica/` is wiped on every start).
    ///
    /// # Errors
    ///
    /// A path that is not absolute; a source directory that does not exist; a cache that is, or is
    /// inside, or holds, a source directory, or is inside the state directory.
    pub fn open(config: &CodeConfig, state: &Path) -> Result<Self, String> {
        let absolute = |what: &str, p: &Path| -> Result<(), String> {
            if p.is_absolute() {
                Ok(())
            } else {
                Err(format!(
                    "{what} {}: an absolute path is required",
                    p.display()
                ))
            }
        };
        absolute("cache", &config.cache)?;
        make_dir(&config.cache).map_err(|e| format!("cache {}: {e}", config.cache.display()))?;
        let cache = fs::canonicalize(&config.cache)
            .map_err(|e| format!("cache {}: {e}", config.cache.display()))?;
        let state = fs::canonicalize(state).map_err(|e| format!("{}: {e}", state.display()))?;
        if cache.starts_with(&state) {
            return Err(format!(
                "cache {}: inside the state directory, whose replica is wiped on every start",
                cache.display()
            ));
        }
        let mut sources: Vec<Box<dyn CodeSource>> = Vec::new();
        for (i, s) in config.sources.iter().enumerate() {
            let SourceConfig::Fs { path } = s;
            absolute("source", path)?;
            let root =
                fs::canonicalize(path).map_err(|e| format!("source {}: {e}", path.display()))?;
            if !root.is_dir() {
                return Err(format!("source {}: not a directory", path.display()));
            }
            if cache.starts_with(&root) || root.starts_with(&cache) {
                return Err(format!(
                    "cache {} and source {} are one directory or one inside the other: the \
                     cache holds what was checked, a source what was not",
                    cache.display(),
                    root.display()
                ));
            }
            sources.push(Box::new(FsSource::new(root, i)));
        }
        Self::new(cache, sources)
    }

    fn count(&self, name: &str) {
        if let Some((_, n)) = self.mismatches.iter().find(|(m, _)| m == name) {
            n.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Write checked `bytes` to the cache: a temporary file with a name of its own, written,
    /// synced, renamed onto its digest, and the directory synced. A failure only costs the cache
    /// the entry: the bytes are good and are returned.
    fn store(&self, digest: &[u8; 32], bytes: &[u8]) {
        let tmp = self.cache.join(format!(
            "{TEMPORARY}{}-{}",
            std::process::id(),
            self.temporary.fetch_add(1, Ordering::Relaxed)
        ));
        let write = || -> std::io::Result<()> {
            let mut f = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
            fs::rename(&tmp, file_of(&self.cache, digest))?;
            File::open(&self.cache)?.sync_all()
        };
        if let Err(e) = write() {
            let _ = fs::remove_file(&tmp);
            note(&format!(
                "WARNING: the cache did not keep {}: {e}",
                short(digest)
            ));
        }
    }

    /// Clear what is not an entry from `path`: a file or a link is removed; a directory is moved
    /// aside (never deleted), so that it cannot keep this digest out of the cache for good.
    fn clear(&self, path: &Path, digest: &[u8; 32]) {
        let is_dir = fs::symlink_metadata(path).is_ok_and(|m| m.is_dir());
        let done = if is_dir {
            let aside = self.cache.join(format!(
                "{ASIDE}{}-{}",
                hex::encode(&digest[..6]),
                self.temporary.fetch_add(1, Ordering::Relaxed)
            ));
            fs::rename(path, aside)
        } else {
            fs::remove_file(path)
        };
        if let Err(e) = done {
            note(&format!(
                "WARNING: the cache could not clear what stands at {}: {e}",
                short(digest)
            ));
        }
    }

    /// The bytes of the cache's entry for `digest`, if it holds a good one. A bad entry is
    /// counted, logged and deleted, so that the sources are asked.
    fn cached(&self, digest: &[u8; 32], len: Option<u64>) -> Option<Vec<u8>> {
        let path = file_of(&self.cache, digest);
        let limit = read_limit(len).ok()?;
        match read_regular(&path, limit) {
            Ok(None) => None,
            Ok(Some(bytes)) if is_what_it_names(&bytes, digest, len) => Some(bytes),
            Ok(Some(_)) => {
                self.count(CACHE_NAME);
                note(&format!(
                    "the cache entry for {} does not match its hash; discarded",
                    short(digest)
                ));
                let _ = fs::remove_file(&path);
                None
            }
            Err(SourceError::NotRegular(why)) => {
                // Proven not to be an entry: a link, a directory or a special file stands where
                // bytes belong. Count it, and clear it so that a good entry can take its place.
                self.count(CACHE_NAME);
                note(&format!(
                    "the cache entry for {} is refused: {why}",
                    short(digest)
                ));
                self.clear(&path, digest);
                None
            }
            Err(e) => {
                // Not proven bad: the file may be fine and the failure the host's (descriptors,
                // permissions, a disk error). Leave it, and ask the sources.
                note(&format!(
                    "the cache entry for {} could not be read, and is left as it is: {e}",
                    short(digest)
                ));
                None
            }
        }
    }
}

/// The start of a temporary file's name in the cache: never a digest, so never an entry.
const TEMPORARY: &str = ".tmp-";
/// The start of the name a directory found at a digest's place is moved to: never a digest.
const ASIDE: &str = ".moved-";

impl CodeFetch for Fetcher {
    fn name(&self) -> &str {
        "code-fetcher"
    }

    fn fetch(&self, digest: &[u8; 32], len: Option<u64>) -> Option<Vec<u8>> {
        // An artefact over the cap is not asked for anywhere.
        if let Err(e) = read_limit(len) {
            note(&format!("{} is not fetched: {e}", short(digest)));
            return None;
        }
        if let Some(bytes) = self.cached(digest, len) {
            return Some(bytes);
        }
        for source in &self.sources {
            let bytes = match source.fetch(digest, len) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => continue,
                Err(e) => {
                    // A link or a special file where bytes belong is a wrong answer: counted.
                    if matches!(e, SourceError::NotRegular(_)) {
                        self.count(source.name());
                    }
                    note(&format!(
                        "{} failed for {}: {e}",
                        source.name(),
                        short(digest)
                    ));
                    continue;
                }
            };
            if is_what_it_names(&bytes, digest, len) {
                self.store(digest, &bytes);
                return Some(bytes);
            }
            self.count(source.name());
            note(&format!(
                "bytes for {} from {} do not match their hash; discarded",
                short(digest),
                source.name()
            ));
        }
        None
    }

    fn mismatches(&self) -> Vec<(String, u64)> {
        self.mismatches
            .iter()
            .map(|(name, n)| (name.clone(), n.load(Ordering::Relaxed)))
            .collect()
    }
}

/// The fetcher `serve --code-config <file>` asks for, if the flag is given. Without it, `None`:
/// the node has no source of code, and every `Upgrade` is refused, as before.
///
/// # Errors
///
/// The file does not read or parse, or [`Fetcher::open`] refuses what it says.
pub fn from_args(args: &[String], state: &Path) -> Result<Option<std::sync::Arc<Fetcher>>, String> {
    // The flag is given once and with a value. A flag with none (an empty variable in a unit file)
    // must not read as "no code source": the node would start, look healthy, and refuse every
    // `Upgrade` without saying why.
    let at: Vec<usize> = (0..args.len())
        .filter(|i| args[*i] == "--code-config")
        .collect();
    let path = match at[..] {
        [] => return Ok(None),
        [i] => match args.get(i + 1) {
            Some(v) if !v.is_empty() && !v.starts_with("--") => v.clone(),
            _ => return Err("--code-config: a file is required after it".into()),
        },
        _ => return Err("--code-config: given more than once".into()),
    };
    let text = fs::read_to_string(&path).map_err(|e| format!("--code-config {path}: {e}"))?;
    let config: CodeConfig =
        serde_json::from_str(&text).map_err(|e| format!("--code-config {path}: {e}"))?;
    Fetcher::open(&config, state)
        .map(|f| Some(std::sync::Arc::new(f)))
        .map_err(|e| format!("--code-config {path}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    /// A directory under the system's temporary one, removed on drop.
    struct Dir(PathBuf);

    impl Dir {
        fn new(what: &str) -> Self {
            static N: AtomicU64 = AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "aledgent-code-source-{what}-{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&p).unwrap();
            Dir(fs::canonicalize(p).unwrap())
        }
        fn path(&self) -> &Path {
            &self.0
        }
        fn put(&self, name: &str, bytes: &[u8]) {
            fs::write(self.0.join(name), bytes).unwrap();
        }
        fn put_bytes(&self, bytes: &[u8]) -> [u8; 32] {
            let d = artefact_digest(bytes);
            self.put(&hex::encode(d), bytes);
            d
        }
        fn files(&self) -> Vec<String> {
            let mut v: Vec<String> = fs::read_dir(&self.0)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            v.sort();
            v
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fetcher(cache: &Dir, sources: &[&Dir]) -> Fetcher {
        let sources = sources
            .iter()
            .enumerate()
            .map(|(i, d)| Box::new(FsSource::new(d.path().to_path_buf(), i)) as Box<dyn CodeSource>)
            .collect();
        Fetcher::new(cache.path().to_path_buf(), sources).unwrap()
    }

    fn count(f: &Fetcher, name: &str) -> u64 {
        f.mismatches()
            .into_iter()
            .find(|(n, _)| n == name)
            .unwrap()
            .1
    }

    /// A source that answers what it is given and counts how often it was asked.
    struct Scripted {
        name: String,
        answer: Result<Option<Vec<u8>>, SourceError>,
        asked: AtomicUsize,
    }

    impl CodeSource for Scripted {
        fn name(&self) -> &str {
            &self.name
        }
        fn fetch(&self, _: &[u8; 32], _: Option<u64>) -> Result<Option<Vec<u8>>, SourceError> {
            self.asked.fetch_add(1, Ordering::Relaxed);
            self.answer.clone()
        }
    }

    #[test]
    fn t037_06_a_file_that_holds_the_bytes_is_returned_and_cached() {
        let (cache, src) = (Dir::new("cache"), Dir::new("src"));
        let d = src.put_bytes(b"the package");
        let f = fetcher(&cache, &[&src]);
        assert_eq!(f.fetch(&d, Some(11)), Some(b"the package".to_vec()));
        assert_eq!(cache.files(), vec![hex::encode(d)]);
        // Without the source, the cache answers.
        let g = fetcher(&cache, &[]);
        assert_eq!(g.fetch(&d, Some(11)), Some(b"the package".to_vec()));
        // A digest nobody holds is not here.
        assert_eq!(f.fetch(&[9; 32], Some(1)), None);
    }

    #[test]
    fn t037_06_bytes_that_do_not_match_their_hash_are_never_returned() {
        let (cache, src) = (Dir::new("cache"), Dir::new("src"));
        let good = artefact_digest(b"the package");
        // Wrong bytes under the digest's name.
        src.put(&hex::encode(good), b"another package");
        let f = fetcher(&cache, &[&src]);
        assert_eq!(f.fetch(&good, Some(15)), None);
        assert_eq!(f.fetch(&good, None), None);
        // The right bytes, asked for with another length.
        let right = Dir::new("right");
        right.put_bytes(b"the package");
        let g = fetcher(&cache, &[&right]);
        assert_eq!(g.fetch(&good, Some(10)), None);
        assert_eq!(g.fetch(&good, Some(12)), None);
        // Nothing was cached.
        assert!(cache.files().is_empty(), "{:?}", cache.files());
        // A cache entry that does not match is no better.
        cache.put(&hex::encode(good), b"another package");
        let h = fetcher(&cache, &[]);
        assert_eq!(h.fetch(&good, Some(15)), None);
        assert_eq!(h.fetch(&good, Some(11)), None);
    }

    #[test]
    fn t037_06_a_mismatch_is_counted_and_the_next_source_is_tried() {
        let (cache, lying, honest) = (Dir::new("cache"), Dir::new("lying"), Dir::new("honest"));
        let d = honest.put_bytes(b"the package");
        lying.put(&hex::encode(d), b"not the package");
        let f = fetcher(&cache, &[&lying, &honest]);
        assert_eq!(count(&f, "fs:0"), 0);
        assert_eq!(f.fetch(&d, Some(11)), Some(b"the package".to_vec()));
        // Once per discarded answer, labelled by source.
        assert_eq!(count(&f, "fs:0"), 1);
        assert_eq!(count(&f, "fs:1"), 0);
        assert_eq!(count(&f, "cache"), 0);
        // The good copy is cached: the next fetch asks nobody.
        assert_eq!(f.fetch(&d, Some(11)), Some(b"the package".to_vec()));
        assert_eq!(count(&f, "fs:0"), 1);
        // The counter reports the cache and every source, at zero too.
        let names: Vec<String> = f.mismatches().into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, vec!["cache", "fs:0", "fs:1"]);
    }

    #[test]
    fn t037_06_the_cache_is_written_only_after_the_check() {
        let cache = Dir::new("cache");
        let d = artefact_digest(b"expected");
        let liar = Scripted {
            name: "liar".into(),
            answer: Ok(Some(b"something else".to_vec())),
            asked: AtomicUsize::new(0),
        };
        let f = Fetcher::new(cache.path().to_path_buf(), vec![Box::new(liar)]).unwrap();
        assert_eq!(f.fetch(&d, None), None);
        assert!(cache.files().is_empty(), "{:?}", cache.files());
        assert_eq!(count(&f, "liar"), 1);
        // A source that fails, or holds nothing, is no mismatch.
        let failing = Scripted {
            name: "failing".into(),
            answer: Err(SourceError::Failed("down".into())),
            asked: AtomicUsize::new(0),
        };
        let none = Scripted {
            name: "none".into(),
            answer: Ok(None),
            asked: AtomicUsize::new(0),
        };
        let g = Fetcher::new(
            cache.path().to_path_buf(),
            vec![Box::new(failing), Box::new(none)],
        )
        .unwrap();
        assert_eq!(g.fetch(&d, None), None);
        assert_eq!(count(&g, "failing") + count(&g, "none"), 0);
        assert!(cache.files().is_empty());
    }

    #[test]
    fn t037_06_a_torn_cache_write_leaves_no_entry() {
        let cache = Dir::new("cache");
        let d = artefact_digest(b"the package");
        // A write that stopped half-way left its temporary file behind.
        cache.put(&format!("{TEMPORARY}1-0"), b"the pack");
        let f = fetcher(&cache, &[]);
        // It is cleaned when the fetcher opens, and is never an entry.
        assert!(cache.files().is_empty(), "{:?}", cache.files());
        assert_eq!(f.fetch(&d, Some(11)), None);
        cache.put(&format!("{TEMPORARY}1-0"), b"the pack");
        assert_eq!(f.fetch(&d, Some(11)), None);
    }

    #[test]
    fn t037_06_a_code_config_flag_without_a_file_stops_the_start() {
        let state = Dir::new("state");
        let flag = || "--code-config".to_owned();
        // The flag is last: an empty variable in a unit file. It must not read as "no source".
        let r = from_args(&["serve".to_owned(), flag()], state.path());
        assert!(r.err().unwrap().contains("a file is required"));
        // Empty value, or another flag where the value belongs.
        assert!(from_args(&[flag(), String::new()], state.path()).is_err());
        assert!(
            from_args(
                &[flag(), "--state".to_owned(), "x".to_owned()],
                state.path()
            )
            .is_err()
        );
        // Twice.
        let twice = [flag(), "a.json".to_owned(), flag(), "b.json".to_owned()];
        assert!(
            from_args(&twice, state.path())
                .err()
                .unwrap()
                .contains("more than once")
        );
        // Not given at all: no source, as before.
        assert!(
            from_args(&["serve".to_owned()], state.path())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn t037_06_an_unreadable_cache_entry_is_left_where_it_is() {
        use std::os::unix::fs::PermissionsExt as _;
        let (cache, src) = (Dir::new("cache"), Dir::new("src"));
        let d = src.put_bytes(b"the package");
        let f = fetcher(&cache, &[]);
        let entry = cache.path().join(hex::encode(d));
        fs::write(&entry, b"the package").unwrap();
        fs::set_permissions(&entry, fs::Permissions::from_mode(0o000)).unwrap();
        if File::open(&entry).is_ok() {
            return; // running as a user the mode does not bind: nothing to show
        }
        // No source holds it: the read fails, nothing is returned, and the entry is not deleted.
        assert_eq!(f.fetch(&d, Some(11)), None);
        assert!(fs::symlink_metadata(&entry).is_ok(), "the entry stays");
        assert_eq!(
            count(&f, "cache"),
            0,
            "an I/O failure is not a wrong answer"
        );
        // Readable again, it is the same good entry.
        fs::set_permissions(&entry, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(f.fetch(&d, Some(11)), Some(b"the package".to_vec()));
    }

    #[test]
    fn t037_06_a_link_or_a_directory_in_the_cache_is_cleared_and_counted() {
        use std::os::unix::fs::symlink;
        let (cache, src, elsewhere) = (Dir::new("cache"), Dir::new("src"), Dir::new("elsewhere"));
        let (a, b) = (
            src.put_bytes(b"linked entry"),
            src.put_bytes(b"planted entry"),
        );
        let target = elsewhere.put_bytes(b"linked entry");
        symlink(
            elsewhere.path().join(hex::encode(target)),
            cache.path().join(hex::encode(a)),
        )
        .unwrap();
        fs::create_dir(cache.path().join(hex::encode(b))).unwrap();
        let f = fetcher(&cache, &[&src]);
        // Both are replaced by the checked bytes from the source, and each was counted.
        assert_eq!(f.fetch(&a, Some(12)), Some(b"linked entry".to_vec()));
        assert_eq!(f.fetch(&b, Some(13)), Some(b"planted entry".to_vec()));
        assert_eq!(count(&f, "cache"), 2);
        for (d, bytes) in [(a, &b"linked entry"[..]), (b, b"planted entry")] {
            let meta = fs::symlink_metadata(cache.path().join(hex::encode(d))).unwrap();
            assert!(meta.is_file(), "a regular file now");
            assert_eq!(fs::read(cache.path().join(hex::encode(d))).unwrap(), bytes);
        }
        // The planted directory was moved aside, not deleted.
        assert!(
            cache.files().iter().any(|n| n.starts_with(ASIDE)),
            "{:?}",
            cache.files()
        );
    }

    #[test]
    fn t037_06_a_link_or_a_special_file_in_a_source_is_counted() {
        use std::os::unix::fs::symlink;
        let (cache, src, elsewhere) = (Dir::new("cache"), Dir::new("src"), Dir::new("elsewhere"));
        let d = artefact_digest(b"linked");
        let target = elsewhere.put_bytes(b"linked");
        symlink(
            elsewhere.path().join(hex::encode(target)),
            src.path().join(hex::encode(d)),
        )
        .unwrap();
        let f = fetcher(&cache, &[&src]);
        assert_eq!(f.fetch(&d, Some(6)), None);
        assert_eq!(
            count(&f, "fs:0"),
            1,
            "a link where bytes belong is a wrong answer"
        );
        // An I/O failure is not.
        let failing = Scripted {
            name: "failing".into(),
            answer: Err(SourceError::Failed("down".into())),
            asked: AtomicUsize::new(0),
        };
        let g = Fetcher::new(cache.path().to_path_buf(), vec![Box::new(failing)]).unwrap();
        assert_eq!(g.fetch(&d, Some(6)), None);
        assert_eq!(count(&g, "failing"), 0);
    }

    #[test]
    fn t037_06_the_cache_is_private_to_its_owner() {
        use std::os::unix::fs::PermissionsExt as _;
        let (state, src, root) = (Dir::new("state"), Dir::new("src"), Dir::new("root"));
        let d = src.put_bytes(b"the package");
        let config = CodeConfig {
            cache: root.path().join("a/b/cache"),
            sources: vec![SourceConfig::Fs {
                path: src.path().to_path_buf(),
            }],
        };
        let f = Fetcher::open(&config, state.path()).unwrap();
        f.fetch(&d, Some(11)).unwrap();
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let cache = fs::canonicalize(root.path().join("a/b/cache")).unwrap();
        assert_eq!(mode(&cache), 0o700);
        assert_eq!(mode(cache.parent().unwrap()), 0o700);
        assert_eq!(mode(&cache.join(hex::encode(d))), 0o600);
    }

    #[test]
    fn t037_06_a_cache_that_cannot_be_written_still_returns_the_checked_bytes() {
        use std::os::unix::fs::PermissionsExt as _;
        let (cache, src) = (Dir::new("cache"), Dir::new("src"));
        let d = src.put_bytes(b"the package");
        let f = fetcher(&cache, &[&src]);
        fs::set_permissions(cache.path(), fs::Permissions::from_mode(0o500)).unwrap();
        let wrote = File::create(cache.path().join("probe")).is_ok();
        let got = f.fetch(&d, Some(11));
        fs::set_permissions(cache.path(), fs::Permissions::from_mode(0o700)).unwrap();
        if wrote {
            return; // running as a user the mode does not bind
        }
        assert_eq!(got, Some(b"the package".to_vec()));
        assert!(cache.files().is_empty());
    }

    #[test]
    fn t037_06_a_corrupt_cache_entry_is_discarded_and_fetched_again() {
        let (cache, src) = (Dir::new("cache"), Dir::new("src"));
        let d = src.put_bytes(b"the package");
        cache.put(&hex::encode(d), b"bit rot, same length");
        let f = fetcher(&cache, &[&src]);
        assert_eq!(f.fetch(&d, Some(11)), Some(b"the package".to_vec()));
        assert_eq!(count(&f, "cache"), 1);
        assert_eq!(
            fs::read(cache.path().join(hex::encode(d))).unwrap(),
            b"the package"
        );
    }

    #[test]
    fn t037_06_the_file_name_is_the_hash_and_nothing_else() {
        let d = [0xab; 32];
        let p = file_of(Path::new("/srv/code"), &d);
        assert_eq!(p, Path::new("/srv/code").join("ab".repeat(32)));
        // A source directory holding files named like paths gives nothing for them.
        let (cache, src) = (Dir::new("cache"), Dir::new("src"));
        src.put("core.vos", b"package");
        src.put("agent_runtime.pvm", b"runtime");
        let f = fetcher(&cache, &[&src]);
        assert_eq!(f.fetch(&artefact_digest(b"package"), Some(7)), None);
        assert_eq!(f.fetch(&artefact_digest(b"runtime"), Some(7)), None);
        assert_eq!(f.mismatches().iter().map(|(_, n)| n).sum::<u64>(), 0);
    }

    #[test]
    fn t037_06_fs_refuses_symlinks_directories_and_special_files() {
        use std::os::unix::fs::symlink;
        let (cache, src, elsewhere) = (Dir::new("cache"), Dir::new("src"), Dir::new("elsewhere"));
        let name = |b: &[u8]| hex::encode(artefact_digest(b));
        // A link to a file with the right bytes.
        let target = elsewhere.put_bytes(b"linked");
        symlink(
            elsewhere.path().join(hex::encode(target)),
            src.path().join(name(b"linked")),
        )
        .unwrap();
        // A directory under a digest's name.
        fs::create_dir(src.path().join(name(b"directory"))).unwrap();
        // A FIFO under a digest's name: opening it must not block.
        let fifo = src.path().join(name(b"fifo"));
        let made = std::process::Command::new("mkfifo").arg(&fifo).status();
        let f = fetcher(&cache, &[&src]);
        for b in [&b"linked"[..], b"directory"] {
            let d = artefact_digest(b);
            assert!(
                FsSource::new(src.path().to_path_buf(), 0)
                    .fetch(&d, Some(b.len() as u64))
                    .is_err()
            );
            assert_eq!(f.fetch(&d, Some(b.len() as u64)), None);
        }
        if made.is_ok_and(|s| s.success()) {
            let d = artefact_digest(b"fifo");
            assert!(
                FsSource::new(src.path().to_path_buf(), 0)
                    .fetch(&d, Some(4))
                    .is_err()
            );
            assert_eq!(f.fetch(&d, Some(4)), None);
        }
        assert!(cache.files().is_empty());
        // A regular file is read.
        let ok = src.put_bytes(b"plain");
        assert_eq!(f.fetch(&ok, Some(5)), Some(b"plain".to_vec()));
    }

    #[test]
    fn t037_06_a_manifest_read_is_bounded() {
        let src = Dir::new("src");
        let big = vec![7u8; (MANIFEST_READ_CAP as usize) * 4];
        let d = src.put_bytes(&big);
        let got = FsSource::new(src.path().to_path_buf(), 0)
            .fetch(&d, None)
            .unwrap()
            .unwrap();
        // One byte over the cap is read, to see that the file is longer, and no more.
        assert_eq!(got.len() as u64, MANIFEST_READ_CAP + 1);
        let cache = Dir::new("cache");
        let f = fetcher(&cache, &[&src]);
        assert_eq!(f.fetch(&d, None), None);
        assert_eq!(count(&f, "fs:0"), 1);
        assert!(cache.files().is_empty());
    }

    #[test]
    fn t037_06_an_artefact_longer_than_its_length_is_a_mismatch() {
        let (cache, src) = (Dir::new("cache"), Dir::new("src"));
        let d = src.put_bytes(&vec![1u8; 4096]);
        let got = FsSource::new(src.path().to_path_buf(), 0)
            .fetch(&d, Some(10))
            .unwrap()
            .unwrap();
        assert_eq!(got.len(), 11, "length plus one, not the whole file");
        let f = fetcher(&cache, &[&src]);
        assert_eq!(f.fetch(&d, Some(10)), None);
        assert_eq!(count(&f, "fs:0"), 1);
        assert!(f.fetch(&d, Some(4096)).is_some());
    }

    #[test]
    fn t037_06_an_artefact_over_the_cap_is_not_read() {
        let cache = Dir::new("cache");
        let asked = Arc::new(Scripted {
            name: "never".into(),
            answer: Ok(Some(Vec::new())),
            asked: AtomicUsize::new(0),
        });
        struct Shared(Arc<Scripted>);
        impl CodeSource for Shared {
            fn name(&self) -> &str {
                self.0.name()
            }
            fn fetch(&self, d: &[u8; 32], l: Option<u64>) -> Result<Option<Vec<u8>>, SourceError> {
                self.0.fetch(d, l)
            }
        }
        let f = Fetcher::new(
            cache.path().to_path_buf(),
            vec![Box::new(Shared(Arc::clone(&asked)))],
        )
        .unwrap();
        assert_eq!(f.fetch(&[1; 32], Some(ARTEFACT_CAP + 1)), None);
        assert_eq!(
            asked.asked.load(Ordering::Relaxed),
            0,
            "no source was asked"
        );
        // The source itself refuses too.
        let src = Dir::new("src");
        assert_eq!(
            FsSource::new(src.path().to_path_buf(), 0).fetch(&[1; 32], Some(ARTEFACT_CAP + 1)),
            Err(SourceError::TooLarge(ARTEFACT_CAP + 1))
        );
    }

    #[test]
    fn t037_06_a_cache_inside_a_source_or_the_state_is_refused() {
        let (state, src, cache) = (Dir::new("state"), Dir::new("src"), Dir::new("cache"));
        let config = |cache: &Path, src: &Path| CodeConfig {
            cache: cache.to_path_buf(),
            sources: vec![SourceConfig::Fs {
                path: src.to_path_buf(),
            }],
        };
        assert!(Fetcher::open(&config(cache.path(), src.path()), state.path()).is_ok());
        // The cache is a source, or inside one, or holds one.
        let same = Fetcher::open(&config(src.path(), src.path()), state.path());
        assert!(same.err().unwrap().contains("one directory"));
        let inside = Fetcher::open(&config(&src.path().join("c"), src.path()), state.path());
        assert!(inside.err().unwrap().contains("one directory"));
        let holds = Fetcher::open(&config(cache.path(), &cache.path().join("s")), state.path());
        assert!(holds.is_err(), "a source that does not exist");
        fs::create_dir(cache.path().join("s")).unwrap();
        let holds = Fetcher::open(&config(cache.path(), &cache.path().join("s")), state.path());
        assert!(holds.err().unwrap().contains("one directory"));
        // The cache is inside the state directory.
        let in_state = Fetcher::open(
            &config(&state.path().join("code"), src.path()),
            state.path(),
        );
        assert!(in_state.err().unwrap().contains("state directory"));
        // Relative paths are refused.
        let relative = CodeConfig {
            cache: PathBuf::from("cache"),
            sources: Vec::new(),
        };
        assert!(Fetcher::open(&relative, state.path()).is_err());
        let relative = config(cache.path(), Path::new("src"));
        assert!(Fetcher::open(&relative, state.path()).is_err());
    }

    #[test]
    fn t037_06_concurrent_fetches_of_one_hash_leave_one_good_entry() {
        let (cache, src) = (Dir::new("cache"), Dir::new("src"));
        let bytes = vec![5u8; 200_000];
        let d = src.put_bytes(&bytes);
        let f = Arc::new(fetcher(&cache, &[&src]));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (f, bytes) = (Arc::clone(&f), bytes.clone());
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        assert_eq!(f.fetch(&d, Some(200_000)), Some(bytes.clone()));
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(cache.files(), vec![hex::encode(d)]);
        assert_eq!(fs::read(cache.path().join(hex::encode(d))).unwrap(), bytes);
        assert_eq!(f.mismatches().iter().map(|(_, n)| n).sum::<u64>(), 0);
    }

    #[test]
    fn t037_06_the_code_source_file_names_only_what_the_host_knows() {
        let (state, src, cache) = (Dir::new("state"), Dir::new("src"), Dir::new("cache"));
        let file = state.path().join("code.json");
        let write = |text: String| fs::write(&file, text).unwrap();
        let args = vec!["--code-config".to_owned(), file.display().to_string()];
        // The file of the proposal.
        write(format!(
            r#"{{ "cache": "{}", "sources": [ {{ "kind": "fs", "path": "{}" }} ] }}"#,
            cache.path().display(),
            src.path().display()
        ));
        let f = from_args(&args, state.path()).unwrap().expect("a fetcher");
        assert_eq!(f.name(), "code-fetcher");
        // A kind the host does not know, a key it does not know: the start fails.
        write(format!(
            r#"{{ "cache": "{}", "sources": [ {{ "kind": "s3", "uri": "s3://bucket" }} ] }}"#,
            cache.path().display()
        ));
        assert!(from_args(&args, state.path()).is_err());
        write(format!(
            r#"{{ "cache": "{}", "sources": [], "extra": 1 }}"#,
            cache.path().display()
        ));
        assert!(from_args(&args, state.path()).is_err());
        write(format!(
            r#"{{ "cache": "{}", "sources": [ {{ "kind": "fs", "path": "{}", "extra": 1 }} ] }}"#,
            cache.path().display(),
            src.path().display()
        ));
        assert!(from_args(&args, state.path()).is_err());
        // Without the flag there is no source: every `Upgrade` is refused, as before.
        assert!(from_args(&[], state.path()).unwrap().is_none());
    }

    /// `AC-K7.2` through the `fs` source, and `staging`'s own checks over the fetcher: a version
    /// whose artefacts are held nowhere, or held with other bytes, is refused and nothing is kept;
    /// the good version is staged, the lying directory listed first.
    mod staging {
        use super::*;
        use core_binding::staging::{self, StageRefusal};
        use ledger_core::code::{
            Artefact, BuildProfile, CodeManifest, KernelIdentity, Platform, Role, WriterKind,
        };

        fn manifest(generation: u64, package: &[u8], runtime: &[u8]) -> CodeManifest {
            let profile = || BuildProfile {
                target: "riscv64em-vos".into(),
                toolchain: "nightly".into(),
                opt_level: "s".into(),
                features: Vec::new(),
                source_revision: "0123abcd".into(),
                cargo_lock: [0; 32],
            };
            let artefact = |role, bytes: &[u8]| Artefact {
                role,
                digest: artefact_digest(bytes),
                len: bytes.len() as u64,
                identities: Vec::new(),
                profile: profile(),
            };
            CodeManifest {
                writer: WriterKind::Core,
                platform: Platform {
                    name: "vos-saga".into(),
                    revision: "c54fc1ff".into(),
                    identities: Vec::new(),
                },
                generation,
                artefacts: vec![
                    artefact(Role::Package, package),
                    artefact(Role::Runtime, runtime),
                ],
                kernel: KernelIdentity {
                    revision: "e9a7b08".into(),
                    checksum: None,
                },
                constructions: Vec::new(),
                messages: Vec::new(),
            }
        }

        #[test]
        fn t037_06_ac_k7_2_through_the_fs_source() {
            let (cache, lying, honest) = (Dir::new("cache"), Dir::new("lying"), Dir::new("honest"));
            let m = manifest(1, b"package of H", b"runtime");
            let h = m.version();
            // Unknown to every source.
            let f = fetcher(&cache, &[&lying, &honest]);
            assert!(matches!(
                staging::fetch(&f, &h, 0),
                Err(StageRefusal::NotRetrievable(_))
            ));
            // The manifest and the runtime are good; the package is wrong bytes under its name.
            honest.put_bytes(&m.to_bytes());
            honest.put_bytes(b"runtime");
            lying.put_bytes(&m.to_bytes());
            lying.put_bytes(b"runtime");
            lying.put(&hex::encode(artefact_digest(b"package of H")), b"a lie");
            let only_lying = fetcher(&cache, &[&lying]);
            assert!(matches!(
                staging::fetch(&only_lying, &h, 0),
                Err(StageRefusal::NotRetrievable(_))
            ));
            assert_eq!(count(&only_lying, "fs:0"), 1);
            assert!(
                !cache
                    .files()
                    .contains(&hex::encode(artefact_digest(b"package of H")))
            );
            honest.put_bytes(b"package of H");
            // With the good directory behind the lying one, H is staged through the fallback.
            let both = fetcher(&cache, &[&lying, &honest]);
            let checked = staging::fetch(&both, &h, 0).expect("staged");
            assert_eq!(checked.package, b"package of H");
            assert_eq!(checked.runtime.as_deref(), Some(&b"runtime"[..]));
            assert_eq!(count(&both, "fs:0"), 1);
            assert_eq!(count(&both, "fs:1"), 0);
            // Another generation than the writer's next is refused, as before.
            assert!(matches!(
                staging::fetch(&both, &h, 1),
                Err(StageRefusal::Generation { .. })
            ));
        }
    }
}
