// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The daily reconciliation (`T-018-06`, `NFR-9`, `INV-5`, `INV-6`, `REQ-PV-4`): axes 1 and 2
//! at one minor unit with zero tolerance, and the plug-in point of axis 3.
//!
//! - **Axis 1, per (account, asset)** — every kernel account is its own row (`DEC-2`: each holding
//!   account too):
//!   - *(a) commitments*: the entry commitments of the delta log's history summed per (account,
//!     layer, side) against the account leaf the store holds. Pedersen commitments add, so point
//!     equality is exact: one minor unit shows. No opening, no privileged read.
//!   - *(b) rebuild*: `F-006`'s leaf store rebuilt from the delta log, its root against the
//!     store's, and every rebuilt account leaf against the stored one, bit for bit.
//!   - *(c) external source*: during shadow and cutover (`DEC-14`), the book's balances against
//!     an independent source ([`ledger_core::ledger::operator::ExternalSource`]). That needs
//!     openings, so it runs **inside the core** as the one instrumented privileged read
//!     (`Ledger::reconcile_external`, counted by `crates/ledger-core/tests/privileged_reads.rs`),
//!     with a justification, and returns only mismatches. Outside migration there is no such
//!     source, and (a) and (b) are the independent sources (plan open point 4, the owner's).
//! - **Axis 2, per asset**: over every kernel account, the sum of (credits − debits) per currency
//!   and layer is the identity point; so is the sum of every entry of the history; and the trial
//!   balance on openings is exactly zero (`INV-5`).
//! - **Axis 3**, per asset: the book against the settlement layer's view of what aledgent holds,
//!   through `F-017`'s comparison (`REQ-PR-5`, `AC-J3.2`). `F-017` built no settlement-position
//!   account: the book's position at a cut is derived from the settling postings behind the final
//!   records below the cut, and compared with the positions the adapter reported at that cut. The
//!   daily job asks the core to compare at the latest reported cut ([`compare_through`]); a cut
//!   older than a day is itself a finding.
//! - **`Suspense`** (`REQ-LA-2`, `RISK-8`): every non-zero `Suspense` balance, and since when
//!   ([`suspense_report`]).
//!
//! **Recording.** Every run is a JSON record ([`Run::record`]) the exporter reads, and an audited
//! operator record: the daily comparison is `compare_settlement` with the run's summary as its
//! justification ([`daily`]). Any difference, or an axis not run in 24 hours, pages
//! (`ops/prometheus/rules/aledgent.yml`), and never opens a ticket.
//!
//! **No repair.** The job only reads, and records. A correction is an operation through `CTR-OPS`
//! (`REQ-AT-3`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use cipher_clerk::{Account, AccountId as KernelAccountId};
use curve25519_dalek::ristretto::{CompressedRistretto, RistrettoPoint};
use curve25519_dalek::traits::Identity;
use ingress::settlement::ComparisonBody;
use ledger_core::Ledger;
use ledger_core::account::AccountKind;
use ledger_core::audit::AuditSink;
use ledger_core::export::Leaves;
use ledger_core::export::rederive::{Finding as Rederived, check_entries};
use ledger_core::ledger::Ctx;
use ledger_core::ledger::operator::{ExternalSource, MismatchReason};
use ledger_core::query::journal::Sums;
use ledger_kernel::store::{KeySet, LeafStore};
use ledger_kernel::vault::VaultBackend;
use serde::{Deserialize, Serialize};

use crate::Error;
use crate::signals::{AxisSignal, SuspenseSignal};

/// What the reconciliation reads of the book: the delta log's history (its transfers, and the
/// account leaves it folds to) and the account leaves the store holds. A copy: the fault-injection
/// tests change it, never the store.
#[derive(Clone, Debug)]
pub struct BookSnapshot {
    /// The history, folded from the committed delta log.
    pub history: Leaves,
    /// The account leaves as the store holds them, for every account the log names.
    pub stored: BTreeMap<KernelAccountId, Option<Account>>,
    /// The store's composite root.
    pub root: [u8; 32],
}

impl BookSnapshot {
    /// Read `store`.
    pub fn read<S: LeafStore + ?Sized>(store: &S) -> Result<BookSnapshot, Error> {
        let err = |e: ledger_kernel::store::StoreError| Error::Core(format!("the store: {e:?}"));
        let history = match store.last_committed().map_err(err)? {
            Some(last) => Leaves::from_log(store, last).map_err(err)?,
            None => Leaves::default(),
        };
        let keys = KeySet {
            accounts: history.accounts.keys().copied().collect(),
            ..KeySet::default()
        };
        let stored = store.load(&keys).map_err(err)?.accounts;
        Ok(BookSnapshot {
            history,
            stored,
            root: store.root().map_err(err)?,
        })
    }

