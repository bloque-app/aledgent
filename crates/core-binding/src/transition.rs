// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The Agent's transition (`T-005-03`–`T-005-07`): one invocation in, one committed state or
//! nothing, and a reply.
//!
//! This is the whole of what the core Agent does, written against a [`Rows`] store so that it
//! runs the same inside the VOS actor (`agents/core`, over its Linear storage rows) and natively
//! (the host's and the conformance suite's native port). The actor is a thin shell around
//! [`transition`].
//!
//! Every write is buffered and applied only once the transition has succeeded, so an invocation
//! the Agent refuses leaves its state exactly as it was. A panic inside the library aborts the
//! invocation, and VOS commits nothing either.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use ledger_core::Ledger;
use ledger_core::anchor::clerk::ClerkSigner;
use ledger_core::call::{Call, Summary};
use ledger_core::export::canon;
use ledger_core::leaves::kind;
use ledger_core::lifecycle::LifecycleConfig;
use ledger_core::lifecycle::keys::NotificationId;
use ledger_core::state::{LeafEntry, State};
use ledger_kernel::book::Book;
use ledger_kernel::keys::KeyRing;
use ledger_kernel::store::{LeafStore, SubRoots};

use crate::envelope::{
    AgentRefusal, BatchEnvelope, BlobKey, Committed, Envelope, GenesisEnvelope, InputEnvelope,
    MAX_BATCH_INPUTS, MAX_BATCH_PACKS, Pack, Private, Reply, decode, encode,
};
use crate::hot::{self, CONFIG, HOT, Hot, LAYOUT, MAX_BUDGET, RingEntry};
use crate::service::{ServiceRefusal, ServiceWitness, root_of};
use crate::vault::PrivateVault;
use crate::witness::{KernelWitness, WitnessRefusal, WitnessStore};

/// The Agent's storage rows.
pub trait Rows {
    /// One row.
    fn get(&self, key: &[u8]) -> Option<Vec<u8>>;
    /// Write one row.
    fn put(&mut self, key: &[u8], value: Vec<u8>);
    /// Remove one row.
    fn remove(&mut self, key: &[u8]);
}

/// Rows in memory: the native Agent, and tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemRows(pub BTreeMap<Vec<u8>, Vec<u8>>);

impl Rows for MemRows {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        self.0.get(key).cloned()
    }
    fn put(&mut self, key: &[u8], value: Vec<u8>) {
        self.0.insert(key.to_vec(), value);
    }
    fn remove(&mut self, key: &[u8]) {
        self.0.remove(key);
    }
}

/// What one invocation brings besides its message.
pub struct Invocation<'a> {
    /// The VOS principal the invocation is from (`ctx.agent_origin()`).
    pub origin: Option<[u8; 32]>,
    /// The slot VOS observed (`ctx.agent_observed_slot()`).
    pub slot: Option<u64>,
    /// The message: an encoded [`Envelope`].
    pub message: &'a [u8],
    /// The caller blobs, by content (`ctx.invocation_blob`).
    pub blob: &'a dyn Fn(&BlobKey) -> Option<Vec<u8>>,
    /// The code generation compiled into the code that runs this transition (`T-037-15`): the
    /// guest's `code_generation()`, the record `ledger_core::code::generation_of` reads. The fence
    /// compares it with the writer's state at every input ([`crate::fence`]).
    pub generation: u64,
}

/// Buffered writes, applied only when the transition succeeds.
#[derive(Default)]
pub(crate) struct Writes {
    puts: BTreeMap<Vec<u8>, Option<Vec<u8>>>,
}

impl Writes {
    pub(crate) fn put(&mut self, key: Vec<u8>, value: Vec<u8>) {
        self.puts.insert(key, Some(value));
    }
    pub(crate) fn remove(&mut self, key: Vec<u8>) {
        self.puts.insert(key, None);
    }
    fn apply(self, rows: &mut dyn Rows) {
        for (k, v) in self.puts {
            match v {
                Some(v) => rows.put(&k, v),
                None => rows.remove(&k),
            }
        }
    }
    /// Take `later`'s writes over these.
    fn merge(&mut self, later: Writes) {
        self.puts.extend(later.puts);
    }
}

