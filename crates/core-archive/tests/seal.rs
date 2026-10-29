// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The archive's seal over a directory store (`F-044`): every object sealed, opened with Vault
//! Transit (a stand-in here; `tests/transit.rs` runs a real Vault) or with the offline recovery key
//! alone, write-once kept, tampering refused, the writer's keyless read-back, rotation, objects
//! written before the seal, and the copy into a sealed archive.

mod common;

use std::sync::atomic::Ordering;

use common::{LocalTransit, recipients, seal_json, transit_keys};
use core_archive::age::{self, Identity};
use core_archive::archive::{Archive, ArchiveSink as _, ArchiveSource as _};
use core_archive::record::{
    CheckpointHeader, FORMAT, Point, SegmentHeader, Span, encode_checkpoint, encode_segment,
    sha256_hex,
};
use core_archive::seal::{self, Keys, MAGIC_LINE, Sealed, Unsealed};
use core_archive::store::{FsStore, ObjectStore, Put, StoreError};
use core_archive::{Access, open_with};

fn point() -> Point {
    Point {
        inputs: 3,
        entries: 3,
        log_head: "00".repeat(32),
        kernel_root: "11".repeat(32),
        service_root: "22".repeat(32),
        audit: "33".repeat(32),
        at: 1,
        archive_bytes: 5,
        replies_bytes: 6,
    }
}

/// A segment record of `archive` and `replies` bytes.
fn segment(n: u64, archive: &[u8], replies: &[u8]) -> Vec<u8> {
    let header = SegmentHeader {
        format: FORMAT,
        book: "ab".repeat(32),
        n,
        prev: String::new(),
        archive: Span {
            start: 0,
            end: archive.len() as u64,
            sha256: sha256_hex(archive),
        },
        replies: Span {
            start: 0,
            end: replies.len() as u64,
            sha256: sha256_hex(replies),
        },
        cut: true,
        point: Some(point()),
        taken_at: 7,
    };
    encode_segment(&header, archive, replies)
}

fn sealed(dir: &std::path::Path, version: u32, recovery: &Identity, keys: Keys) -> Sealed {
    Sealed::new(
        Box::new(FsStore::open(dir).unwrap()),
        Some(recipients(version, recovery)),
        keys,
        Unsealed::Accept,
    )
}

fn reader(dir: &std::path::Path, keys: Keys) -> Sealed {
    Sealed::new(
        Box::new(FsStore::open(dir).unwrap()),
        None,
        keys,
        Unsealed::Accept,
    )
}

fn with_recovery(id: &Identity) -> Keys {
    Keys {
        transit: None,
        recovery: vec![Identity::parse(&id.to_bech32()).unwrap()],
    }
}

fn with_transit() -> (Keys, std::sync::Arc<LocalTransit>) {
    struct Shared(std::sync::Arc<LocalTransit>);
    impl seal::TransitUnwrap for Shared {
        fn unwrap(
            &self,
            key: &str,
            version: u32,
            wrapped: &[u8],
        ) -> Result<age::FileKey, StoreError> {
            self.0.unwrap(key, version, wrapped)
        }
    }
    let t = std::sync::Arc::new(LocalTransit::default());
    (
        Keys {
            transit: Some(Box::new(Shared(std::sync::Arc::clone(&t)))),
            recovery: Vec::new(),
        },
        t,
    )
}

