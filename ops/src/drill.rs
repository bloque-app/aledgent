// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The restore drill (`T-018-03`, `NFR-10`, `INV-5`, `INV-6`, `INV-19`, `AC-K5.1`).
//!
//! The same procedure as a restore ([`crate::restore`]), on an isolated host with no network
//! path to production: one VOS identity must never run on two live machines (VOS's source),
//! and a drill must never reach a production store. It then checks, and records:
//!
//! - **`INV-6`** — every stored balance equals the sum of its entries: `F-014`'s re-derivation
//!   (entries against balances, openings against commitments, the service store's balances
//!   against the entries), and `F-006`'s commitment check on the rebuilt leaf store;
//! - **`INV-5`** — the trial balance is zero per asset: on commitments over every kernel account
//!   the delta log ever touched, on the rebuilt leaf store, and exactly on openings;
//! - **the roots** — the restored and rebuilt composite root equals the last committed root, and
//!   the book root the last committed book root;
//! - **`INV-19`** — every `Final` anchor's commitment is on the settlement layer, and recomputes
//!   from rebuilt state at its sequence wherever the restored log holds that state
//!   ([`anchor_check`]). The check is external: it reads the layer, **per book** when governance
//!   has re-founded the book on the venue ([`BookAnchor`], amendment 0039);
//! - **`NFR-4`** — every operation reference ingress acknowledged exists in the restored core, in
//!   the same or a later state;
//! - **recovery time and data-loss window**, recorded with the result.
//!
//! **Cadence** (accepted proposal): before production (a gate of `M-3`), then monthly, and after
//! every change of the VOS or CipherClerk pin. The result is a JSON record ([`DrillReport`]) in
//! the drill's results directory, and the `aledgent_drill_*` metrics the exporter reads from it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use ingress::boundary::{CoreReply, Outcome};
use ingress::store::{InputKind, Row, Status};
use ingress::wire::{CommandBody, state_named};
use ledger_core::Ledger;
use ledger_core::anchor::CheckpointStatus;
use ledger_core::export::Leaves;
use ledger_core::export::rederive::{Acknowledgement, Finding};
use ledger_core::id::OperationId;
use ledger_core::query::trial::{commitment_trial, log_accounts};
use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::VaultBackend;
use serde::{Deserialize, Serialize};
use verify::{AnchorCheck, AnchorSource, NumberedAnchors, Unread};

use crate::Error;
use crate::restore::{RebuildReport, ReplayReport, RescanReport};

/// One check's result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Verdict {
    /// It holds.
    Passed,
    /// It does not.
    Failed,
    /// It cannot run yet: its owning feature has not landed. Never counted as passed.
    Deferred,
    /// It ran and could not decide: what it needed to read is in state the node it read from no
    /// longer keeps (an earlier book's anchors, after a governance reset of the settlement
    /// contract; amendment 0039). It is **not** a pass and it blocks one: [`DrillReport::passed`]
    /// is false while any check is `Unknown`, so `aledgent_drill_passed` stays 0 and the restore
    /// of an archive is refused. It is not a failure either: nothing was found wrong. The remedy
    /// is to read again from a node that keeps every block's state (an archive node).
    Unknown,
}

/// One of the drill's checks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Check {
    /// Its name: the requirement it shows.
    pub name: String,
    /// Its verdict.
    pub verdict: Verdict,
    /// What it found.
    pub detail: String,
}

/// How many of a failing check's findings its detail lists. A check that fails over a whole
/// archive can fail on tens of thousands of items (the 2026-10-15 restore drill's report was 3.4 MB
/// for one `NFR-4` run); the count says how many, and these say what kind.
pub const MAX_EXAMPLES: usize = 20;

fn check(name: &str, findings: Vec<String>) -> Check {
    Check {
        name: name.to_owned(),
        verdict: if findings.is_empty() {
            Verdict::Passed
        } else {
            Verdict::Failed
        },
        detail: findings_detail(&findings),
    }
}

/// A failing check's detail: the count, and at most [`MAX_EXAMPLES`] of the findings.
fn findings_detail(findings: &[String]) -> String {
    match findings {
        [] => "none found".to_owned(),
        all if all.len() <= MAX_EXAMPLES => format!("{} found: {}", all.len(), all.join("; ")),
        all => format!(
            "{} found, the first {MAX_EXAMPLES}: {}; and {} more",
            all.len(),
            all[..MAX_EXAMPLES].join("; "),
            all.len() - MAX_EXAMPLES
        ),
    }
}

/// What the book stood at before the failure, from the source's last commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Expected {
    /// The kernel's composite root.
    pub kernel_root: [u8; 32],
    /// The book root.
    pub book_root: [u8; 32],
    /// The input number.
    pub inputs: u64,
}