/// The Agent's rows with a batch's buffered writes over them: what each input of a batch reads,
/// so that it sees the writes of the inputs before it (`T-031-02`). Nothing reaches the rows
/// until the batch ends.
struct Overlay<'a> {
    rows: &'a dyn Rows,
    writes: Writes,
}

impl Rows for Overlay<'_> {
    fn get(&self, key: &[u8]) -> Option<Vec<u8>> {
        match self.writes.puts.get(key) {
            Some(v) => v.clone(),
            None => self.rows.get(key),
        }
    }
    fn put(&mut self, key: &[u8], value: Vec<u8>) {
        self.writes.put(key.to_vec(), value);
    }
    fn remove(&mut self, key: &[u8]) {
        self.writes.remove(key.to_vec());
    }
}

fn refused(r: AgentRefusal) -> Reply {
    Reply::Refused(r)
}

fn blob(inv: &Invocation<'_>, key: &BlobKey, what: &str) -> Result<Vec<u8>, AgentRefusal> {
    (inv.blob)(key).ok_or_else(|| AgentRefusal::Malformed(alloc::format!("no {what} blob")))
}

fn read_hot(rows: &dyn Rows) -> Result<Option<Hot>, AgentRefusal> {
    rows.get(HOT)
        .map(|b| hot::decode_hot(&b).map_err(AgentRefusal::Malformed))
        .transpose()
}

fn service_refusal(r: ServiceRefusal) -> AgentRefusal {
    AgentRefusal::Witness(alloc::format!("service: {r:?}"))
}

/// The deliveries a witness gives: each notification's delivery leaf, recorded or proven absent
/// (`REQ-RT-4`). They are the core binding's own leaves ([`kind::DELIVERED`]), not the library's.
fn delivered_leaves(leaves: &[LeafEntry]) -> Result<BTreeMap<[u8; 32], Option<u64>>, AgentRefusal> {
    let bad = |_| AgentRefusal::Malformed(String::from("a delivery leaf"));
    let mut out = BTreeMap::new();
    for (k, key, value) in leaves {
        if *k != kind::DELIVERED {
            continue;
        }
        let id: [u8; 32] = canon::from_bytes(key).map_err(bad)?;
        let at: Option<u64> = value
            .as_deref()
            .map(canon::from_bytes::<u64>)
            .transpose()
            .map_err(bad)?;
        out.insert(id, at);
    }
    Ok(out)
}

/// Every leaf of a whole store, present: what genesis's service root is computed over.
fn all_leaves(state: &State) -> Vec<LeafEntry> {
    state
        .all_leaves()
        .into_iter()
        .map(|(k, key, value)| (k, key, Some(value)))
        .collect()
}

/// The six sub-roots of an empty kernel tree.
pub fn empty_subroots() -> SubRoots {
    ledger_kernel::store::empty_subroots()
}

fn keys_of(private: &Private) -> KeyRing {
    private.key_ring()
}

fn fault(e: impl core::fmt::Debug) -> AgentRefusal {
    AgentRefusal::Fault(alloc::format!("{e:?}"))
}

/// Run one invocation over `rows` (module documentation) and return the encoded [`Reply`].
pub fn transition(rows: &mut dyn Rows, inv: &Invocation<'_>) -> Vec<u8> {
    encode(&run(rows, inv))
}

/// [`transition`], with the reply as a value.
pub fn run(rows: &mut dyn Rows, inv: &Invocation<'_>) -> Reply {
    let envelope = match decode::<Envelope>(inv.message) {
        Ok(e) => e,
        Err(e) => return refused(AgentRefusal::Malformed(e)),
    };
    let hot = match read_hot(rows) {
        Ok(h) => h,
        Err(e) => return refused(e),
    };
    let result = match envelope {
        Envelope::Genesis(g) => genesis(rows, hot, inv, *g),
        Envelope::Input(i) => input(rows, hot, inv, i),
        Envelope::Archived { through, log_hash } => archived(rows, hot, inv, through, log_hash),
        #[cfg(feature = "mapped")]
        Envelope::Mapped(m) => crate::mapped::run(rows, inv, m),
        Envelope::Batch(b) => batch(rows, hot, inv, b),
        Envelope::StateGenesis(g) => state_genesis(hot, inv, *g),
    };
    match result {
        Ok((reply, writes)) => {
            writes.apply(rows);
            reply
        }
        Err(r) => refused(r),
    }
}

