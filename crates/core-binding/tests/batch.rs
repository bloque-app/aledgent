// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Batched invocations (`F-031`, `T-031-02`, `T-031-03`), through the native Agent: the transition
//! the VOS actor runs, without the runtime.
//!
//! A batch must be indistinguishable from its inputs sent one at a time (`REQ-RT-1`, `INV-18`):
//! the same roots, summaries, log hashes and ring entries. A refused input commits the inputs
//! before it and nothing after it; a batch re-sent after an unknown outcome changes the book once.

mod common;

use common::*;
use core_binding::envelope::{
    AgentRefusal, BatchEnvelope, BlobKey, Committed, Envelope, MAX_BATCH_INPUTS, Pack, Reply,
    decode, encode,
};
use core_binding::host::{
    AgentPort, BatchLimits, DRIVER, DriveError, NativePort, PINNED_VOS_ACTOR_GAS, PortError,
};
use core_binding::hot::{Hot, Limits, RingEntry};
use core_binding::transition::{Invocation, MemRows, run};
use core_binding::witness::KernelWitness;
use ledger_core::asset::Minor;
use ledger_core::call::Summary;
use ledger_core::id::AccountId;
use ledger_core::principal::Caller;

/// The native Agent, watched: it records every invocation's sizes and reply, and can tamper with
/// the kernel witness of one input number, wherever that input travels.
struct Watch {
    inner: NativePort,
    /// Tamper with this input's kernel witness.
    tamper: Option<u64>,
    /// Only when it travels in a batch.
    only_batched: bool,
    /// Each invocation: its message's and blobs' sizes, and its reply.
    seen: Vec<(usize, usize, Option<Reply>)>,
}

impl Watch {
    fn new() -> Self {
        Watch {
            inner: NativePort::new(),
            tamper: None,
            only_batched: false,
            seen: Vec::new(),
        }
    }

    fn tampered(witness: &[u8]) -> Vec<u8> {
        let mut w = KernelWitness::from_bytes(witness).unwrap();
        let j = w.journals.first_mut().unwrap().1.as_mut().unwrap();
        j.code ^= 1;
        w.to_bytes()
    }

    fn find(&self, blobs: &[Vec<u8>], key: &BlobKey) -> usize {
        blobs
            .iter()
            .position(|b| self.inner.blob_key(b) == *key)
            .expect("the envelope names a blob it carries")
    }

    /// The invocation, with the target input's witness tampered with.
    fn rewrite(&self, message: Vec<u8>, mut blobs: Vec<Vec<u8>>) -> (Vec<u8>, Vec<Vec<u8>>) {
        let Some(target) = self.tamper else {
            return (message, blobs);
        };
        match decode::<Envelope>(&message) {
            Ok(Envelope::Input(mut i)) if i.seq == target && !self.only_batched => {
                let at = self.find(&blobs, &i.witness);
                blobs[at] = Self::tampered(&blobs[at]);
                i.witness = self.inner.blob_key(&blobs[at]);
                (encode(&Envelope::Input(i)), blobs)
            }
            Ok(Envelope::Batch(mut b)) if b.inputs.iter().any(|i| i.seq == target) => {
                let k = b.inputs.iter().position(|i| i.seq == target).unwrap();
                let old = b.inputs[k].witness;
                let mut new = None;
                let mut out = Vec::new();
                for (pk, _) in b.packs.iter_mut() {
                    let bytes = blobs[self.find(&blobs, pk)].clone();
                    let pack: Pack<'_> = decode(&bytes).unwrap();
                    if !pack.blobs.iter().any(|(bk, _)| *bk == old) {
                        out.push(bytes);
                        continue;
                    }
                    let mut owned: Vec<(BlobKey, Vec<u8>)> =
                        pack.blobs.iter().map(|(k, b)| (*k, b.to_vec())).collect();
                    for (bk, data) in owned.iter_mut() {
                        if *bk == old {
                            *data = Self::tampered(data);
                            *bk = self.inner.blob_key(data);
                            new = Some(*bk);
                        }
                    }
                    let repacked = encode(&Pack {
                        blobs: owned.iter().map(|(k, d)| (*k, d.as_slice())).collect(),
                    });
                    *pk = self.inner.blob_key(&repacked);
                    out.push(repacked);
                }
                b.inputs[k].witness = new.unwrap();
                (encode(&Envelope::Batch(b)), out)
            }
            _ => (message, blobs),
        }
    }
}

