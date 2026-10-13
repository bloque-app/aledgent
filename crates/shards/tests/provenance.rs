// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! R-03 (`provenance`): a receiving shard applies only the credits its sender's committee certified
//! (`REQ-RT-13`, `REQ-RT-11`, `INV-18`). Whoever carries them, the relay included, cannot write one.
//! The receiver's rule in `enforce`; `report`, the deployed mode, is `tests/r03_report.rs`.

use ed25519_dalek::SigningKey;
use proptest::prelude::*;
use shards::provenance::{
    Accepted, AgentId, Certificate, Claim, Committee, Outbox, Receiver, Refused, Signature,
    StreamId, shard_agent,
};
use shards::{Account, Command, Credit, Directory, Outcome, ShardId, book, feed};

const USD: u32 = 840;
const PROGRAM: [u8; 32] = [0xA1; 32];

/// The sender's voters (three, quorum two) and the relay's own key.
struct Voters {
    keys: Vec<SigningKey>,
    committee: Committee,
}

fn voters(agent: AgentId, seed: u8) -> Voters {
    let keys: Vec<SigningKey> = (0..3)
        .map(|i| SigningKey::from_bytes(&[seed.wrapping_add(i); 32]))
        .collect();
    let committee = Committee::new(
        agent,
        keys.iter().map(|k| k.verifying_key().to_bytes()).collect(),
        2,
    )
    .expect("a committee");
    Voters { keys, committee }
}

impl Voters {
    /// The certificate of the sender's commit `commit`, signed by `signers`.
    fn certify(&self, commit: u64, outbox: &Outbox, signers: &[u16]) -> Certificate {
        let claim = Claim {
            agent: self.committee.agent(),
            commit,
            program: PROGRAM,
            committee: self.committee.id(),
            heads: outbox.heads(),
        };
        let signatures = signers
            .iter()
            .map(|i| Signature::sign(&self.keys[usize::from(*i)], *i, &self.committee, &claim))
            .collect();
        Certificate { claim, signatures }
    }
}

fn account(shard: ShardId, dir: &Directory, from: u32) -> Account {
    (from..)
        .map(|i| {
            let mut a = [0u8; 16];
            a[..4].copy_from_slice(&i.to_be_bytes());
            a
        })
        .find(|a| dir.shard_of(a) == shard)
        .expect("an account in the shard")
}

fn op(n: u64) -> [u8; 16] {
    let mut o = [0u8; 16];
    o[..8].copy_from_slice(&n.to_be_bytes());
    o[8] = 0x3E;
    o
}

fn bytes(c: &Credit) -> Vec<u8> {
    postcard::to_allocvec(c).expect("a credit encodes")
}

/// Two shards, a funded payer in shard 0 and a payee in shard 1; shard 0 has decided `n`
/// transfers of 10 to the payee, so its outbox to shard 1 holds `n` credits.
struct World {
    shards: Vec<shards::Shard>,
    payee: Account,
    credits: Vec<Credit>,
    stream: StreamId,
    outbox: Outbox,
    voters: Voters,
    receiver: Receiver,
}

fn world(n: u64) -> World {
    let dir = Directory::even(2);
    let mut shards = book(2);
    let payer = account(ShardId(0), &dir, 0);
    let payee = account(ShardId(1), &dir, 0);
    shards[0].execute(&[Command::Open {
        account: payer,
        asset: USD,
    }]);
    shards[1].execute(&[Command::Open {
        account: payee,
        asset: USD,
    }]);
    feed(&mut shards, &[(payer, USD), (payee, USD)]);
    shards[0].execute(&[Command::Issue {
        op: op(0),
        account: payer,
        asset: USD,
        amount: 1_000_000,
    }]);
    let transfers: Vec<Command> = (1..=n)
        .map(|i| Command::Transfer {
            op: op(i),
            payer,
            payee,
            asset: USD,
            amount: 10,
        })
        .collect();
    let out = shards[0].execute(&transfers);
    assert!(out.iter().all(|o| matches!(o, Outcome::Debited { .. })));
    let credits = shards[0].outbound(ShardId(1));
    assert_eq!(credits.len() as u64, n);

    // The sender's runtime chains each credit as it enqueues it, in the same commit.
    let stream = StreamId::credit(ShardId(0), ShardId(1));
    let mut outbox = Outbox::default();
    for c in &credits {
        assert_eq!(outbox.push(stream, &bytes(c)), c.seq);
    }
    let sender = shard_agent(ShardId(0));
    let voters = voters(sender, 1);
    let mut receiver = Receiver::new(PROGRAM);
    receiver.pin(voters.committee.clone());
    receiver.expect(stream, sender);
    World {
        shards,
        payee,
        credits,
        stream,
        outbox,
        voters,
        receiver,
    }
}

