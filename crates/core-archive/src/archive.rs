// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The archive's sink and source over a write-once object store, and how a deployment names one.
//!
//! The two traits follow VOS's checkpoint-archive sink and source (`gap/5-checkpoint-archive`:
//! `CheckpointArchiveSink`, `CheckpointArchiveSource`, `contiguous_archived_end`), keyed by what
//! the go-live core host has: segment numbers of its own log rather than certified Raft indices,
//! and checkpoints of a single Local Agent's runtime state rather than quorum-certified bundles.
//! When `F-021` brings Shared Agents and VOS's own archive (`DEP-11`), the same [`ObjectStore`]
//! backs VOS's traits: the bucket, its lock and its key stay; the records change.
//!
//! Uploading is maintenance: nothing the core commits, acknowledges or answers ever waits on a
//! sink call ([`crate::archiver`]).

use std::path::PathBuf;

use crate::record::{
    CHECKPOINTS, CheckpointHeader, MAX_RECORD_BYTES, SEGMENTS, SegmentHeader, checkpoint_name,
    decode_checkpoint, decode_checkpoint_header, decode_segment, decode_segment_header, number_of,
    segment_name,
};
use crate::s3::{CredentialSource, LockMode, S3Config, S3Store};
use crate::seal::{Keys, Recipients, Sealed, Unsealed};
use crate::store::{FsStore, ObjectStore, Put, StoreError};

/// Publication side.
pub trait ArchiveSink: Send + Sync {
    /// Publish segment `n`'s record, once.
    fn put_segment(&self, n: u64, record: &[u8]) -> Result<Put, StoreError>;
    /// Publish the checkpoint at segment `n`, once.
    fn put_checkpoint(&self, n: u64, record: &[u8]) -> Result<Put, StoreError>;
}

/// Retrieval side. Selection is not verification: [`crate::fetch()`] checks what it reads.
pub trait ArchiveSource: Send + Sync {
    /// Every segment number published, ascending.
    fn segments(&self) -> Result<Vec<u64>, StoreError>;
    /// Every checkpoint's segment number, ascending.
    fn checkpoints(&self) -> Result<Vec<u64>, StoreError>;
    /// Segment `n`'s record.
    fn get_segment(&self, n: u64) -> Result<Option<Vec<u8>>, StoreError>;
    /// The checkpoint at segment `n`'s record.
    fn get_checkpoint(&self, n: u64) -> Result<Option<Vec<u8>>, StoreError>;
}

/// The highest `n` such that every segment `1..=n` is in `numbers` (ascending). Anything after a
/// gap is not recoverable from this archive.
pub fn contiguous_end(numbers: &[u64]) -> u64 {
    let mut end = 0;
    for &n in numbers {
        if n == end + 1 {
            end = n;
        } else if n > end + 1 {
            break;
        }
    }
    end
}

/// An archive over one object store.
pub struct Archive(Box<dyn ObjectStore>);

impl Archive {
    /// The archive in `store`.
    pub fn new(store: Box<dyn ObjectStore>) -> Archive {
        Archive(store)
    }

    /// Where it is (no credential).
    pub fn describe(&self) -> String {
        self.0.describe()
    }

    /// The store it is over.
    pub fn store(&self) -> &dyn ObjectStore {
        self.0.as_ref()
    }

    /// Segment `n`'s header and digest, read with no key (`F-044`): from the record when the
    /// store holds it as it is (and then checked whole), from what its seal keeps in clear when it
    /// is sealed. What the archiver reads back when it resumes.
    pub fn segment_head(&self, n: u64) -> Result<Option<(SegmentHeader, String)>, StoreError> {
        let Some(h) = self.0.head(&segment_name(n), MAX_RECORD_BYTES)? else {
            return Ok(None);
        };
        let bad = |e: crate::record::Malformed| StoreError::Corrupt(format!("segment {n}: {e}"));
        if h.is_whole() {
            let s = decode_segment(&h.clear).map_err(bad)?;
            return Ok(Some((s.header, s.digest)));
        }
        Ok(Some((
            decode_segment_header(&h.clear).map_err(bad)?,
            h.sha256,
        )))
    }