/// What the store holds is sealed: no payload in it, and it opens with Transit or with the
/// recovery key alone, each on its own; a reader with neither is told what would open it.
#[test]
fn f_044_every_object_is_sealed_and_opens_with_either_key() {
    let tmp = tempfile::tempdir().unwrap();
    let recovery = Identity::generate();
    let w = sealed(tmp.path(), 1, &recovery, Keys::none());
    let payload = b"THE-LOG-ENTRIES-PAYLOAD";
    let rec = segment(1, payload, b"REPLIES-PAYLOAD\n");
    assert_eq!(
        w.put_once("segments/1.seg", &rec),
        Ok(Put { created: true })
    );
    let raw = std::fs::read(tmp.path().join("segments/1.seg")).unwrap();
    assert!(raw.starts_with(MAGIC_LINE));
    assert!(
        raw.windows(payload.len()).all(|w| w != payload),
        "a payload is never in clear"
    );
    // The writer opens what it sealed in this run, with no key of its own.
    assert_eq!(w.get("segments/1.seg", 1 << 20).unwrap().unwrap(), rec);

    let by_recovery = reader(tmp.path(), with_recovery(&recovery));
    assert_eq!(
        by_recovery.get("segments/1.seg", 1 << 20).unwrap().unwrap(),
        rec
    );
    let (keys, transit) = with_transit();
    let by_transit = reader(tmp.path(), keys);
    assert_eq!(
        by_transit.get("segments/1.seg", 1 << 20).unwrap().unwrap(),
        rec
    );
    assert_eq!(transit.calls.load(Ordering::Relaxed), 1);

    let none = reader(tmp.path(), Keys::none());
    let e = none.get("segments/1.seg", 1 << 20).unwrap_err();
    assert!(
        matches!(&e, StoreError::Sealed(m) if m.contains("Transit archive v1") && m.contains("X25519")),
        "{e}"
    );
    let stranger = reader(tmp.path(), with_recovery(&Identity::generate()));
    assert!(matches!(
        stranger.get("segments/1.seg", 1 << 20),
        Err(StoreError::Sealed(_))
    ));
    // The bound is the plaintext's.
    assert!(matches!(
        by_recovery.get("segments/1.seg", rec.len() as u64 - 1),
        Err(StoreError::TooLarge(_))
    ));
}

/// The age part is a stock age file: the recovery key opens it with nothing of the seal's
/// (stripping the seal line and the clear bytes is all a person with `age` has to do).
#[test]
fn f_044_the_recovery_key_opens_the_age_part_alone() {
    let tmp = tempfile::tempdir().unwrap();
    let recovery = Identity::generate();
    let w = sealed(tmp.path(), 1, &recovery, Keys::none());
    let rec = segment(1, b"entries", b"replies\n");
    w.put_once("segments/1.seg", &rec).unwrap();
    let raw = std::fs::read(tmp.path().join("segments/1.seg")).unwrap();
    let after_magic = &raw[MAGIC_LINE.len()..];
    let nl = after_magic.iter().position(|b| *b == b'\n').unwrap();
    let line: serde_json::Value = serde_json::from_slice(&after_magic[..nl]).unwrap();
    let clear = line["clear"].as_u64().unwrap() as usize;
    assert!(clear > 0, "a segment's record header is in clear");
    let age_file = &after_magic[nl + 1 + clear..];
    let (header, at) = age::parse_header(age_file).unwrap();
    let file_key = header
        .stanzas
        .iter()
        .find_map(|s| recovery.unwrap(s))
        .expect("an X25519 stanza for the recovery key");
    age::check_mac(&header, &file_key).unwrap();
    assert_eq!(
        age::decrypt_payload(&file_key, &age_file[at..], 1 << 20).unwrap(),
        rec
    );
}