/// The data-loss window.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataLoss {
    /// Inputs the source had applied at the failure, if known.
    pub inputs_before: Option<u64>,
    /// Inputs the restored core holds.
    pub inputs_restored: u64,
    /// The difference: inputs lost.
    pub inputs_lost: u64,
    /// The received-log's own window: its WAL archive's `archive_timeout`, in seconds, when it
    /// was restored from the archive.
    pub wal_window_s: Option<u32>,
}

/// A drill's record. Written as JSON; the exporter reads the latest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrillReport {
    /// The drill's identity.
    pub id: String,
    /// Where it ran: an isolated host, by name.
    pub host: String,
    /// The backup restored, by identity.
    pub backup: String,
    /// When the drill started, in milliseconds since the Unix epoch.
    pub started_at: u64,
    /// Wall time from the start of the restore to a core that answers, in milliseconds.
    pub recovery_time_ms: u64,
    /// The data-loss window.
    pub data_loss: DataLoss,
    /// The ordered replay.
    pub replay: ReplayReport,
    /// The leaf store's rebuild.
    pub rebuild: RebuildReport,
    /// The adapter's re-scan.
    pub rescan: Option<RescanReport>,
    /// The size of the book restored: postings, kernel accounts, received-log rows.
    pub size: Size,
    /// The checks.
    pub checks: Vec<Check>,
}

/// The size of what a drill restored.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Size {
    /// Postings in the book.
    pub postings: u64,
    /// Kernel accounts.
    pub kernel_accounts: u64,
    /// Rows of the received-log.
    pub rows: u64,
    /// Bytes of the core backup's export.
    pub export_bytes: u64,
}

impl DrillReport {
    /// Whether every check that can run passed, and nothing was lost. A check that is
    /// [`Verdict::Unknown`] is not a pass: the drill has not passed until it is decided.
    pub fn passed(&self) -> bool {
        self.data_loss.inputs_lost == 0
            && self
                .checks
                .iter()
                .all(|c| !matches!(c.verdict, Verdict::Failed | Verdict::Unknown))
            && self.checks.iter().any(|c| c.verdict == Verdict::Passed)
    }

    /// Whether a check could not decide ([`Verdict::Unknown`]) and none failed: the drill neither
    /// passed nor found a fault, and wants a read from a node that keeps past state.
    pub fn undecided(&self) -> bool {
        undecided(&self.checks)
    }

    /// Write the record into `dir` as `drill-<id>.json`. Returns its path.
    pub fn record(&self, dir: &Path) -> Result<PathBuf, Error> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("drill-{}.json", self.id));
        let json = serde_json::to_vec_pretty(self).map_err(|e| Error::Config(e.to_string()))?;
        std::fs::write(&path, json)?;
        Ok(path)
    }

    /// The latest drill record in `dir`, by start time.
    pub fn latest(dir: &Path) -> Result<Option<DrillReport>, Error> {
        if !dir.exists() {
            return Ok(None);
        }
        let mut best: Option<DrillReport> = None;
        for e in std::fs::read_dir(dir)? {
            let p = e?.path();
            let is_drill = p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("drill-") && n.ends_with(".json"));
            if !is_drill {
                continue;
            }
            let r: DrillReport = serde_json::from_slice(&std::fs::read(&p)?)
                .map_err(|e| Error::Config(format!("{}: {e}", p.display())))?;
            if best.as_ref().is_none_or(|b| r.started_at > b.started_at) {
                best = Some(r);
            }
        }
        Ok(best)
    }
}

/// The acknowledgements the received-log holds (`NFR-4`): every command row the core decided
/// with an operation, and the state its reply showed; and every command row still undecided
/// that ingress acknowledged at `Received` (a transfer), which must exist once forwarded.
pub fn acknowledgements(rows: &[Row]) -> Result<(Vec<Acknowledgement>, Vec<OperationId>), Error> {
    let mut acks = Vec::new();
    let mut pending = Vec::new();
    for r in rows.iter().filter(|r| r.new.kind == InputKind::Command) {
        // The settlement adapter's reports are observations at coordinates, not operations: the
        // core keeps them in its settlement store, and the re-scan checks them (`REQ-IN-5`).
        let body: Option<CommandBody> = serde_json::from_slice(&r.new.body).ok();
        if matches!(body, Some(CommandBody::Adapter(_))) {
            continue;
        }
        if r.status != Status::Decided {
            if !r.new.synchronous {
                pending.push(OperationId::from_ref(ledger_kernel::posting::OperationRef(
                    r.new.op_ref.0,
                )));
            }
            continue;
        }
        let bytes = r.core_reply.as_deref().unwrap_or_default();
        let reply: CoreReply = serde_json::from_slice(bytes)
            .map_err(|e| Error::Restore(format!("a stored reply: {e}")))?;
        if let Outcome::Decided {
            operation_reference,
            state,
            ..
        } = reply.outcome
        {
            let op = OperationId::parse(&operation_reference)
                .map_err(|_| Error::Restore("a stored reply's operation reference".into()))?;
            acks.push(Acknowledgement {
                op,
                state: state.and_then(|s| state_named(&s.state)),
            });
        }
    }
    Ok((acks, pending))
}

