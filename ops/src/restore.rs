// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The restore procedure (`T-018-02`, `NFR-4`, `REQ-RT-3`, `INV-6`, `AC-K5.1`), and
//! `opsctl restore`'s library.
//!
//! 1. **The core** from its latest backup ([`crate::backup::core`]): the `F-014` export imported
//!    into an empty deployment, whose import checks every section digest, both roots, the openings
//!    against every commitment, the audit head and the operator's `ImportBook` authority before it
//!    writes anything. Its input number `s_b` is the sidecar's.
//! 2. **Ingress** to the latest point: PostgreSQL from its base backup and WAL archive
//!    ([`crate::backup::pg`]).
//! 3. **Ordered replay**: every input the received-log holds with `core_input_seq > s_b`, in that
//!    order, through the core's boundary — the path the forwarder takes. Every core input passes
//!    through ingress (commands, ticks, delivery records, the adapter's reports), so the replay is
//!    complete. Each replayed input must come back with the input number and the reply the log
//!    recorded: the core is deterministic (`REQ-RT-3`), so any other reply is a failed restore,
//!    never a new truth. The core's journal is rebuilt from the log for inputs `1..=s_b`, so a
//!    later start can replay it from genesis like any other.
//! 4. **The leaf store**, rebuilt from the restored book's committed delta log
//!    ([`rebuild_leaf_store`], `F-006`): every commit's root checked, the final root the book's,
//!    and the store's commitment checks of `INV-6` and `INV-5` run on it.
//! 5. **The adapter**: its PostgreSQL store to the latest point, then steps that **resume from the
//!    head of the chain of entries it last accepted** ([`resume_adapter`], amendment 0040): each
//!    folds the entries the layer recorded since that head, checks them against the head the layer
//!    holds, and reports them, until it accepts the layer's head. Behind the node's 256 blocks of
//!    events, it catches up from the history endpoint. The settlement layer is the truth for
//!    settlement: anything it recorded while aledgent was down is read from it, and the chain's
//!    head shows it was read in full.
//!
//! Undecided rows — logged by ingress and not yet decided when the core failed — are left to the
//! forwarder, which sends them once ingress runs over the restored core.

use std::collections::BTreeMap;
use std::path::Path;

use ingress::boundary::{Core, CoreReply};
use ingress::inprocess::{CoreGenesis, InProcessCore, Journal};
use ingress::service::input_of;
use ingress::store::{Row, Status};
use leaf_store::rebuild::{inv_5_findings, inv_6_findings};
use leaf_store::{ArchiveConfig, BookStore, StoreConfig};
use ledger_core::Ledger;
use ledger_core::audit::AuditSink;
use ledger_core::export::Checkpoints;
use ledger_core::ledger::Ctx;
use ledger_core::ledger::operator::ImportTarget;
use ledger_kernel::book::{BindingFault, Book};
use ledger_kernel::keys::KeyRing;
use ledger_kernel::store::{LeafStore, composite_root};
use ledger_kernel::vault::VaultBackend;
use serde::{Deserialize, Serialize};
use settle::adapter::{Adapter, CHAIN_ALERT};
use settle::chain::Chain;
use settle::link::Transport;

use crate::Error;
use crate::backup::core::CoreBundle;

/// What the ordered replay did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayReport {
    /// The backup point `s_b`.
    pub backup_point: u64,
    /// Inputs replayed after it.
    pub replayed: u64,
    /// The last input number the restored core holds.
    pub last_input: u64,
    /// Rows the log holds undecided, left to the forwarder.
    pub undecided: u64,
    /// The posting sequence the imported book continued from.
    pub imported_sequence: u64,
    /// How many delta-log commits the import made: the restored log's first commits, which hold
    /// the export's leaves in chunks, not the history before it.
    #[serde(default)]
    pub import_commits: u64,
}

/// The decided rows of the received-log, by input number, checked gapless from 1: an input the
/// core applied whose row the log lost would be an acknowledged input lost (`NFR-4`).
pub fn decided_inputs(rows: &[Row]) -> Result<BTreeMap<u64, &Row>, Error> {
    let mut by_seq = BTreeMap::new();
    for r in rows.iter().filter(|r| r.status == Status::Decided) {
        let seq = r
            .core_input_seq
            .ok_or_else(|| Error::Restore("a decided row has no input number".into()))?;
        if by_seq.insert(seq, r).is_some() {
            return Err(Error::Restore(format!("input {seq} is held by two rows")));
        }
    }
    for (i, seq) in by_seq.keys().enumerate() {
        if *seq != i as u64 + 1 {
            return Err(Error::Restore(format!(
                "the received-log has no row for input {}",
                i + 1
            )));
        }
    }
    Ok(by_seq)
}