    /// The checkpoint at segment `n`'s header, read with no key, as [`Archive::segment_head`].
    pub fn checkpoint_head(&self, n: u64) -> Result<Option<CheckpointHeader>, StoreError> {
        let Some(h) = self.0.head(&checkpoint_name(n), MAX_RECORD_BYTES)? else {
            return Ok(None);
        };
        let bad = |e: crate::record::Malformed| StoreError::Corrupt(format!("checkpoint {n}: {e}"));
        if h.is_whole() {
            return decode_checkpoint(&h.clear)
                .map(|c| Some(c.header))
                .map_err(bad);
        }
        decode_checkpoint_header(&h.clear).map(Some).map_err(bad)
    }

    fn numbers(&self, prefix: &str) -> Result<Vec<u64>, StoreError> {
        let mut out: Vec<u64> = self
            .0
            .list(prefix)?
            .iter()
            .filter_map(|n| number_of(n))
            .collect();
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }
}

/// Where a chainspec is kept in an archive (`F-036`): by its digest, once.
pub fn chainspec_name(digest_hex: &str) -> String {
    format!("chainspec/{digest_hex}.bin")
}

impl Archive {
    /// Keep the book's chainspec beside its segments (`F-036`, `REQ-RT-3`): a restore rebuilds the
    /// genesis state from it. Written once; a second put of the same digest is a no-op.
    pub fn put_chainspec(&self, digest_hex: &str, bytes: &[u8]) -> Result<Put, StoreError> {
        self.0.put_once(&chainspec_name(digest_hex), bytes)
    }

    /// The chainspec of this digest, if the archive keeps it.
    pub fn get_chainspec(&self, digest_hex: &str) -> Result<Option<Vec<u8>>, StoreError> {
        self.0.get(&chainspec_name(digest_hex), MAX_RECORD_BYTES)
    }

    /// Keep the passing `verify-genesis` report of the chainspec of this digest beside it
    /// (`F-036` `T-036-25`): a restore lays it out again, so a restored chainspec book takes
    /// commands as the original did. Written once; the same bytes again are a no-op, other bytes
    /// an error.
    pub fn put_verified(&self, digest_hex: &str, report: &[u8]) -> Result<Put, StoreError> {
        self.0.put_once(&verified_name(digest_hex), report)
    }

    /// The `verify-genesis` report kept for the chainspec of this digest, if any.
    pub fn get_verified(&self, digest_hex: &str) -> Result<Option<Vec<u8>>, StoreError> {
        self.0.get(&verified_name(digest_hex), MAX_RECORD_BYTES)
    }
}

/// Where a chainspec's `verify-genesis` report is kept in an archive (`T-036-25`).
pub fn verified_name(digest_hex: &str) -> String {
    format!("chainspec/{digest_hex}.verified.json")
}

impl ArchiveSink for Archive {
    fn put_segment(&self, n: u64, record: &[u8]) -> Result<Put, StoreError> {
        self.0.put_once(&segment_name(n), record)
    }

    fn put_checkpoint(&self, n: u64, record: &[u8]) -> Result<Put, StoreError> {
        self.0.put_once(&checkpoint_name(n), record)
    }
}

impl ArchiveSource for Archive {
    fn segments(&self) -> Result<Vec<u64>, StoreError> {
        self.numbers(SEGMENTS)
    }

    fn checkpoints(&self) -> Result<Vec<u64>, StoreError> {
        self.numbers(CHECKPOINTS)
    }

    fn get_segment(&self, n: u64) -> Result<Option<Vec<u8>>, StoreError> {
        self.0.get(&segment_name(n), MAX_RECORD_BYTES)
    }