fn check_driver(hot: &Hot, inv: &Invocation<'_>) -> Result<(), AgentRefusal> {
    if inv.origin != Some(hot.driver) {
        return Err(AgentRefusal::NotDriver);
    }
    Ok(())
}

fn genesis(
    _rows: &dyn Rows,
    hot: Option<Hot>,
    inv: &Invocation<'_>,
    g: GenesisEnvelope,
) -> Result<(Reply, Writes), AgentRefusal> {
    if hot.is_some() {
        return Err(AgentRefusal::AlreadyFounded);
    }
    // The founding invocation fixes the driver: it is itself authorised by the Agent's authority
    // key (an Authority receipt), so only the Agent's owner can name the driver.
    let driver = inv.origin.ok_or(AgentRefusal::NotDriver)?;
    let l = g.limits;
    if l.budget == 0 || l.budget > MAX_BUDGET || l.ring == 0 || l.outbox == 0 {
        return Err(AgentRefusal::Malformed(String::from("the limits")));
    }
    let private: Private =
        decode(&blob(inv, &g.private, "private")?).map_err(AgentRefusal::Malformed)?;
    let witness = KernelWitness::from_bytes(&blob(inv, &g.witness, "witness")?)
        .map_err(AgentRefusal::Malformed)?;
    let store = WitnessStore::new(witness, empty_subroots(), None, 0).map_err(witness_refusal)?;
    let vault = PrivateVault::new(None, BTreeMap::new());
    let book = Book::bootstrap(store, vault, keys_of(&private))
        .map_err(|e| AgentRefusal::Fault(alloc::format!("bootstrap: {e:?}")))?;
    let lifecycle = g.genesis.lifecycle.clone();
    let (ledger, _founded) = Ledger::genesis(book, g.genesis, g.op, g.at).map_err(fault)?;
    if ledger.book().store().missed() {
        return Err(AgentRefusal::UnprovenRead);
    }
    let mut w = Writes::default();
    let config = encode(&lifecycle);
    let service_root = root_of(&all_leaves(ledger.state()));
    let notified = ledger.state().operations().outbox().count() as u64;
    let store = ledger.book().store();
    let subroots = store.subroots().map_err(fault)?;
    let audit = ledger.state().chain_head();
    let hot = Hot {
        layout: LAYOUT,
        driver,
        subroots,
        last_commit: store.last_committed().map_err(fault)?,
        next_posting: store.next_posting_seq().map_err(fault)?,
        service_root,
        audit_emitted: audit.emitted,
        audit_head: audit.head,
        audit_shipped: audit.shipped,
        inputs: 0,
        log_head: [0; 32],
        archived: 0,
        ring_sizes: Vec::new(),
        delivered: 0,
        notified,
        limits: g.limits,
        config_len: config.len() as u64,
        observed_slot: inv.slot.unwrap_or(0),
        code: g.code.code,
        generation: 0,
        construction: ledger_core::anchor::construction(),
        upgrade: None,
        authority: g.code.authority,
        authority_next: None,
        continued_at: None,
    };
    let bytes = hot.bytes();
    if bytes > hot.limits.budget {
        return Err(AgentRefusal::OverBudget {
            bytes,
            budget: hot.limits.budget,
        });
    }
    w.put(CONFIG.to_vec(), config);
    w.put(HOT.to_vec(), encode(&hot));
    Ok((
        Reply::Committed(Committed {
            seq: 0,
            summaries: Vec::new(),
            roots: hot.roots(),
            next_posting: hot.next_posting,
            log_hash: [0; 32],
            state_bytes: bytes,
        }),
        w,
    ))
}

