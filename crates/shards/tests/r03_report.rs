// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! R-03 in report mode (owner ruling `rq-01`, 2026-10-05: "do the checks anyway, but don't gate"):
//! every message between the book's Agents is checked against its sender's certified commit, in
//! the receiving shard's own transition, and a failure is reported, never refused. `enforce` stays
//! available; `off` checks nothing. Through the shard Agents' transition ([`NativePort`]) with the
//! host's replicas checking every reply, and through the native shard for the cases a cluster's
//! relay cannot produce (`REQ-RT-13`, `REQ-RT-11`, `INV-18`, `NFR-8`).

use std::sync::Arc;

use shards::attest::{Attestor, log_line, native_program, test_voters};
use shards::decide::Round;
use shards::host::{Cluster, NativePort, accounts, open_all};
use shards::principals::{Principals, PrincipalsCommand};
use shards::provenance::{Check, Mode, Outbox, Refused, Stream, StreamId, Verdict, Voter};
use shards::{Account, Command, Directory, Faults, Outcome, ShardId, book, feed};

const USD: u32 = 840;
const ROOT: [u8; 32] = [0x52; 32];
const DRIVER: [u8; 32] = [0xD3; 32];
const SEED: [u8; 32] = [0x03; 32];

fn op(n: u64) -> [u8; 16] {
    let mut o = [0u8; 16];
    o[..8].copy_from_slice(&n.to_be_bytes());
    o[8] = 0x03;
    o
}

/// `n` shards over `accts`, each open and funded.
fn cluster(n: u16, accts: &[Account]) -> Cluster<NativePort> {
    let ports = (0..n).map(|_| NativePort::new(DRIVER)).collect();
    let mut c = Cluster::found(ports, NativePort::new(DRIVER), ROOT, SEED).expect("founded");
    let all: Vec<(Account, u32)> = accts.iter().map(|a| (*a, USD)).collect();
    open_all(&mut c, &all).expect("opened");
    for (i, a) in accts.iter().enumerate() {
        c.operator(Command::Issue {
            op: op(1 << 40 | i as u64),
            account: *a,
            asset: USD,
            amount: 1_000,
        });
    }
    c.drain().expect("funded");
    c
}

/// Transfers between every pair of accounts, many of them across shards.
fn transfer_all(c: &mut Cluster<NativePort>, accts: &[Account], rounds: usize) {
    for r in 0..rounds {
        for (i, payer) in accts.iter().enumerate() {
            let payee = accts[(i + r + 1) % accts.len()];
            c.transfer(None, *payer, payee, USD, 1 + (i % 3) as u64)
                .expect("routed");
        }
        c.step().expect("a step");
    }
    c.drain().expect("drained");
}

#[test]
fn req_rt_13_report_is_the_default_and_an_honest_relay_verifies_every_stream() {
    let accts = accounts(24);
    let mut c = cluster(3, &accts);
    assert_eq!(c.r03().mode(), Mode::Report);
    // The relay at its worst short of forging: it duplicates, reorders, drops heads and loses
    // acknowledgements. Exactly once holds, and so does every check.
    c.faults = Faults {
        duplicate: true,
        reorder: true,
        drop_head: 1,
        lose_acks: true,
    };
    transfer_all(&mut c, &accts, 6);
    let r = &c.r03().report;
    assert!(r.verified > 0, "{r:?}");
    assert_eq!(r.failures, 0, "{:?}", r.incidents);
    assert_eq!(r.failing_streams(), 0);
    assert_eq!(
        r.unverified_messages(),
        0,
        "idle: every message applied is verified"
    );
    assert!(c.trial_balance(USD).holds());
    assert!(c.verify_exactly_once().expect("read").is_empty());
    // Every shard checked the feed and its peers' credits, and verified them to their heads.
    for (i, s) in c.shards().iter().enumerate() {
        let me = ShardId(i as u16);
        let r = s.replica();
        assert_eq!(r.feed_verified(), r.feed_applied(), "{me:?}: the feed");
        for p in (0..3).map(ShardId).filter(|p| *p != me) {
            assert_eq!(r.verified_from(p), r.applied_from(p), "{me:?} from {p:?}");
            assert!(r.applied_from(p) > 0);
            // `rq-05`: every trim this shard took from its peer was its peer's, certified.
            use shards::decide::State;
            let trimmed = r.state().trimmed(p);
            assert!(trimmed > 0);
            assert_eq!(r.trim_verified(p), trimmed, "{me:?}: trims from {p:?}");
        }
    }
}