    /// The history's transfers against the stored accounts.
    fn as_stored(&self) -> Leaves {
        let mut l = self.history.clone();
        l.accounts = self
            .stored
            .iter()
            .filter_map(|(id, a)| a.clone().map(|a| (*id, a)))
            .collect();
        l
    }
}

/// One difference, at one minor unit and zero tolerance. No figure of any balance.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "finding", rename_all = "kebab-case")]
pub enum Finding {
    /// Axis 1(a): an account leaf's balance on a layer is not the sum of its entries.
    Commitment {
        /// The kernel account, hex.
        account: String,
        /// The layer.
        layer: u8,
    },
    /// Axis 1(a): an entry names an account the store does not hold, or is not a commitment.
    Entry {
        /// What.
        detail: String,
    },
    /// Axis 1(b): the rebuilt store differs from the stored one.
    Rebuild {
        /// What.
        detail: String,
    },
    /// Axis 1(c): the external source disagrees.
    External {
        /// The ledger account, `did:`-shaped.
        account: String,
        /// The asset.
        asset: String,
        /// Why.
        reason: String,
    },
    /// Axis 2: a (currency, layer) whose commitments do not sum to the identity.
    Unbalanced {
        /// The kernel currency tag.
        currency: u32,
        /// The layer.
        layer: usize,
        /// Where: over the account leaves, or over the history's entries.
        over: String,
    },
    /// Axis 2: an asset whose trial balance on openings is not zero.
    TrialBalance {
        /// The asset.
        asset: String,
    },
    /// Axis 3: the book's position and the settlement layer's differ at a cut (`F-017`).
    Settlement {
        /// The asset.
        asset: String,
        /// The cut: the inbox length at the finalized block compared.
        cut: u64,
        /// The layer's position less the book's, minor units: an aggregate, no account's.
        difference: String,
    },
    /// Axis 3: the latest cut the adapter reported is older than [`MAX_CUT_AGE_MS`].
    StaleCut {
        /// The cut.
        cut: u64,
        /// Its age, in milliseconds.
        age: u64,
    },
}

/// One axis's result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AxisResult {
    /// The axis: 1, 2 or 3.
    pub axis: u8,
    /// Whether it could run.
    pub ran: bool,
    /// What it found; empty when it reconciles.
    pub findings: Vec<Finding>,
}

/// Axis 1(a) over `snap`.
pub fn axis_1a(snap: &BookSnapshot) -> Vec<Finding> {
    let mut out: Vec<Finding> = check_entries(&snap.as_stored())
        .into_iter()
        .map(|f| match f {
            Rederived::Balance { account, layer } => Finding::Commitment {
                account: hex::encode(account.0),
                layer,
            },
            other => Finding::Entry {
                detail: format!("{other:?}"),
            },
        })
        .collect();
    for (id, a) in &snap.stored {
        if a.is_none() {
            out.push(Finding::Entry {
                detail: format!(
                    "the log names account {} the store lacks",
                    hex::encode(id.0)
                ),
            });
        }
    }
    out
}

/// Axis 1(b) over `snap`: the leaf store rebuilt from the delta log of `store` against the stored
/// leaves and root.
pub fn axis_1b<S: LeafStore + ?Sized>(snap: &BookSnapshot, store: &S) -> Vec<Finding> {
    let mut out = Vec::new();
    let rebuilt = match crate::restore::rebuild_leaf_store(store, None) {
        Ok(r) => r,
        Err(e) => {
            return vec![Finding::Rebuild {
                detail: e.to_string(),
            }];
        }
    };
    if rebuilt.root != hex::encode(snap.root) {
        out.push(Finding::Rebuild {
            detail: "the rebuilt root is not the store's".into(),
        });
    }
    out.extend(
        rebuilt
            .inv_6
            .into_iter()
            .map(|d| Finding::Rebuild { detail: d }),
    );
    for (id, folded) in &snap.history.accounts {
        if snap.stored.get(id).and_then(Option::as_ref) != Some(folded) {
            out.push(Finding::Rebuild {
                detail: format!("account {} differs from its rebuild", hex::encode(id.0)),
            });
        }
    }
    out
}