/// The checks on a restored core (module documentation). `acks` are the received-log's
/// acknowledgements; `expected`, the source's last committed roots, when the drill has them.
pub fn checks<S: LeafStore, V: VaultBackend>(
    ledger: &Ledger<S, V>,
    acks: &[Acknowledgement],
    expected: Option<&Expected>,
    rebuild: &RebuildReport,
    anchors: Option<(&dyn AnchorSource, u64)>,
) -> Vec<Check> {
    let mut out = Vec::new();
    // INV-6 and NFR-4, from F-014's re-derivation.
    let found = match ledger.rederive(acks) {
        Ok(()) => Vec::new(),
        Err(f) => f,
    };
    let (missing, rest): (Vec<Finding>, Vec<Finding>) = found
        .into_iter()
        .partition(|f| matches!(f, Finding::Missing(_) | Finding::Behind(_)));
    let mut inv6: Vec<String> = rest.iter().map(|f| format!("{f:?}")).collect();
    inv6.extend(rebuild.inv_6.iter().map(|f| format!("leaf store: {f}")));
    out.push(check("INV-6", inv6));
    // INV-5: commitments over every kernel account, the rebuilt store, and openings exactly.
    let mut inv5 = Vec::new();
    let store = ledger.book().store();
    match log_accounts(store).and_then(|a| commitment_trial(store, &a)) {
        Ok(t) => inv5.extend(
            t.unbalanced
                .iter()
                .map(|(c, l)| format!("commitments: currency {c}, layer {l}")),
        ),
        Err(e) => inv5.push(format!("the store: {e:?}")),
    }
    inv5.extend(
        rebuild
            .inv_5
            .iter()
            .map(|(c, l)| format!("leaf store: currency {c}, layer {l}")),
    );
    for asset in ledger.state().registry().iter() {
        match ledger.trial_balance(&asset.id()) {
            Ok(0) => {}
            Ok(n) => inv5.push(format!("{}: openings sum to {n}", asset.id().as_str())),
            Err(e) => inv5.push(format!("{}: {e:?}", asset.id().as_str())),
        }
    }
    out.push(check("INV-5", inv5));
    // The roots.
    let mut roots = Vec::new();
    if !rebuild.root_matches {
        roots.push("the rebuilt leaf store's root is not the book's".to_owned());
    }
    if let Some(e) = expected {
        match ledger.book().root() {
            Ok(r) if r == e.kernel_root => {}
            Ok(r) => roots.push(format!(
                "composite root {} is not the last committed {}",
                hex::encode(r),
                hex::encode(e.kernel_root)
            )),
            Err(f) => roots.push(format!("the book: {f:?}")),
        }
        match ledger.checkpoint() {
            Ok((_, r)) if r == e.book_root => {}
            Ok((_, r)) => roots.push(format!(
                "book root {} is not the last committed {}",
                hex::encode(r),
                hex::encode(e.book_root)
            )),
            Err(f) => roots.push(format!("the book: {f:?}")),
        }
    }
    out.push(check("composite-root", roots));
    out.push(match anchors {
        Some((layer, import_commits)) => anchor_check(ledger, import_commits, layer),
        None => Check {
            name: "INV-19".into(),
            verdict: Verdict::Deferred,
            detail: "no read of the settlement layer was given: the anchors are not checked".into(),
        },
    });
    let missing: BTreeSet<String> = missing.iter().map(|f| format!("{f:?}")).collect();
    out.push(check("NFR-4", missing.into_iter().collect()));
    out
}

/// Whether a set of checks has one that could not decide ([`Verdict::Unknown`]) and none that
/// failed: neither a pass nor a fault.
pub fn undecided(checks: &[Check]) -> bool {
    checks.iter().all(|c| c.verdict != Verdict::Failed)
        && checks.iter().any(|c| c.verdict == Verdict::Unknown)
}