impl AgentPort for Watch {
    fn invoke(&mut self, message: Vec<u8>, blobs: Vec<Vec<u8>>) -> Result<Vec<u8>, PortError> {
        let (message, blobs) = self.rewrite(message, blobs);
        let sizes = (message.len(), blobs.iter().map(Vec::len).sum());
        let r = self.inner.invoke(message, blobs);
        let reply = r.as_ref().ok().map(|b| decode::<Reply>(b).unwrap());
        self.seen.push((sizes.0, sizes.1, reply));
        r
    }
    fn blob_key(&self, bytes: &[u8]) -> BlobKey {
        self.inner.blob_key(bytes)
    }
    fn hot(&mut self) -> Result<Option<Hot>, PortError> {
        self.inner.hot()
    }
    fn entry(&mut self, seq: u64) -> Result<Option<RingEntry>, PortError> {
        self.inner.entry(seq)
    }
}

/// A book with a funded account, through a watched Agent; nothing archived after the setup.
fn book() -> (Fx<Watch>, Caller, AccountId) {
    let mut fx = Fx::new(Watch::new(), Limits::DEFAULT);
    let (p, a) = fx.funded(1_000_000);
    (fx, p, a)
}

fn hot<P: AgentPort>(fx: &mut Fx<P>) -> Hot {
    fx.driver.port_mut().hot().unwrap().unwrap()
}

/// Reserve on the replica, and stage it.
fn stage_reserve(fx: &mut Fx<Watch>, p: Caller, a: AccountId, amount: u64) -> u64 {
    let ctx = fx.ctx(p);
    fx.replica
        .reserve(&ctx, a, usd(), Minor::new(amount))
        .unwrap();
    fx.driver
        .stage(&mut fx.replica, Vec::new())
        .unwrap()
        .expect("a call to send")
}

/// What two runs must agree on, for one committed input.
fn essence(c: &Committed) -> (u64, Vec<Summary>, [u8; 32], u64, [u8; 32]) {
    (
        c.seq,
        c.summaries.clone(),
        c.roots.kernel,
        c.next_posting,
        c.log_hash,
    )
}

/// The hot row, without what differs between one invocation and several: the observed slot.
fn same_hot(mut h: Hot) -> Hot {
    h.observed_slot = 0;
    h
}

fn ring<P: AgentPort>(fx: &mut Fx<P>, from: u64, to: u64) -> Vec<String> {
    (from..=to)
        .map(|s| format!("{:?}", fx.driver.port_mut().entry(s).unwrap()))
        .collect()
}

#[test]
fn req_rt_1_inv_18_a_batch_commits_exactly_as_its_inputs_one_by_one() {
    let n = 12;
    let (mut one, p, a) = book();
    let (mut many, q, b) = book();
    let start = hot(&mut one).inputs;
    assert_eq!(start, hot(&mut many).inputs);
    let mut singles = Vec::new();
    for i in 0..n {
        let ctx = one.ctx(p);
        one.replica
            .reserve(&ctx, a, usd(), Minor::new(10 + i))
            .unwrap();
        singles.push(one.sync().unwrap().unwrap());
    }
    let invocations = many.driver.port().seen.len();
    for i in 0..n {
        stage_reserve(&mut many, q, b, 10 + i);
    }
    assert_eq!(many.driver.staged(), n as usize);
    let batched = many.driver.flush().unwrap();
    assert_eq!(
        many.driver.port().seen.len(),
        invocations + 1,
        "one invocation"
    );
    assert!(matches!(
        many.driver.port().seen.last(),
        Some((_, _, Some(Reply::Batch(r)))) if r.len() == n as usize
    ));
    assert_eq!(
        batched.iter().map(essence).collect::<Vec<_>>(),
        singles.iter().map(essence).collect::<Vec<_>>()
    );
    assert_eq!(
        batched.iter().map(|c| c.roots).collect::<Vec<_>>(),
        singles.iter().map(|c| c.roots).collect::<Vec<_>>()
    );
    let end = start + n;
    assert_eq!(
        ring(&mut one, start + 1, end),
        ring(&mut many, start + 1, end)
    );
    assert_eq!(same_hot(hot(&mut one)), same_hot(hot(&mut many)));
    // And the next input, sent alone, carries on from the batch.
    let ctx = many.ctx(q);
    many.replica.reserve(&ctx, b, usd(), Minor::new(1)).unwrap();
    assert_eq!(
        many.sync().unwrap().unwrap().summaries,
        vec![Summary::Accepted]
    );
}