/// Axis 1(c): `source` against the book, inside the core, as a privileged read by `ctx`'s
/// principal with `justification`, recorded in `sink` first.
pub fn axis_1c<S: LeafStore, V: VaultBackend>(
    ledger: &mut Ledger<S, V>,
    ctx: &Ctx,
    source: &dyn ExternalSource,
    justification: &[u8],
    sink: &mut dyn AuditSink,
) -> Result<Vec<Finding>, Error> {
    let found = ledger
        .reconcile_external(ctx, source, justification, sink)
        .map_err(|r| Error::Core(format!("the external read was refused: {r:?}")))?;
    Ok(found
        .into_iter()
        .map(|m| Finding::External {
            account: m.account.to_did(),
            asset: m.asset.as_str().to_owned(),
            reason: match m.reason {
                MismatchReason::Differs(k) => format!("differs: {k:?}"),
                MismatchReason::MissingInBook => "missing in the book".into(),
                MismatchReason::MissingInSource => "missing in the source".into(),
            },
        })
        .collect())
}

fn point(bytes: &[u8; 32]) -> Option<RistrettoPoint> {
    CompressedRistretto(*bytes).decompress()
}

/// Axis 2 on commitments over `snap`: the stored account leaves, and the history's entries.
pub fn axis_2_commitments(snap: &BookSnapshot) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut leaves: BTreeMap<(u32, usize), RistrettoPoint> = BTreeMap::new();
    for a in snap.stored.values().flatten() {
        for (layer, pair) in a.balances.iter().enumerate() {
            let (Some(dr), Some(cr)) = (point(&pair.dr.0), point(&pair.cr.0)) else {
                out.push(Finding::Entry {
                    detail: format!("account {}: not a commitment", hex::encode(a.id.0)),
                });
                continue;
            };
            *leaves
                .entry((a.ledger, layer))
                .or_insert_with(RistrettoPoint::identity) += cr - dr;
        }
    }
    let mut entries: BTreeMap<(u32, usize), RistrettoPoint> = BTreeMap::new();
    for t in snap.history.transfers.values() {
        for e in &t.entries {
            let Some(p) = point(&e.amount.0) else {
                continue;
            };
            let s = entries
                .entry((e.ledger, e.layer.as_index()))
                .or_insert_with(RistrettoPoint::identity);
            match e.direction {
                cipher_clerk::Direction::Credit => *s += p,
                cipher_clerk::Direction::Debit => *s -= p,
            }
        }
    }
    for (over, sums) in [("account leaves", leaves), ("entries", entries)] {
        for ((currency, layer), p) in sums {
            if p != RistrettoPoint::identity() {
                out.push(Finding::Unbalanced {
                    currency,
                    layer,
                    over: over.into(),
                });
            }
        }
    }
    out
}

/// Axis 2 on openings: every asset's trial balance is exactly zero.
pub fn axis_2_openings<S: LeafStore, V: VaultBackend>(ledger: &Ledger<S, V>) -> Vec<Finding> {
    ledger
        .state()
        .registry()
        .iter()
        .filter(|a| !matches!(ledger.trial_balance(&a.id()), Ok(0)))
        .map(|a| Finding::TrialBalance {
            asset: a.id().as_str().to_owned(),
        })
        .collect()
}

/// How old the settlement layer's latest reported cut may be before axis 3 finds it stale: a day.
/// The adapter reports positions whenever a final anchor wants its comparison (hourly by default)
/// or the positions changed, so a cut older than this means anchoring or the adapter stopped.
pub const MAX_CUT_AGE_MS: u64 = 86_400_000;

/// Axis 3's comparison (`REQ-PR-5`, `AC-J3.1`, `AC-J3.2`): `F-017`'s `compare_settlement` at the
/// latest cut the adapter reported, per asset. Implementations say whether it could run and what
/// it found.
pub trait SettlementComparator {
    /// The findings, or `None` when no comparison could be made.
    fn compare(&self) -> Option<Vec<Finding>>;
}

/// Axis 3 could not run, and why: no positions report yet (`ERR-EvidenceInsufficient`), the core
/// refused, ingress did not answer. Recorded as not run, which pages as "no run in 24 h".
#[derive(Clone, Debug, Default)]
pub struct NotRun(pub String);

impl SettlementComparator for NotRun {
    fn compare(&self) -> Option<Vec<Finding>> {
        None
    }
}