/// What the settlement layer says about one checkpoint's anchor, **in the book the checkpoint
/// belongs to** (amendment 0039). After a governance reset of the settlement contract a venue
/// holds more than one anchor sequence, and two books can both hold an anchor of one number; the
/// read must not answer for a number alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BookAnchor {
    /// The anchor of that number in anchor sequence `epoch` carries the checkpoint's commitment.
    /// `current` says whether that is the venue's current sequence rather than a book re-founded
    /// since (which a restore of an older book's backup legitimately finds).
    Verified {
        /// The anchor sequence.
        epoch: u32,
        /// Whether it is the venue's current one.
        current: bool,
    },
    /// The layer contradicts the checkpoint: no anchor, or another commitment, in its book.
    Failed(String),
    /// The layer could not be read for this checkpoint's book (an earlier book's anchor, or a
    /// superseded one, on a node that keeps no past state): neither proven nor contradicted.
    /// Never counted as a pass.
    Unknown(String),
}

/// The anchor sequence a book lives in, from one of its checkpoints: the sequence whose anchor of
/// that number carries the checkpoint's commitment. The commitment binds the book's state, so at
/// most one sequence matches (bar a hash collision). It is the way the verifier names a book too
/// (`verify::check`): comparing the sequence's book name with the book's genesis waits on `q1` of
/// amendment 0039.
pub fn pinned_epoch(read: &NumberedAnchors, commitment: &[u8; 32]) -> Option<u32> {
    match AnchorCheck::of(read, commitment) {
        AnchorCheck::Final { sequence, .. } => Some(sequence.epoch),
        _ => None,
    }
}

/// `read` (every sequence's anchor of the checkpoint's number), judged for a checkpoint with
/// `commitment` of the book that lives in anchor sequence `pin`, when that is known
/// ([`pinned_epoch`]).
///
/// - The commitment is some sequence's anchor of the number: [`BookAnchor::Verified`], and it
///   says which sequence.
/// - Otherwise, **with the book's sequence known**, only that sequence's anchor can be this
///   book's: another commitment there, or none there at all, is [`BookAnchor::Failed`] whatever
///   another sequence's anchor of the number is or whether it could be read; the book's own
///   anchor not readable is [`BookAnchor::Unknown`].
/// - Otherwise (no checkpoint of the book is proven in any sequence yet): no anchor, or anchors
///   that are all other commitments, are `Failed`; if a sequence that may hold the number
///   could not be read, [`BookAnchor::Unknown`], with what could not be read.
///
/// A venue that was never reset has the one sequence, and this is the read it always was, except
/// that a superseded anchor a node no longer keeps is `Unknown`, not a failure of the book.
pub fn book_anchor(read: &NumberedAnchors, commitment: &[u8; 32], pin: Option<u32>) -> BookAnchor {
    match AnchorCheck::of(read, commitment) {
        AnchorCheck::Final {
            sequence, current, ..
        } => BookAnchor::Verified {
            epoch: sequence.epoch,
            current,
        },
        AnchorCheck::Missing => BookAnchor::Failed("the settlement layer holds no anchor".into()),
        AnchorCheck::Mismatch { stored } => BookAnchor::Failed(format!(
            "the settlement layer holds another commitment{}",
            where_(&stored)
        )),
        AnchorCheck::Unknown { stored, unread } => match pin {
            Some(p) if stored.iter().any(|(s, _)| s.epoch == p) => BookAnchor::Failed(format!(
                "the settlement layer holds another commitment in anchor sequence {p}, the book's own"
            )),
            Some(p) if !unread.iter().any(|u| concerns(u, p)) => BookAnchor::Failed(format!(
                "the settlement layer holds no anchor of this number in anchor sequence {p}, the book's own"
            )),
            _ => BookAnchor::Unknown(format!("needs an archive node: {}", unread_text(&unread))),
        },
        AnchorCheck::Unreadable(why) => {
            BookAnchor::Failed(format!("the settlement layer could not be read: {why}"))
        }
    }
}

/// ` (in anchor sequence a, b)` when the venue has been reset or the number is held in more than
/// one sequence; nothing for a venue that never was, so its texts read as they always did.
fn where_(stored: &[(verify::Sequence, [u8; 32])]) -> String {
    if stored.len() > 1 || stored.iter().any(|(s, _)| s.epoch > 0) {
        let epochs: Vec<String> = stored.iter().map(|(s, _)| s.epoch.to_string()).collect();
        format!(" (in anchor sequence {})", epochs.join(", "))
    } else {
        String::new()
    }
}

/// Whether `u` is about anchor sequence `epoch`, or about sequences that were not identified and
/// so may include it.
fn concerns(u: &Unread, epoch: u32) -> bool {
    match u {
        Unread::Anchor { sequence, .. } => sequence.epoch == epoch,
        Unread::Before { epoch: first, .. } => epoch < *first,
    }
}