/// Write once, with randomised sealing: the same record again is an exact retry, other bytes a
/// conflict; and through S3's path (the store compares bytes, then the seal compares lines).
#[test]
fn f_044_write_once_holds_under_randomised_sealing() {
    let tmp = tempfile::tempdir().unwrap();
    let recovery = Identity::generate();
    let rec = segment(1, b"one", b"r\n");
    let w = sealed(tmp.path(), 1, &recovery, Keys::none());
    assert!(w.put_once("segments/1.seg", &rec).unwrap().created);
    assert_eq!(
        w.put_once("segments/1.seg", &rec),
        Ok(Put { created: false })
    );
    // Another writer (another epoch, another file key) sees the same.
    let w2 = sealed(tmp.path(), 1, &recovery, Keys::none());
    assert_eq!(
        w2.put_once("segments/1.seg", &rec),
        Ok(Put { created: false })
    );
    assert_eq!(
        w2.put_once("segments/1.seg", &segment(1, b"two", b"r\n")),
        Err(StoreError::Conflict("segments/1.seg".into()))
    );
    // An object written before the seal is compared byte for byte.
    let plain = FsStore::open(tmp.path()).unwrap();
    plain.put_once("chainspec/aa.bin", b"spec").unwrap();
    assert_eq!(
        w2.put_once("chainspec/aa.bin", b"spec"),
        Ok(Put { created: false })
    );
    assert!(matches!(
        w2.put_once("chainspec/aa.bin", b"other"),
        Err(StoreError::Conflict(_))
    ));
}

/// The writer after a restart holds no key that opens the archive, and still reads what it needs:
/// a segment's header and digest, a checkpoint's header, a verify-genesis report.
#[test]
fn f_044_a_restarted_writer_reads_heads_with_no_key() {
    let tmp = tempfile::tempdir().unwrap();
    let recovery = Identity::generate();
    let rec = segment(1, b"entries", b"replies\n");
    let state = b"the agent's state";
    let ck = encode_checkpoint(
        &CheckpointHeader {
            format: FORMAT,
            book: "ab".repeat(32),
            n: 1,
            segment: sha256_hex(&rec),
            point: point(),
            bytes: state.len() as u64,
            sha256: sha256_hex(state),
            taken_at: 9,
        },
        state,
    );
    {
        let a = Archive::new(Box::new(sealed(tmp.path(), 1, &recovery, Keys::none())));
        a.put_segment(1, &rec).unwrap();
        a.put_checkpoint(1, &ck).unwrap();
        a.put_chainspec(&"cd".repeat(32), b"GENESIS-STATE").unwrap();
        a.put_verified(&"cd".repeat(32), b"{\"verified\":true}")
            .unwrap();
    }
    let a = Archive::new(Box::new(sealed(tmp.path(), 1, &recovery, Keys::none())));
    let (h, digest) = a.segment_head(1).unwrap().unwrap();
    assert_eq!((h.n, digest), (1, sha256_hex(&rec)));
    assert_eq!(a.checkpoint_head(1).unwrap().unwrap().taken_at, 9);
    assert_eq!(
        a.get_verified(&"cd".repeat(32)).unwrap().as_deref(),
        Some(&b"{\"verified\":true}"[..])
    );
    // What is not in clear stays sealed to it.
    assert!(matches!(a.get_segment(1), Err(StoreError::Sealed(_))));
    assert!(matches!(
        a.get_chainspec(&"cd".repeat(32)),
        Err(StoreError::Sealed(_))
    ));
    let raw = std::fs::read(
        tmp.path()
            .join(format!("chainspec/{}.bin", "cd".repeat(32))),
    )
    .unwrap();
    assert!(raw.windows(13).all(|w| w != b"GENESIS-STATE"));
}

/// Tampering is refused: a flipped byte, an object moved to another name, a seal line rewritten
/// to match, and a clear byte changed.
#[test]
fn f_044_tampering_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let recovery = Identity::generate();
    let rec = segment(1, b"entries", b"replies\n");
    sealed(tmp.path(), 1, &recovery, Keys::none())
        .put_once("segments/1.seg", &rec)
        .unwrap();
    let raw = std::fs::read(tmp.path().join("segments/1.seg")).unwrap();
    let r = reader(tmp.path(), with_recovery(&recovery));
    let try_open = |bytes: &[u8], name: &str| r.open(name, bytes, 1 << 20);
    assert_eq!(try_open(&raw, "segments/1.seg").unwrap(), rec);

    let mut flipped = raw.clone();
    let last = flipped.len() - 1;
    flipped[last] ^= 1;
    assert!(matches!(
        try_open(&flipped, "segments/1.seg"),
        Err(StoreError::Corrupt(_))
    ));
    assert!(matches!(
        try_open(&raw, "segments/2.seg"),
        Err(StoreError::Corrupt(m)) if m.contains("another name")
    ));
    let mut renamed = raw.clone();
    let at = raw
        .windows(14)
        .position(|w| w == b"segments/1.seg")
        .unwrap();
    renamed[at..at + 14].copy_from_slice(b"segments/2.seg");
    assert!(matches!(
        try_open(&renamed, "segments/2.seg"),
        Err(StoreError::Corrupt(m)) if m.contains("does not bind")
    ));
    // A byte of the clear header changed (its book).
    let mut clear = raw.clone();
    let at = raw.windows(4).position(|w| w == b"abab").unwrap();
    clear[at] = b'c';
    assert!(matches!(
        try_open(&clear, "segments/1.seg"),
        Err(StoreError::Corrupt(_))
    ));
}

