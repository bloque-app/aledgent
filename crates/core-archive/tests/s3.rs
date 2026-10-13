// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The archive's S3 sink against a real S3 API (`T-005-15`): write once, Object Lock and KMS on
//! every object, read back, listed. CI runs it against MinIO with Object Lock and a KMS key
//! (`.github/workflows/ci.yml`, job `archive-s3`); elsewhere it is skipped unless
//! `ALEDGENT_TEST_S3` names a bucket (an `s3://` spec with `kms-key-id`, `lock-mode` and
//! `retain-days`), with credentials in the environment.

use core_archive::archive::{ArchiveSink as _, ArchiveSource as _, parse_s3};
use core_archive::s3::{CredentialSource, S3Store};
use core_archive::store::{ObjectStore as _, Put, StoreError};
use core_archive::{Access, Archive};

fn spec() -> Option<String> {
    match std::env::var("ALEDGENT_TEST_S3") {
        Ok(s) if !s.is_empty() => Some(s),
        _ => {
            assert!(
                std::env::var("ALEDGENT_REQUIRE_S3_TEST").is_err(),
                "ALEDGENT_TEST_S3 is required in this job"
            );
            eprintln!("skipped: ALEDGENT_TEST_S3 is not set");
            None
        }
    }
}

#[test]
fn t_005_15_the_s3_sink_writes_once_encrypted_and_locked() {
    let Some(spec) = spec() else {
        return;
    };
    let rest = spec.strip_prefix("s3://").expect("an s3:// spec");
    let mut config = parse_s3(rest).unwrap();
    config
        .check_writer()
        .expect("the test bucket is written as production is");
    // A fresh prefix per run: the objects are locked and stay.
    let run = format!(
        "{}run-{}-{}/",
        config.prefix,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    config.prefix = run;
    let store = S3Store::new(config.clone(), CredentialSource::Default);
    assert_eq!(
        store.put_once("segments/a.seg", b"one"),
        Ok(Put { created: true })
    );
    assert_eq!(
        store.put_once("segments/a.seg", b"one"),
        Ok(Put { created: false })
    );
    assert_eq!(
        store.put_once("segments/a.seg", b"two"),
        Err(StoreError::Conflict("segments/a.seg".into()))
    );
    assert_eq!(
        store.get("segments/a.seg", 3).unwrap().as_deref(),
        Some(&b"one"[..])
    );
    assert!(matches!(
        store.get("segments/a.seg", 2),
        Err(StoreError::TooLarge(_))
    ));
    assert_eq!(store.get("segments/b.seg", 3).unwrap(), None);
    let p = store
        .protection("segments/a.seg")
        .unwrap()
        .expect("present");
    assert_eq!(p.encryption.as_deref(), Some("aws:kms"), "{p:?}");
    assert!(p.lock_mode.is_some() && p.retain_until.is_some(), "{p:?}");
    // The archive's records through the sink and source, paged listing included.
    let archive = Archive::new(Box::new(S3Store::new(
        config.clone(),
        CredentialSource::Default,
    )));
    for n in 1..=3 {
        archive
            .put_segment(n, format!("record {n}").as_bytes())
            .unwrap();
    }
    archive.put_checkpoint(2, b"state").unwrap();
    assert_eq!(archive.segments().unwrap(), vec![1, 2, 3]);
    assert_eq!(archive.checkpoints().unwrap(), vec![2]);
    assert_eq!(
        archive.get_segment(2).unwrap().as_deref(),
        Some(&b"record 2"[..])
    );
    // A reader needs no protections of its own; a writer without them is refused.
    let mut bare = config.clone();
    bare.kms_key_id = None;
    assert!(bare.check_writer().is_err());
    let read_spec = format!(
        "s3://{}/{}?region={}",
        config.bucket, config.prefix, config.region
    );
    assert!(core_archive::open(&read_spec, Access::Write).is_err());
}