/// A comparison `compare_settlement` made: every asset that differs by as little as one minor unit
/// is a finding, and so is a cut the adapter reported more than [`MAX_CUT_AGE_MS`] before.
#[derive(Clone, Debug)]
pub struct Compared {
    /// The comparison, as ingress returned it.
    pub comparison: ComparisonBody,
    /// When the adapter reported the cut compared, if known.
    pub reported_at: Option<u64>,
    /// When axis 3 ran.
    pub now: u64,
}

impl SettlementComparator for Compared {
    fn compare(&self) -> Option<Vec<Finding>> {
        let c = &self.comparison;
        let mut out: Vec<Finding> = c
            .rows
            .iter()
            .filter(|r| r.difference.trim_start_matches('-') != "0")
            .map(|r| Finding::Settlement {
                asset: r.asset.clone(),
                cut: c.cut,
                difference: r.difference.clone(),
            })
            .collect();
        if let Some(at) = self.reported_at {
            let age = self.now.saturating_sub(at);
            if age > MAX_CUT_AGE_MS {
                out.push(Finding::StaleCut { cut: c.cut, age });
            }
        }
        Some(out)
    }
}

/// Axis 3 through `comparator`.
pub fn axis_3(comparator: &dyn SettlementComparator) -> AxisResult {
    match comparator.compare() {
        Some(findings) => AxisResult {
            axis: 3,
            ran: true,
            findings,
        },
        None => AxisResult {
            axis: 3,
            ran: false,
            findings: Vec::new(),
        },
    }
}

/// One `Suspense` (account, asset) not at zero (`REQ-LA-2`): which, and since when. No figure.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SuspenseRow {
    /// The ledger account, `did:`-shaped.
    pub account: String,
    /// The asset.
    pub asset: String,
    /// When its balance last left zero.
    pub since: u64,
}

/// The `Suspense` report (`REQ-LA-2`, `RISK-8`): every `Suspense` account's non-zero balance in any
/// balance kind, with how long it has not been zero. `Suspense` must be reconciled to zero on a
/// cadence; the report is how the cadence is watched.
pub fn suspense_report<S: LeafStore, V: VaultBackend>(ledger: &Ledger<S, V>) -> Vec<SuspenseRow> {
    let state = ledger.state();
    let journal = state.journal();
    let mut out = Vec::new();
    for acct in state
        .all_accounts()
        .filter(|a| a.kind() == AccountKind::Suspense)
    {
        for asset in state.registry().iter() {
            let id = acct.id();
            let asset = asset.id();
            if journal.balance_now(&id, &asset) == Sums::default() {
                continue;
            }
            // Walk the account's postings in the asset: `since` is when it last left zero.
            let mut sums = Sums::default();
            let mut since = acct.opened_at();
            for p in journal.position_postings(&id, &asset, 0, u64::MAX, usize::MAX) {
                let was_zero = sums == Sums::default();
                for l in p
                    .lines
                    .iter()
                    .filter(|l| l.account == id && l.asset == asset)
                {
                    sums.add(l);
                }
                if was_zero && sums != Sums::default() {
                    since = p.at;
                }
            }
            out.push(SuspenseRow {
                account: id.to_did(),
                asset: asset.as_str().to_owned(),
                since,
            });
        }
    }
    out.sort();
    out
}

/// One reconciliation run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    /// When it ran, in milliseconds since the Unix epoch.
    pub at: u64,
    /// Its axes.
    pub axes: Vec<AxisResult>,
    /// The `Suspense` report.
    #[serde(default)]
    pub suspense: Vec<SuspenseRow>,
    /// The operation reference of the audited `compare_settlement` that recorded this run on the
    /// audit trail, its justification the run's summary; `None` when axis 3 could not run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audited: Option<String>,
}

impl Run {
    /// Whether every axis ran and found nothing.
    pub fn clean(&self) -> bool {
        self.axes.iter().all(|a| a.ran && a.findings.is_empty())
    }

    /// The axis `n`.
    pub fn axis(&self, n: u8) -> Option<&AxisResult> {
        self.axes.iter().find(|a| a.axis == n)
    }