/// Rotation: objects sealed to Transit v1 and v2 both open (each with its own version), a file
/// key serves an epoch (one Transit call for many objects), and the recovery key opens both.
#[test]
fn f_044_transit_versions_and_epochs() {
    let tmp = tempfile::tempdir().unwrap();
    let recovery = Identity::generate();
    let v1 = sealed(tmp.path(), 1, &recovery, Keys::none());
    for n in 1..=5u64 {
        v1.put_once(&format!("segments/{n}.seg"), &segment(n, b"a", b"b\n"))
            .unwrap();
    }
    let v2 = sealed(tmp.path(), 2, &recovery, Keys::none());
    v2.put_once("segments/6.seg", &segment(6, b"a", b"b\n"))
        .unwrap();
    let (keys, transit) = with_transit();
    let r = reader(tmp.path(), keys);
    for n in 1..=6u64 {
        assert!(
            r.get(&format!("segments/{n}.seg"), 1 << 20)
                .unwrap()
                .is_some()
        );
    }
    assert_eq!(
        transit.calls.load(Ordering::Relaxed),
        2,
        "one call per epoch, not per object"
    );
    // Vault without v1 (min_decryption_version raised): v1's objects no longer open by Transit.
    let gone = std::sync::Arc::new(LocalTransit {
        without: Some(1),
        ..LocalTransit::default()
    });
    struct Shared(std::sync::Arc<LocalTransit>);
    impl seal::TransitUnwrap for Shared {
        fn unwrap(&self, k: &str, v: u32, w: &[u8]) -> Result<age::FileKey, StoreError> {
            self.0.unwrap(k, v, w)
        }
    }
    let r = reader(
        tmp.path(),
        Keys {
            transit: Some(Box::new(Shared(gone))),
            recovery: Vec::new(),
        },
    );
    assert!(matches!(
        r.get("segments/1.seg", 1 << 20),
        Err(StoreError::Sealed(_))
    ));
    assert!(r.get("segments/6.seg", 1 << 20).unwrap().is_some());
    let r = reader(tmp.path(), with_recovery(&recovery));
    assert!(r.get("segments/1.seg", 1 << 20).unwrap().is_some());
}

/// Objects written before the seal read as they are, unless the archive refuses them.
#[test]
fn f_044_unsealed_objects_are_read_unless_refused() {
    let tmp = tempfile::tempdir().unwrap();
    FsStore::open(tmp.path())
        .unwrap()
        .put_once("segments/1.seg", b"old")
        .unwrap();
    assert_eq!(
        reader(tmp.path(), Keys::none())
            .get("segments/1.seg", 3)
            .unwrap()
            .as_deref(),
        Some(&b"old"[..])
    );
    let strict = Sealed::new(
        Box::new(FsStore::open(tmp.path()).unwrap()),
        None,
        Keys::none(),
        Unsealed::Refuse,
    );
    assert!(matches!(
        strict.get("segments/1.seg", 3),
        Err(StoreError::Corrupt(_))
    ));
}

