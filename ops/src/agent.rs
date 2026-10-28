// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The Agent core's restore from its off-node archive (`T-005-16`, `NFR-4`, `NFR-10`, `INV-5`,
//! `INV-6`, `INV-19`), and the replay of ingress's received-log after its point.
//!
//! `opsctl restore core --from <archive>` (in-process core: [`crate::restore`]):
//!
//! 1. **Read and check the archive** ([`core_archive::fetch()`]): every segment's digest and
//!    offsets, the entries' hash chain, the replies gapless, the restore point at the chain's end.
//! 2. **Re-derive the book natively** ([`verify()`]): genesis from the deployment's configuration,
//!    then every archived input applied in order, each reaching its archived roots
//!    (`core_binding::host::replay_in`, `REQ-RT-3`). The drill's checks run on that book
//!    ([`crate::drill::checks`]): `INV-6`, `INV-5`, the roots at the archive's point, `INV-19`
//!    against the settlement layer's anchors, and `NFR-4` against every archived reply. One more
//!    check reads the layer: an anchor **after** the book's last checkpoint means the layer saw
//!    a book this archive does not reach, and the restore is refused (`archive-behind-anchor`).
//!    Both read the layer **per book** ([`behind_anchor`], [`crate::drill::anchor_check`]): after
//!    a governance reset of the settlement contract (amendment 0039) the venue holds one anchor
//!    sequence per book, and another book's anchor of the same number is not this book's.
//!    Any failed check refuses the restore before anything is laid out, and so does a check that
//!    is [`Verdict::Unknown`] (an anchor the node cannot read: read from an archive node).
//! 3. **Lay it out** on the fresh node ([`core_archive::lay_out`]) and, with `--core-host`, run
//!    `aledgent-core-host restore` there: the Agent, from the archive's latest checkpoint or
//!    founded afresh, is re-sent every archived input after it, each checked against its entry.
//!
//! Then, with ingress's received-log intact, [`replay_tail`] (`opsctl restore core-tail`) re-sends
//! every input the log shows decided after the archive's point, in order, through the restored
//! core's boundary, and each must come back with the reply the log recorded: the window between
//! the last archived segment and the failure is re-decided, not lost (`NFR-4`).

use std::collections::BTreeSet;

use core_binding::hot::RingEntry;
use ingress::boundary::{Core, CoreReply, InputKey, Outcome};
use ingress::service::input_of;
use ingress::store::Row;
use ingress::wire::state_named;
use ledger_core::call::Call;
use ledger_core::export::rederive::Acknowledgement;
use ledger_core::id::OperationId;
use ledger_kernel::store::LeafStore as _;
use serde::{Deserialize, Serialize};
use verify::{AnchorSource, NumberedAnchors, Unread};

use crate::Error;
use crate::drill::{Check, Verdict, checks, pinned_epoch};
use crate::restore::{RebuildReport, decided_inputs, rebuild_leaf_store};

/// The acknowledgements the archived replies show (`NFR-4`): every decided operation, and the
/// state its reply showed.
///
/// A reply to one of the settlement adapter's inputs (a declaration, an observation, a check, a
/// head, a gap, a re-scan of the earlier code, an anchor, a position cut, an attestation) names the
/// reference ingress minted for that input, and no operation: the core keeps what the adapter
/// reported in its settlement store, at coordinates, and the adapter's sync shows them complete
/// (`REQ-IN-5`). No operation is
/// behind that reference, so it cannot be lost as one, and holding it to
/// [`ledger_core::export::rederive::check_acknowledged`] refused every restore of a book that has
/// an adapter (the 2026-10-15 restore drill: 63,947 of 70,761 replies). The archive's entries carry
/// each input's call, so such a reply is told by its reference being the one an archived
/// `Call::Adapter` ran under. The adapter's acknowledgement of an instruction, its signing record and
/// its evidence check name the *real* operation, not the minted one, and stay checked, as does
/// every other command's reply. The received-log's own check ([`crate::drill::acknowledgements`])
/// skips the adapter's commands for the same reason.
pub fn acknowledgements(
    replies: &[(InputKey, CoreReply)],
    entries: &[RingEntry],
) -> Vec<Acknowledgement> {
    let adapter_inputs: BTreeSet<OperationId> = entries
        .iter()
        .flat_map(|e| &e.calls)
        .filter_map(|c| match c {
            Call::Adapter { ctx, .. } => Some(ctx.op),
            _ => None,
        })
        .collect();
    let mut out = Vec::new();
    for (_, r) in replies {
        if let Outcome::Decided {
            operation_reference,
            state,
            ..
        } = &r.outcome
            && let Ok(op) = OperationId::parse(operation_reference)
            && !adapter_inputs.contains(&op)
        {
            out.push(Acknowledgement {
                op,
                state: state.as_ref().and_then(|s| state_named(&s.state)),
            });
        }
    }
    out
}