#[test]
fn req_rt_13_report_a_forged_credit_is_applied_and_reported_at_once() {
    let accts = accounts(12);
    let mut c = cluster(2, &accts);
    c.forge = true;
    transfer_all(&mut c, &accts, 3);
    let r = &c.r03().report;
    assert!(r.failures > 0);
    assert_eq!(r.by_reason.get("forged").copied(), Some(r.failures));
    assert!(
        r.failing_streams() > 0,
        "the stream stays failing: its chain no longer meets the sender's"
    );
    let i = &r.incidents[0];
    assert_eq!(i.mode, Mode::Report);
    assert!(matches!(i.check.stream, Stream::Credit(_)));
    let line = log_line(i.receiver, &i.check, i.mode);
    assert!(
        line.starts_with("{\"event\":\"r03_check_failed\",\"mode\":\"report\""),
        "{line}"
    );
    assert!(line.contains("\"reason\":\"forged\"") && line.ends_with("\"applied_anyway\":true}"));
    // Not gated: the forged unit was applied, and the second reconciliation axis disagrees too.
    assert!(!c.trial_balance(USD).holds());
}

#[test]
fn req_rt_13_enforce_refuses_a_forged_credit_and_applies_the_genuine_one_once() {
    let accts = accounts(12);
    let mut c = cluster(2, &accts);
    c.set_r03(Mode::Enforce, native_program(), test_voters(&SEED, 2))
        .expect("pinned");
    c.forge = true;
    transfer_all(&mut c, &accts, 3);
    let r = &c.r03().report;
    assert!(r.failures > 0, "the forgeries were seen");
    // None was applied: the genuine credits arrived on the honest steps, once each.
    assert!(c.trial_balance(USD).holds());
    assert!(c.verify_exactly_once().expect("read").is_empty());
}

#[test]
fn req_rt_13_off_checks_nothing_and_a_forgery_goes_unseen() {
    let accts = accounts(12);
    let mut c = cluster(2, &accts);
    c.set_r03(Mode::Off, native_program(), test_voters(&SEED, 2))
        .expect("pinned");
    c.forge = true;
    transfer_all(&mut c, &accts, 3);
    assert_eq!(c.r03().report.checks, 0);
    assert!(!c.trial_balance(USD).holds(), "only reconciliation sees it");
}

#[test]
fn req_rt_13_a_relay_restart_certifies_the_heads_again() {
    let accts = accounts(12);
    let mut c = cluster(2, &accts);
    for (i, payer) in accts.iter().enumerate() {
        c.transfer(None, *payer, accts[(i + 5) % accts.len()], USD, 2)
            .expect("routed");
    }
    c.step().expect("a step");
    c.recover().expect("recovered");
    c.drain().expect("drained");
    let r = &c.r03().report;
    assert_eq!(r.failures, 0, "{:?}", r.incidents);
    assert_eq!(r.unverified_messages(), 0);
}

// ---- The native shard: what a cluster's relay cannot produce ----------------------------------

/// Two native shards and their test voters, every shard pinned in `mode`; a funded payer in
/// shard 0 and a payee in shard 1; shard 0 has decided `n` transfers of 10 to the payee.
struct Pair {
    shards: Vec<shards::Shard>,
    attestor: Attestor,
    payee: Account,
}

fn pair(mode: Mode, n: u64) -> Pair {
    let dir = Directory::even(2);
    let mut shards = book(2);
    let attestor = Attestor::new(mode, native_program(), test_voters(&SEED, 2), 2).expect("voters");
    for s in &mut shards {
        s.pin(&attestor.pins());
    }
    let find = |shard: ShardId| {
        accounts(64)
            .into_iter()
            .find(|a| dir.shard_of(a) == shard)
            .expect("an account")
    };
    let (payer, payee) = (find(ShardId(0)), find(ShardId(1)));
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
    Pair {
        shards,
        attestor,
        payee,
    }
}

impl Pair {
    /// Shard 0's commit certified by its voter.
    fn certify(&mut self) {
        let out = self.shards[0].out_heads();
        self.attestor.certify_shard(ShardId(0), 1, &out, &[]);
    }

    /// Deliver credits `first..=last` to shard 1, with the certificate the relay would pick.
    fn deliver(&mut self, from: u64, to: u64, attest: bool) -> Check {
        let credits: Vec<_> = self.shards[0]
            .outbound(ShardId(1))
            .into_iter()
            .filter(|c| (from..=to).contains(&c.seq))
            .collect();
        let attest = if attest {
            self.attestor
                .for_credits(ShardId(0), ShardId(1), to)
                .into_iter()
                .collect()
        } else {
            Vec::new()
        };
        let reply = self.shards[1]
            .round(&Round {
                credits: vec![(ShardId(0), credits)],
                attest,
                ..Round::default()
            })
            .expect("a round");
        assert_eq!(reply.checks.len(), 1, "{:?}", reply.checks);
        reply.checks[0]
    }
}