/// The copy of an old archive into a sealed one: every object under the same name, sealed, and
/// decrypted back. Run again with no reader key, nothing is written and nothing claims to be
/// verified; with the recovery key, every object is verified.
#[test]
fn f_044_an_old_archive_copies_into_a_sealed_one() {
    let tmp = tempfile::tempdir().unwrap();
    let (old, new) = (tmp.path().join("old"), tmp.path().join("new"));
    let o = FsStore::open(&old).unwrap();
    for n in 1..=3u64 {
        o.put_once(&format!("segments/{n:016x}.seg"), &segment(n, b"x", b"y\n"))
            .unwrap();
    }
    o.put_once("chainspec/aa.bin", b"spec").unwrap();
    let recovery = Identity::generate();
    let prefixes = ["segments/", "checkpoints/", "chainspec/"];
    let copied = seal::copy(
        &o,
        &sealed(&new, 1, &recovery, Keys::none()),
        &prefixes,
        1 << 20,
    )
    .unwrap();
    assert_eq!((copied.created, copied.present), (4, 0));
    assert!(copied.unverified.is_empty());
    let keyless = seal::copy(
        &o,
        &sealed(&new, 1, &recovery, Keys::none()),
        &prefixes,
        1 << 20,
    )
    .unwrap();
    assert_eq!((keyless.created, keyless.present), (0, 0));
    assert_eq!(keyless.unverified.len(), 4, "no key: none claimed verified");
    let keyed = seal::copy(
        &o,
        &sealed(&new, 1, &recovery, with_recovery(&recovery)),
        &prefixes,
        1 << 20,
    )
    .unwrap();
    assert_eq!(
        (keyed.created, keyed.present, keyed.unverified.len()),
        (0, 4, 0)
    );
    let r = reader(&new, with_recovery(&recovery));
    for name in o.list("segments/").unwrap() {
        assert_eq!(
            r.get(&name, 1 << 20).unwrap(),
            o.get(&name, 1 << 20).unwrap()
        );
        assert!(
            std::fs::read(new.join(&name))
                .unwrap()
                .starts_with(MAGIC_LINE)
        );
    }

    // A copy damaged after it was written (its seal line intact): the verifying run fails.
    let victim = new.join("chainspec/aa.bin");
    let mut raw = std::fs::read(&victim).unwrap();
    let last = raw.len() - 1;
    raw[last] ^= 1;
    std::fs::remove_file(&victim).unwrap();
    std::fs::write(&victim, &raw).unwrap();
    assert!(matches!(
        seal::copy(
            &o,
            &sealed(&new, 1, &recovery, with_recovery(&recovery)),
            &prefixes,
            1 << 20
        ),
        Err(StoreError::Corrupt(_))
    ));

    // A sealed source is refused, not sealed twice; read through a reader, it copies.
    let again = tmp.path().join("again");
    assert!(matches!(
        seal::copy(
            &FsStore::open(&new).unwrap(),
            &sealed(&again, 1, &recovery, Keys::none()),
            &["segments/"],
            1 << 20
        ),
        Err(StoreError::Corrupt(m)) if m.contains("sealed already")
    ));
    let through = seal::copy(
        &reader(&new, with_recovery(&recovery)),
        &sealed(&again, 1, &recovery, Keys::none()),
        &["segments/"],
        1 << 20,
    )
    .unwrap();
    assert_eq!(through.created, 3);
}

/// A store that hands back what it holds with its last byte flipped.
struct Damaging(FsStore);

impl ObjectStore for Damaging {
    fn put_once(&self, name: &str, bytes: &[u8]) -> Result<Put, StoreError> {
        self.0.put_once(name, bytes)
    }
    fn get(&self, name: &str, max: u64) -> Result<Option<Vec<u8>>, StoreError> {
        Ok(self.0.get(name, max)?.map(|mut b| {
            if let Some(x) = b.last_mut() {
                *x ^= 1;
            }
            b
        }))
    }
    fn list(&self, prefix: &str) -> Result<Vec<String>, StoreError> {
        self.0.list(prefix)
    }
    fn describe(&self) -> String {
        self.0.describe()
    }
}