impl World {
    /// Deliver `credits` (numbered from `first`) with `cert`: shard 1 applies what the receiver
    /// lets through, and nothing if it refuses.
    fn deliver(
        &mut self,
        first: u64,
        credits: &[Credit],
        cert: &Certificate,
    ) -> Result<Accepted, Refused> {
        let encoded: Vec<Vec<u8>> = credits.iter().map(bytes).collect();
        let refs: Vec<&[u8]> = encoded.iter().map(Vec::as_slice).collect();
        let a = self.receiver.accept(&self.stream, first, &refs, cert)?;
        self.shards[1].apply_credits(ShardId(0), &credits[a.skip..a.skip + a.count]);
        Ok(a)
    }

    fn payee_balance(&self) -> i128 {
        self.shards[1].balance(&self.payee, USD).expect("open")
    }

    /// The value in transit from shard 0 to shard 1 according to both shards' positions, and
    /// according to the credits still unapplied: equal when nothing was forged (`NFR-9`, second
    /// axis; `REQ-RT-14`).
    fn in_transit_matches(&self) -> bool {
        let positions =
            self.shards[0].transit_to(ShardId(1), USD) + self.shards[1].transit_to(ShardId(0), USD);
        let applied = self.shards[1].applied_from(ShardId(0));
        let unapplied: i128 = self
            .credits
            .iter()
            .filter(|c| c.seq > applied)
            .map(|c| i128::from(c.amount))
            .sum();
        positions == unapplied
    }
}

#[test]
fn req_rt_13_credits_the_sender_committed_are_applied_once() {
    let mut w = world(4);
    let cert = w.voters.certify(7, &w.outbox, &[0, 2]);
    let credits = w.credits.clone();
    let a = w.deliver(1, &credits, &cert).expect("certified");
    assert_eq!((a.skip, a.count, a.head.seq), (0, 4, 4));
    assert_eq!(w.payee_balance(), 40);
    assert!(w.in_transit_matches());
    // The relay sends everything again with the same certificate: duplicates, nothing applied.
    let a = w.deliver(1, &credits, &cert).expect("a repeat");
    assert_eq!((a.skip, a.count), (4, 0));
    assert_eq!(w.payee_balance(), 40);
}

#[test]
fn req_rt_13_without_the_check_a_forged_credit_is_applied_and_only_reconciliation_sees_it() {
    // Today's default (R-03 option (d)): the relay invokes the receiver and the runtime applies
    // the next sequence, whoever wrote it.
    let mut w = world(1);
    let mut forged = w.credits[0];
    forged.amount = 500_000;
    w.shards[1].apply_credits(ShardId(0), &[forged]);
    assert_eq!(w.payee_balance(), 500_000, "value created in shard 1");
    assert_eq!(
        w.shards[1].trial_balance(USD),
        0,
        "each shard still balances"
    );
    assert!(!w.in_transit_matches(), "only the cross-shard axis sees it");
}

#[test]
fn req_rt_13_a_changed_credit_is_refused_and_nothing_applies() {
    let mut w = world(3);
    let cert = w.voters.certify(7, &w.outbox, &[0, 1]);
    for change in 0..4 {
        let mut credits = w.credits.clone();
        match change {
            0 => credits[1].amount = 500_000,
            1 => credits[2].payee[0] ^= 1,
            2 => credits.swap(0, 1),
            _ => credits[0].op[15] ^= 1,
        }
        assert_eq!(w.deliver(1, &credits, &cert), Err(Refused::Forged));
        assert_eq!(w.payee_balance(), 0);
        assert_eq!(w.shards[1].applied_from(ShardId(0)), 0);
    }
    assert_eq!(w.receiver.head(&w.stream).seq, 0, "nothing verified");
    // The genuine segment still goes through afterwards.
    let credits = w.credits.clone();
    w.deliver(1, &credits, &cert).expect("certified");
    assert_eq!(w.payee_balance(), 30);
}

#[test]
fn req_rt_13_a_credit_past_the_certified_head_waits() {
    let mut w = world(2);
    let cert = w.voters.certify(7, &w.outbox, &[1, 2]);
    let mut credits = w.credits.clone();
    let mut invented = credits[1];
    invented.seq = 3;
    invented.op = op(99);
    invented.amount = 777;
    credits.push(invented);
    let a = w.deliver(1, &credits, &cert).expect("the certified prefix");
    assert_eq!(a.count, 2, "the invented third credit is not verified");
    assert_eq!(w.payee_balance(), 20);
    assert_eq!(w.shards[1].applied_from(ShardId(0)), 2);
}

