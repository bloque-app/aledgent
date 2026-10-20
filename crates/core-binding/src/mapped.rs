// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The Agent's transition over its own committed maps (`F-031` lever 2, `DEC-22`, `T-031-07`).
//!
//! Layout 3. The kernel's six trees and the service tree are committed maps in the Agent's rows
//! ([`crate::committed`]), with the leaves in the encodings they have everywhere else: the
//! kernel's leaf contents under CipherClerk's hashes, the service's canonical `(kind, key, value)`
//! under [`ledger_core::delta::library_leaf_hash`]. The roots are the roots layout 2 reports, so
//! the driver's comparison with its replica does not change.
//!
//! What an invocation costs follows what it touches:
//!
//! - **The kernel.** The library runs over [`RowStore`], which reads each leaf it asks for (one
//!   row) and writes each leaf it changes with its path. No witness, no proof.
//! - **The service.** The host names the service leaves the input reads and writes, keys only
//!   ([`ServiceKeys`]): the Agent loads exactly those from its map into a partial store, and a run
//!   that reads anything else commits nothing, as in layout 2. After the run it writes the leaves
//!   whose value changed, and nothing else.
//! - **Private inputs** stay a caller blob: the key custody's answers, the clerk's signatures (`F-035`: never a key), the openings of the
//!   accounts touched (`REQ-RT-6`). Nothing private enters a row.
//!
//! Trade-off, stated plainly: on a Local Agent the rows are part of the Agent's image, which VOS
//! moves whole on every invocation and caps at 4 MiB and 16 384 rows. A book grows into those
//! ceilings within a few hundred operations. The layout pays on a Shared Agent over VOS's external
//! state blocks (`DEP-1`, `DEP-11`), where only the rows touched move.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::rc::Rc;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::RefCell;

use ledger_core::Ledger;
use ledger_core::account::Time;
use ledger_core::anchor::clerk::ClerkSigner;
use ledger_core::call::{Call, Summary};
use ledger_core::delta::{library_key, library_leaf_hash};
use ledger_core::export::canon;
use ledger_core::id::OperationId;
use ledger_core::leaves::kind;
use ledger_core::ledger::Genesis;
use ledger_core::lifecycle::LifecycleConfig;
use ledger_core::lifecycle::keys::NotificationId;
use ledger_core::state::{LeafEntry, State};
use ledger_kernel::book::Book;
use ledger_kernel::store::LeafStore;
use serde::{Deserialize, Serialize};

use crate::committed::{self, CommittedMap, EmptyChain};
use crate::envelope::{AgentRefusal, BlobKey, Committed, Private, Reply, decode, encode};
use crate::hot::{self, CONFIG, HOT, Hot, Limits, MAX_BUDGET, RingEntry};
use crate::rowstore::{Overlay, RowStore, Shared};
use crate::service::probe;
use crate::transition::{Invocation, Rows, Writes};
use crate::vault::PrivateVault;

pub use crate::hot::LAYOUT_MAPPED;

/// The service tree's map: after the kernel's six.
pub const SERVICE_TREE: u8 = 6;

/// The service leaves an input reads and writes, by key (module documentation).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceKeys {
    /// Each leaf's kind and canonical key.
    pub leaves: Vec<(u8, Vec<u8>)>,
    /// The kinds the input reads whole.
    pub whole: Vec<u8>,
}

/// The founding invocation of a layout-3 Agent.
#[derive(Clone, Serialize, Deserialize)]
pub struct MappedGenesis {
    /// The reviewed genesis configuration (`REQ-AS-2`).
    pub genesis: Genesis,
    /// Genesis's operation reference.
    pub op: OperationId,
    /// Genesis's time.
    pub at: Time,
    /// The Agent's bounds.
    pub limits: Limits,
    /// The private inputs (the key-ring root).
    pub private: BlobKey,
    /// The code it is founded under (`T-037-02`).
    pub code: crate::envelope::FoundingCode,
}

/// One input to a layout-3 Agent: as [`crate::envelope::InputEnvelope`], with the service keys
/// in place of the two witnesses.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MappedInput {
    /// Its input number: the Agent's last plus one.
    pub seq: u64,
    /// The library calls.
    pub calls: Vec<Call>,
    /// Deliveries acknowledged by their recipients (`REQ-RT-4`).
    pub deliveries: Vec<([u8; 32], Time)>,
    /// The delta ring's archive acknowledgement.
    pub archived: Option<(u64, [u8; 32])>,
    /// The service leaves it touches.
    pub service: ServiceKeys,
    /// The private-input blob.
    pub private: BlobKey,
}