/// A chainspec book's founding (`StateGenesisEnvelope`): the hot row and the configuration,
/// from the genesis state the host built. Nothing is executed.
fn state_genesis(
    hot: Option<Hot>,
    inv: &Invocation<'_>,
    g: crate::envelope::StateGenesisEnvelope,
) -> Result<(Reply, Writes), AgentRefusal> {
    if hot.is_some() {
        return Err(AgentRefusal::AlreadyFounded);
    }
    let driver = inv.origin.ok_or(AgentRefusal::NotDriver)?;
    let l = g.limits;
    if l.budget == 0 || l.budget > MAX_BUDGET || l.ring == 0 || l.outbox == 0 {
        return Err(AgentRefusal::Malformed(String::from("the limits")));
    }
    if g.chainspec.chunks == 0 {
        return Err(AgentRefusal::Malformed(String::from("the chainspec")));
    }
    let config = encode(&g.lifecycle);
    let s = g.state;
    let hot = Hot {
        layout: LAYOUT,
        driver,
        subroots: s.subroots,
        last_commit: s.last_commit,
        next_posting: s.next_posting,
        service_root: s.service_root,
        audit_emitted: s.audit_emitted,
        audit_head: s.audit_head,
        audit_shipped: s.audit_shipped,
        inputs: 0,
        log_head: [0; 32],
        archived: 0,
        ring_sizes: Vec::new(),
        delivered: 0,
        notified: s.notified,
        limits: g.limits,
        config_len: config.len() as u64,
        observed_slot: inv.slot.unwrap_or(0),
        code: g.code.code,
        generation: 0,
        construction: ledger_core::anchor::construction(),
        upgrade: None,
        authority: g.code.authority,
        authority_next: None,
        continued_at: None,
    };
    let bytes = hot.bytes();
    if bytes > hot.limits.budget {
        return Err(AgentRefusal::OverBudget {
            bytes,
            budget: hot.limits.budget,
        });
    }
    let mut w = Writes::default();
    w.put(CONFIG.to_vec(), config);
    w.put(HOT.to_vec(), encode(&hot));
    Ok((
        Reply::Committed(Committed {
            seq: 0,
            summaries: Vec::new(),
            roots: hot.roots(),
            next_posting: hot.next_posting,
            log_hash: [0; 32],
            state_bytes: bytes,
        }),
        w,
    ))
}

fn witness_refusal(r: WitnessRefusal) -> AgentRefusal {
    AgentRefusal::Witness(alloc::format!("{r:?}"))
}

/// Trim the ring up to `through`, if `log_hash` is that entry's.
pub(crate) fn trim(
    rows: &dyn Rows,
    hot: &mut Hot,
    w: &mut Writes,
    through: u64,
    log_hash: [u8; 32],
) -> Result<(), AgentRefusal> {
    if through <= hot.archived {
        return Ok(());
    }
    if through > hot.inputs {
        return Err(AgentRefusal::Malformed(String::from(
            "archived past the last input",
        )));
    }
    let entry = rows
        .get(&hot::ring_key(through))
        .ok_or_else(|| AgentRefusal::Malformed(String::from("no ring entry to archive")))?;
    let entry: RingEntry = decode(&entry).map_err(AgentRefusal::Malformed)?;
    if entry.log_hash() != log_hash {
        return Err(AgentRefusal::Malformed(String::from(
            "the archived entry's log hash is not the Agent's",
        )));
    }
    for seq in hot.archived + 1..=through {
        w.remove(hot::ring_key(seq));
    }
    let drop = (through - hot.archived) as usize;
    hot.ring_sizes.drain(..drop.min(hot.ring_sizes.len()));
    hot.archived = through;
    Ok(())
}

fn archived(
    rows: &dyn Rows,
    hot: Option<Hot>,
    inv: &Invocation<'_>,
    through: u64,
    log_hash: [u8; 32],
) -> Result<(Reply, Writes), AgentRefusal> {
    let mut hot = hot.ok_or(AgentRefusal::NotFounded)?;
    check_driver(&hot, inv)?;
    let mut w = Writes::default();
    trim(rows, &mut hot, &mut w, through, log_hash)?;
    w.put(HOT.to_vec(), encode(&hot));
    Ok((
        Reply::Archived {
            through: hot.archived,
        },
        w,
    ))
}