#[test]
fn req_rt_13_the_relay_cannot_certify() {
    let mut w = world(1);
    let credits = w.credits.clone();
    let genuine = w.voters.certify(7, &w.outbox, &[0, 1]);

    // The relay signs in a voter's place with its own key.
    let relay = SigningKey::from_bytes(&[0xEE; 32]);
    let mut cert = genuine.clone();
    cert.signatures[1] = Signature::sign(&relay, 1, &w.voters.committee, &cert.claim);
    assert_eq!(w.deliver(1, &credits, &cert), Err(Refused::BadSignature));

    // One voter's signature only.
    let one = w.voters.certify(7, &w.outbox, &[2]);
    assert_eq!(
        w.deliver(1, &credits, &one),
        Err(Refused::InsufficientQuorum)
    );

    // One voter's signature twice.
    let mut twice = one.clone();
    twice.signatures.push(twice.signatures[0]);
    assert_eq!(w.deliver(1, &credits, &twice), Err(Refused::NonCanonical));

    // A signer outside the committee.
    let mut outside = genuine.clone();
    outside.signatures[1].signer = 3;
    assert_eq!(w.deliver(1, &credits, &outside), Err(Refused::NonCanonical));

    // The relay's own committee, claiming to be the sender's.
    let fake = voters(shard_agent(ShardId(0)), 0xC0);
    let cert = fake.certify(7, &w.outbox, &[0, 1]);
    assert_eq!(w.deliver(1, &credits, &cert), Err(Refused::WrongCommittee));

    // A claim the voters signed, with the heads changed afterwards.
    let mut changed = genuine.clone();
    changed.claim.heads[0].1.seq += 1;
    assert_eq!(w.deliver(1, &credits, &changed), Err(Refused::BadSignature));

    // Another Agent's genuine certificate.
    let other = voters(shard_agent(ShardId(2)), 0x40);
    w.receiver.pin(other.committee.clone());
    let cert = other.certify(7, &w.outbox, &[0, 1]);
    assert_eq!(w.deliver(1, &credits, &cert), Err(Refused::WrongAgent));

    // A commit under another runtime program.
    let mut claim = genuine.claim.clone();
    claim.program = [0xB2; 32];
    let signatures = [0u16, 1]
        .iter()
        .map(|i| {
            Signature::sign(
                &w.voters.keys[usize::from(*i)],
                *i,
                &w.voters.committee,
                &claim,
            )
        })
        .collect();
    let cert = Certificate { claim, signatures };
    assert_eq!(w.deliver(1, &credits, &cert), Err(Refused::WrongProgram));

    assert_eq!(w.payee_balance(), 0);
    w.deliver(1, &credits, &genuine).expect("certified");
    assert_eq!(w.payee_balance(), 10);
}

#[test]
fn inv_18_segments_resume_from_the_verified_head() {
    let mut w = world(5);
    // The sender committed three, then two more; each commit certified on its own.
    let mut first = Outbox::default();
    for c in &w.credits[..3] {
        first.push(w.stream, &bytes(c));
    }
    let c3 = w.voters.certify(3, &first, &[0, 1]);
    let c5 = w.voters.certify(5, &w.outbox, &[1, 2]);
    let all = w.credits.clone();

    assert_eq!(w.deliver(2, &all[1..], &c3), Err(Refused::Gap));
    assert_eq!(w.deliver(1, &all[..2], &c3), Err(Refused::Incomplete));
    let a = w.deliver(1, &all[..3], &c3).expect("through 3");
    assert_eq!(a.head.seq, 3);
    // A stale certificate moves nothing.
    let a = w.deliver(1, &all, &c3).expect("stale");
    assert_eq!(a.count, 0);
    // The relay resends from 2 (it forgot what was applied): 2 and 3 are skipped, 4 and 5 checked.
    let a = w.deliver(2, &all[1..], &c5).expect("through 5");
    assert_eq!((a.skip, a.count, a.head.seq), (2, 2, 5));
    assert_eq!(w.payee_balance(), 50);
    assert!(w.in_transit_matches());
}