/// A layout-3 invocation.
#[derive(Clone, Serialize, Deserialize)]
pub enum Mapped {
    /// Found the book.
    Genesis(Box<MappedGenesis>),
    /// Apply one input.
    Input(MappedInput),
}

/// A service leaf's stored bytes: its canonical key, length-prefixed, then its canonical value.
fn stored(key: &[u8], value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + key.len() + value.len());
    out.extend_from_slice(&(key.len() as u32).to_be_bytes());
    out.extend_from_slice(key);
    out.extend_from_slice(value);
    out
}

fn parse(bytes: &[u8]) -> Result<(Vec<u8>, Vec<u8>), AgentRefusal> {
    let bad = || AgentRefusal::Malformed(String::from("a service leaf row"));
    let n = u32::from_be_bytes(
        bytes
            .get(..4)
            .ok_or_else(bad)?
            .try_into()
            .map_err(|_| bad())?,
    ) as usize;
    let key = bytes.get(4..4 + n).ok_or_else(bad)?.to_vec();
    Ok((key, bytes[4 + n..].to_vec()))
}

fn corrupt(c: committed::Corrupt) -> AgentRefusal {
    AgentRefusal::Malformed(alloc::format!("a committed map: {}", c.0))
}

fn fault(e: impl core::fmt::Debug) -> AgentRefusal {
    AgentRefusal::Fault(alloc::format!("{e:?}"))
}

/// Write `leaves` into the service map, each to its value (`None` removes it).
fn put_service(
    rows: &mut dyn Rows,
    empty: &EmptyChain,
    leaves: &[LeafEntry],
) -> Result<(), AgentRefusal> {
    let mut map = CommittedMap::new(rows, SERVICE_TREE, empty);
    for (k, key, value) in leaves {
        let at = library_key(*k, key);
        let leaf = value
            .as_deref()
            .map(|v| (library_leaf_hash(*k, key, Some(v)), stored(key, v)));
        map.put(&at, leaf.as_ref().map(|(h, b)| (*h, b.as_slice())))
            .map_err(corrupt)?;
    }
    Ok(())
}

/// The service leaves `keys` name, and every leaf of each kind read whole, as the map holds them.
fn load_service(rows: &dyn Rows, keys: &ServiceKeys) -> Result<Vec<LeafEntry>, AgentRefusal> {
    let mut out: BTreeMap<(u8, Vec<u8>), Option<Vec<u8>>> = BTreeMap::new();
    for (k, key) in &keys.leaves {
        let value =
            match committed::get(rows, SERVICE_TREE, &library_key(*k, key)).map_err(corrupt)? {
                Some((_, bytes)) => {
                    let (stored_key, value) = parse(&bytes)?;
                    if stored_key != *key {
                        return Err(AgentRefusal::Malformed(String::from(
                            "a service key collision",
                        )));
                    }
                    Some(value)
                }
                None => None,
            };
        out.insert((*k, key.clone()), value);
    }
    for k in &keys.whole {
        for (_, bytes) in
            committed::leaves_under(rows, SERVICE_TREE, &probe(*k), 8).map_err(corrupt)?
        {
            let (key, value) = parse(&bytes)?;
            out.insert((*k, key), Some(value));
        }
    }
    Ok(out.into_iter().map(|((k, key), v)| (k, key, v)).collect())
}

fn read_hot(rows: &dyn Rows) -> Result<Option<Hot>, AgentRefusal> {
    rows.get(HOT)
        .map(|b| crate::hot::decode_hot(&b).map_err(AgentRefusal::Malformed))
        .transpose()
}

fn private_of(inv: &Invocation<'_>, key: &BlobKey) -> Result<Private, AgentRefusal> {
    let bytes =
        (inv.blob)(key).ok_or_else(|| AgentRefusal::Malformed(String::from("no private blob")))?;
    decode(&bytes).map_err(AgentRefusal::Malformed)
}

