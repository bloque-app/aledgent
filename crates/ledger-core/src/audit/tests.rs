// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The record's encoding and hash, and the trail's rules (T-013-01, T-013-02).

use super::*;
use alloc::vec;

fn op(n: u8) -> OperationId {
    OperationId::from_ref(OperationRef([n; 16]))
}

fn who(n: u8) -> AuthorityUsed {
    AuthorityUsed {
        kind: AuthorityKind::Operator,
        grant: Some(GrantId::mint(&op(n), 0)),
        principal: IdentityId::mint(&op(n), 1),
        credential: CredentialId::mint(&op(n), 2),
        on_behalf_of: Some(IdentityId::mint(&op(n.wrapping_add(1)), 0)),
    }
}

fn record(seq: u64, prev: [u8; 32]) -> AuditRecord {
    AuditRecord {
        audit_seq: seq,
        prev_hash: prev,
        class: AuditClass::OperatorIntervention,
        action: AuditAction::Command(Command::FreezeAccount),
        authorities: vec![who(1)],
        subject: vec![
            AuditSubject::Account(AccountId::mint(&op(3), 0)),
            AuditSubject::Operation(op(4)),
            AuditSubject::Asset(AssetId::parse("USD").unwrap()),
            AuditSubject::Posting(7),
            AuditSubject::Grant(GrantId::mint(&op(5), 0)),
            AuditSubject::Identity(IdentityId::mint(&op(6), 0)),
            AuditSubject::Review([4; 32]),
            AuditSubject::Evidence([5; 32]),
        ],
        operation_ref: op(9),
        at: 42,
        justification: Some(Justification::new(b"ticket 17").unwrap()),
        outcome: AuditOutcome::Accepted,
    }
}

fn chain(n: u64) -> Vec<AuditRecord> {
    let mut out: Vec<AuditRecord> = Vec::new();
    let mut prev = [0u8; 32];
    for i in 0..n {
        let r = record(i, prev);
        prev = r.hash();
        out.push(r);
    }
    out
}

#[test]
fn req_at_1_a_record_reads_back_as_itself() {
    let mut r = record(3, [9; 32]);
    assert_eq!(AuditRecord::decode(&r.encode()), Some(r.clone()));
    r.action = AuditAction::Transition {
        from: State::Accepted,
        to: State::Settled,
        trigger: Trigger::ApproveReversal,
    };
    r.outcome = AuditOutcome::Refused(Refusal::ApprovalRequired);
    r.justification = None;
    assert_eq!(AuditRecord::decode(&r.encode()), Some(r.clone()));
    r.action = AuditAction::PrivilegedRead(ReadKind::Balances);
    assert_eq!(AuditRecord::decode(&r.encode()), Some(r.clone()));
    // Trailing bytes, or a truncated record, are not a record.
    let mut long = r.encode();
    long.push(0);
    assert_eq!(AuditRecord::decode(&long), None);
    let short = &r.encode()[..10];
    assert_eq!(AuditRecord::decode(short), None);
}

#[test]
fn req_at_1_every_command_has_a_class_and_a_code() {
    let mut codes: Vec<u8> = COMMANDS.iter().map(|c| command_code(*c)).collect();
    codes.sort_unstable();
    codes.dedup();
    assert_eq!(codes.len(), COMMANDS.len());
    for c in COMMANDS {
        let _ = AuditClass::of(c);
    }
}

#[test]
fn err_justification_required_for_a_blank_justification() {
    assert_eq!(Justification::new(b""), Err(Refusal::JustificationRequired));
    assert_eq!(
        Justification::new(b" \t\n "),
        Err(Refusal::JustificationRequired)
    );
    assert!(Justification::new(b"x").is_ok());
    // The text never reaches a Debug line.
    let j = Justification::new(b"secret-ish text").unwrap();
    assert!(!alloc::format!("{j:?}").contains("secret"));
}

#[test]
fn req_at_2_the_trail_is_append_only_and_refuses_rewrites() {
    let mut t = AuditTrail::new();
    let c = chain(3);
    for r in &c {
        assert_eq!(t.append(r.clone()), Ok(true));
    }
    // At least once: the same record again is taken once.
    assert_eq!(t.append(c[1].clone()), Ok(false));
    assert_eq!(t.len(), 3);
    // Another record at a held number is a rewrite.
    let mut other = c[1].clone();
    other.at += 1;
    assert_eq!(t.append(other), Err(TrailError::Rewrite { seq: 1 }));
    // A gap is refused.
    let mut far = record(5, t.head().unwrap().1);
    assert_eq!(
        t.append(far.clone()),
        Err(TrailError::Gap {
            expected: 3,
            got: 5
        })
    );
    // A record that does not carry the head's hash is refused.
    far.audit_seq = 3;
    far.prev_hash = [1; 32];
    assert_eq!(t.append(far), Err(TrailError::ChainBreak { seq: 3 }));
    assert_eq!(t.verify(), Ok(t.head()));
}

#[test]
fn req_at_2_a_chain_break_is_detected() {
    let mut c = chain(5);
    assert!(AuditTrail::verify_chain(&c).is_ok());
    // A changed record breaks the next link.
    c[2].at = 1_000;
    assert_eq!(
        AuditTrail::verify_chain(&c),
        Err(TrailError::ChainBreak { seq: 3 })
    );
    // A removed record is a gap.
    let mut c = chain(5);
    c.remove(1);
    assert_eq!(
        AuditTrail::verify_chain(&c),
        Err(TrailError::Gap {
            expected: 1,
            got: 2
        })
    );
}

#[test]
fn req_at_2_reads_by_subject_principal_class_and_time() {
    let mut t = AuditTrail::new();
    let mut prev = [0u8; 32];
    for i in 0..6u64 {
        let mut r = record(i, prev);
        r.at = i * 10;
        r.class = if i % 2 == 0 {
            AuditClass::ValueMoving
        } else {
            AuditClass::OperatorIntervention
        };
        r.authorities = vec![who(i as u8 % 3)];
        r.subject = vec![AuditSubject::Account(AccountId::mint(&op(i as u8 % 2), 0))];
        prev = r.hash();
        t.append(r).unwrap();
    }
    let q = |q: AuditQuery| t.query(&q).iter().map(|r| r.audit_seq).collect::<Vec<_>>();
    assert_eq!(
        q(AuditQuery {
            class: Some(AuditClass::ValueMoving),
            ..AuditQuery::default()
        }),
        vec![0, 2, 4]
    );
    assert_eq!(
        q(AuditQuery {
            subject: Some(AuditSubject::Account(AccountId::mint(&op(1), 0))),
            ..AuditQuery::default()
        }),
        vec![1, 3, 5]
    );
    assert_eq!(
        q(AuditQuery {
            principal: Some(who(0).principal),
            ..AuditQuery::default()
        }),
        vec![0, 3]
    );
    assert_eq!(
        q(AuditQuery {
            from: Some(20),
            until: Some(40),
            ..AuditQuery::default()
        }),
        vec![2, 3]
    );
}