fn input(
    rows: &dyn Rows,
    hot: Option<Hot>,
    inv: &Invocation<'_>,
    i: InputEnvelope,
) -> Result<(Reply, Writes), AgentRefusal> {
    let mut hot = hot.ok_or(AgentRefusal::NotFounded)?;
    check_driver(&hot, inv)?;
    if hot.layout == crate::hot::LAYOUT_MAPPED {
        return Err(AgentRefusal::Malformed(String::from(
            "a witnessed input to a layout-3 Agent",
        )));
    }
    let mut w = Writes::default();
    if let Some((through, log_hash)) = i.archived {
        trim(rows, &mut hot, &mut w, through, log_hash)?;
    }
    // Exactly once (`INV-18`): an input the Agent applied answers as a duplicate while its entry
    // is in the ring.
    if i.seq <= hot.inputs {
        if let Some(entry) = rows.get(&hot::ring_key(i.seq)) {
            let entry: RingEntry = decode(&entry).map_err(AgentRefusal::Malformed)?;
            return Ok((
                Reply::Duplicate {
                    seq: i.seq,
                    log_hash: entry.log_hash(),
                },
                Writes::default(),
            ));
        }
        return Err(AgentRefusal::OutOfOrder {
            expected: hot.inputs + 1,
        });
    }
    if i.seq != hot.inputs + 1 {
        return Err(AgentRefusal::OutOfOrder {
            expected: hot.inputs + 1,
        });
    }
    // The fence (`T-037-15`, `crate::fence`): at every input, before anything of it runs.
    let way_out = crate::fence::way_out_input(&i.calls, i.deliveries.len())?;
    let mut code = crate::fence::WriterCode::of(&hot);
    crate::fence::admit(inv.generation, &code, i.seq, way_out)?;
    // Backpressure (`T-005-07`): nothing more until the driver archives.
    if hot.inputs - hot.archived >= hot.limits.ring {
        return Err(AgentRefusal::RingFull);
    }
    if let Some(c) = i
        .calls
        .iter()
        .find(|c| matches!(c, Call::Unsupported { .. }))
    {
        return Err(AgentRefusal::Unsupported(String::from(c.method())));
    }

    let private: Private =
        decode(&blob(inv, &i.private, "private")?).map_err(AgentRefusal::Malformed)?;
    let witness = KernelWitness::from_bytes(&blob(inv, &i.witness, "witness")?)
        .map_err(AgentRefusal::Malformed)?;
    let store = WitnessStore::new(witness, hot.subroots, hot.last_commit, hot.next_posting)
        .map_err(witness_refusal)?;
    let openings = PrivateVault::decode_rows(&private.openings).map_err(AgentRefusal::Malformed)?;
    let vault = PrivateVault::new(hot.last_commit, openings);
    let book = match Book::open(store, vault, keys_of(&private)) {
        Ok(b) => b,
        Err(e) => return Err(AgentRefusal::Fault(alloc::format!("open: {e:?}"))),
    };
    // The service leaves the input touches, proven against the Agent's service root, as a
    // partial store that refuses anything else (`crate::service`).
    let service: ServiceWitness =
        decode(&blob(inv, &i.service, "service")?).map_err(AgentRefusal::Malformed)?;
    service.verify(&hot.service_root).map_err(service_refusal)?;
    if service.whole.contains(&kind::DELIVERED) {
        return Err(AgentRefusal::Malformed(String::from(
            "deliveries witnessed whole",
        )));
    }
    let mut delivered_at = delivered_leaves(&service.leaves)?;
    let library: Vec<LeafEntry> = service
        .leaves
        .iter()
        .filter(|(k, _, _)| *k != kind::DELIVERED)
        .cloned()
        .collect();
    let mut state = State::partial(&hot.audit());
    state
        .load_leaves(&library, &service.whole)
        .map_err(|e| AgentRefusal::Witness(alloc::format!("service: {e:?}")))?;
    drop(library);
    let config: LifecycleConfig = decode(
        &rows
            .get(CONFIG)
            .ok_or_else(|| AgentRefusal::Malformed(String::from("no configuration")))?,
    )
    .map_err(AgentRefusal::Malformed)?;
    // An input that only records deliveries shrinks the backlog: it is never refused for it.
    if !i.calls.is_empty() && hot.notified.saturating_sub(hot.delivered) >= hot.limits.outbox {
        return Err(AgentRefusal::OutboxFull);
    }
    let mut ledger = Ledger::from_parts(book, state, config);
    // What a checkpoint this input creates binds (`T-037-03`): the code that executes it, as the
    // code map names it (`crate::fence`), and the hot row's mode. A way out is decided against the
    // input's own number (`T-037-15`).
    ledger.set_code(code.code_for(i.seq), hot.authority);
    ledger.set_input(i.seq);
    let clerk_ref = private.clerk.as_ref().map(|c| c as &dyn ClerkSigner);
    let mut summaries = Vec::with_capacity(i.calls.len());
    for c in &i.calls {
        let s = crate::calls::apply(&mut ledger, c, clerk_ref);
        if ledger.book().store().missed() || ledger.state().missed() {
            return Err(AgentRefusal::UnprovenRead);
        }
        if let Summary::Failed(e) = &s {
            return Err(AgentRefusal::Fault(e.clone()));
        }
        summaries.push(s);
    }
    // The fence's last word, then what the input did to the writer's code (`T-037-15`).
    crate::fence::settle(inv.generation, &mut code, i.seq, ledger.take_way_out())?;
    code.store(&mut hot);
    // Deliveries: recorded once, for notifications the core committed (`REQ-RT-4`). Both the
    // notification and its delivery leaf must be witnessed.
    let mut delivered = Vec::new();
    for (id, at) in &i.deliveries {
        let known = ledger
            .state()
            .operations()
            .notification(&NotificationId(*id))
            .is_some();
        let slot = delivered_at.get_mut(id).ok_or(AgentRefusal::UnprovenRead)?;
        if known && slot.is_none() {
            *slot = Some(*at);
            hot.delivered += 1;
            delivered.push((*id, *at));
        }
    }
    if ledger.state().missed() {
        return Err(AgentRefusal::UnprovenRead);
    }
    hot.notified += ledger.state().operations().outbox_created() as u64;

    // The service root after, from the same proof, over every leaf given or created.
    let mut after = ledger.state().witnessed_leaves();
    after.extend(delivered_at.iter().map(|(id, at)| {
        (
            kind::DELIVERED,
            canon::to_bytes(id),
            at.map(|t| canon::to_bytes(&t)),
        )
    }));
    hot.service_root = service.root_after(&after).map_err(service_refusal)?;
    drop(after);
    let store = ledger.book().store();
    hot.subroots = store.subroots().map_err(fault)?;
    hot.last_commit = store.last_committed().map_err(fault)?;
    hot.next_posting = store.next_posting_seq().map_err(fault)?;
    let audit = ledger.state().chain_head();
    hot.audit_emitted = audit.emitted;
    hot.audit_head = audit.head;
    hot.audit_shipped = audit.shipped;
    let entry = RingEntry {
        seq: i.seq,
        code: hot.code,
        calls: i.calls,
        deliveries: delivered,
        roots: hot.roots(),
        next_posting: hot.next_posting,
        prev: hot.log_head,
    };
    let log_hash = entry.log_hash();
    let entry = encode(&entry);
    hot.ring_sizes.push(entry.len() as u64);
    w.put(hot::ring_key(i.seq), entry);
    hot.inputs = i.seq;
    hot.log_head = log_hash;
    hot.observed_slot = inv.slot.unwrap_or(hot.observed_slot);
    let bytes = hot.bytes();
    if bytes > hot.limits.budget {
        return Err(AgentRefusal::OverBudget {
            bytes,
            budget: hot.limits.budget,
        });
    }
    w.put(HOT.to_vec(), encode(&hot));
    Ok((
        Reply::Committed(Committed {
            seq: i.seq,
            summaries,
            roots: hot.roots(),
            next_posting: hot.next_posting,
            log_hash,
            state_bytes: bytes,
        }),
        w,
    ))
}

