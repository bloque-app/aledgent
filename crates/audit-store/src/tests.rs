// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The audit store's rules on disk (T-013-02, `REQ-AT-2`, `NFR-6`): rewrites and truncation are
//! refused or detected, a chain break is detected, reads by each key work, and the store keeps to
//! its own media and credential.

use super::*;
use ledger_core::account::Time;
use ledger_core::audit::{AuditAction, AuditClass, AuditOutcome, AuditSubject, Justification};
use ledger_core::authority::{AuthorityKind, AuthorityUsed};
use ledger_core::id::{AccountId, CredentialId, IdentityId, OperationId};
use ledger_core::state::Command;
use ledger_kernel::posting::OperationRef;

fn op(n: u8) -> OperationId {
    OperationId::from_ref(OperationRef([n; 16]))
}

fn who(n: u8) -> AuthorityUsed {
    AuthorityUsed {
        kind: AuthorityKind::Operator,
        grant: None,
        principal: IdentityId::mint(&op(n), 0),
        credential: CredentialId::mint(&op(n), 1),
        on_behalf_of: None,
    }
}

fn chain(n: u64) -> Vec<AuditRecord> {
    let mut out: Vec<AuditRecord> = Vec::new();
    let mut prev = [0u8; 32];
    for i in 0..n {
        let r = AuditRecord {
            audit_seq: i,
            prev_hash: prev,
            class: if i % 2 == 0 {
                AuditClass::ValueMoving
            } else {
                AuditClass::OperatorIntervention
            },
            action: AuditAction::Command(Command::Issue),
            authorities: vec![who((i % 3) as u8)],
            subject: vec![AuditSubject::Account(AccountId::mint(
                &op((i % 2) as u8),
                0,
            ))],
            operation_ref: op(100 + i as u8),
            at: i * 10 as Time,
            justification: (i % 2 == 1).then(|| Justification::new(b"why").unwrap()),
            outcome: AuditOutcome::Accepted,
        };
        prev = r.hash();
        out.push(r);
    }
    out
}

struct Dirs {
    _root: tempfile::TempDir,
    config: AuditStoreConfig,
}

fn dirs() -> Dirs {
    let root = tempfile::tempdir().unwrap();
    let book = root.path().join("book");
    fs::create_dir_all(&book).unwrap();
    let config = AuditStoreConfig {
        dir: root.path().join("audit"),
        book_dir: book,
        segment_records: 3,
    };
    Dirs {
        _root: root,
        config,
    }
}

fn cred() -> WriterCredential {
    WriterCredential::new(b"audit-writer-secret".to_vec())
}

fn filled(d: &Dirs, n: u64) -> FileAuditStore {
    FileAuditStore::init(&d.config, &cred()).unwrap();
    let mut s = FileAuditStore::open(&d.config, &cred()).unwrap();
    for r in chain(n) {
        s.append(&r).unwrap();
    }
    s
}

#[test]
fn req_at_2_records_survive_a_reopen_across_segments() {
    let d = dirs();
    let s = filled(&d, 8);
    let head = AuditSink::head(&s).unwrap();
    drop(s);
    // 8 records at 3 per segment: three files.
    assert!(segment_path(&d.config.dir, 2).exists());
    assert!(!segment_path(&d.config.dir, 3).exists());
    let s = FileAuditStore::read(&d.config.dir).unwrap();
    assert_eq!(s.len(), 8);
    assert_eq!(AuditSink::head(&s).unwrap(), head);
    assert_eq!(s.get(5), Some(&chain(8)[5]));
}

#[test]
fn req_at_2_a_rewrite_is_refused_and_a_repeat_is_taken_once() {
    let d = dirs();
    let mut s = filled(&d, 4);
    let c = chain(4);
    // At least once: the same record again changes nothing.
    assert_eq!(s.append(&c[2]), Ok(()));
    assert_eq!(s.len(), 4);
    let mut other = c[2].clone();
    other.at += 1;
    assert_eq!(
        s.append(&other),
        Err(SinkError::Refused(TrailError::Rewrite { seq: 2 }))
    );
    let mut far = chain(6)[5].clone();
    far.audit_seq = 6;
    assert!(matches!(
        s.append(&far),
        Err(SinkError::Refused(TrailError::Gap { .. }))
    ));
    drop(s);
    assert_eq!(FileAuditStore::read(&d.config.dir).unwrap().len(), 4);
}