#[test]
fn req_rt_13_the_sender_chains_its_credits_as_the_model_does() {
    let p = pair(Mode::Report, 5);
    let stream = StreamId::credit(ShardId(0), ShardId(1));
    let mut model = Outbox::default();
    for c in p.shards[0].outbound(ShardId(1)) {
        model.push(stream, &postcard::to_allocvec(&c).expect("encodes"));
    }
    assert_eq!(
        p.shards[0].out_heads(),
        vec![(ShardId(1), model.head(&stream))]
    );
}

#[test]
fn req_rt_13_unattested_credits_are_verified_by_a_later_certificate() {
    let mut p = pair(Mode::Report, 4);
    p.certify();
    // The relay carries three credits with no certificate: applied, not yet verified.
    let c = p.deliver(1, 3, false);
    assert_eq!(c.verdict, Verdict::Unattested);
    assert_eq!((c.applied, c.verified), (3, 0));
    // The fourth, with the commit's certificate (head 4): the chain over all four lands on it.
    let c = p.deliver(4, 4, true);
    assert_eq!(c.verdict, Verdict::Verified { through: 4 });
    assert_eq!((c.applied, c.verified), (4, 4));
    assert_eq!(
        p.shards[1].balance(&p.payee, USD),
        Some(40),
        "report applies everything"
    );
}

#[test]
fn req_rt_13_a_certificate_the_receiver_never_pinned_is_reported() {
    let mut p = pair(Mode::Report, 2);
    // The relay signs with keys of its own (another committee): the claim names a committee the
    // receiver did not pin.
    let mut relay = Attestor::new(
        Mode::Report,
        native_program(),
        test_voters(&[0xEE; 32], 2),
        2,
    )
    .expect("voters");
    relay.certify_shard(ShardId(0), 1, &p.shards[0].out_heads(), &[]);
    let credits = p.shards[0].outbound(ShardId(1));
    let reply = p.shards[1]
        .round(&Round {
            credits: vec![(ShardId(0), credits)],
            attest: relay
                .for_credits(ShardId(0), ShardId(1), 2)
                .into_iter()
                .collect(),
            ..Round::default()
        })
        .expect("a round");
    assert_eq!(
        reply.checks[0].verdict,
        Verdict::Failed(Refused::WrongCommittee)
    );
    assert_eq!(
        p.shards[1].balance(&p.payee, USD),
        Some(20),
        "reported, not gated"
    );
}

#[test]
fn req_rt_13_another_program_is_reported() {
    let mut p = pair(Mode::Report, 1);
    let voters: Vec<Arc<dyn Voter>> = test_voters(&SEED, 2);
    let mut other = Attestor::new(Mode::Report, [0x77; 32], voters, 2).expect("voters");
    other.certify_shard(ShardId(0), 1, &p.shards[0].out_heads(), &[]);
    let credits = p.shards[0].outbound(ShardId(1));
    let reply = p.shards[1]
        .round(&Round {
            credits: vec![(ShardId(0), credits)],
            attest: other
                .for_credits(ShardId(0), ShardId(1), 1)
                .into_iter()
                .collect(),
            ..Round::default()
        })
        .expect("a round");
    assert_eq!(
        reply.checks[0].verdict,
        Verdict::Failed(Refused::WrongProgram)
    );
}

#[test]
fn req_rt_13_enforce_holds_unattested_credits_until_a_certificate_covers_them() {
    let mut p = pair(Mode::Enforce, 3);
    p.certify();
    let c = p.deliver(1, 2, false);
    assert_eq!(c.verdict, Verdict::Unattested);
    assert_eq!(c.applied, 0, "nothing unverified applies");
    let c = p.deliver(1, 3, true);
    assert_eq!(c.verdict, Verdict::Verified { through: 3 });
    assert_eq!(p.shards[1].balance(&p.payee, USD), Some(30));
}

/// The principals Agent's registry, having opened one account: version 1, certified by its voter.
fn registry(p: &mut Pair) -> Principals<shards::kv::MemKv> {
    let mut registry: Principals<shards::kv::MemKv> = Principals::default();
    registry.apply(&PrincipalsCommand::Open {
        account: accounts(64)[40],
        asset: USD,
    });
    assert_eq!(registry.version(), 1);
    assert_eq!(p.shards[1].feed_applied(), 0);
    p.attestor
        .certify_principals(1, registry.version(), registry.digest());
    registry
}