    fn get_checkpoint(&self, n: u64) -> Result<Option<Vec<u8>>, StoreError> {
        self.0.get(&checkpoint_name(n), MAX_RECORD_BYTES)
    }
}

/// The Object Lock retention every archived object carries unless the spec says otherwise: 180 days
/// in compliance mode (David, 2026-10-25, relaxing the owner's 400 days of 2026-10-05, `ab-q01`).
/// Retention itself is forever (`NFR-22`): the bucket has no lifecycle rule that deletes current
/// versions, so an object outlives its lock. If the archive enters PCI DSS scope the lock becomes
/// 365 days (requirement 10.5.1 keeps audit history for twelve months); Colombia's ten years come
/// later. Both are a `retain-days` in the deployment's spec, not a change here.
pub const DEFAULT_RETAIN_DAYS: u32 = 180;

/// What the archive is for: a writer (the core host) must have S3's protections; a reader (a
/// restore) need not.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// The core host's sink.
    Write,
    /// A restore's source.
    Read,
}

/// The seal's part of a spec (`F-044`): options of every kind of store.
#[derive(Debug, Default)]
struct SealOptions {
    seal: Option<PathBuf>,
    vault: Option<String>,
    transit_mount: Option<String>,
    refuse_unsealed: bool,
}

/// Split the seal's options off `query`, giving back the others.
fn seal_options(query: &str) -> Result<(SealOptions, String), String> {
    let mut o = SealOptions::default();
    let mut rest = Vec::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair
            .split_once('=')
            .ok_or_else(|| format!("archive option {pair:?} has no value"))?;
        match k {
            "seal" => o.seal = Some(PathBuf::from(v)),
            "vault" => o.vault = Some(v.to_owned()),
            "transit-mount" => o.transit_mount = Some(v.to_owned()),
            "unsealed" => {
                o.refuse_unsealed = match v {
                    "accept" => false,
                    "refuse" => true,
                    _ => return Err(format!("unsealed={v:?}: accept or refuse")),
                }
            }
            _ => rest.push(pair),
        }
    }
    if o.vault.is_some() != o.transit_mount.is_some() {
        return Err("vault= and transit-mount= go together".into());
    }
    Ok((o, rest.join("&")))
}

/// Open the archive a deployment names (`--archive`, `--from`):
///
/// ```text
/// fs:<directory>[?<seal options>]
/// s3://<bucket>[/<prefix>]?region=<region>[&kms-key-id=<key>][&lock-mode=compliance|governance
///      &retain-days=<days>][&endpoint=<scheme://host:port>&path-style=true][&<seal options>]
///
/// seal options (F-044): seal=<seal file>                      a writer seals to its keys
///                       vault=<https://…>&transit-mount=<m>   a reader opens with Vault Transit
///                       unsealed=accept|refuse                objects written before the seal
/// ```
///
/// Object Lock defaults to compliance mode for [`DEFAULT_RETAIN_DAYS`] days. `lock-mode` and
/// `retain-days` override it, together; nothing turns it off. A writer over S3 needs the seal
/// (`seal=`) or KMS (`kms-key-id`), and may have both (`S3Config::check_writer`).
///
/// Credentials and keys never appear here: S3's come from the environment, a file or the instance
/// role ([`crate::s3`]); a reader's Vault token and recovery identity from the files the
/// environment names ([`Keys::from_env`]). The seal file holds public keys only.
///
/// **A writer is given no key** (`F-044`): whatever the environment names, the core host never
/// holds a key or a token that opens the archive. It reads back only what the seal keeps in clear.
pub fn open(spec: &str, access: Access) -> Result<Archive, String> {
    open_with(spec, access, keys_for(spec, access, Keys::from_env)?)
}