fn take(shared: Shared<'_>) -> Result<Overlay<'_>, AgentRefusal> {
    Rc::try_unwrap(shared)
        .map(RefCell::into_inner)
        .map_err(|_| AgentRefusal::Fault(String::from("the overlay is still shared")))
}

fn merge(w: &mut Writes, overlay: Overlay<'_>) {
    for (k, v) in overlay.into_writes() {
        match v {
            Some(v) => w.put(k, v),
            None => w.remove(k),
        }
    }
}

/// Run one layout-3 invocation over `rows`: the writes to apply when it commits.
pub(crate) fn run(
    rows: &dyn Rows,
    inv: &Invocation<'_>,
    m: Mapped,
) -> Result<(Reply, Writes), AgentRefusal> {
    let hot = read_hot(rows)?;
    match m {
        Mapped::Genesis(g) => genesis(rows, hot, inv, *g),
        Mapped::Input(i) => input(rows, hot, inv, i),
    }
}

fn genesis(
    rows: &dyn Rows,
    hot: Option<Hot>,
    inv: &Invocation<'_>,
    g: MappedGenesis,
) -> Result<(Reply, Writes), AgentRefusal> {
    if hot.is_some() {
        return Err(AgentRefusal::AlreadyFounded);
    }
    let driver = inv.origin.ok_or(AgentRefusal::NotDriver)?;
    let l = g.limits;
    if l.budget == 0 || l.budget > MAX_BUDGET || l.ring == 0 || l.outbox == 0 {
        return Err(AgentRefusal::Malformed(String::from("the limits")));
    }
    let private = private_of(inv, &g.private)?;
    let empty = committed::empty_chain();
    let shared: Shared<'_> = Rc::new(RefCell::new(Overlay::new(rows)));
    let store = RowStore::new(Rc::clone(&shared), &empty, None, 0);
    let book = Book::bootstrap(
        store,
        PrivateVault::new(None, BTreeMap::new()),
        private.key_ring(),
    )
    .map_err(|e| AgentRefusal::Fault(alloc::format!("bootstrap: {e:?}")))?;
    let lifecycle = g.genesis.lifecycle.clone();
    let (ledger, _) = Ledger::genesis(book, g.genesis, g.op, g.at).map_err(fault)?;
    let store = ledger.book().store();
    let subroots = store.subroots().map_err(fault)?;
    let last_commit = store.last_committed().map_err(fault)?;
    let next_posting = store.next_posting_seq().map_err(fault)?;
    let leaves: Vec<LeafEntry> = ledger
        .state()
        .all_leaves()
        .into_iter()
        .map(|(k, key, v)| (k, key, Some(v)))
        .collect();
    let notified = ledger.state().operations().outbox().count() as u64;
    let audit = ledger.state().chain_head();
    drop(ledger);
    let mut overlay = take(shared)?;
    put_service(&mut overlay, &empty, &leaves)?;
    let service_root = committed::root(&overlay, SERVICE_TREE, &empty).map_err(corrupt)?;
    let config = encode(&lifecycle);
    let hot = Hot {
        layout: LAYOUT_MAPPED,
        driver,
        subroots,
        last_commit,
        next_posting,
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
    let mut w = Writes::default();
    merge(&mut w, overlay);
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

fn input(
    rows: &dyn Rows,
    hot: Option<Hot>,
    inv: &Invocation<'_>,
    i: MappedInput,
) -> Result<(Reply, Writes), AgentRefusal> {
    let mut hot = hot.ok_or(AgentRefusal::NotFounded)?;
    if inv.origin != Some(hot.driver) {
        return Err(AgentRefusal::NotDriver);
    }
    if hot.layout != LAYOUT_MAPPED {
        return Err(AgentRefusal::Malformed(String::from(
            "a layout-3 input to another layout",
        )));
    }
    let mut w = Writes::default();
    if let Some((through, log_hash)) = i.archived {
        crate::transition::trim(rows, &mut hot, &mut w, through, log_hash)?;
    }
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
    if !i.calls.is_empty() && hot.notified.saturating_sub(hot.delivered) >= hot.limits.outbox {
        return Err(AgentRefusal::OutboxFull);
    }
    let private = private_of(inv, &i.private)?;
    let empty = committed::empty_chain();
    let shared: Shared<'_> = Rc::new(RefCell::new(Overlay::new(rows)));
    let store = RowStore::new(
        Rc::clone(&shared),
        &empty,
        hot.last_commit,
        hot.next_posting,
    );
    let openings = PrivateVault::decode_rows(&private.openings).map_err(AgentRefusal::Malformed)?;
    let vault = PrivateVault::new(hot.last_commit, openings);
    let book = Book::open(store, vault, private.key_ring())
        .map_err(|e| AgentRefusal::Fault(alloc::format!("open: {e:?}")))?;
    // The service leaves the host named, from the Agent's own map.
    let loaded = load_service(rows, &i.service)?;
    let before: BTreeMap<(u8, Vec<u8>), Option<Vec<u8>>> = loaded
        .iter()
        .map(|(k, key, v)| ((*k, key.clone()), v.clone()))
        .collect();
    let mut delivered_at: BTreeMap<[u8; 32], Option<u64>> = BTreeMap::new();
    for (k, key, value) in &loaded {
        if *k == kind::DELIVERED {
            let bad = |_| AgentRefusal::Malformed(String::from("a delivery leaf"));
            let id: [u8; 32] = canon::from_bytes(key).map_err(bad)?;
            let at = value
                .as_deref()
                .map(canon::from_bytes::<u64>)
                .transpose()
                .map_err(bad)?;
            delivered_at.insert(id, at);
        }
    }
    let library: Vec<LeafEntry> = loaded
        .into_iter()
        .filter(|(k, _, _)| *k != kind::DELIVERED)
        .collect();
    let whole: Vec<u8> = i
        .service
        .whole
        .iter()
        .copied()
        .filter(|k| *k != kind::DELIVERED)
        .collect();
    let mut state = State::partial(&hot.audit());
    state
        .load_leaves(&library, &whole)
        .map_err(|e| AgentRefusal::Witness(alloc::format!("service: {e:?}")))?;
    drop(library);
    let config: LifecycleConfig = decode(
        &rows
            .get(CONFIG)
            .ok_or_else(|| AgentRefusal::Malformed(String::from("no configuration")))?,
    )
    .map_err(AgentRefusal::Malformed)?;
    let mut ledger = Ledger::from_parts(book, state, config);
    // What a checkpoint this input creates binds (`T-037-03`): the code that executes it, as the
    // code map names it (`crate::fence`), and the hot row's mode. The input's own number, which a
    // way out is decided against (`T-037-15`), and at which a scheduled change of mode takes force
    // (`T-037-05`).
    ledger.set_code(code.code_for(i.seq), hot.authority);
    ledger.set_authority_next(hot.authority_next);
    ledger.set_input(i.seq);
    let clerk_ref = private.clerk.as_ref().map(|c| c as &dyn ClerkSigner);
    let mut summaries = Vec::with_capacity(i.calls.len());
    for c in &i.calls {
        let s = crate::calls::apply(&mut ledger, c, clerk_ref);
        if ledger.state().missed() {
            return Err(AgentRefusal::UnprovenRead);
        }
        if let Summary::Failed(e) = &s {
            return Err(AgentRefusal::Fault(e.clone()));
        }
        summaries.push(s);
    }
    // What the input did to the writer's code (`T-037-15`), decided the same by either generation.
    crate::fence::decide(&mut code, i.seq, way_out, ledger.take_way_out());
    code.store(&mut hot);
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
    // The mode in force and any scheduled change of it, as the input leaves them (`T-037-05`).
    hot.authority = ledger.authority();
    hot.authority_next = ledger.authority_next();
    // The leaves whose value changed: the only service rows the invocation writes.
    let mut after = ledger.state().witnessed_leaves();
    after.extend(delivered_at.iter().map(|(id, at)| {
        (
            kind::DELIVERED,
            canon::to_bytes(id),
            at.map(|t| canon::to_bytes(&t)),
        )
    }));
    let changed: Vec<LeafEntry> = after
        .into_iter()
        .filter(|(k, key, v)| {
            before
                .get(&(*k, key.clone()))
                .map_or(v.is_some(), |old| old != v)
        })
        .collect();
    let store = ledger.book().store();
    hot.subroots = store.subroots().map_err(fault)?;
    hot.last_commit = store.last_committed().map_err(fault)?;
    hot.next_posting = store.next_posting_seq().map_err(fault)?;
    let audit = ledger.state().chain_head();
    hot.audit_emitted = audit.emitted;
    hot.audit_head = audit.head;
    hot.audit_shipped = audit.shipped;
    drop(ledger);
    let mut overlay = take(shared)?;
    put_service(&mut overlay, &empty, &changed)?;
    hot.service_root = committed::root(&overlay, SERVICE_TREE, &empty).map_err(corrupt)?;
    merge(&mut w, overlay);
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