#[test]
fn req_at_2_truncation_is_detected() {
    let d = dirs();
    drop(filled(&d, 5));
    // Cut the last frame off the last segment.
    let last = segment_path(&d.config.dir, 1);
    let bytes = fs::read(&last).unwrap();
    let first_frame = 8 + u64::from_be_bytes(bytes[..8].try_into().unwrap()) as usize + 32;
    fs::write(&last, &bytes[..first_frame]).unwrap();
    assert_eq!(
        FileAuditStore::read(&d.config.dir).unwrap_err(),
        AuditStoreError::Truncated { head: 4, found: 4 }
    );
    // Removing a whole segment is caught the same way.
    let d = dirs();
    drop(filled(&d, 5));
    fs::remove_file(segment_path(&d.config.dir, 1)).unwrap();
    assert_eq!(
        FileAuditStore::read(&d.config.dir).unwrap_err(),
        AuditStoreError::Truncated { head: 4, found: 3 }
    );
    // And removing `HEAD` with it is not a clean store either.
    fs::remove_file(d.config.dir.join(HEAD_FILE)).unwrap();
    assert!(matches!(
        FileAuditStore::read(&d.config.dir).unwrap_err(),
        AuditStoreError::Corrupt { .. }
    ));
}

#[test]
fn req_at_2_a_chain_break_is_detected() {
    let d = dirs();
    drop(filled(&d, 5));
    // Change one byte of a record's body: its stored hash no longer matches.
    let seg = segment_path(&d.config.dir, 0);
    let mut bytes = fs::read(&seg).unwrap();
    let at = 8 + 20;
    bytes[at] ^= 0x01;
    fs::write(&seg, &bytes).unwrap();
    assert!(matches!(
        FileAuditStore::read(&d.config.dir).unwrap_err(),
        AuditStoreError::Corrupt { .. } | AuditStoreError::Chain(_)
    ));

    // Replace a whole record, with a hash that matches it: the next link breaks.
    let d = dirs();
    drop(filled(&d, 3));
    let seg = segment_path(&d.config.dir, 0);
    let mut forged = chain(3);
    forged[1].at = 999;
    let bytes: Vec<u8> = forged.iter().flat_map(frame).collect();
    fs::write(&seg, bytes).unwrap();
    assert!(matches!(
        FileAuditStore::read(&d.config.dir).unwrap_err(),
        AuditStoreError::Chain(TrailError::ChainBreak { seq: 2 })
    ));
}

#[test]
fn nfr_6_reads_by_subject_principal_class_and_time() {
    let d = dirs();
    drop(filled(&d, 6));
    let s = FileAuditStore::read(&d.config.dir).unwrap();
    let seqs = |q: AuditQuery| s.query(&q).iter().map(|r| r.audit_seq).collect::<Vec<_>>();
    assert_eq!(
        seqs(AuditQuery {
            class: Some(AuditClass::OperatorIntervention),
            ..AuditQuery::default()
        }),
        vec![1, 3, 5]
    );
    assert_eq!(
        seqs(AuditQuery {
            subject: Some(AuditSubject::Account(AccountId::mint(&op(0), 0))),
            ..AuditQuery::default()
        }),
        vec![0, 2, 4]
    );
    assert_eq!(
        seqs(AuditQuery {
            principal: Some(who(2).principal),
            ..AuditQuery::default()
        }),
        vec![2, 5]
    );
    assert_eq!(
        seqs(AuditQuery {
            from: Some(10),
            until: Some(30),
            ..AuditQuery::default()
        }),
        vec![1, 2]
    );
}

#[test]
fn req_at_2_the_core_ships_here_and_a_lost_record_raises_an_incident() {
    use ledger_core::Ledger;
    use ledger_core::asset::AssetKind;
    use ledger_core::identity::IdentityKind;
    use ledger_core::ledger::auditing::AuditGap;
    use ledger_core::ledger::{AssetDecl, Ctx, Genesis};
    use ledger_core::lifecycle::LifecycleConfig;
    use ledger_core::operator::IncidentKind;
    use ledger_core::principal::{Caller, Verifier};
    use ledger_kernel::book::Book;
    use ledger_kernel::keys::KeyRing;
    use ledger_kernel::store::MemStore;
    use ledger_kernel::vault::MemVault;

    let book = Book::bootstrap(
        MemStore::new(),
        MemVault::new(),
        KeyRing::from_root([2; 32]),
    )
    .unwrap();
    let genesis = Genesis {
        assets: vec![AssetDecl {
            id: ledger_core::asset::AssetId::parse("USD").unwrap(),
            location: Some(ledger_core::asset::AssetLocation::parse("Here(101)").unwrap()),
            precision: 2,
            kind: AssetKind::Fiat,
            deprecated: false,
        }],
        operator_verifier: Verifier::new(vec![1]),
        lifecycle: LifecycleConfig::new([2; 16], 60, 3_600),
        chainspec: None,
        spec: None,
    };
    let (mut l, founded) = Ledger::genesis(book, genesis, op(0), 1).unwrap();
    let operator = Caller::new(founded.operator, founded.operator_credential);
    for n in 1..4u8 {
        let ctx = Ctx {
            caller: operator,
            op: op(n),
            now: u64::from(n) + 1,
        };
        l.open_identity(&ctx, IdentityKind::Person, None, Verifier::new(vec![n]))
            .unwrap();
    }
    let d = dirs();
    FileAuditStore::init(&d.config, &cred()).unwrap();
    let mut s = FileAuditStore::open(&d.config, &cred()).unwrap();
    assert_eq!(l.ship_audit(&mut s), Ok(4));
    assert_eq!(l.check_audit_head(&s, 10), Ok(()));
    // Shipping again sends nothing: the outbox is empty.
    assert_eq!(l.ship_audit(&mut s), Ok(0));
    drop(s);
    let reopened = FileAuditStore::read(&d.config.dir).unwrap();
    assert_eq!(reopened.len(), 4);
    assert_eq!(l.check_audit_head(&reopened, 11), Ok(()));

    // A store that lost its last record, and passes its own checks, is behind the core.
    let d2 = dirs();
    FileAuditStore::init(&d2.config, &cred()).unwrap();
    let mut short = FileAuditStore::open(&d2.config, &cred()).unwrap();
    for r in reopened.query(&AuditQuery::default()).into_iter().take(3) {
        short.append(r).unwrap();
    }
    assert_eq!(
        l.check_audit_head(&short, 12),
        Err(AuditGap::Behind {
            store: Some(2),
            acknowledged: 3
        })
    );
    assert!(
        l.state()
            .ops()
            .incidents()
            .any(|i| i.kind == IncidentKind::AuditGap)
    );
}