/// What the native re-derivation found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verified {
    /// The checks.
    pub checks: Vec<Check>,
    /// The leaf store rebuilt from the re-derived book's delta log.
    pub rebuild: RebuildReport,
    /// Postings in the book.
    pub postings: u64,
}

impl Verified {
    /// Whether every check that can run passed. A check that is [`Verdict::Unknown`] is not a
    /// pass: nothing is laid out until it is decided.
    pub fn passed(&self) -> bool {
        self.checks
            .iter()
            .all(|c| !matches!(c.verdict, Verdict::Failed | Verdict::Unknown))
            && self.checks.iter().any(|c| c.verdict == Verdict::Passed)
    }

    /// Whether no check failed and some could not decide ([`Verdict::Unknown`]).
    pub fn undecided(&self) -> bool {
        crate::drill::undecided(&self.checks)
    }
}

/// `archive-behind-anchor` (module documentation): is there an anchor of the **archived book**
/// after its last checkpoint, `last_number`? `next` is every sequence's anchor of
/// `last_number + 1`. `book` is the anchor sequence the archived book lives in, when one of its
/// checkpoints proves it ([`pinned_epoch`]); without one the archive is taken to be of the
/// venue's current book, which is the only one that is still anchoring (and the detail says so).
///
/// Only the book's own sequence counts: after a reset, the previous book's anchor of the same
/// number is no sign that this archive is behind.
///
/// - an anchor of that number in the book's sequence, read or not (an anchor that is no longer the
///   latest is proven to exist even if the node cannot read it): `Failed`;
/// - the sequences before the first one identified could not be identified and may include the
///   book's: `Unknown`, read from an archive node;
/// - otherwise `Passed`.
pub fn behind_anchor(
    last_number: u64,
    next: &NumberedAnchors,
    book: Option<u32>,
) -> (Verdict, String) {
    let epoch = book.unwrap_or(next.current_epoch);
    let after = last_number + 1;
    let of = if next.current_epoch > 0 {
        format!(
            " in anchor sequence {epoch}{}",
            if book.is_some() {
                ""
            } else {
                " (the venue's current one: no checkpoint of the archived book is proven on the layer to say otherwise)"
            }
        )
    } else {
        String::new()
    };
    if next.found.iter().any(|(s, _)| s.epoch == epoch) {
        return (
            Verdict::Failed,
            format!(
                "the settlement layer holds anchor {after}{of}, after the archive's last checkpoint \
                 {last_number}: the archive does not reach the book the layer anchored"
            ),
        );
    }
    for u in &next.unread {
        match u {
            Unread::Anchor { sequence, .. } if sequence.epoch == epoch => {
                return (
                    Verdict::Failed,
                    format!(
                        "the settlement layer holds anchor {after}{of} (this node cannot read it: it \
                         is no longer the latest), after the archive's last checkpoint \
                         {last_number}: the archive does not reach the book the layer anchored"
                    ),
                );
            }
            Unread::Before { epoch: first, .. } if epoch < *first => {
                return (
                    Verdict::Unknown,
                    format!(
                        "the anchor sequences before {first} could not be identified (read from an \
                         archive node): whether the book's sequence {epoch} holds anchor {after} \
                         is unknown"
                    ),
                );
            }
            _ => {}
        }
    }
    (
        Verdict::Passed,
        format!("no anchor after checkpoint {last_number}{of}"),
    )
}