/// A copy whose read-back does not decrypt is an error in the run that sealed it, never a count.
#[test]
fn f_044_a_copy_that_does_not_read_back_fails_the_run() {
    let tmp = tempfile::tempdir().unwrap();
    let o = FsStore::open(tmp.path().join("old")).unwrap();
    o.put_once("segments/a.seg", b"record").unwrap();
    let recovery = Identity::generate();
    let to = Sealed::new(
        Box::new(Damaging(FsStore::open(tmp.path().join("new")).unwrap())),
        Some(recipients(1, &recovery)),
        Keys::none(),
        Unsealed::Accept,
    );
    assert!(matches!(
        seal::copy(&o, &to, &["segments/"], 1 << 20),
        Err(StoreError::Corrupt(_))
    ));
}

/// The spec: `seal=` makes a writer seal (a reader ignores it), the seal file is checked, and the
/// seal's options are refused where they do not belong.
#[test]
fn f_044_the_spec_names_the_seal() {
    let tmp = tempfile::tempdir().unwrap();
    let recovery = Identity::generate();
    let file = tmp.path().join("seal.json");
    std::fs::write(
        &file,
        seal_json("archive", 1, &transit_keys()[0], &[&recovery]),
    )
    .unwrap();
    let dir = tmp.path().join("archive");
    let spec = format!("fs:{}?seal={}", dir.display(), file.display());
    let w = open_with(&spec, Access::Write, Keys::none()).unwrap();
    assert!(
        w.describe()
            .contains("sealed: Transit archive v1, 1 recovery key")
    );
    w.put_segment(1, &segment(1, b"a", b"b\n")).unwrap();
    assert!(
        std::fs::read(dir.join(core_archive::record::segment_name(1)))
            .unwrap()
            .starts_with(MAGIC_LINE)
    );
    let r = open_with(&spec, Access::Read, with_recovery(&recovery)).unwrap();
    assert_eq!(r.segments().unwrap(), vec![1]);
    assert!(r.get_segment(1).unwrap().is_some());

    // A seal file without a recovery key, with a short RSA key, or with unknown fields: refused.
    let no_recovery = seal_json("archive", 1, &transit_keys()[0], &[]);
    std::fs::write(&file, no_recovery).unwrap();
    assert!(open_with(&spec, Access::Write, Keys::none()).is_err());
    let short = rsa::RsaPrivateKey::new(&mut rand_core::OsRng, 2048).unwrap();
    std::fs::write(&file, seal_json("archive", 1, &short, &[&recovery])).unwrap();
    let e = open_with(&spec, Access::Write, Keys::none()).err().unwrap();
    assert!(e.contains("2048 bits"), "{e}");
    assert!(
        open_with(&format!("{spec}&region=x"), Access::Read, Keys::none()).is_err(),
        "fs: takes the seal's options only"
    );
    assert!(
        open_with(
            &format!("fs:{}?vault=https://v.example", dir.display()),
            Access::Read,
            Keys::none()
        )
        .is_err(),
        "vault= needs transit-mount="
    );
    assert!(
        open_with(
            &format!("fs:{}?unsealed=maybe", dir.display()),
            Access::Read,
            Keys::none()
        )
        .is_err()
    );
    // S3: a writer with the seal and no KMS key is accepted; with neither, refused.
    std::fs::write(
        &file,
        seal_json("archive", 1, &transit_keys()[0], &[&recovery]),
    )
    .unwrap();
    assert!(
        open_with(
            &format!("s3://b/p?region=us-east-2&seal={}", file.display()),
            Access::Write,
            Keys::none()
        )
        .is_ok()
    );
    assert!(open_with("s3://b/p?region=us-east-2", Access::Write, Keys::none()).is_err());
    // A Vault address that would carry a token in clear is refused.
    assert!(
        core_archive::seal::VaultTransit::new(
            "http://vault.example",
            "transit",
            core_archive::seal::TokenSource::Fixed(Default::default())
        )
        .is_err()
    );
}