    /// The run's summary, as the audit record's justification carries it: every count, no
    /// figure, and the digest of axes 1 and 2's findings and the `Suspense` report. Axis 3's
    /// comparison is the recording command's own result, recorded beside it.
    pub fn summary(&self) -> String {
        let count = |n: u8| {
            self.axis(n).map_or("not run".to_owned(), |a| {
                if a.ran {
                    format!("{} differences", a.findings.len())
                } else {
                    "not run".to_owned()
                }
            })
        };
        // Axis 3's comparison is the recording command's own result: the digest covers the rest.
        let before: Vec<&AxisResult> = self.axes.iter().filter(|a| a.axis != 3).collect();
        let digest = crate::checksum::sha256_hex(
            &serde_json::to_vec(&(&before, &self.suspense)).unwrap_or_default(),
        );
        format!(
            "NFR-9 daily reconciliation at {}: axis 1 {}, axis 2 {}; Suspense {} non-zero; findings sha256 {digest}",
            self.at,
            count(1),
            count(2),
            self.suspense.len()
        )
    }

    /// The `Suspense` signal at `now`.
    pub fn suspense_signal(&self, now: u64) -> SuspenseSignal {
        SuspenseSignal {
            nonzero: self.suspense.len() as u64,
            oldest_age: self
                .suspense
                .iter()
                .map(|r| now.saturating_sub(r.since))
                .max()
                .unwrap_or(0),
        }
    }

    /// Write the run into `dir` as `run-<at>.json`. Returns its path.
    pub fn record(&self, dir: &Path) -> Result<PathBuf, Error> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("run-{:020}.json", self.at));
        let json = serde_json::to_vec_pretty(self).map_err(|e| Error::Config(e.to_string()))?;
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }
}

/// What a run needs beside the book: the external source of axis 1(c), with the principal, the
/// justification and the audit sink of its privileged read; and the comparator of axis 3.
pub struct Sources<'a> {
    /// Axis 1(c)'s source, if one is configured (shadow and cutover only).
    pub external: Option<External<'a>>,
    /// Axis 3's comparator.
    pub settlement: &'a dyn SettlementComparator,
}

/// Axis 1(c)'s source and its privileged read's attribution.
pub struct External<'a> {
    /// The source.
    pub source: &'a dyn ExternalSource,
    /// The operator, with the `PrivilegedRead` kind.
    pub ctx: Ctx,
    /// Why (`REQ-PV-4`).
    pub justification: &'a [u8],
    /// The audit store.
    pub sink: &'a mut dyn AuditSink,
}

/// Run every axis over `ledger` at `at`. `snap` is read from the ledger's store unless given (the
/// fault-injection tests give a changed copy).
pub fn run<S: LeafStore, V: VaultBackend>(
    ledger: &mut Ledger<S, V>,
    snap: Option<BookSnapshot>,
    sources: Sources<'_>,
    at: u64,
) -> Result<Run, Error> {
    let snap = match snap {
        Some(s) => s,
        None => BookSnapshot::read(ledger.book().store())?,
    };
    let mut one = axis_1a(&snap);
    one.extend(axis_1b(&snap, ledger.book().store()));
    if let Some(x) = sources.external {
        one.extend(axis_1c(ledger, &x.ctx, x.source, x.justification, x.sink)?);
    }
    let mut two = axis_2_commitments(&snap);
    two.extend(axis_2_openings(ledger));
    Ok(Run {
        at,
        axes: vec![
            AxisResult {
                axis: 1,
                ran: true,
                findings: one,
            },
            AxisResult {
                axis: 2,
                ran: true,
                findings: two,
            },
            axis_3(sources.settlement),
        ],
        suspense: suspense_report(ledger),
        audited: None,
    })
}

/// Ingress, as the daily job reaches it: a request as the operations principal, its status and
/// its JSON body.
pub trait IngressLink: Send + Sync {
    /// `method path` with `body`.
    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> impl std::future::Future<Output = Result<(u16, serde_json::Value), String>> + Send;
}

/// Ingress's surface in process, with the operations principal's token.
#[derive(Clone)]
pub struct RouterLink {
    /// The surface.
    pub router: axum::Router,
    /// The operations principal's token.
    pub token: String,
}

impl IngressLink for RouterLink {
    async fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<(u16, serde_json::Value), String> {
        let (s, v) =
            ingress::testkit::call(&self.router, method, path, Some(&self.token), &[], body).await;
        Ok((s.as_u16(), v))
    }
}