/// Rebuild the in-process core's journal from the received-log alone: every decided input, in
/// input order, from the first. A core started on it replays from genesis to the log's last input
/// (`REQ-RT-3`). The way back when no core backup can be opened (`ops/runbooks/key-loss.md`): slower
/// than a backup plus replay, and it needs the log from its first row. Returns the inputs written.
pub fn rebuild_journal<J: Journal>(rows: &[Row], journal: &mut J) -> Result<u64, Error> {
    let inputs = decided_inputs(rows)?;
    for (seq, row) in &inputs {
        let input = input_of(row).map_err(Error::Restore)?;
        journal.append(*seq, &input).map_err(Error::Restore)?;
    }
    Ok(inputs.len() as u64)
}

fn stored_reply(row: &Row) -> Result<CoreReply, Error> {
    let bytes = row
        .core_reply
        .as_deref()
        .ok_or_else(|| Error::Restore("a decided row has no reply".into()))?;
    serde_json::from_slice(bytes).map_err(|e| Error::Restore(format!("a stored reply: {e}")))
}

/// Who imports, and why: the operator with the `ImportBook` kind in the imported book.
pub struct ImportBy<'a> {
    /// The operator, the operation reference of the import, the time.
    pub ctx: Ctx,
    /// The justification recorded with the import (`REQ-AT-1`).
    pub justification: &'a [u8],
}

/// What [`restore_pass_through`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PassThroughRestored {
    /// Entries the export carried.
    pub carried: u64,
    /// Rows of the restored log that lacked their data and got it back.
    pub written: u64,
}

/// The pass-through data a core backup carried, restored onto the restored received-log
/// (`REQ-PO-5`: "an import restores it"; `REQ-RC-3`; ruling `pq8-export` (a)): each operation's
/// data back on its command row, sent by the same principal. A row holding the same data is left as
/// it is; a row holding other data, sent by another principal, or missing, fails the restore: the
/// log and the backup are not of one history. Run it after [`restore_core`], on the log `rows`
/// came from.
pub async fn restore_pass_through<L: ingress::store::Store>(
    log: &L,
    carried: &[ingress::store::Carried],
) -> Result<PassThroughRestored, Error> {
    let written = log
        .restore_pass_through(carried)
        .await
        .map_err(|e| Error::Restore(format!("the pass-through data: {e}")))?;
    Ok(PassThroughRestored {
        carried: carried.len() as u64,
        written,
    })
}