/// Step 2 (module documentation): re-derive the archived book on `founding` and run the checks.
/// `layer` reads the settlement layer's anchors; without it `INV-19` is `Deferred`, never passed.
pub fn verify(
    fetched: &core_archive::Fetched,
    founding: &core_binding::core::Founding,
    layer: Option<&dyn AnchorSource>,
) -> Result<Verified, Error> {
    let f = founding;
    let (replica, _service) =
        core_binding::host::replay(&f.keys, f.genesis.clone(), f.op, f.at, &fetched.entries)
            .map_err(|e| Error::Restore(format!("the archive does not re-derive: {e:?}")))?;
    let store = replica.book().store();
    let rebuild = rebuild_leaf_store(store, None)?;
    let acks = acknowledgements(&fetched.replies_parsed, &fetched.entries);
    let mut out = checks(&replica, &acks, None, &rebuild, layer.map(|l| (l, 0)));
    // The roots at the archive's point: the chain's last entry, re-derived.
    let p = &fetched.point;
    let mut at_point = Vec::new();
    match replica.book().root() {
        Ok(r) if p.entries == 0 || hex::encode(r) == p.kernel_root => {}
        Ok(r) => at_point.push(format!(
            "the re-derived kernel root {} is not the archive's {}",
            hex::encode(r),
            p.kernel_root
        )),
        Err(e) => at_point.push(format!("the book: {e:?}")),
    }
    out.push(Check {
        name: "archive-point".into(),
        verdict: if at_point.is_empty() {
            Verdict::Passed
        } else {
            Verdict::Failed
        },
        detail: if at_point.is_empty() {
            format!(
                "input {} (Agent input {}), {} archived replies",
                p.inputs,
                p.entries,
                fetched.replies_parsed.len()
            )
        } else {
            at_point.join("; ")
        },
    });
    // An anchor after the book's last checkpoint: the archive is behind what the layer saw.
    if let Some(layer) = layer {
        let checkpoints = replica.state().anchoring();
        let last = checkpoints.checkpoints().max_by_key(|c| c.number).copied();
        let (verdict, detail) = match last {
            Some(c) => {
                // The book's own anchor sequence, from the latest checkpoint the layer proves.
                let book = checkpoints
                    .checkpoints()
                    .filter(|f| f.status == ledger_core::anchor::CheckpointStatus::Final)
                    .max_by_key(|f| f.number)
                    .and_then(|f| {
                        let read = layer.anchors(&f.anchored_in(), f.number).ok()?;
                        pinned_epoch(&read, &f.commitment)
                    });
                match layer.anchors(&c.anchored_in(), c.number + 1) {
                    Ok(next) => behind_anchor(c.number, &next, book),
                    Err(e) => (
                        Verdict::Failed,
                        format!("the settlement layer could not be read: {e}"),
                    ),
                }
            }
            None => (
                Verdict::Passed,
                "the archived book has no checkpoint yet".into(),
            ),
        };
        out.push(Check {
            name: "archive-behind-anchor".into(),
            verdict,
            detail,
        });
    }
    Ok(Verified {
        checks: out,
        rebuild,
        postings: store.next_posting_seq().unwrap_or(0),
    })
}

/// What [`replay_tail`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TailReport {
    /// The archive's point: the core input number the restored core stood at.
    pub after: u64,
    /// Inputs re-sent and answered as the log recorded.
    pub replayed: u64,
    /// The last input number the log shows decided.
    pub last: u64,
}