/// A batch (`T-031-02`, [`BatchEnvelope`]): each input applied as [`input`] applies it alone, over
/// the writes of the inputs before it, with a prefix commit. The first refusal ends the batch:
/// the inputs before it commit, it answers its refusal, and the rest answer
/// [`AgentRefusal::Skipped`]. Only a batch the Agent cannot read at all, or not from its driver,
/// is refused whole.
fn batch(
    rows: &dyn Rows,
    hot: Option<Hot>,
    inv: &Invocation<'_>,
    b: BatchEnvelope,
) -> Result<(Reply, Writes), AgentRefusal> {
    check_driver(&hot.ok_or(AgentRefusal::NotFounded)?, inv)?;
    let n = b.inputs.len();
    let covered = b
        .packs
        .iter()
        .try_fold(0usize, |t, (_, c)| t.checked_add(*c as usize));
    if n == 0
        || n > MAX_BATCH_INPUTS
        || b.packs.len() > MAX_BATCH_PACKS
        || covered != Some(n)
        || b.packs.iter().any(|(_, c)| *c == 0)
    {
        return Err(AgentRefusal::Malformed(String::from("the batch's shape")));
    }
    let mut overlay = Overlay {
        rows,
        writes: Writes::default(),
    };
    let mut replies = Vec::with_capacity(n);
    let mut stopped = false;
    let mut inputs = b.inputs.into_iter();
    for (key, count) in b.packs {
        let run: Vec<InputEnvelope> = inputs.by_ref().take(count as usize).collect();
        if stopped {
            replies.extend(run.iter().map(|_| refused(AgentRefusal::Skipped)));
            continue;
        }
        // One pack in the heap at a time: fetched here, its blobs split out of it before any of
        // its inputs runs, and each blob handed to the last input that asks for it, not copied
        // (`PackBlobs`).
        let blobs = match blob(inv, &key, "pack").and_then(|bytes| PackBlobs::of(&bytes, &run)) {
            Ok(b) => b,
            Err(r) => {
                stop_run(&mut replies, run.len(), r);
                stopped = true;
                continue;
            }
        };
        let fetch = |k: &BlobKey| blobs.fetch(k);
        let one = Invocation {
            origin: inv.origin,
            slot: inv.slot,
            message: inv.message,
            blob: &fetch,
            generation: inv.generation,
        };
        for i in run {
            if stopped {
                replies.push(refused(AgentRefusal::Skipped));
                continue;
            }
            let applied = read_hot(&overlay).and_then(|hot| input(&overlay, hot, &one, i));
            match applied {
                Ok((reply, w)) => {
                    overlay.writes.merge(w);
                    replies.push(reply);
                }
                Err(r) => {
                    replies.push(refused(r));
                    stopped = true;
                }
            }
        }
    }
    let writes = overlay.writes;
    Ok((Reply::Batch(replies), writes))
}

