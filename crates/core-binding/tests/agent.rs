// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The binding's tests, through the native Agent: the transition the VOS actor runs, without the
//! runtime (`agents/core-host` runs the same through VOS; the conformance suite runs the
//! library's cases through both).

mod common;

use common::*;
use core_binding::envelope::{AgentRefusal, BlobKey, Envelope, Private, decode, encode};
use core_binding::host::{
    AgentPort, DRIVER, DriveError, Driver, NativePort, PortError, replay, rows_bytes,
};
use core_binding::hot::{Hot, Limits, MAX_BUDGET, RingEntry};
use core_binding::service::ServiceWitness;
use core_binding::transition::MemRows;
use core_binding::witness::KernelWitness;
use ledger_core::asset::Minor;
use ledger_core::call::Summary;

/// How the spy tampers with the next input's witness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tamper {
    /// Change a witnessed leaf: the book's journal's code.
    Leaf,
    /// Send the previous input's witness again.
    Stale,
    /// Change a witnessed service leaf's value.
    ServiceLeaf,
    /// Send the previous input's service witness again.
    ServiceStale,
    /// Withhold the service leaf of this kind, its subtree folded into the proof so that the
    /// root still verifies: a host hiding what the Agent must read.
    ServiceHidden(u8),
}

/// The native Agent, watched: it keeps every private input and witness it was sent, can run a
/// twin Agent on the same invocations, can tamper with the next witness, and can invoke as
/// another principal.
struct Spy {
    inner: NativePort,
    twin: Option<NativePort>,
    privates: Vec<Private>,
    witnesses: Vec<Vec<u8>>,
    services: Vec<Vec<u8>>,
    tamper: Option<Tamper>,
    as_other: bool,
}

impl Spy {
    fn new() -> Self {
        Spy {
            inner: NativePort::new(),
            twin: None,
            privates: Vec::new(),
            witnesses: Vec::new(),
            services: Vec::new(),
            tamper: None,
            as_other: false,
        }
    }

    fn rows(&self) -> MemRows {
        self.inner.rows.clone()
    }

    fn keyed<'a>(&self, blobs: &'a [Vec<u8>], key: &BlobKey) -> &'a Vec<u8> {
        blobs
            .iter()
            .find(|b| self.inner.blob_key(b) == *key)
            .expect("the envelope names a blob it carries")
    }
}