/// Re-send, through `core`, every input the received-log shows decided after `after`, in order;
/// each must come back under its input number with the reply the log recorded (`REQ-RT-3`), and
/// any other reply stops the replay: a divergence, never a new truth. Rows still undecided are
/// the forwarder's, once ingress runs.
pub async fn replay_tail<C: Core>(core: &C, rows: &[Row], after: u64) -> Result<TailReport, Error> {
    let inputs = decided_inputs(rows)?;
    let last = inputs.keys().next_back().copied().unwrap_or(0);
    let mut replayed = 0;
    for (seq, row) in inputs.range(after + 1..) {
        let input = input_of(row).map_err(Error::Restore)?;
        let reply = core
            .apply(&input)
            .await
            .map_err(|e| Error::Core(format!("input {seq}: {e:?}")))?;
        let stored: CoreReply =
            serde_json::from_slice(row.core_reply.as_deref().unwrap_or_default())
                .map_err(|e| Error::Restore(format!("input {seq}'s stored reply: {e}")))?;
        if reply != stored {
            return Err(Error::Restore(format!(
                "input {seq} replayed to another reply than the received-log recorded: stop \
                 (ops/runbooks/reconciliation-failure.md)"
            )));
        }
        replayed += 1;
    }
    Ok(TailReport {
        after,
        replayed,
        last,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use verify::{PublicAnchor, Sequence};

    fn seq(epoch: u32) -> Sequence {
        Sequence {
            epoch,
            base: 0,
            book: (epoch > 0).then_some([0xB0 + epoch as u8; 32]),
        }
    }

    fn anchor() -> PublicAnchor {
        PublicAnchor {
            commitment: [1; 32],
            stored_at: 3,
            read_at: 4,
        }
    }

    fn unread(epoch: u32) -> Unread {
        Unread::Anchor {
            sequence: seq(epoch),
            why: "State already discarded".into(),
        }
    }

    /// No reset: as it always was, and the texts are the ones the operator knows.
    #[test]
    fn t_026_12_without_a_reset_an_anchor_after_the_last_checkpoint_refuses_the_restore() {
        let (v, d) = behind_anchor(4, &NumberedAnchors::first_sequence(None), Some(0));
        assert_eq!(
            (v, d.as_str()),
            (Verdict::Passed, "no anchor after checkpoint 4")
        );
        let (v, d) = behind_anchor(4, &NumberedAnchors::first_sequence(Some(anchor())), Some(0));
        assert_eq!(v, Verdict::Failed, "{d}");
        assert!(
            d.contains("anchor 5, after the archive's last checkpoint 4"),
            "{d}"
        );
        // The anchor is no longer the latest and the node cannot read it: it exists all the same.
        let gone = NumberedAnchors {
            current_epoch: 0,
            found: vec![],
            unread: vec![unread(0)],
        };
        assert_eq!(behind_anchor(4, &gone, Some(0)).0, Verdict::Failed);
    }

    /// After a reset the old book's anchor of the same number is another book's: not a sign the
    /// archive of the new book is behind; the new book's own anchor of it is.
    #[test]
    fn t_026_12_after_a_reset_only_the_archived_books_own_sequence_counts() {
        // Archive of the new book (sequence 1) at checkpoint 4; the old book (sequence 0) held up
        // to 17, so both numbers 5 exist there. Archive node:
        let old_only = NumberedAnchors {
            current_epoch: 1,
            found: vec![(seq(0), anchor())],
            unread: vec![],
        };
        let (v, d) = behind_anchor(4, &old_only, Some(1));
        assert_eq!(v, Verdict::Passed, "{d}");
        // Plain node: the old book's anchor of 5 is unreadable. Still not this book's.
        let plain = NumberedAnchors {
            current_epoch: 1,
            found: vec![],
            unread: vec![unread(0)],
        };
        assert_eq!(behind_anchor(4, &plain, Some(1)).0, Verdict::Passed);
        // The new book has anchored 5: the archive is behind, read or not.
        let both = NumberedAnchors {
            current_epoch: 1,
            found: vec![(seq(1), anchor()), (seq(0), anchor())],
            unread: vec![],
        };
        assert_eq!(behind_anchor(4, &both, Some(1)).0, Verdict::Failed);
        let superseded = NumberedAnchors {
            current_epoch: 1,
            found: vec![(seq(0), anchor())],
            unread: vec![unread(1)],
        };
        assert_eq!(behind_anchor(4, &superseded, Some(1)).0, Verdict::Failed);
        // No checkpoint of the book is proven on the layer yet (the new book before its first
        // anchor): the venue's current book is taken, and the text says so.
        let (v, d) = behind_anchor(0, &plain, None);
        assert_eq!(v, Verdict::Passed, "{d}");
        assert!(
            d.contains("anchor sequence 1") && d.contains("current one"),
            "{d}"
        );
        // The archive is of the old book: its own sequence's next anchor counts.
        let (v, _) = behind_anchor(17, &both, Some(0));
        assert_eq!(v, Verdict::Failed);
    }

    /// The sequences before the first identified one may include the book's: unknown, which is
    /// not a pass; a book in an identified sequence is not excused by it.
    #[test]
    fn t_026_12_a_book_in_an_unidentified_sequence_is_unknown() {
        let second = NumberedAnchors {
            current_epoch: 2,
            found: vec![],
            unread: vec![Unread::Before {
                epoch: 2,
                last_anchor: Some(5),
                why: "past state".into(),
            }],
        };
        let (v, d) = behind_anchor(4, &second, Some(0));
        assert_eq!(v, Verdict::Unknown, "{d}");
        assert_eq!(behind_anchor(4, &second, Some(2)).0, Verdict::Passed);
    }
}