/// The daily job's axis 3 through ingress (`F-017`'s `TODO(F-018)`: the daily comparison). It
/// reads when the adapter last reported the layer's positions (`GET /v1/ops/anchors`), then asks
/// the core to compare at that cut (`POST /v1/ops/settlement/comparisons`), as the operations
/// principal with the `compare_settlement` operator kind, with `summary` as the justification: the
/// comparison and the summary of the run go on the audit trail together, in one operator record.
/// Returns the comparator and the recording command's operation reference.
pub async fn compare_through<L: IngressLink>(
    link: &L,
    summary: &str,
    now: u64,
) -> (Box<dyn SettlementComparator + Send + Sync>, Option<String>) {
    let reported_at = match link.call("GET", "/v1/ops/anchors", None).await {
        Ok((200, v)) => v["latest_comparison"]["at"].as_u64(),
        _ => None,
    };
    let reply = link
        .call(
            "POST",
            "/v1/ops/settlement/comparisons",
            Some(serde_json::json!({ "justification": summary })),
        )
        .await;
    let (status, v) = match reply {
        Ok(r) => r,
        Err(e) => return (Box::new(NotRun(format!("ingress: {e}"))), None),
    };
    if status != 200 {
        return (Box::new(NotRun(format!("{status}: {}", v["error"]))), None);
    }
    let audited = v["operation_reference"].as_str().map(str::to_owned);
    match serde_json::from_value::<ComparisonBody>(v["result"]["anchoring"]["comparison"].clone()) {
        Ok(comparison) => (
            Box::new(Compared {
                comparison,
                reported_at,
                now,
            }),
            audited,
        ),
        Err(e) => (Box::new(NotRun(format!("the comparison: {e}"))), audited),
    }
}

/// The daily run (`NFR-9`, `AC-J3.1`): axes 1 and 2 and the `Suspense` report over the running
/// core, under its lock; then axis 3 through ingress, which also records the run's summary on the
/// audit trail ([`compare_through`]); then the run's JSON record in `records`. `external` is axis
/// 1(c)'s source during shadow and cutover.
///
/// `TODO(F-005)`: the core's host schedules it daily; in process it is a library call.
pub async fn daily<S, V, L>(
    core: &ingress::inprocess::InProcessCore<S, V>,
    link: &L,
    external: Option<External<'_>>,
    at: u64,
    records: &Path,
) -> Result<Run, Error>
where
    S: LeafStore + Send + Sync + 'static,
    V: VaultBackend + Send + Sync + 'static,
    L: IngressLink,
{
    // Axis 3 runs through ingress after axes 1 and 2, and replaces this.
    static PENDING: NotRun = NotRun(String::new());
    let mut run = core
        .checkpoint(|l, _| {
            run(
                l,
                None,
                Sources {
                    external,
                    settlement: &PENDING,
                },
                at,
            )
        })
        .ok_or_else(|| Error::Core("the core is not running".into()))??;
    let (comparator, audited) = compare_through(link, &run.summary(), at).await;
    run.axes.retain(|a| a.axis != 3);
    run.axes.push(axis_3(comparator.as_ref()));
    run.audited = audited;
    run.record(records)?;
    Ok(run)
}

/// The latest run recorded in `dir`, by time.
pub fn latest_run(dir: &Path) -> Result<Option<Run>, Error> {
    if !dir.exists() {
        return Ok(None);
    }
    let mut names: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    names.sort();
    match names.last() {
        Some(p) => serde_json::from_slice(&std::fs::read(p)?)
            .map(Some)
            .map_err(|e| Error::Config(format!("{}: {e}", p.display()))),
        None => Ok(None),
    }
}

/// The latest run of each axis recorded in `dir`: for the exporter. An axis that has not run is
/// reported with no run time.
pub fn latest_axes(dir: &Path) -> Result<Vec<AxisSignal>, Error> {
    let mut out: BTreeMap<u8, AxisSignal> = (1..=3)
        .map(|axis| {
            (
                axis,
                AxisSignal {
                    axis,
                    last_run: None,
                    differences: 0,
                    ran: false,
                },
            )
        })
        .collect();
    if dir.exists() {
        let mut names: Vec<PathBuf> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        names.sort();
        for p in names {
            let run: Run = serde_json::from_slice(&std::fs::read(&p)?)
                .map_err(|e| Error::Config(format!("{}: {e}", p.display())))?;
            for a in run.axes {
                let s = out.entry(a.axis).or_insert(AxisSignal {
                    axis: a.axis,
                    last_run: None,
                    differences: 0,
                    ran: false,
                });
                if a.ran && s.last_run.is_none_or(|t| run.at >= t) {
                    s.last_run = Some(run.at);
                    s.differences = a.findings.len() as u64;
                    s.ran = true;
                }
            }
        }
    }
    Ok(out.into_values().collect())
}