/// Steps 1 and 3 (module documentation): the core restored from `bundle` into the empty `store`
/// and `vault`, then every decided input of `rows` after the backup point replayed in order.
/// `genesis` carries the core's key root from the KMS; `factory` bootstraps a fresh book for any
/// later start; `journal` is empty and receives every input; `clerk` is the deployment's clerk key
/// (`F-017`), from its own custody, for the disclosures the replay reissues. `sink` is the audit
/// store, restored to at least the export's head.
#[allow(clippy::too_many_arguments)]
pub async fn restore_core<S, V, J>(
    bundle: CoreBundle,
    genesis: CoreGenesis,
    store: S,
    vault: V,
    factory: impl Fn() -> Result<Book<S, V>, BindingFault> + Send + 'static,
    mut journal: J,
    clerk: Option<ingress::inprocess::SharedClerk>,
    rows: &[Row],
    by: &ImportBy<'_>,
    sink: &mut dyn AuditSink,
) -> Result<(InProcessCore<S, V>, ReplayReport), Error>
where
    S: LeafStore + Send + Sync + 'static,
    V: VaultBackend + Send + Sync + 'static,
    J: Journal,
{
    let CoreBundle {
        manifest,
        export,
        sidecar,
        openings,
        ..
    } = bundle;
    let point = manifest
        .point
        .ok_or_else(|| Error::Config("a core backup names its point".into()))?;
    let s_b = sidecar.inputs;
    // Both backups must be of one history: every input the core backup holds has its row, and
    // the replies the sidecar kept are the ones the log recorded.
    let inputs = decided_inputs(rows)?;
    let last = inputs.keys().next_back().copied().unwrap_or(0);
    if last < s_b {
        return Err(Error::Restore(format!(
            "the received-log ends at input {last}, before the core backup's {s_b}: \
             restore ingress to a later point"
        )));
    }
    for (key, reply) in &sidecar.replies {
        if let Some(row) = inputs.get(&reply.input_seq)
            && (row.new.op_ref != *key || stored_reply(row)? != *reply)
        {
            return Err(Error::Restore(format!(
                "input {} differs between the core backup and the received-log",
                reply.input_seq
            )));
        }
    }
    // Step 1: import. `F-014`'s import checks a book root at a posting sequence; `F-017`'s anchors
    // commit to the kernel root only, so the root checked here is the backup's own (F-017's open
    // point, the owner's). The drill's INV-19 check reads the layer's anchors instead.
    let mut anchors = Checkpoints::new();
    anchors.record(point.sequence, export.manifest.book_root());
    let target = ImportTarget {
        store,
        vault,
        keys: KeyRing::from_root(genesis.root),
        openings,
        config: genesis.genesis.lifecycle.clone(),
    };
    let (ledger, imported) =
        Ledger::import_book(&export, target, &anchors, &by.ctx, by.justification, sink)
            .map_err(|f| Error::Export(format!("{:?}", f.error)))?;
    // The journal holds the history for later starts.
    for (seq, row) in inputs.range(..=s_b) {
        let input = input_of(row).map_err(Error::Restore)?;
        journal.append(*seq, &input).map_err(Error::Restore)?;
    }
    let import_commits = ledger
        .book()
        .store()
        .last_committed()
        .ok()
        .flatten()
        .map_or(0, |c| c + 1);
    let mut core = InProcessCore::restored(genesis, journal, factory, ledger, &sidecar)
        .map_err(Error::Restore)?;
    if let Some(c) = clerk {
        core = core.with_clerk(c);
    }
    core.start().map_err(Error::Core)?;
    // Step 3: the ordered replay.
    let mut replayed = 0;
    for (seq, row) in inputs.range(s_b + 1..) {
        let input = input_of(row).map_err(Error::Restore)?;
        let reply = core
            .apply(&input)
            .await
            .map_err(|e| Error::Core(format!("input {seq}: {e:?}")))?;
        if reply != stored_reply(row)? {
            return Err(Error::Restore(format!(
                "input {seq} replayed to another reply than the log recorded"
            )));
        }
        replayed += 1;
    }
    let undecided = rows.iter().filter(|r| r.status != Status::Decided).count() as u64;
    Ok((
        core,
        ReplayReport {
            backup_point: s_b,
            replayed,
            last_input: last,
            undecided,
            imported_sequence: imported.sequence,
            import_commits,
        },
    ))
}

/// What the leaf store's rebuild found (step 4).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RebuildReport {
    /// Delta-log records applied.
    pub records: u64,
    /// The rebuilt store's composite root, hex.
    pub root: String,
    /// Whether it is the book's.
    pub root_matches: bool,
    /// `INV-6` at the store: accounts whose balances are not the sums of their entries.
    pub inv_6: Vec<String>,
    /// `INV-5` at the store: (currency, layer) pairs that do not balance.
    pub inv_5: Vec<(u32, usize)>,
}

impl RebuildReport {
    /// Whether the rebuild is the book, and the store's checks find nothing.
    pub fn is_clean(&self) -> bool {
        self.root_matches && self.inv_6.is_empty() && self.inv_5.is_empty()
    }
}