#[test]
fn req_rt_1_a_refused_input_commits_the_inputs_before_it_and_skips_the_rest() {
    let (mut reference, p0, a0) = book();
    let (mut fx, p, a) = book();
    let start = hot(&mut fx).inputs;
    // The reference: only the first two inputs, one at a time.
    for i in 0..2 {
        let ctx = reference.ctx(p0);
        reference
            .replica
            .reserve(&ctx, a0, usd(), Minor::new(10 + i))
            .unwrap();
        reference.sync().unwrap().unwrap();
    }
    fx.driver.port_mut().tamper = Some(start + 3);
    for i in 0..5 {
        stage_reserve(&mut fx, p, a, 10 + i);
    }
    let r = fx.driver.flush();
    assert!(
        matches!(r, Err(DriveError::Refused(AgentRefusal::Witness(_)))),
        "{r:?}"
    );
    let Some((_, _, Some(Reply::Batch(replies)))) = fx.driver.port().seen.last().cloned() else {
        panic!("a batch reply");
    };
    assert!(matches!(replies[0], Reply::Committed(_)));
    assert!(matches!(replies[1], Reply::Committed(_)));
    assert!(matches!(
        replies[2],
        Reply::Refused(AgentRefusal::Witness(_))
    ));
    assert_eq!(replies[3], Reply::Refused(AgentRefusal::Skipped));
    assert_eq!(replies[4], Reply::Refused(AgentRefusal::Skipped));
    // The driver confirmed the prefix; the Agent holds exactly it, as two singles leave it.
    assert_eq!(fx.driver.seq(), start + 2);
    assert_eq!(same_hot(hot(&mut fx)), same_hot(hot(&mut reference)));
    assert_eq!(
        ring(&mut fx, start + 1, start + 2),
        ring(&mut reference, start + 1, start + 2)
    );
    assert!(fx.driver.port_mut().entry(start + 3).unwrap().is_none());
}

#[test]
fn inv_18_a_batch_resent_after_an_unknown_outcome_changes_the_book_once() {
    let (mut fx, p, a) = book();
    let start = hot(&mut fx).inputs;
    for i in 0..6 {
        stage_reserve(&mut fx, p, a, 10 + i);
    }
    // The Agent commits the batch and the answer is lost: the driver re-sends it, and every
    // input answers as a duplicate.
    fx.driver.port_mut().inner.fail_next = Some(true);
    let done = fx.driver.flush().unwrap();
    assert_eq!(done.len(), 6);
    let Some((_, _, Some(Reply::Batch(replies)))) = fx.driver.port().seen.last().cloned() else {
        panic!("a batch reply");
    };
    assert!(
        replies.iter().all(|r| matches!(r, Reply::Duplicate { .. })),
        "{replies:?}"
    );
    let h = hot(&mut fx);
    assert_eq!(h.inputs, start + 6, "exactly one state change per input");
    assert_eq!(h.roots(), done.last().unwrap().roots);
}