/// The keys [`open`] gives: none to a writer; to a reader, those `from_env` finds for the spec's
/// Vault.
fn keys_for(
    spec: &str,
    access: Access,
    from_env: impl FnOnce(Option<(&str, &str)>) -> Result<Keys, String>,
) -> Result<Keys, String> {
    let (_, query) = split_spec(spec)?;
    let (options, _) = seal_options(query)?;
    match access {
        Access::Write => Ok(Keys::none()),
        Access::Read => from_env(
            options
                .vault
                .as_deref()
                .zip(options.transit_mount.as_deref()),
        ),
    }
}

fn split_spec(spec: &str) -> Result<(&str, &str), String> {
    if let Some(rest) = spec.strip_prefix("fs:") {
        return Ok(rest.split_once('?').unwrap_or((rest, "")));
    }
    let Some(rest) = spec.strip_prefix("s3://") else {
        return Err(format!(
            "{spec:?}: an archive is fs:<dir> or s3://<bucket>/<prefix>?…"
        ));
    };
    Ok(rest.split_once('?').unwrap_or((rest, "")))
}

/// [`open`], with the reader's keys given rather than taken from the environment.
pub fn open_with(spec: &str, access: Access, keys: Keys) -> Result<Archive, String> {
    let (path, query) = split_spec(spec)?;
    let (options, others) = seal_options(query)?;
    let recipients = match (&options.seal, access) {
        (Some(p), Access::Write) => Some(Recipients::load(p)?),
        // A reader opens; the seal file is a writer's.
        (Some(_), Access::Read) | (None, _) => None,
    };
    let inner: Box<dyn ObjectStore> = if spec.starts_with("fs:") {
        if path.is_empty() {
            return Err("fs: needs a directory".into());
        }
        if !others.is_empty() {
            return Err(format!("fs: takes the seal's options only, not {others:?}"));
        }
        Box::new(FsStore::open(path).map_err(|e| e.to_string())?)
    } else {
        let config = parse_s3(&if others.is_empty() {
            path.to_owned()
        } else {
            format!("{path}?{others}")
        })?;
        if access == Access::Write {
            config.check_writer(recipients.is_some())?;
        }
        Box::new(S3Store::new(config, CredentialSource::Default))
    };
    let unsealed = if options.refuse_unsealed {
        Unsealed::Refuse
    } else {
        Unsealed::Accept
    };
    Ok(Archive::new(Box::new(Sealed::new(
        inner, recipients, keys, unsealed,
    ))))
}