#[test]
fn req_rt_13_a_stream_the_receiver_does_not_take_is_refused() {
    let mut w = world(1);
    let cert = w.voters.certify(7, &w.outbox, &[0, 1]);
    let other = StreamId::credit(ShardId(0), ShardId(2));
    let refs: Vec<Vec<u8>> = w.credits.iter().map(bytes).collect();
    let refs: Vec<&[u8]> = refs.iter().map(Vec::as_slice).collect();
    assert_eq!(
        w.receiver.accept(&other, 1, &refs, &cert),
        Err(Refused::UnknownStream)
    );
    w.receiver.expect(other, shard_agent(ShardId(0)));
    assert_eq!(
        w.receiver.accept(&other, 1, &refs, &cert),
        Err(Refused::NotInClaim)
    );
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Any single changed byte in any delivered credit is refused, and the untouched segment is
    /// accepted after it.
    #[test]
    fn req_rt_13_any_changed_byte_is_refused(n in 1u64..12, at in any::<prop::sample::Index>(), bit in 0u8..8) {
        let w = world(n);
        let cert = w.voters.certify(1, &w.outbox, &[0, 2]);
        let mut encoded: Vec<Vec<u8>> = w.credits.iter().map(bytes).collect();
        let total: usize = encoded.iter().map(Vec::len).sum();
        let mut k = at.index(total);
        for m in encoded.iter_mut() {
            if k < m.len() {
                m[k] ^= 1 << bit;
                break;
            }
            k -= m.len();
        }
        let mut r = w.receiver.clone();
        let refs: Vec<&[u8]> = encoded.iter().map(Vec::as_slice).collect();
        prop_assert_eq!(r.accept(&w.stream, 1, &refs, &cert), Err(Refused::Forged));
        let good: Vec<Vec<u8>> = w.credits.iter().map(bytes).collect();
        let refs: Vec<&[u8]> = good.iter().map(Vec::as_slice).collect();
        prop_assert_eq!(r.accept(&w.stream, 1, &refs, &cert).map(|a| a.count), Ok(n as usize));
    }
}

/// The verification's cost, natively, beside the operation whose PVM gas is measured
/// (`curve-bench`: a Ristretto scalar multiplication with decompression and compression, 3.92 M
/// gas): the ratio estimates the guest's gas. Run with `--release --ignored --nocapture`.
#[test]
#[ignore]
fn measure_verification_cost() {
    use curve25519_dalek::ristretto::{CompressedRistretto, RistrettoPoint};
    use curve25519_dalek::scalar::Scalar;
    use std::hint::black_box;
    use std::time::Instant;

    const N: u32 = 2_000;
    let w = world(64);
    let cert = w.voters.certify(1, &w.outbox, &[0, 1]);
    let one = w.voters.certify(1, &w.outbox, &[0]);
    let encoded: Vec<Vec<u8>> = w.credits.iter().map(bytes).collect();
    let refs: Vec<&[u8]> = encoded.iter().map(Vec::as_slice).collect();

    let p = (RistrettoPoint::mul_base(&Scalar::from(7u64)))
        .compress()
        .to_bytes();
    let k = Scalar::from(0x1234_5678_9abc_def0u64);
    let t = Instant::now();
    for _ in 0..N {
        let q = CompressedRistretto(black_box(p))
            .decompress()
            .expect("a point");
        black_box((black_box(k) * q).compress());
    }
    let mult = t.elapsed().as_nanos() / u128::from(N);

    let mut solo = Committee::new(
        w.voters.committee.agent(),
        vec![w.voters.keys[0].verifying_key().to_bytes()],
        1,
    )
    .expect("one voter");
    let solo_cert = {
        let claim = Claim {
            committee: solo.id(),
            ..one.claim.clone()
        };
        let s = Signature::sign(&w.voters.keys[0], 0, &solo, &claim);
        Certificate {
            claim,
            signatures: vec![s],
        }
    };
    let mut r1 = Receiver::new(PROGRAM);
    r1.pin(solo.clone());
    let t = Instant::now();
    for _ in 0..N {
        r1.check(&w.voters.committee.agent(), black_box(&solo_cert))
            .expect("valid");
    }
    let quorum1 = t.elapsed().as_nanos() / u128::from(N);
    solo = w.voters.committee.clone();
    let mut r2 = Receiver::new(PROGRAM);
    r2.pin(solo);
    let t = Instant::now();
    for _ in 0..N {
        r2.check(&w.voters.committee.agent(), black_box(&cert))
            .expect("valid");
    }
    let quorum2 = t.elapsed().as_nanos() / u128::from(N);

    let t = Instant::now();
    for _ in 0..N {
        let mut r = w.receiver.clone();
        black_box(
            r.accept(&w.stream, 1, black_box(&refs), &cert)
                .expect("valid"),
        );
    }
    let segment = t.elapsed().as_nanos() / u128::from(N);
    let per_message = segment.saturating_sub(quorum2) / 64;

    // Gas estimates, in thousands: the native ratio to the scalar multiplication's 3 920 k.
    let est = |ns: u128| ns * 3_920 / mult.max(1);
    println!("ristretto k*P (decompress, compress): {mult} ns = 3920 k gas measured in the PVM");
    println!(
        "certificate, quorum 1 of 1: {quorum1} ns, about {} k gas",
        est(quorum1)
    );
    println!(
        "certificate, quorum 2 of 3: {quorum2} ns, about {} k gas",
        est(quorum2)
    );
    println!(
        "segment of 64 credits with quorum 2: {segment} ns; per credit {per_message} ns, about {} k gas",
        est(per_message)
    );
    println!("encoded credit: {} bytes", encoded[0].len());
}