#[test]
fn inv_18_a_batch_refused_midway_and_resent_commits_its_prefix_once() {
    let (mut fx, p, a) = book();
    let start = hot(&mut fx).inputs;
    fx.driver.port_mut().tamper = Some(start + 4);
    for i in 0..6 {
        stage_reserve(&mut fx, p, a, 10 + i);
    }
    fx.driver.port_mut().inner.fail_next = Some(true);
    let r = fx.driver.flush();
    assert!(matches!(r, Err(DriveError::Refused(_))), "{r:?}");
    let Some((_, _, Some(Reply::Batch(replies)))) = fx.driver.port().seen.last().cloned() else {
        panic!("a batch reply");
    };
    // The re-sent batch: the committed prefix answers as duplicates, the refusal again.
    assert!(
        replies[..3]
            .iter()
            .all(|r| matches!(r, Reply::Duplicate { .. }))
    );
    assert!(matches!(
        replies[3],
        Reply::Refused(AgentRefusal::Witness(_))
    ));
    assert_eq!(hot(&mut fx).inputs, start + 3);
}

#[test]
fn req_cs_7_staging_flushes_before_a_bound_is_passed() {
    let (mut fx, p, a) = book();
    let start = hot(&mut fx).inputs;
    let before = fx.driver.port().seen.len();
    // Room for about three ordinary inputs' blobs in one invocation.
    let one = {
        let ctx = fx.ctx(p);
        fx.replica.reserve(&ctx, a, usd(), Minor::new(1)).unwrap();
        fx.sync().unwrap().unwrap();
        fx.driver.port().seen.last().unwrap().1
    };
    let limits = BatchLimits {
        max_blob_bytes: 3 * one + one / 2,
        max_pack_bytes: one + one / 2,
        ..BatchLimits::DEFAULT
    };
    fx.driver.set_batch_limits(limits);
    let sent = fx.driver.port().seen.len();
    assert_eq!(sent, before + 1);
    for i in 0..10 {
        stage_reserve(&mut fx, p, a, 10 + i);
    }
    let done = fx.driver.flush().unwrap();
    assert_eq!(done.len(), 10, "every input committed, in order");
    assert_eq!(
        done.iter().map(|c| c.seq).collect::<Vec<_>>(),
        (start + 2..=start + 11).collect::<Vec<_>>()
    );
    let seen = &fx.driver.port().seen[sent..];
    assert!(seen.len() >= 4, "{} invocations", seen.len());
    for (message, blobs, _) in seen {
        assert!(*blobs <= limits.max_blob_bytes, "{blobs} B of blobs");
        assert!(
            *message <= limits.max_message_bytes,
            "{message} B of message"
        );
    }
    // And by count alone.
    fx.driver.set_batch_limits(BatchLimits::with_inputs(4));
    let sent = fx.driver.port().seen.len();
    for i in 0..10 {
        stage_reserve(&mut fx, p, a, 100 + i);
    }
    assert_eq!(fx.driver.flush().unwrap().len(), 10);
    assert_eq!(fx.driver.port().seen.len() - sent, 3, "4, 4 and 2");
    // By VOS's actor gas: three ordinary inputs to an invocation at the pinned VOS.
    fx.driver
        .set_batch_limits(BatchLimits::DEFAULT.with_actor_gas(PINNED_VOS_ACTOR_GAS));
    let sent = fx.driver.port().seen.len();
    for i in 0..7 {
        stage_reserve(&mut fx, p, a, 300 + i);
    }
    assert_eq!(fx.driver.flush().unwrap().len(), 7);
    assert_eq!(fx.driver.port().seen.len() - sent, 3, "3, 3 and 1");
    // A message bound smaller than two inputs sends each alone.
    fx.driver.set_batch_limits(BatchLimits {
        max_message_bytes: 64,
        ..BatchLimits::DEFAULT
    });
    let sent = fx.driver.port().seen.len();
    for i in 0..3 {
        stage_reserve(&mut fx, p, a, 200 + i);
    }
    assert_eq!(fx.driver.flush().unwrap().len(), 3);
    assert_eq!(fx.driver.port().seen.len() - sent, 3);
    assert_eq!(hot(&mut fx).inputs, start + 1 + 10 + 10 + 7 + 3);
}