impl AgentPort for Spy {
    fn invoke(
        &mut self,
        mut message: Vec<u8>,
        mut blobs: Vec<Vec<u8>>,
    ) -> Result<Vec<u8>, PortError> {
        if let Ok(Envelope::Input(mut i)) = decode::<Envelope>(&message) {
            let private: Private = decode(self.keyed(&blobs, &i.private)).unwrap();
            self.privates.push(private);
            let witness = self.keyed(&blobs, &i.witness).clone();
            let service = self.keyed(&blobs, &i.service).clone();
            let tamper = self.tamper.take();
            let sent = match tamper {
                Some(Tamper::Leaf) => {
                    let mut w = KernelWitness::from_bytes(&witness).unwrap();
                    let j = w.journals.first_mut().unwrap().1.as_mut().unwrap();
                    j.code ^= 1;
                    w.to_bytes()
                }
                Some(Tamper::Stale) => self.witnesses.last().unwrap().clone(),
                _ => witness.clone(),
            };
            let sent_service = match tamper {
                Some(Tamper::ServiceLeaf) => {
                    let mut w: ServiceWitness = decode(&service).unwrap();
                    let leaf = w.leaves.iter_mut().find(|l| l.2.is_some()).unwrap();
                    leaf.2.as_mut().unwrap()[0] ^= 1;
                    encode(&w)
                }
                Some(Tamper::ServiceStale) => self.services.last().unwrap().clone(),
                Some(Tamper::ServiceHidden(kind)) => {
                    let mut w: ServiceWitness = decode(&service).unwrap();
                    let at = w.leaves.iter().position(|l| l.0 == kind).unwrap();
                    w.conceal(at);
                    encode(&w)
                }
                _ => service.clone(),
            };
            if sent != witness || sent_service != service {
                blobs.retain(|b| *b != witness && *b != service);
                i.witness = self.inner.blob_key(&sent);
                i.service = self.inner.blob_key(&sent_service);
                blobs.push(sent);
                blobs.push(sent_service);
                message = encode(&Envelope::Input(i));
            }
            self.witnesses.push(witness);
            self.services.push(service);
        }
        if let Some(twin) = self.twin.as_mut() {
            let a = twin.invoke(message.clone(), blobs.clone());
            let b = self.inner.invoke(message, blobs);
            assert_eq!(a, b, "two Agents gave two replies to one invocation");
            assert_eq!(twin.rows, self.inner.rows, "two Agents reached two states");
            return b;
        }
        if self.as_other {
            let saved = self.inner.origin;
            self.inner.origin = [0x66; 32];
            let r = self.inner.invoke(message, blobs);
            self.inner.origin = saved;
            return r;
        }
        self.inner.invoke(message, blobs)
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

fn spy(limits: Limits) -> Fx<Spy> {
    Fx::new(Spy::new(), limits)
}

fn hot<P: AgentPort>(fx: &mut Fx<P>) -> Hot {
    fx.driver.port_mut().hot().unwrap().unwrap()
}

/// Reserve `amount` on the replica and send it: the Agent's answer, not unwrapped.
fn reserve<P: AgentPort>(
    fx: &mut Fx<P>,
    p: ledger_core::principal::Caller,
    a: ledger_core::id::AccountId,
    amount: u64,
) -> Result<Option<core_binding::envelope::Committed>, DriveError> {
    let ctx = fx.ctx(p);
    let _ = fx.replica.reserve(&ctx, a, usd(), Minor::new(amount));
    fx.sync()
}

#[test]
fn req_rt_1_a_reservation_commits_through_the_agent() {
    let mut fx = Fx::native();
    let (p, a) = fx.funded(10_000);
    let before = hot(&mut fx);
    let c = reserve(&mut fx, p, a, 2_500).unwrap().unwrap();
    assert_eq!(c.summaries, vec![Summary::Accepted]);
    let after = hot(&mut fx);
    // One transition: the kernel's roots, the service image, the ring and the input number
    // moved together (REQ-RT-1).
    assert_eq!(after.inputs, before.inputs + 1);
    assert_ne!(after.subroots, before.subroots);
    assert_ne!(after.service_root, before.service_root);
    assert_eq!(after.roots(), c.roots);
}

// T-005-02 ------------------------------------------------------------------------------------

#[test]
fn req_rt_1_a_transition_over_the_budget_is_refused_and_writes_nothing() {
    // The state no longer grows with the book (the service store is leaves): what grows is the
    // ring of unarchived inputs. A small budget and a host that never archives reach it.
    let budget = 4 * 1024;
    let mut fx = spy(Limits {
        budget,
        ring: 1_000,
        ..Limits::DEFAULT
    });
    let (p, a) = fx.funded(1_000_000);
    let mut refused = None;
    for i in 0..200 {
        let rows = fx.driver.port().rows();
        match reserve(&mut fx, p, a, 10 + i) {
            Ok(_) => {}
            Err(DriveError::Refused(r)) => {
                assert_eq!(fx.driver.port().rows(), rows, "a refused transition wrote");
                refused = Some(r);
                break;
            }
            Err(e) => panic!("{e:?}"),
        }
    }
    match refused {
        Some(AgentRefusal::OverBudget { bytes, budget }) => {
            assert!(bytes > budget);
            assert_eq!(budget, 4 * 1024);
        }
        other => panic!("expected the budget's refusal, got {other:?}"),
    }
    let h = hot(&mut fx);
    assert!(
        h.bytes() <= h.limits.budget,
        "the committed state is within H"
    );
}

#[test]
fn req_rt_1_genesis_refuses_a_budget_over_the_ceiling() {
    let r = Driver::found(
        NativePort::new(),
        keys(),
        genesis(),
        op(0),
        1,
        Limits {
            budget: MAX_BUDGET + 1,
            ..Limits::DEFAULT
        },
    );
    assert!(matches!(
        r.err(),
        Some(DriveError::Refused(AgentRefusal::Malformed(_)))
    ));
}

// T-005-03 ------------------------------------------------------------------------------------

#[test]
fn req_pn_1_an_invocation_from_another_principal_is_refused() {
    let mut fx = spy(Limits::DEFAULT);
    let (p, a) = fx.funded(10_000);
    let rows = fx.driver.port().rows();
    fx.driver.port_mut().as_other = true;
    let r = reserve(&mut fx, p, a, 100);
    assert_eq!(r.err(), Some(DriveError::Refused(AgentRefusal::NotDriver)));
    assert_eq!(fx.driver.port().rows(), rows, "nothing committed");
}

#[test]
fn req_cs_4_library_refusals_come_back_as_their_err_codes() {
    let mut fx = Fx::native();
    let (p, a) = fx.funded(1_000);
    // The library refuses, and records the refusal (F-007): the Agent commits it, and says which.
    let c = reserve(&mut fx, p, a, 5_000).unwrap().unwrap();
    assert_eq!(
        c.summaries,
        vec![Summary::Refused("ERR-InsufficientAvailable".into())]
    );
}

// T-005-04 ------------------------------------------------------------------------------------

#[test]
fn inv_2_a_tampered_leaf_is_refused_with_no_commit() {
    let mut fx = spy(Limits::DEFAULT);
    let (p, a) = fx.funded(10_000);
    let rows = fx.driver.port().rows();
    fx.driver.port_mut().tamper = Some(Tamper::Leaf);
    let r = reserve(&mut fx, p, a, 100);
    assert!(
        matches!(r, Err(DriveError::Refused(AgentRefusal::Witness(_)))),
        "{r:?}"
    );
    assert_eq!(fx.driver.port().rows(), rows);
}

#[test]
fn inv_2_a_stale_witness_is_refused_with_no_commit() {
    let mut fx = spy(Limits::DEFAULT);
    let (p, a) = fx.funded(10_000);
    reserve(&mut fx, p, a, 100).unwrap();
    let rows = fx.driver.port().rows();
    fx.driver.port_mut().tamper = Some(Tamper::Stale);
    let r = reserve(&mut fx, p, a, 100);
    assert!(
        matches!(r, Err(DriveError::Refused(AgentRefusal::Witness(_)))),
        "{r:?}"
    );
    assert_eq!(fx.driver.port().rows(), rows);
}

#[test]
fn inv_2_a_tampered_service_leaf_is_refused_with_no_commit() {
    let mut fx = spy(Limits::DEFAULT);
    let (p, a) = fx.funded(10_000);
    let rows = fx.driver.port().rows();
    fx.driver.port_mut().tamper = Some(Tamper::ServiceLeaf);
    let r = reserve(&mut fx, p, a, 100);
    assert!(
        matches!(&r, Err(DriveError::Refused(AgentRefusal::Witness(w))) if w.contains("Root")),
        "{r:?}"
    );
    assert_eq!(fx.driver.port().rows(), rows);
}

#[test]
fn inv_2_a_stale_service_witness_is_refused_with_no_commit() {
    let mut fx = spy(Limits::DEFAULT);
    let (p, a) = fx.funded(10_000);
    reserve(&mut fx, p, a, 100).unwrap();
    let rows = fx.driver.port().rows();
    fx.driver.port_mut().tamper = Some(Tamper::ServiceStale);
    let r = reserve(&mut fx, p, a, 100);
    assert!(
        matches!(r, Err(DriveError::Refused(AgentRefusal::Witness(_)))),
        "{r:?}"
    );
    assert_eq!(fx.driver.port().rows(), rows);
}

/// `T-005-04` for the service store: a host that withholds a service leaf the command reads,
/// behind a proof that still gives the Agent's root, gets nothing committed. The Agent's store
/// answers only what it was shown, and a read of anything else fails closed.
#[test]
fn inv_2_a_service_read_outside_the_witness_fails_closed() {
    use ledger_core::leaves::kind;
    let mut fx = spy(Limits::DEFAULT);
    let (p, a) = fx.funded(10_000);
    let rows = fx.driver.port().rows();
    // The payer's ledger account: every reservation reads it.
    fx.driver.port_mut().tamper = Some(Tamper::ServiceHidden(kind::LEDGER_ACCOUNT));
    let r = reserve(&mut fx, p, a, 100);
    assert_eq!(r, Err(DriveError::Refused(AgentRefusal::UnprovenRead)));
    assert_eq!(fx.driver.port().rows(), rows);
}

/// A tick's due deadlines are read by their keys (finding F18 of `F-032`): a tick whose due entry
/// is withheld from the witness is refused before it runs. Before, the tick read the deadline
/// index whole; a kind claimed whole with a leaf withheld is refused by the witness itself
/// (`core_binding::service`'s tests).
#[test]
fn inv_2_a_tick_with_a_due_deadline_withheld_is_refused() {
    use ledger_core::leaves::kind;
    let mut fx = spy(Limits::DEFAULT);
    let (p, a) = fx.funded(10_000);
    for i in 0..3 {
        reserve(&mut fx, p, a, 100 + i).unwrap();
        fx.driver.archived();
    }
    let rows = fx.driver.port().rows();
    fx.driver.port_mut().tamper = Some(Tamper::ServiceHidden(kind::DEADLINE));
    let _ = fx.replica.tick(5_000);
    let r = fx.sync();
    assert_eq!(r, Err(DriveError::Refused(AgentRefusal::UnprovenRead)));
    assert_eq!(fx.driver.port().rows(), rows);
}

#[test]
fn inv_2_a_read_outside_the_witness_fails_closed_at_the_store() {
    use core_binding::witness::WitnessStore;
    use leaf_store::BookStore;
    use leaf_store::trie::TreeId;
    use leaf_store::witness::keys_of;
    use ledger_kernel::book::Book;
    use ledger_kernel::keys::KeyRing;
    use ledger_kernel::store::{KeySet, LeafStore};
    use ledger_kernel::vault::MemVault;

    let book = Book::bootstrap(
        BookStore::in_memory().unwrap(),
        MemVault::new(),
        KeyRing::from_root(ROOT),
    )
    .unwrap();
    let journal = book.journal();
    let (store, _, _) = book.into_parts();
    let view = store.view().unwrap();
    let mut keys = KeySet::default();
    keys.journals.insert(journal);
    let pre = view.load(&keys).unwrap();
    let proofs = keys_of(&keys)
        .iter()
        .enumerate()
        .map(|(i, k)| core_binding::host::kernel_proof(&view, TreeId(i as u8), k).unwrap())
        .collect();
    let ws = WitnessStore::new(
        KernelWitness::new(&pre, proofs),
        store.subroots().unwrap(),
        store.last_committed().unwrap(),
        store.next_posting_seq().unwrap(),
    )
    .unwrap();
    // The witnessed key answers; any other fails closed, and the store remembers it.
    assert!(ws.load(&keys).is_ok());
    assert!(!ws.missed());
    let mut other = KeySet::default();
    other.accounts.insert(cipher_clerk::AccountId([9; 16]));
    assert!(ws.load(&other).is_err());
    assert!(ws.missed());
}

// T-005-05 ------------------------------------------------------------------------------------

#[test]
fn req_rt_6_no_opening_or_key_reaches_the_agents_state() {
    let mut fx = spy(Limits::DEFAULT);
    let (p, a) = fx.funded(10_000);
    for i in 0..5 {
        reserve(&mut fx, p, a, 100 + i).unwrap();
        fx.driver.archived();
    }
    let state = rows_bytes(&fx.driver.port().rows());
    let privates = &fx.driver.port().privates;
    let mut secrets: Vec<Vec<u8>> = vec![ROOT.to_vec()];
    for pr in privates {
        for (_, sums) in &pr.openings {
            // The two blindings of a row (its values are small integers).
            secrets.push(sums[16..48].to_vec());
            secrets.push(sums[64..96].to_vec());
        }
    }
    assert!(secrets.len() > 1, "the inputs carried openings");
    for s in secrets.iter().filter(|s| s.iter().any(|b| *b != 0)) {
        assert!(
            !state.windows(s.len()).any(|w| w == s.as_slice()),
            "a private input reached the Agent's state"
        );
    }
}

/// `F-035`, guardrail 13, `NFR-8`: the Agent's private inputs carry what the key custody
/// answered (public keys, signatures, blindings), never a key: not the root, not the registrar's
/// key, not an account's key.
#[test]
fn nfr_8_no_key_reaches_the_agents_private_inputs() {
    let mut fx = spy(Limits::DEFAULT);
    let (p, a) = fx.funded(10_000);
    for i in 0..3 {
        reserve(&mut fx, p, a, 100 + i).unwrap();
        fx.driver.archived();
    }
    let privates = &fx.driver.port().privates;
    assert!(
        privates.iter().any(|pr| !pr.keys.is_empty()),
        "the inputs carried the custody's answers"
    );
    let root = ledger_kernel::keys::RootKeys::new(ROOT);
    let mut keys: Vec<[u8; 32]> = vec![ROOT, root.registrar_secret().to_bytes()];
    for pr in privates {
        for (req, _) in &pr.keys {
            if let Some(
                ledger_kernel::keys::KeyRequest::AccountKey(id)
                | ledger_kernel::keys::KeyRequest::SignAccount(id, _),
            ) = ledger_kernel::keys::KeyRequest::decode(req)
            {
                keys.push(root.account_secret(&cipher_clerk::AccountId(id)).to_bytes());
            }
        }
    }
    assert!(keys.len() > 2, "the inputs named accounts");
    for pr in privates {
        let bytes = core_binding::envelope::encode(pr);
        for k in &keys {
            assert!(
                !bytes.windows(32).any(|w| w == k.as_slice()),
                "a key reached a private input"
            );
        }
    }
}

#[test]
fn inv_3_the_negativity_check_passes_and_fails_through_the_agent() {
    let mut fx = Fx::native();
    let (p, a) = fx.funded(1_000);
    let ok = reserve(&mut fx, p, a, 1_000).unwrap().unwrap();
    assert_eq!(ok.summaries, vec![Summary::Accepted]);
    fx.driver.archived();
    let no = reserve(&mut fx, p, a, 1).unwrap().unwrap();
    assert_eq!(
        no.summaries,
        vec![Summary::Refused("ERR-InsufficientAvailable".into())]
    );
}

// T-005-06 ------------------------------------------------------------------------------------

#[test]
fn req_rt_7_the_same_inputs_give_the_same_transition() {
    let mut port = Spy::new();
    port.twin = Some(NativePort::new());
    let mut fx = Fx::new(port, Limits::DEFAULT);
    let (p, a) = fx.funded(10_000);
    for i in 0..4 {
        reserve(&mut fx, p, a, 10 + i).unwrap();
        fx.driver.archived();
    }
    // Every invocation above ran on two Agents and gave byte-identical replies and states.
    assert_eq!(
        fx.driver.port().twin.as_ref().unwrap().rows,
        fx.driver.port().rows()
    );
}

// T-005-07 ------------------------------------------------------------------------------------

#[test]
fn req_rt_4_nothing_is_trimmed_before_it_is_archived_and_a_full_ring_refuses() {
    let mut fx = spy(Limits {
        ring: 3,
        ..Limits::DEFAULT
    });
    // Genesis's inputs, archived as they go.
    let (p, a) = fx.funded(10_000);
    let mut archive = fx.archive.clone();
    // Three inputs the host does not archive yet: all three stay in the ring.
    for i in 0..3 {
        reserve(&mut fx, p, a, 10 + i).unwrap();
    }
    let h = hot(&mut fx);
    for seq in h.archived + 1..=h.inputs {
        assert!(
            fx.driver.port_mut().entry(seq).unwrap().is_some(),
            "input {seq} was trimmed before it was archived"
        );
    }
    // The fourth is refused: backpressure, nothing committed.
    let rows = fx.driver.port().rows();
    assert_eq!(
        reserve(&mut fx, p, a, 99).err(),
        Some(DriveError::Refused(AgentRefusal::RingFull))
    );
    assert_eq!(fx.driver.port().rows(), rows);
    // The replica ran it: the host rebuilds from its archive and the Agent's ring, archives, and
    // the Agent takes inputs again.
    let port = std::mem::replace(fx.driver.port_mut(), Spy::new());
    let (mut driver, mut replica) =
        Driver::recover(port, keys(), genesis(), op(0), 1, &mut archive).unwrap();
    assert_eq!(archive.len() as u64, driver.seq());
    driver.archived();
    let ctx = fx.ctx(p);
    replica.reserve(&ctx, a, usd(), Minor::new(99)).unwrap();
    let c = driver.sync(&mut replica, Vec::new()).unwrap().unwrap();
    assert_eq!(c.summaries, vec![Summary::Accepted]);
    let h = driver.port_mut().hot().unwrap().unwrap();
    assert_eq!(
        h.archived,
        h.inputs - 1,
        "the acknowledgement trimmed the ring"
    );
    assert!(driver.port_mut().entry(h.archived).unwrap().is_none());
}

#[test]
fn req_cs_7_a_full_outbox_refuses_until_deliveries_are_recorded() {
    let mut fx = spy(Limits {
        outbox: 3,
        ..Limits::DEFAULT
    });
    let (p, a) = fx.funded(10_000);
    let mut full = false;
    for i in 0..20 {
        match reserve(&mut fx, p, a, 10 + i) {
            Ok(_) => {
                fx.archive.push(fx.driver.last_entry.clone().unwrap());
                fx.driver.archived();
            }
            Err(DriveError::Refused(AgentRefusal::OutboxFull)) => {
                full = true;
                break;
            }
            Err(e) => panic!("{e:?}"),
        }
    }
    assert!(full, "the outbox's bound refused an input");
    // The replica ran the refused input: the host rebuilds, records every delivery the
    // recipients acknowledged, and the Agent takes inputs again.
    let port = std::mem::replace(fx.driver.port_mut(), Spy::new());
    let mut archive = fx.archive.clone();
    let (mut driver, mut replica) =
        Driver::recover(port, keys(), genesis(), op(0), 1, &mut archive).unwrap();
    driver.archived();
    let deliveries: Vec<([u8; 32], u64)> = replica
        .state()
        .operations()
        .outbox()
        .map(|n| (n.id.0, 50))
        .collect();
    assert!(!deliveries.is_empty());
    driver.sync(&mut replica, deliveries).unwrap().unwrap();
    driver.archived();
    let ctx = fx.ctx(p);
    replica.reserve(&ctx, a, usd(), Minor::new(1)).unwrap();
    let c = driver.sync(&mut replica, Vec::new()).unwrap().unwrap();
    assert_eq!(c.summaries, vec![Summary::Accepted]);
}

// T-005-08 ------------------------------------------------------------------------------------

#[test]
fn inv_18_a_retry_after_an_unknown_outcome_changes_the_book_once() {
    let mut fx = spy(Limits::DEFAULT);
    let (p, a) = fx.funded(10_000);
    let before = hot(&mut fx);
    // The Agent commits, and the answer is lost (a 503 after the run): the driver sends the same
    // envelope again and the Agent answers it as a duplicate.
    fx.driver.port_mut().inner.fail_next = Some(true);
    let c = reserve(&mut fx, p, a, 100).unwrap().unwrap();
    assert_eq!(c.summaries, vec![Summary::Accepted]);
    let after = hot(&mut fx);
    assert_eq!(after.inputs, before.inputs + 1, "exactly one state change");
    assert_eq!(after.roots(), c.roots);
}

// T-005-09 ------------------------------------------------------------------------------------

#[test]
fn req_rt_3_replaying_the_committed_inputs_from_genesis_reproduces_every_root() {
    let mut fx = Fx::native();
    let (p, a) = fx.funded(10_000);
    let mut archive = fx.archive.clone();
    for i in 0..6 {
        reserve(&mut fx, p, a, 10 + i).unwrap();
        archive.push(fx.driver.last_entry.clone().unwrap());
        fx.driver.archived();
    }
    // `replay` checks every entry's roots on the way.
    let (replica, service) = replay(&keys(), genesis(), op(0), 1, &archive).unwrap();
    let h = hot(&mut fx);
    assert_eq!(archive.last().unwrap().roots, h.roots());
    assert_eq!(
        replica.book().store().inner().roots().kernel,
        fx.replica.book().store().inner().roots().kernel
    );
    // INV-6: the service tree rebuilt from the committed inputs is the Agent's.
    assert_eq!(service.root(), h.service_root);
}

#[test]
fn nfr_4_every_crash_point_recovers_with_no_lost_acknowledged_operation() {
    let mut fx = spy(Limits::DEFAULT);
    let (p, a) = fx.funded(10_000);
    let mut archive = fx.archive.clone();
    let mut acknowledged = 0u64;
    // Crash points of one input, each followed by a host restart: (0) the Agent never ran it;
    // (1) the Agent committed it and the host died before archiving it; (2) the host archived it
    // and died before acknowledging it.
    for crash in 0..3 {
        let ctx = fx.ctx(p);
        fx.replica.reserve(&ctx, a, usd(), Minor::new(7)).unwrap();
        if crash == 0 {
            fx.driver.port_mut().inner.fail_next = Some(false);
        }
        let r = fx.sync();
        match crash {
            0 => assert!(matches!(
                r,
                Err(DriveError::Port(PortError::Unavailable(_)))
            )),
            1 => {
                r.unwrap();
            }
            _ => {
                r.unwrap();
                acknowledged += 1;
                archive.push(fx.driver.last_entry.clone().unwrap());
            }
        }
        // Restart: rebuild from the archive and whatever the Agent's ring holds beyond it.
        let port = std::mem::replace(fx.driver.port_mut(), Spy::new());
        let (driver, replica) =
            Driver::recover(port, keys(), genesis(), op(0), 1, &mut archive).unwrap();
        fx.driver = driver;
        fx.replica = replica;
        fx.driver.archived();
    }
    // Every acknowledged input is in the book, and the book is the Agent's.
    let h = hot(&mut fx);
    assert_eq!(archive.len() as u64, h.inputs);
    assert!(acknowledged >= 1);
    let c = reserve(&mut fx, p, a, 1).unwrap().unwrap();
    assert_eq!(c.summaries, vec![Summary::Accepted]);
    assert_eq!(fx.driver.port().inner.origin, DRIVER);
}
