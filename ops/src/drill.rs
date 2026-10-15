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
//!   ([`anchor_check`]). The check is external: it reads the layer;
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
use verify::AnchorSource;

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
    /// Whether every check that can run passed, and nothing was lost.
    pub fn passed(&self) -> bool {
        self.data_loss.inputs_lost == 0
            && self.checks.iter().all(|c| c.verdict != Verdict::Failed)
            && self.checks.iter().any(|c| c.verdict == Verdict::Passed)
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

/// `INV-19` against the settlement layer (`F-017`): for every `Final` checkpoint of the restored
/// book, the layer's anchor at its number carries its commitment, and the commitment is the one
/// its fields produce; and wherever the restored delta log holds a committed state at the
/// checkpoint's entry sequence, the kernel root recomputed from those rebuilt leaves is the one it
/// committed to. `import_commits` is how many commits the import made (`ReplayReport`): the
/// restored log's first commits hold the export's leaves in chunks, not the states before the
/// backup point, so checkpoints before it are checked against the layer only, and the record says
/// how many.
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
    let (mut recomputed, mut layer_only) = (0u64, 0u64);
    for c in &finals {
        if !c.consistent() {
            findings.push(format!(
                "checkpoint {}: its fields do not produce its commitment",
                c.number
            ));
        }
        // In the venue the checkpoint was anchored in (`F-027` T-027-04).
        match layer.anchor(&c.anchored_in(), c.number) {
            Ok(Some(a)) if a.commitment == c.commitment => {}
            Ok(Some(_)) => findings.push(format!(
                "checkpoint {}: the settlement layer holds another commitment",
                c.number
            )),
            Ok(None) => findings.push(format!(
                "checkpoint {}: the settlement layer holds no anchor",
                c.number
            )),
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
    if out.verdict == Verdict::Passed {
        out.detail = format!(
            "{} Final anchors: {recomputed} recomputed from rebuilt state, {layer_only} before the backup point checked against the settlement layer only",
            finals.len()
        );
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
}