#[test]
fn req_at_2_own_media_and_own_credential() {
    let d = dirs();
    // Inside the book's directory: refused.
    let inside = AuditStoreConfig {
        dir: d.config.book_dir.join("audit"),
        ..d.config.clone()
    };
    assert_eq!(
        FileAuditStore::init(&inside, &cred()),
        Err(AuditStoreError::SameMedia)
    );
    FileAuditStore::init(&d.config, &cred()).unwrap();
    assert_eq!(
        FileAuditStore::init(&d.config, &cred()),
        Err(AuditStoreError::Initialisation)
    );
    // The wrong credential cannot open it to write.
    assert_eq!(
        FileAuditStore::open(&d.config, &WriterCredential::new(b"guess".to_vec())).unwrap_err(),
        AuditStoreError::Credential
    );
    // A reader cannot append.
    let mut r = FileAuditStore::read(&d.config.dir).unwrap();
    assert_eq!(r.append(&chain(1)[0]), Err(SinkError::Unavailable));
    // The credential is not stored, only its hash.
    let stored = fs::read(d.config.dir.join(CREDENTIAL_FILE)).unwrap();
    assert!(!stored.windows(6).any(|w| w == b"secret"));
    assert!(!format!("{:?}", cred()).contains("secret"));
}

/// `T-013-13`, `REQ-AT-2`: the shipper's writer takes each record once and in order, whatever is
/// offered again; a rewrite, a gap and a broken link are refused; what it wrote reads back
/// through the reader's checks; and it survives a reopen, recent and old records alike.
#[test]
fn req_at_2_the_appender_takes_each_record_once_in_order_and_survives_a_reopen() {
    let d = dirs();
    let c = chain(10);
    // An empty directory is initialised on first open.
    let mut a = AuditAppender::open_with_window(&d.config, &cred(), 3).unwrap();
    assert_eq!(AuditSink::head(&a).unwrap(), None);
    a.append_batch(&c[..4]).unwrap();
    // Offered again with the next ones: the known prefix is accepted, the rest appended once.
    a.append_batch(&c[2..6]).unwrap();
    a.append(&c[5]).unwrap();
    assert_eq!(AuditSink::head(&a).unwrap(), Some((5, c[5].hash())));
    // A rewrite (recent, then old, past the window of 3), a gap and a broken link.
    let mut other = c[4].clone();
    other.at += 1;
    assert_eq!(
        a.append(&other),
        Err(SinkError::Refused(TrailError::Rewrite { seq: 4 }))
    );
    let mut old = c[1].clone();
    old.at += 1;
    assert_eq!(
        a.append(&old),
        Err(SinkError::Refused(TrailError::Rewrite { seq: 1 }))
    );
    assert_eq!(a.append(&c[1]), Ok(()));
    assert_eq!(
        a.append(&c[7]),
        Err(SinkError::Refused(TrailError::Gap {
            expected: 6,
            got: 7
        }))
    );
    let mut broken = c[6].clone();
    broken.prev_hash = [9; 32];
    assert_eq!(
        a.append(&broken),
        Err(SinkError::Refused(TrailError::ChainBreak { seq: 6 }))
    );
    drop(a);
    // A restart: the head is read back from the segments; the rest goes on from there.
    let mut a = AuditAppender::open_with_window(&d.config, &cred(), 3).unwrap();
    assert_eq!(AuditSink::head(&a).unwrap(), Some((5, c[5].hash())));
    a.append_batch(&c[3..10]).unwrap();
    drop(a);
    let s = FileAuditStore::read(&d.config.dir).unwrap();
    assert_eq!(s.len(), 10);
    let held: Vec<AuditRecord> = s
        .query(&AuditQuery::default())
        .into_iter()
        .cloned()
        .collect();
    assert_eq!(held, c);
    // Its own credential, as the store's.
    assert_eq!(
        AuditAppender::open(&d.config, &WriterCredential::new(b"guess".to_vec())).unwrap_err(),
        AuditStoreError::Credential
    );
}