/// A pack's blobs, owned one by one, for the run of inputs it carries (`T-031-02`, `REQ-RT-1`).
///
/// The guest's heap holds the pack while its inputs run. Copying each input's blobs out of the
/// pack's bytes held them twice while the input ran: at a full-size book's depth a seed chunk
/// brings about 45 KB of witness, a pack each, and four such inputs an invocation took the guest
/// past its 256 KiB (`verify-genesis` of a full-size chainspec, 2026-10-11). Here the pack's
/// bytes are dropped once its blobs are split out, before any input runs, and the last fetch of a
/// blob takes it: an input's blobs are in the heap once, and leave it with the input.
///
/// A fetch answers what the copy answered: the first entry under the key, of the key's length,
/// every time. Only those entries are copied out of the pack, one for each key the run's inputs
/// name, whatever else the pack holds; each is copied again for every fetch of its key but the
/// last the run's inputs can make, which takes it.
struct PackBlobs {
    blobs: core::cell::RefCell<Vec<PackBlob>>,
}

/// For a key the run's inputs name and the pack holds: how many more fetches of it the inputs
/// can make, and its blob (the pack's first entry under the key, of the key's length) until the
/// last of them.
type PackBlob = (BlobKey, u32, Option<Vec<u8>>);

impl PackBlobs {
    fn of(bytes: &[u8], run: &[InputEnvelope]) -> Result<PackBlobs, AgentRefusal> {
        let pack = decode::<Pack<'_>>(bytes).map_err(AgentRefusal::Malformed)?;
        let mut demand: Vec<(BlobKey, u32)> = Vec::new();
        for k in run.iter().flat_map(|i| [i.witness, i.service, i.private]) {
            match demand.iter_mut().find(|(d, _)| *d == k) {
                Some((_, n)) => *n += 1,
                None => demand.push((k, 1)),
            }
        }
        // A key the pack does not hold stays out: its fetch answers nothing, as before.
        let blobs = demand
            .into_iter()
            .filter_map(|(k, n)| {
                pack.blobs
                    .iter()
                    .find(|(pk, b)| *pk == k && b.len() as u64 == k.len)
                    .map(|(_, b)| (k, n, Some(b.to_vec())))
            })
            .collect();
        Ok(PackBlobs {
            blobs: core::cell::RefCell::new(blobs),
        })
    }