#[test]
fn req_rt_13_a_forged_feed_entry_is_reported() {
    let mut p = pair(Mode::Report, 0);
    let registry = registry(&mut p);
    let mut entries = registry.feed(0, usize::MAX);
    // The relay changes the account the entry opens.
    if let shards::Change::Opened { account, .. } = &mut entries[0].change {
        account[15] ^= 1;
    }
    let reply = p.shards[1]
        .round(&Round {
            feed: entries,
            attest: p.attestor.for_feed(1).into_iter().collect(),
            ..Round::default()
        })
        .expect("a round");
    assert_eq!(
        reply.checks,
        vec![Check {
            stream: Stream::Feed,
            verdict: Verdict::Failed(Refused::Forged),
            applied: 1,
            verified: 0,
        }]
    );
}

#[test]
fn req_rt_13_a_genuine_feed_entry_is_verified() {
    let mut p = pair(Mode::Report, 0);
    let registry = registry(&mut p);
    let reply = p.shards[1]
        .round(&Round {
            feed: registry.feed(0, usize::MAX),
            attest: p.attestor.for_feed(1).into_iter().collect(),
            ..Round::default()
        })
        .expect("a round");
    assert_eq!(reply.checks[0].verdict, Verdict::Verified { through: 1 });
    assert_eq!(p.shards[1].feed_verified(), 1);
}

// ---- Trims (owner ruling `rq-05`, 2026-10-05: checked before multi-host goes live) -----------

#[test]
fn req_rt_13_report_a_forged_trim_is_applied_and_reported() {
    let accts = accounts(12);
    let mut c = cluster(2, &accts);
    c.forge_trims = true;
    transfer_all(&mut c, &accts, 4);
    let r = &c.r03().report;
    let trims: Vec<_> = r
        .incidents
        .iter()
        .filter(|i| matches!(i.check.stream, Stream::Trim(_)))
        .collect();
    assert!(!trims.is_empty(), "{:?}", r.incidents);
    assert!(
        trims
            .iter()
            .all(|i| i.check.verdict == Verdict::Failed(Refused::Forged) && i.mode == Mode::Report)
    );
    let line = log_line(trims[0].receiver, &trims[0].check, Mode::Report);
    assert!(line.contains("\"stream\":\"trim/") && line.contains("\"applied_anyway\":true"));
    // Not gated: the source forgot credits its peer had not applied; the relay still had them.
    assert!(c.trial_balance(USD).holds());
    assert!(c.verify_exactly_once().expect("read").is_empty());
}

#[test]
fn req_rt_13_enforce_refuses_a_forged_trim_and_the_genuine_one_applies_once() {
    let accts = accounts(12);
    let mut c = cluster(2, &accts);
    c.set_r03(Mode::Enforce, native_program(), test_voters(&SEED, 2))
        .expect("pinned");
    c.forge_trims = true;
    transfer_all(&mut c, &accts, 4);
    let r = &c.r03().report;
    assert!(
        r.incidents
            .iter()
            .any(|i| matches!(i.check.stream, Stream::Trim(_))),
        "the forged trims were seen: {:?}",
        r.incidents
    );
    assert!(c.trial_balance(USD).holds());
    assert!(c.verify_exactly_once().expect("read").is_empty());
    // Every outbox is trimmed exactly to what its peer applied, through verified trims only.
    use shards::decide::State;
    for (i, s) in c.shards().iter().enumerate() {
        let me = ShardId(i as u16);
        let peer = ShardId(1 - i as u16);
        let r = s.replica();
        let enqueued = r.state().last_enqueued(peer);
        assert_eq!(r.state().trimmed(peer), enqueued, "{me:?}");
        assert_eq!(r.trim_verified(peer), enqueued, "{me:?}");
    }
}

impl Pair {
    /// Shard 1 applies every credit shard 0 sent, certified; its commit is certified in turn,
    /// with its applied head. Returns the sequence it applied through.
    fn apply_all(&mut self) -> u64 {
        self.certify();
        let n = self.shards[0].outbound(ShardId(1)).len() as u64;
        let c = self.deliver(1, n, true);
        assert_eq!(c.verdict, Verdict::Verified { through: n });
        let acks = self.shards[1].ack_heads();
        self.attestor.certify_shard(ShardId(1), 2, &[], &acks);
        n
    }