/// Step 4: rebuild `F-006`'s leaf store from the committed delta log of `book`, into `dir` (a
/// redb store and its archive) or in memory. Every commit's root is checked against the log's,
/// the final root against the book's, and the store's `INV-6` and `INV-5` checks run on it.
pub fn rebuild_leaf_store<S: LeafStore + ?Sized>(
    book: &S,
    dir: Option<&Path>,
) -> Result<RebuildReport, Error> {
    let store_err = |e: ledger_kernel::store::StoreError| Error::Restore(format!("{e:?}"));
    let mut target = match dir {
        Some(d) => {
            BookStore::open(
                d,
                StoreConfig::default(),
                ArchiveConfig {
                    fsync: false,
                    ..ArchiveConfig::default()
                },
            )
            .map_err(|e| Error::Restore(format!("{e:?}")))?
            .0
        }
        None => BookStore::projection_in_memory().map_err(|e| Error::Restore(format!("{e:?}")))?,
    };
    if target.last_committed().map_err(store_err)?.is_some() {
        return Err(Error::Restore(
            "the leaf store's target is not empty".into(),
        ));
    }
    let mut records = 0;
    if let Some(last) = book.last_committed().map_err(store_err)? {
        for seq in 0..=last {
            let rec = book
                .log_record(seq)
                .map_err(store_err)?
                .ok_or_else(|| Error::Restore(format!("no delta-log record {seq}")))?;
            let sub = target.commit(rec.seq, &rec.delta).map_err(store_err)?;
            if composite_root(&sub) != rec.root_after {
                return Err(Error::Restore(format!(
                    "commit {seq}: the rebuilt root is not the log's"
                )));
            }
            records += 1;
        }
    }
    let root = target.root().map_err(store_err)?;
    let view = target.view().map_err(store_err)?;
    Ok(RebuildReport {
        records,
        root: hex::encode(root),
        root_matches: root == book.root().map_err(store_err)?,
        inv_6: inv_6_findings(&view).map_err(store_err)?,
        inv_5: inv_5_findings(&view).map_err(store_err)?,
    })
}

/// Where the restored adapter stands after resuming (step 5, amendment 0040).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeReport {
    /// The head the adapter's store had accepted when it was restored: its position, if any.
    pub resumed_from: Option<u64>,
    /// The position of the layer's chain head at the finalized block when the adapter stopped.
    pub head: u64,
    /// The head the adapter accepted then: its position.
    pub accepted: Option<u64>,
    /// Whether the accepted head is the layer's, position and digest: every entry since the head
    /// it resumed from was read, shown complete, and taken in by the core.
    pub at_head: bool,
    /// The adapter's open alerts that stop observation (`chain:`, `unreadable:`, `reset:`).
    pub stopped: Vec<String>,
    /// Steps it took.
    pub steps: u64,
}

impl ResumeReport {
    /// Whether the book, the adapter and the layer agree: the layer's head accepted, nothing
    /// stopping observation.
    pub fn is_clean(&self) -> bool {
        self.at_head && self.stopped.is_empty()
    }
}

/// Step 5: run the restored adapter, which resumes from the head it last accepted, until it
/// accepts the layer's head at the finalized block (at most `max_steps` steps), or an alert stops
/// it at the first position it cannot show complete. Nothing is re-read on a timer: each step
/// folds only the entries since the accepted head (`REQ-IN-5`).
pub async fn resume_adapter<C: Chain, S: settle::store::Store, T: Transport>(
    adapter: &mut Adapter<C, S, T>,
    max_steps: u64,
) -> Result<ResumeReport, Error> {
    let instance = adapter.config().instance;
    let failed = |e: settle::adapter::AdapterError| Error::Restore(format!("the adapter: {e}"));
    let layer = |e: settle::chain::ChainError| Error::Restore(format!("the layer: {e:?}"));
    let store =
        |e: settle::store::StoreError| Error::Restore(format!("the adapter's store: {e:?}"));
    let resumed_from = adapter
        .store()
        .accepted_head(instance)
        .await
        .map_err(store)?
        .map(|a| a.position);
    let mut steps = 0;
    loop {
        adapter.step().await.map_err(failed)?;
        steps += 1;
        let fin = adapter.chain().finalized().await.map_err(layer)?;
        let head = adapter
            .chain()
            .chain_head(&fin, instance)
            .await
            .map_err(layer)?
            .ok_or_else(|| Error::Restore("the instance is not registered".into()))?;
        let accepted = adapter
            .store()
            .accepted_head(instance)
            .await
            .map_err(store)?;
        let at_head =
            accepted.is_some_and(|a| a.position == head.position && a.digest == head.digest);
        let stopped: Vec<String> = adapter
            .store()
            .alerts()
            .await
            .map_err(store)?
            .into_iter()
            .filter(|a| a.cleared_at.is_none())
            .map(|a| a.key)
            .filter(|k| {
                k.starts_with(&format!("{CHAIN_ALERT}:"))
                    || k.starts_with("unreadable:")
                    || k.starts_with("reset:")
            })
            .collect();
        if at_head || !stopped.is_empty() {
            return Ok(ResumeReport {
                resumed_from,
                head: head.position,
                accepted: accepted.map(|a| a.position),
                at_head,
                stopped,
                steps,
            });
        }
        if steps >= max_steps {
            return Err(Error::Restore(format!(
                "the adapter has not accepted the layer's head at {} after {steps} steps",
                head.position
            )));
        }
    }
}