#[test]
fn t_031_02_a_batch_the_agent_cannot_read_is_refused_whole() {
    let (mut fx, p, a) = book();
    let rows = fx.driver.port().inner.rows.clone();
    let hot0 = hot(&mut fx);
    let _ = (p, a);
    let invoke = |rows: &mut MemRows, b: BatchEnvelope| {
        let message = encode(&Envelope::Batch(b));
        let none = |_: &BlobKey| None;
        run(
            rows,
            &Invocation {
                origin: Some(DRIVER),
                slot: Some(1),
                message: &message,
                blob: &none,
            },
        )
    };
    let input = |seq| core_binding::envelope::InputEnvelope {
        seq,
        calls: Vec::new(),
        deliveries: Vec::new(),
        archived: None,
        witness: BlobKey {
            hash: [1; 32],
            len: 1,
        },
        service: BlobKey {
            hash: [2; 32],
            len: 1,
        },
        private: BlobKey {
            hash: [3; 32],
            len: 1,
        },
    };
    let pack = BlobKey {
        hash: [9; 32],
        len: 9,
    };
    let mut r = rows.clone();
    // Empty, too many inputs, packs that do not cover the inputs, too many packs.
    for b in [
        BatchEnvelope {
            inputs: Vec::new(),
            packs: Vec::new(),
        },
        BatchEnvelope {
            inputs: (1..=MAX_BATCH_INPUTS as u64 + 1).map(input).collect(),
            packs: vec![(pack, MAX_BATCH_INPUTS as u32 + 1)],
        },
        BatchEnvelope {
            inputs: vec![input(hot0.inputs + 1), input(hot0.inputs + 2)],
            packs: vec![(pack, 1)],
        },
        BatchEnvelope {
            inputs: (1..=5).map(|s| input(hot0.inputs + s)).collect(),
            packs: vec![(pack, 1); 5],
        },
    ] {
        assert!(matches!(
            invoke(&mut r, b),
            Reply::Refused(AgentRefusal::Malformed(_))
        ));
        assert_eq!(r, rows, "nothing written");
    }
    // A missing pack: its first input answers why, and nothing commits.
    let reply = invoke(
        &mut r,
        BatchEnvelope {
            inputs: vec![input(hot0.inputs + 1), input(hot0.inputs + 2)],
            packs: vec![(pack, 2)],
        },
    );
    assert_eq!(
        reply,
        Reply::Batch(vec![
            Reply::Refused(AgentRefusal::Malformed("no pack blob".into())),
            Reply::Refused(AgentRefusal::Skipped),
        ])
    );
    assert_eq!(r, rows);
}

#[test]
fn t_031_02_a_pack_reads_back_borrowed() {
    let blobs: Vec<(BlobKey, Vec<u8>)> = (0..3u8)
        .map(|i| {
            (
                BlobKey {
                    hash: [i; 32],
                    len: 4,
                },
                vec![i; 4],
            )
        })
        .collect();
    let bytes = encode(&Pack {
        blobs: blobs.iter().map(|(k, b)| (*k, b.as_slice())).collect(),
    });
    let back: Pack<'_> = decode(&bytes).unwrap();
    assert_eq!(
        back.blobs
            .iter()
            .map(|(k, b)| (*k, b.to_vec()))
            .collect::<Vec<_>>(),
        blobs
    );
}

/// The guest decodes what the host encodes only if both number `Envelope`'s variants alike.
/// Postcard encodes a variant by its index, the host always builds `mapped` and the production
/// guest does not: a feature-gated variant before `Batch` made every batch of two or more inputs a
/// malformed message in the VOS guest (no such batch committed on the core Agent from 47155ab
/// until this test). The variants every build has keep the same indices, and the gated one is last.
#[test]
fn envelope_variant_indices_are_the_same_in_every_build() {
    let archived = Envelope::Archived {
        through: 7,
        log_hash: [9; 32],
    };
    assert_eq!(encode(&archived)[0], 2, "Archived");
    let batch = Envelope::Batch(BatchEnvelope {
        inputs: Vec::new(),
        packs: Vec::new(),
    });
    let bytes = encode(&batch);
    assert_eq!(bytes[0], 3, "Batch");
    assert!(matches!(decode::<Envelope>(&bytes), Ok(Envelope::Batch(_))));
}