/// `REQ-AT-2`: a crash between a batch's frames and its `HEAD` leaves records past `HEAD` that the
/// batch's pending mark accounts for, and the store opens; records past it, or a pending mark
/// that names another record, were written outside the store.
#[test]
fn req_at_2_a_batch_cut_short_by_a_crash_opens_and_anything_more_is_refused() {
    let d = dirs();
    let c = chain(6);
    let mut a = AuditAppender::open(&d.config, &cred()).unwrap();
    a.append_batch(&c[..2]).unwrap();
    drop(a);
    // The batch of 2..5 crashed after its frames: its intent is on disk, `HEAD` is not.
    write_mark(&d.config.dir, PENDING_FILE, (4, c[4].hash())).unwrap();
    let mut f = OpenOptions::new()
        .append(true)
        .open(segment_path(&d.config.dir, 0))
        .unwrap();
    f.write_all(&frame(&c[2])).unwrap();
    drop(f);
    let mut f = OpenOptions::new()
        .append(true)
        .create(true)
        .open(segment_path(&d.config.dir, 1))
        .unwrap();
    for r in &c[3..5] {
        f.write_all(&frame(r)).unwrap();
    }
    drop(f);
    let mut a = AuditAppender::open(&d.config, &cred()).unwrap();
    assert_eq!(AuditSink::head(&a).unwrap(), Some((4, c[4].hash())));
    // The shipper's retry of the whole batch appends only what is missing.
    a.append_batch(&c[2..6]).unwrap();
    drop(a);
    assert_eq!(FileAuditStore::read(&d.config.dir).unwrap().len(), 6);

    // Without the pending mark, two records past `HEAD` were appended outside the store.
    let d = dirs();
    let mut a = AuditAppender::open(&d.config, &cred()).unwrap();
    a.append_batch(&c[..2]).unwrap();
    drop(a);
    let mut f = OpenOptions::new()
        .append(true)
        .open(segment_path(&d.config.dir, 0))
        .unwrap();
    f.write_all(&frame(&c[2])).unwrap();
    drop(f);
    let mut f = OpenOptions::new()
        .append(true)
        .create(true)
        .open(segment_path(&d.config.dir, 1))
        .unwrap();
    f.write_all(&frame(&c[3])).unwrap();
    drop(f);
    assert_eq!(
        AuditAppender::open(&d.config, &cred()).unwrap_err(),
        AuditStoreError::Unexpected { head: 1, found: 4 }
    );
    // A pending mark that names another record does not excuse them.
    write_mark(&d.config.dir, PENDING_FILE, (3, [7; 32])).unwrap();
    assert_eq!(
        FileAuditStore::read(&d.config.dir).unwrap_err(),
        AuditStoreError::Unexpected { head: 1, found: 4 }
    );
}

/// `REQ-AT-2`, `T-013-13`: a batch whose write failed after some of its frames reached the disk is
/// retried without writing them twice: the writer reads the segments again first.
#[test]
fn req_at_2_a_batch_retried_after_a_failed_write_is_not_written_twice() {
    let d = dirs();
    let c = chain(7);
    let mut a = AuditAppender::open(&d.config, &cred()).unwrap();
    a.append_batch(&c[..2]).unwrap();
    // The batch 2..7 fails after its intent and its first segment's frames (seq 2): `HEAD` is
    // left at 1, and the writer is told the write failed.
    write_mark(&d.config.dir, PENDING_FILE, (6, c[6].hash())).unwrap();
    let mut f = OpenOptions::new()
        .append(true)
        .open(segment_path(&d.config.dir, 0))
        .unwrap();
    f.write_all(&frame(&c[2])).unwrap();
    drop(f);
    a.stale = true;
    a.append_batch(&c[2..7]).unwrap();
    drop(a);
    let s = FileAuditStore::read(&d.config.dir).unwrap();
    let held: Vec<AuditRecord> = s
        .query(&AuditQuery::default())
        .into_iter()
        .cloned()
        .collect();
    assert_eq!(held, c);
}