/// What could not be read, for a reader.
fn unread_text(unread: &[Unread]) -> String {
    let parts: Vec<String> = unread
        .iter()
        .map(|u| match u {
            Unread::Anchor { sequence, why } => {
                format!("anchor sequence {} may hold it: {why}", sequence.epoch)
            }
            Unread::Before { why, .. } => why.clone(),
        })
        .collect();
    parts.join("; ")
}

/// At most [`MAX_EXAMPLES`] of `items`, and how many more.
fn examples(items: &[String]) -> String {
    if items.len() <= MAX_EXAMPLES {
        items.join("; ")
    } else {
        format!(
            "{}; and {} more",
            items[..MAX_EXAMPLES].join("; "),
            items.len() - MAX_EXAMPLES
        )
    }
}

/// `INV-19` against the settlement layer (`F-017`): for every `Final` checkpoint of the restored
/// book, the layer's anchor at its number carries its commitment, and the commitment is the one
/// its fields produce; and wherever the restored delta log holds a committed state at the
/// checkpoint's entry sequence, the kernel root recomputed from those rebuilt leaves is the one it
/// committed to. `import_commits` is how many commits the import made (`ReplayReport`): the
/// restored log's first commits hold the export's leaves in chunks, not the states before the
/// backup point, so checkpoints before it are checked against the layer only, and the record says
/// how many.
///
/// **Per book** (amendment 0039, `T-026-12`). After a governance reset of the settlement contract
/// the venue holds more than one anchor sequence, and a new book's checkpoints reuse the numbers
/// the old book used. The layer is therefore read for every sequence that used a number
/// ([`AnchorSource::anchors`]), and the checkpoint's commitment decides which sequence is its
/// book ([`book_anchor`]); the book's sequence, once one of its checkpoints is proven in it, is
/// the only one whose anchors can be the book's. The verdict is:
///
/// - **Passed**: every `Final` checkpoint's commitment is its book's anchor, in a sequence the
///   detail names after a reset (the current one, or a book re-founded since, which a restore of
///   an older book's backup finds);
/// - **Failed**: some checkpoint is contradicted (no anchor in its book, another commitment, fields
///   that do not produce the commitment, the layer cannot be reached, or a rebuilt state differs).
///   Unknown checkpoints never hide a failure;
/// - **Unknown**: nothing is contradicted, and some checkpoint's anchor is in state the node
///   keeps no longer (an earlier book's anchor, or a superseded one): the detail says which, and
///   that an archive node decides it. It is not a pass ([`Verdict::Unknown`]).
pub fn anchor_check<S: LeafStore, V: VaultBackend>(
    ledger: &Ledger<S, V>,
    import_commits: u64,
    layer: &dyn AnchorSource,
) -> Check {
    let store = ledger.book().store();
    let finals: Vec<_> = ledger
        .state()
        .anchoring()
        .checkpoints()
        .filter(|c| c.status == CheckpointStatus::Final)
        .copied()
        .collect();
    let mut findings = Vec::new();
    // The committed states after the import, by the next posting sequence each stands at.
    let mut states: std::collections::BTreeMap<u64, Vec<u64>> = std::collections::BTreeMap::new();
    if let Ok(Some(last)) = store.last_committed() {
        let mut next = 0u64;
        for c in 0..=last {
            match store.log_record(c) {
                Ok(Some(r)) => {
                    if let Some(max) = r.delta.postings.iter().map(|p| p.seq).max() {
                        next = next.max(max + 1);
                    }
                }
                _ => findings.push(format!("the delta log has no record {c}")),
            }
            if c + 1 >= import_commits {
                states.entry(next).or_default().push(c);
            }
        }
    }
    // Every sequence's anchor of each checkpoint's number, in the venue the checkpoint was
    // anchored in (`F-027` T-027-04), and from them the book's own sequence: the one holding the
    // latest checkpoint a commitment proves.
    let reads: Vec<_> = finals
        .iter()
        .map(|c| layer.anchors(&c.anchored_in(), c.number))
        .collect();
    let pin = finals
        .iter()
        .zip(&reads)
        .filter_map(|(c, r)| {
            let epoch = pinned_epoch(r.as_ref().ok()?, &c.commitment)?;
            Some((c.number, epoch))
        })
        .max_by_key(|(number, _)| *number)
        .map(|(_, epoch)| epoch);
    let mut unknown = Vec::new();
    let mut by_epoch: std::collections::BTreeMap<u32, u64> = std::collections::BTreeMap::new();
    let mut current_epoch = None;
    let (mut recomputed, mut layer_only) = (0u64, 0u64);
    for (c, read) in finals.iter().zip(&reads) {
        if !c.consistent() {
            findings.push(format!(
                "checkpoint {}: its fields do not produce its commitment",
                c.number
            ));
        }
        match read {
            Ok(read) => {
                current_epoch = Some(read.current_epoch);
                match book_anchor(read, &c.commitment, pin) {
                    BookAnchor::Verified { epoch, .. } => *by_epoch.entry(epoch).or_default() += 1,
                    BookAnchor::Failed(why) => {
                        findings.push(format!("checkpoint {}: {why}", c.number))
                    }
                    BookAnchor::Unknown(why) => {
                        unknown.push(format!("checkpoint {}: {why}", c.number))
                    }
                }
            }
            Err(e) => findings.push(format!("the settlement layer could not be read: {e}")),
        }
        match states.get(&(c.entry_sequence + 1)) {
            Some(commits) => {
                let matches = commits.iter().any(|at| {
                    Leaves::from_log(store, *at).is_ok_and(|l| l.root() == c.kernel_root)
                });
                if matches {
                    recomputed += 1;
                } else {
                    findings.push(format!(
                        "checkpoint {}: no rebuilt state at entry {} has its kernel root",
                        c.number, c.entry_sequence
                    ));
                }
            }
            None => layer_only += 1,
        }
    }
    let mut out = check("INV-19", findings);
    // The sequences the anchors were found in, said only for a venue that has been reset.
    let sequences = match current_epoch {
        Some(current) if current > 0 || by_epoch.keys().any(|e| *e > 0) => format!(
            " [{}]",
            by_epoch
                .iter()
                .map(|(e, n)| format!(
                    "{n} in anchor sequence {e}, {}",
                    if *e == current {
                        "the venue's current one"
                    } else {
                        "a book re-founded since"
                    }
                ))
                .collect::<Vec<_>>()
                .join("; ")
        ),
        _ => String::new(),
    };
    match (out.verdict.clone(), unknown.is_empty()) {
        (Verdict::Passed, true) => {
            out.detail = format!(
                "{} Final anchors: {recomputed} recomputed from rebuilt state, {layer_only} before the backup point checked against the settlement layer only{sequences}",
                finals.len()
            );
        }
        (Verdict::Passed, false) => {
            out.verdict = Verdict::Unknown;
            out.detail = format!(
                "{} of {} Final anchors could not be checked against the settlement layer and are unknown, not passed (read again from an archive node): {}; the rest are on it{sequences}",
                unknown.len(),
                finals.len(),
                examples(&unknown)
            );
        }
        (_, false) => out.detail.push_str(&format!(
            "; and {} more unknown (read from an archive node): {}",
            unknown.len(),
            examples(&unknown)
        )),
        _ => {}
    }
    out
}