/// The age part of a sealed object, as a person with the stock `age` tool cuts it: after the seal
/// line and the clear bytes the seal line counts.
fn age_part(raw: &[u8]) -> &[u8] {
    let after = &raw[MAGIC_LINE.len()..];
    let nl = after.iter().position(|b| *b == b'\n').unwrap();
    let line: serde_json::Value = serde_json::from_slice(&after[..nl]).unwrap();
    &after[nl + 1 + line["clear"].as_u64().unwrap() as usize..]
}

/// Interoperability with the reference implementation (rage, the `age` crate), both ways: it opens
/// what the seal writes with the recovery identity, ignoring the seal's own stanzas; and the seal's
/// age code opens what it writes, across several payload chunks.
#[test]
fn f_044_stock_age_opens_the_seal_and_the_seal_opens_stock_age() {
    use std::io::{Read as _, Write as _};
    let tmp = tempfile::tempdir().unwrap();
    let recovery = Identity::generate();
    let rec: Vec<u8> = (0..200_000u32).map(|i| (i % 253) as u8).collect();
    sealed(tmp.path(), 1, &recovery, Keys::none())
        .put_once("checkpoints/1.ckpt", &rec)
        .unwrap();
    let raw = std::fs::read(tmp.path().join("checkpoints/1.ckpt")).unwrap();
    let theirs: ::age::x25519::Identity = recovery.to_bech32().parse().unwrap();
    let mut opened = Vec::new();
    ::age::Decryptor::new(age_part(&raw))
        .unwrap()
        .decrypt(std::iter::once(&theirs as &dyn ::age::Identity))
        .unwrap()
        .read_to_end(&mut opened)
        .unwrap();
    assert_eq!(opened, rec, "rage opens the seal's age part");

    let to: ::age::x25519::Recipient = recovery.recipient().to_bech32().parse().unwrap();
    let mut file = Vec::new();
    let mut w = ::age::Encryptor::with_recipients(std::iter::once(&to as &dyn ::age::Recipient))
        .unwrap()
        .wrap_output(&mut file)
        .unwrap();
    w.write_all(&rec).unwrap();
    w.finish().unwrap();
    let (header, at) = age::parse_header(&file).unwrap();
    let key = header
        .stanzas
        .iter()
        .find_map(|s| recovery.unwrap(s))
        .unwrap();
    age::check_mac(&header, &key).unwrap();
    assert_eq!(
        age::decrypt_payload(&key, &file[at..], rec.len() as u64).unwrap(),
        rec,
        "the seal's age code opens rage's file"
    );
}

/// A Vault address carries a token: HTTPS to a host, or HTTP to this host only, with its authority
/// parsed (no user information to smuggle another host past a prefix check).
#[test]
fn f_044_a_vault_address_is_parsed_not_prefix_matched() {
    let new = |a: &str| {
        core_archive::seal::VaultTransit::new(
            a,
            "transit",
            core_archive::seal::TokenSource::Fixed(Default::default()),
        )
    };
    for ok in [
        "https://vault.example",
        "https://vault.example:8200",
        "https://vault.example/",
        "http://127.0.0.1:8200",
        "http://localhost",
        "http://[::1]:8200",
    ] {
        assert!(new(ok).is_ok(), "{ok}");
    }
    for bad in [
        "http://vault.example",
        "http://localhost:80@evil.example/",
        "http://localhost@evil.example",
        "https://user@vault.example",
        "http://127.0.0.1.evil.example",
        "http://localhost:80x",
        "https://",
        "ftp://vault.example",
        "http://[::1",
        "https://vault.example:99999999",
    ] {
        assert!(new(bad).is_err(), "{bad}");
    }
}