/// Parse `bucket[/prefix]?k=v&…` (the part of an `s3://` spec after the scheme, without the seal's
/// options).
pub fn parse_s3(rest: &str) -> Result<S3Config, String> {
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (bucket, prefix) = path.split_once('/').unwrap_or((path, ""));
    if bucket.is_empty() {
        return Err("s3:// needs a bucket".into());
    }
    let prefix = prefix.trim_matches('/');
    if !prefix.is_empty() {
        crate::store::check_name(prefix).map_err(|e| e.to_string())?;
    }
    let mut c = S3Config {
        bucket: bucket.to_owned(),
        prefix: if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}/")
        },
        region: String::new(),
        endpoint: None,
        path_style: false,
        kms_key_id: None,
        lock: None,
    };
    let (mut mode, mut days) = (None, None);
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair
            .split_once('=')
            .ok_or_else(|| format!("s3 option {pair:?} has no value"))?;
        match k {
            "region" => c.region = v.to_owned(),
            "endpoint" => c.endpoint = Some(v.to_owned()),
            "path-style" => c.path_style = v == "true" || v == "1",
            "kms-key-id" => c.kms_key_id = Some(v.to_owned()),
            "lock-mode" => {
                mode = Some(match v {
                    "compliance" | "COMPLIANCE" => LockMode::Compliance,
                    "governance" | "GOVERNANCE" => LockMode::Governance,
                    _ => return Err(format!("lock-mode {v:?}: compliance or governance")),
                })
            }
            "retain-days" => {
                days = Some(
                    v.parse::<u32>()
                        .map_err(|_| format!("retain-days {v:?}: a number of days"))?,
                )
            }
            _ => return Err(format!("unknown s3 option {k:?}")),
        }
    }
    if c.region.is_empty() {
        return Err("s3:// needs region=".into());
    }
    c.lock = match (mode, days) {
        (Some(m), Some(d)) => Some((m, d)),
        (None, None) => Some((LockMode::Compliance, DEFAULT_RETAIN_DAYS)),
        _ => return Err("lock-mode and retain-days go together".into()),
    };
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn t_005_15_an_archive_is_named_by_a_spec() {
        let c = parse_s3(
            "aledgent-archive/prod/core?region=us-east-2&kms-key-id=arn:aws:kms:us-east-2:1:key/x&lock-mode=compliance&retain-days=400",
        )
        .unwrap();
        assert_eq!(
            (c.bucket.as_str(), c.prefix.as_str(), c.region.as_str()),
            ("aledgent-archive", "prod/core/", "us-east-2")
        );
        assert_eq!(c.lock, Some((LockMode::Compliance, 400)));
        assert!(c.check_writer(false).is_ok());
        assert!(
            open("s3://b/p?region=us-east-2", Access::Write).is_err(),
            "neither the seal nor a KMS key"
        );
        let d = parse_s3("b/p?region=us-east-2&kms-key-id=alias/k").unwrap();
        assert_eq!(d.lock, Some((LockMode::Compliance, DEFAULT_RETAIN_DAYS)));
        assert_eq!(DEFAULT_RETAIN_DAYS, 180, "David, 2026-10-25");
        assert!(d.check_writer(false).is_ok());
        let e = parse_s3("b/p?region=us-east-2").unwrap();
        assert!(e.check_writer(true).is_ok(), "the seal, no KMS");
        assert!(e.check_writer(false).is_err());
        assert!(open("s3://b/p?region=us-east-2", Access::Read).is_ok());
        assert!(
            open(
                "s3://b/p?region=us-east-2&lock-mode=governance",
                Access::Read
            )
            .is_err()
        );
        assert!(open("http://x", Access::Read).is_err());
        assert_eq!(contiguous_end(&[1, 2, 3, 5]), 3);
        assert_eq!(contiguous_end(&[2, 3]), 0);
        assert_eq!(contiguous_end(&[]), 0);
    }

    /// `F-044`: a writer is never given a key, whatever the environment names; a reader is given
    /// what it names, for the spec's Vault.
    #[test]
    fn f_044_a_writer_is_given_no_key() {
        let spec = "s3://b/p?region=us-east-2&vault=https://v.example&transit-mount=t";
        let found = |vault: Option<(&str, &str)>| {
            assert_eq!(vault, Some(("https://v.example", "t")));
            Ok(Keys {
                transit: None,
                recovery: vec![crate::age::Identity::generate()],
            })
        };
        assert_eq!(
            keys_for(spec, Access::Write, found).unwrap().recovery.len(),
            0
        );
        assert_eq!(
            keys_for(spec, Access::Read, found).unwrap().recovery.len(),
            1
        );
    }

    /// `T-036-25`: the verify-genesis report is kept beside its chainspec, once, and read back.
    #[test]
    fn t_036_25_the_report_is_kept_beside_its_chainspec_once() {
        let dir =
            std::env::temp_dir().join(format!("core-archive-verified-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let a = open(&format!("fs:{}", dir.display()), Access::Write).unwrap();
        let digest = "ab".repeat(32);
        assert_eq!(a.get_verified(&digest).unwrap(), None);
        assert!(
            a.put_verified(&digest, b"{\"verified\":true}")
                .unwrap()
                .created
        );
        assert!(
            !a.put_verified(&digest, b"{\"verified\":true}")
                .unwrap()
                .created
        );
        assert!(a.put_verified(&digest, b"{}").is_err(), "written once");
        assert_eq!(
            a.get_verified(&digest).unwrap().as_deref(),
            Some(b"{\"verified\":true}".as_slice())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