/// The size of a restored book.
pub fn size_of<S: LeafStore, V: VaultBackend>(
    ledger: &Ledger<S, V>,
    rows: usize,
    export_bytes: u64,
) -> Size {
    let store = ledger.book().store();
    Size {
        postings: store.next_posting_seq().unwrap_or(0),
        kernel_accounts: log_accounts(store).map_or(0, |a| a.len() as u64),
        rows: rows as u64,
        export_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("Missing({i})")).collect()
    }

    #[test]
    fn a_check_with_no_findings_passes() {
        let c = check("NFR-4", Vec::new());
        assert_eq!(
            (c.verdict, c.detail.as_str()),
            (Verdict::Passed, "none found")
        );
    }

    #[test]
    fn a_failing_check_lists_every_finding_up_to_the_limit() {
        let c = check("NFR-4", found(MAX_EXAMPLES));
        assert_eq!(c.verdict, Verdict::Failed);
        assert!(
            c.detail.starts_with("20 found: Missing(0); "),
            "{}",
            c.detail
        );
        assert!(c.detail.ends_with("Missing(19)"), "{}", c.detail);
    }

    /// The 2026-10-15 drill's `NFR-4` listed 63,947 items: the detail is the count and 20 of them.
    #[test]
    fn a_failing_check_prints_the_count_and_at_most_twenty_examples() {
        let c = check("NFR-4", found(63_947));
        assert_eq!(c.verdict, Verdict::Failed);
        assert!(
            c.detail
                .starts_with("63947 found, the first 20: Missing(0); Missing(1); "),
            "{}",
            c.detail
        );
        assert!(c.detail.contains("Missing(19)"), "{}", c.detail);
        assert!(!c.detail.contains("Missing(20)"), "{}", c.detail);
        assert!(c.detail.ends_with("and 63927 more"), "{}", c.detail);
        assert!(c.detail.len() < 1_000, "{} bytes", c.detail.len());
    }

    // ---- T-026-12 (amendment 0039): the layer is read per book ----

    use verify::{PublicAnchor, Sequence};

    const OLD: [u8; 32] = [0x0A; 32];
    const NEW: [u8; 32] = [0x0B; 32];

    fn seq(epoch: u32, base: u64) -> Sequence {
        Sequence {
            epoch,
            base,
            book: (epoch > 0).then_some([0xB0 + epoch as u8; 32]),
        }
    }

    fn at(commitment: [u8; 32]) -> PublicAnchor {
        PublicAnchor {
            commitment,
            stored_at: 7,
            read_at: 9,
        }
    }

    fn unread_anchor(epoch: u32) -> Unread {
        Unread::Anchor {
            sequence: seq(epoch, 0),
            why: "State already discarded".into(),
        }
    }

    fn read(current: u32, found: &[(Sequence, [u8; 32])], unread: Vec<Unread>) -> NumberedAnchors {
        NumberedAnchors {
            current_epoch: current,
            found: found.iter().map(|(s, c)| (*s, at(*c))).collect(),
            unread,
        }
    }

    /// No reset: the one sequence, read as it always was.
    #[test]
    fn t_026_12_a_venue_that_was_never_reset_reads_as_before() {
        let c = [1; 32];
        let first = NumberedAnchors::first_sequence(Some(at(c)));
        assert_eq!(
            book_anchor(&first, &c, None),
            BookAnchor::Verified {
                epoch: 0,
                current: true
            }
        );
        assert_eq!(pinned_epoch(&first, &c), Some(0));
        assert_eq!(
            book_anchor(&first, &[2; 32], Some(0)),
            BookAnchor::Failed("the settlement layer holds another commitment".into())
        );
        assert_eq!(
            book_anchor(&NumberedAnchors::first_sequence(None), &c, Some(0)),
            BookAnchor::Failed("the settlement layer holds no anchor".into())
        );
        // A superseded anchor a pruned node no longer keeps: unknown, which is not a pass.
        let pruned = read(0, &[], vec![unread_anchor(0)]);
        assert!(matches!(
            book_anchor(&pruned, &c, Some(0)),
            BookAnchor::Unknown(w) if w.contains("needs an archive node")
        ));
    }

    /// After a reset, the new book's checkpoint numbered at or below the old book's last anchor:
    /// the old book's anchor of the number is another book's. It is not read as this book's, and
    /// not being readable does not make the new book's anchor unreadable.
    #[test]
    fn t_026_12_the_new_books_checkpoint_is_found_in_the_new_book_whatever_the_old_one_holds() {
        // Archive node: both books' anchors of the number.
        let both = read(1, &[(seq(1, 0), NEW), (seq(0, 0), OLD)], vec![]);
        assert_eq!(
            book_anchor(&both, &NEW, None),
            BookAnchor::Verified {
                epoch: 1,
                current: true
            }
        );
        // Plain node: the old book's anchor of the number is not readable.
        let plain = read(1, &[(seq(1, 0), NEW)], vec![unread_anchor(0)]);
        assert_eq!(
            book_anchor(&plain, &NEW, None),
            BookAnchor::Verified {
                epoch: 1,
                current: true
            }
        );
        assert_eq!(pinned_epoch(&plain, &NEW), Some(1));
    }

    /// The old book's checkpoint: proven where the node can read the old book's anchor (the last
    /// one is kept in current state; an archive node has the rest), unknown where it cannot, and
    /// never matched to the new book's anchor of the same number.
    #[test]
    fn t_026_12_an_old_books_checkpoint_is_proven_or_unknown_never_another_books() {
        let archive = read(1, &[(seq(1, 0), NEW), (seq(0, 0), OLD)], vec![]);
        assert_eq!(
            book_anchor(&archive, &OLD, None),
            BookAnchor::Verified {
                epoch: 0,
                current: false
            }
        );
        // The previous book's last anchor reads on any node.
        let last = read(1, &[(seq(0, 0), OLD)], vec![]);
        assert_eq!(
            book_anchor(&last, &OLD, None),
            BookAnchor::Verified {
                epoch: 0,
                current: false
            }
        );
        // A plain node, an earlier number of the old book: the new book holds the number, the
        // old book's anchor is unreadable. The new book's commitment is not the checkpoint's.
        let plain = read(1, &[(seq(1, 0), NEW)], vec![unread_anchor(0)]);
        let got = book_anchor(&plain, &OLD, None);
        assert!(
            matches!(&got, BookAnchor::Unknown(w) if w.contains("needs an archive node")),
            "{got:?}"
        );
        // ... and nothing read at all: still unknown, not "no anchor".
        let nothing = read(1, &[], vec![unread_anchor(0)]);
        assert!(matches!(
            book_anchor(&nothing, &OLD, None),
            BookAnchor::Unknown(_)
        ));
    }

    /// With the book's sequence proven by one of its checkpoints, an unreadable other sequence
    /// cannot excuse a missing or contradicted anchor in the book's own.
    #[test]
    fn t_026_12_unknown_never_hides_a_contradiction_in_the_books_own_sequence() {
        // The new book, pinned to 1: sequence 1 holds another commitment of the number.
        let other = read(1, &[(seq(1, 0), [0xEE; 32])], vec![unread_anchor(0)]);
        assert!(matches!(
            book_anchor(&other, &NEW, Some(1)),
            BookAnchor::Failed(w) if w.contains("another commitment in anchor sequence 1")
        ));
        // Sequence 1 does not hold the number at all (its latest is below it): no anchor.
        let none = read(1, &[], vec![unread_anchor(0)]);
        assert!(matches!(
            book_anchor(&none, &NEW, Some(1)),
            BookAnchor::Failed(w) if w.contains("no anchor of this number in anchor sequence 1")
        ));
        // Without a pin the same read cannot say whose the checkpoint is: unknown.
        assert!(matches!(
            book_anchor(&none, &NEW, None),
            BookAnchor::Unknown(_)
        ));
        // The book's own anchor unreadable (superseded, plain node): unknown, not failed.
        let own = read(1, &[], vec![unread_anchor(1)]);
        assert!(matches!(
            book_anchor(&own, &NEW, Some(1)),
            BookAnchor::Unknown(_)
        ));
        // Plain mismatches and absences fail, with or without a pin; the sequences are named.
        let wrong = read(1, &[(seq(1, 0), [0xEE; 32]), (seq(0, 0), OLD)], vec![]);
        assert!(matches!(
            book_anchor(&wrong, &NEW, None),
            BookAnchor::Failed(w) if w.contains("another commitment (in anchor sequence 1, 0)")
        ));
    }

    /// A second reset: the sequences before the last one cannot be identified on a plain node.
    #[test]
    fn t_026_12_after_a_second_reset_the_earliest_book_is_unknown_on_a_plain_node() {
        let before = Unread::Before {
            epoch: 2,
            last_anchor: Some(5),
            why: "the anchor sequences before 2 could not be identified".into(),
        };
        // The current book's anchor is read: proven, whatever is unidentified before it.
        let r = read(2, &[(seq(2, 0), NEW)], vec![before.clone()]);
        assert_eq!(
            book_anchor(&r, &NEW, None),
            BookAnchor::Verified {
                epoch: 2,
                current: true
            }
        );
        // A checkpoint of the first book: unknown. With the book's sequence unproven, nothing
        // can be said; and a pinned current book would not be excused by it.
        assert!(matches!(
            book_anchor(&r, &OLD, None),
            BookAnchor::Unknown(_)
        ));
        assert!(matches!(
            book_anchor(&r, &OLD, Some(2)),
            BookAnchor::Failed(_)
        ));
        // A pin below the first identified sequence is excused by it.
        assert!(matches!(
            book_anchor(&r, &OLD, Some(0)),
            BookAnchor::Unknown(_)
        ));
    }

    fn check_of(verdict: Verdict) -> Check {
        Check {
            name: "x".into(),
            verdict,
            detail: String::new(),
        }
    }

    /// `Unknown` is not a pass: a drill whose only unproven check is `Unknown` has not passed,
    /// and is told apart from one that failed.
    #[test]
    fn t_026_12_an_unknown_check_is_neither_a_pass_nor_a_failure() {
        let report = |checks: Vec<Check>| DrillReport {
            id: "d".into(),
            host: "h".into(),
            backup: "b".into(),
            started_at: 1,
            recovery_time_ms: 1,
            data_loss: DataLoss::default(),
            replay: Default::default(),
            rebuild: Default::default(),
            rescan: None,
            size: Size::default(),
            checks,
        };
        let all = report(vec![check_of(Verdict::Passed), check_of(Verdict::Passed)]);
        assert!(all.passed() && !all.undecided());
        let unknown = report(vec![check_of(Verdict::Passed), check_of(Verdict::Unknown)]);
        assert!(!unknown.passed() && unknown.undecided());
        let failed = report(vec![
            check_of(Verdict::Failed),
            check_of(Verdict::Unknown),
            check_of(Verdict::Passed),
        ]);
        assert!(!failed.passed() && !failed.undecided());
        // The record survives a round trip, as the exporter reads it.
        let json = serde_json::to_string(&unknown.checks[1]).unwrap();
        assert!(json.contains("\"unknown\""), "{json}");
        assert_eq!(
            serde_json::from_str::<Check>(&json).unwrap().verdict,
            Verdict::Unknown
        );
    }
}