    /// Shard 0 takes a trim from shard 1 through `through`, with shard 1's certificate for
    /// `certified` (none: no certificate).
    fn trim(&mut self, through: u64, certified: Option<u64>) -> Check {
        let attest = certified
            .and_then(|h| self.attestor.for_trim(ShardId(1), ShardId(0), h))
            .into_iter()
            .collect();
        let reply = self.shards[0]
            .round(&Round {
                trims: vec![(ShardId(1), through)],
                attest,
                ..Round::default()
            })
            .expect("a round");
        assert_eq!(reply.checks.len(), 1, "{:?}", reply.checks);
        reply.checks[0]
    }

    fn trimmed(&self) -> u64 {
        use shards::decide::State;
        self.shards[0].state().trimmed(ShardId(1))
    }
}

#[test]
fn req_rt_13_a_certified_trim_is_verified_against_the_senders_own_chain() {
    let mut p = pair(Mode::Report, 4);
    let n = p.apply_all();
    let c = p.trim(n, Some(n));
    assert_eq!(c.verdict, Verdict::Verified { through: n });
    assert_eq!((c.applied, c.verified), (n, n));
    assert!(p.shards[0].outbound(ShardId(1)).is_empty());
}

#[test]
fn req_rt_13_report_a_trim_past_its_certificate_is_applied_and_reported() {
    let mut p = pair(Mode::Report, 6);
    // Shard 1 applies the first four only.
    p.certify();
    let c = p.deliver(1, 4, false);
    assert_eq!(c.applied, 4);
    p.attestor
        .certify_shard(ShardId(1), 2, &[], &p.shards[1].ack_heads());
    // The relay raises the trim to 5 with shard 1's certificate for 4.
    let c = p.trim(5, Some(4));
    assert_eq!(c.verdict, Verdict::Failed(Refused::Forged));
    assert_eq!(p.trimmed(), 5, "report: applied anyway");
}

#[test]
fn req_rt_13_enforce_a_trim_past_its_certificate_is_refused_and_the_genuine_one_applies_once() {
    let mut p = pair(Mode::Enforce, 6);
    p.certify();
    let c = p.deliver(1, 6, true);
    assert_eq!(c.applied, 6);
    let mut acks = p.shards[1].ack_heads();
    // Shard 1's commit after four of them, as a relay would hold it: certified at 4.
    acks[0].1 = shards::provenance::Head {
        seq: 4,
        chain: {
            use shards::decide::State;
            p.shards[0]
                .state()
                .out_chain_at(ShardId(1), 4)
                .expect("not trimmed")
        },
    };
    p.attestor.certify_shard(ShardId(1), 2, &[], &acks);
    let c = p.trim(5, Some(4));
    assert_eq!(c.verdict, Verdict::Failed(Refused::Forged));
    assert_eq!(p.trimmed(), 0, "enforce: refused");
    let c = p.trim(4, Some(4));
    assert_eq!(c.verdict, Verdict::Verified { through: 4 });
    assert_eq!(p.trimmed(), 4);
    // Again: nothing more to trim, nothing checked, nothing moves.
    let reply = p.shards[0]
        .round(&Round {
            trims: vec![(ShardId(1), 4)],
            ..Round::default()
        })
        .expect("a round");
    assert!(reply.checks.is_empty());
    assert_eq!(p.trimmed(), 4);
}

#[test]
fn req_rt_13_a_trim_from_a_peer_that_applied_other_credits_is_reported() {
    let mut p = pair(Mode::Report, 3);
    p.apply_all();
    // A certificate whose applied head carries another chain: the peer applied credits this
    // shard never sent.
    let mut acks = p.shards[1].ack_heads();
    acks[0].1.chain[0] ^= 1;
    let mut other =
        Attestor::new(Mode::Report, native_program(), test_voters(&SEED, 2), 2).expect("voters");
    other.certify_shard(ShardId(1), 3, &[], &acks);
    let reply = p.shards[0]
        .round(&Round {
            trims: vec![(ShardId(1), 3)],
            attest: other
                .for_trim(ShardId(1), ShardId(0), 3)
                .into_iter()
                .collect(),
            ..Round::default()
        })
        .expect("a round");
    assert_eq!(reply.checks[0].verdict, Verdict::Failed(Refused::Forged));
}

#[test]
fn req_rt_13_an_unattested_trim_is_applied_in_report_and_held_in_enforce() {
    for (mode, trimmed) in [(Mode::Report, 2), (Mode::Enforce, 0)] {
        let mut p = pair(mode, 2);
        p.apply_all();
        let c = p.trim(2, None);
        assert_eq!(c.verdict, Verdict::Unattested, "{mode:?}");
        assert_eq!(p.trimmed(), trimmed, "{mode:?}");
    }
}