    fn fetch(&self, k: &BlobKey) -> Option<Vec<u8>> {
        let mut blobs = self.blobs.borrow_mut();
        let (_, left, slot) = blobs.iter_mut().find(|(pk, _, _)| pk == k)?;
        *left = left.saturating_sub(1);
        if *left == 0 {
            slot.take()
        } else {
            slot.clone()
        }
    }
}

/// A run whose pack cannot be read: its first input answers why, the rest are skipped.
fn stop_run(replies: &mut Vec<Reply>, len: usize, why: AgentRefusal) {
    replies.push(refused(why));
    replies.extend((1..len).map(|_| refused(AgentRefusal::Skipped)));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u8, len: u64) -> BlobKey {
        BlobKey { hash: [n; 32], len }
    }

    fn input(witness: BlobKey, service: BlobKey, private: BlobKey) -> InputEnvelope {
        InputEnvelope {
            seq: 1,
            calls: Vec::new(),
            deliveries: Vec::new(),
            archived: None,
            witness,
            service,
            private,
        }
    }

    /// What a fetch answered before `PackBlobs`: a copy of the first entry under the key, of the
    /// key's length.
    fn copied(pack: &Pack<'_>, k: &BlobKey) -> Option<Vec<u8>> {
        pack.blobs
            .iter()
            .find(|(pk, b)| pk == k && b.len() as u64 == k.len)
            .map(|(_, b)| b.to_vec())
    }

    /// `REQ-RT-1`, `T-036-34`: whatever the pack, every fetch the run's inputs make answers what
    /// the copy answered. Here one entry is shared by two inputs (the host never builds such a
    /// pack, but the Agent must not depend on it), a key is held twice with different bytes, a key
    /// has a first entry of the wrong length, an entry no input names, and an input names a key
    /// the pack does not hold. Handing a blob over at its first fetch instead of its last fails
    /// the shared entry's second fetch.
    #[test]
    fn req_rt_1_a_packs_blobs_answer_what_a_copy_answered() {
        let (shared, twice, short, unused, missing) =
            (key(1, 3), key(2, 2), key(3, 4), key(4, 1), key(5, 1));
        let (w1, w2, s1, s2) = (key(6, 1), key(7, 1), key(8, 1), key(9, 1));
        let entries: Vec<(BlobKey, &[u8])> = alloc::vec![
            (shared, b"abc"),
            (twice, b"xy"),
            (twice, b"zz"),
            (short, b"no"),
            (short, b"long"),
            (unused, b"u"),
            (w1, b"1"),
            (w2, b"2"),
            (s1, b"3"),
            (s2, b"4"),
        ];
        let bytes = encode(&Pack { blobs: entries });
        let pack = decode::<Pack<'_>>(&bytes).unwrap();
        let run = [
            input(w1, s1, shared),
            input(w2, twice, shared),
            input(short, s2, shared),
        ];
        let blobs = PackBlobs::of(&bytes, &run).unwrap();
        // Copied out: one entry for each key the inputs name, nothing else (not `unused`).
        assert_eq!(blobs.blobs.borrow().len(), 7);
        // The fetches the inputs make, in their order (private, witness, service).
        for k in [shared, w1, s1, shared, w2, twice, shared, short, s2] {
            assert_eq!(blobs.fetch(&k), copied(&pack, &k), "fetch of {}", k.hash[0]);
        }
        // A key the pack does not hold answers nothing, as before.
        assert_eq!(blobs.fetch(&missing), None);
        // Every blob the inputs named has left the heap with its last fetch.
        assert!(blobs.blobs.borrow().iter().all(|(_, _, b)| b.is_none()));
    }
}
