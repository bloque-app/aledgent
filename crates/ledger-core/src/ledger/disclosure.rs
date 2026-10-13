// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Disclosures (`F-017` T-017-04 to T-017-06; `CTR-DSC`, `REQ-DS-1`–`DS-5`, `REQ-PV-6`,
//! `REQ-AR-5`, `REQ-PR-6`, `REQ-AT-1`).
//!
//! `issue_disclosure`, in order:
//!
//! 1. **Authority** (`REQ-DS-5`). The subject account's owner, or a delegated grant over it that
//!    names `Disclose` — an authority the subject granted. The operator only under a legal
//!    obligation: its operator-surface grant for `LegalDisclosure` and a justification, and only
//!    over an account in delegated custody. Anything else is `ERR-NotDisclosable`.
//! 2. **The checkpoint**: the one named, or the latest final one. Not final: `ERR-AnchorNotFinal`.
//! 3. **The assertion**, mapped to its claim ([`crate::anchor::disclosure`]); a balance kind other
//!    than `available`, or a window past the checkpoint, is `ERR-DisclosureUnsupported`, and so
//!    is a deployment with no assurance declaration.
//! 4. **The truth check** at the checkpoint's entry sequence, from the book's own journal: a false
//!    claim is `ERR-DisclosureFalse` (`REQ-DS-4`).
//! 5. **The envelope** (`REQ-DS-2`): subject, recipient, assertion, checkpoint, the assurance
//!    level with the signed-assertion label (`REQ-AR-5`, `REQ-PR-6`), the time, the claims in
//!    CipherClerk's wire form with `state_root = kernel_root`, and the clerk's signature over
//!    everything, both through [`ClerkSigner`].
//! 6. **Nothing else is revealed** (`REQ-PV-6`): no viewing key, no grant, no read capability.
//!    The recipient's principal reaches exactly what it reached before.
//! 7. **The audit record** (`REQ-AT-1`), class `Disclosure`: the principal, the time, the subject
//!    account and the recipient, and the justification on the legal path. The attribution record
//!    names the subject account, so the disclosure is in the account's action log.
//!
//! The disclosure is kept with its operation, so a repeat under the same client key returns the
//! same bytes (`REQ-OP-10`).

use alloc::collections::BTreeSet;
use alloc::format;
use alloc::vec;
use alloc::vec::Vec;

use ledger_kernel::partition::Partition;
use ledger_kernel::posting::{PostingKind, PostingMeta};
use ledger_kernel::store::{KeySet, LeafStore};
use ledger_kernel::vault::VaultBackend;

use super::{Accepted, Ctx, Ledger};
use crate::account::{AccountKind, Custody, Time};
use crate::anchor::clerk::ClerkSigner;
use crate::anchor::disclosure::{
    Assertion, BalanceKind, Bound, DisclosureRequest, FlowDirection, Recipient,
};
use crate::anchor::{AnchorWrite, Checkpoint, SIGNED_ASSERTION_LABEL};
use crate::audit::{AuditNote, AuditOutcome, AuditSubject, Justification};
use crate::authority::{AuthorityKind, AuthorityUsed, OperationKind};
use crate::eval::{self, Request};
use crate::id::{AccountId, OperationId};
use crate::lifecycle::keys::{Canon, RequestDigest, Surface};
use crate::operator::OperatorOpKind;
use crate::principal::{Caller, Principal, resolve};
use crate::query::journal::{Direction, PostingEntry};
use crate::refusal::Refusal;
use crate::state::{Command, OperationRecord, Write};

use verify::claim::{Claim, Filter, Relation, SignedClaim};
use verify::schnorr::public_commitment;

/// Postings that complete an operation's movement: what "occurred" and "moved" are read from.
const fn completes(kind: PostingKind) -> bool {
    matches!(
        kind,
        PostingKind::Transfer
            | PostingKind::Capture
            | PostingKind::Issue
            | PostingKind::Redeem
            | PostingKind::Conversion
    )
}

fn request_digest(p: &Principal, r: &DisclosureRequest) -> RequestDigest {
    let mut c = Canon::new("issue_disclosure");
    let surface = if r.legal_obligation.is_some() {
        Surface::Operator
    } else {
        Surface::Command
    };
    c.u8(surface.code());
    c.bytes(p.identity.as_bytes());
    c.opt(p.on_behalf_of, |c, id| {
        c.bytes(id.as_bytes());
    });
    c.bytes(r.subject.as_bytes());
    match r.recipient {
        Recipient::Identity(i) => {
            c.u8(1).bytes(i.as_bytes());
        }
        Recipient::Reference(b) => {
            c.u8(2).bytes(&b);
        }
    }
    match r.assertion {
        Assertion::HoldsAtLeast {
            asset,
            amount,
            kind,
        } => {
            c.u8(1)
                .bytes(asset.as_str().as_bytes())
                .u64(amount.get())
                .u8(match kind {
                    BalanceKind::Available => 1,
                    BalanceKind::Reserved => 2,
                    BalanceKind::Total => 3,
                });
        }
        Assertion::OperationOccurred { operation } => {
            c.u8(2).bytes(operation.as_bytes());
        }
        Assertion::OperationMoved {
            operation,
            asset,
            amount,
        } => {
            c.u8(3)
                .bytes(operation.as_bytes())
                .bytes(asset.as_str().as_bytes())
                .u64(amount.get());
        }
        Assertion::FlowWithin {
            asset,
            direction,
            from_sequence,
            to_sequence,
            bound,
        } => {
            c.u8(4)
                .bytes(asset.as_str().as_bytes())
                .u8(match direction {
                    FlowDirection::In => 1,
                    FlowDirection::Out => 2,
                })
                .u64(from_sequence)
                .u64(to_sequence);
            match bound {
                Bound::AtLeast(a) => c.u8(1).u64(a),
                Bound::AtMost(a) => c.u8(2).u64(a),
            };
        }
    }
    c.opt(r.checkpoint, |c, n| {
        c.u64(n);
    });
    c.opt(r.legal_obligation.as_deref(), |c, j| {
        c.bytes(j);
    });
    c.digest()
}

/// The envelope's assertion, in the verifier's terms.
fn envelope_assertion(a: &Assertion) -> verify::Assertion {
    match *a {
        Assertion::HoldsAtLeast {
            asset,
            amount,
            kind,
        } => verify::Assertion::HoldsAtLeast {
            asset: asset.as_str().into(),
            amount: amount.get(),
            kind,
        },
        Assertion::OperationOccurred { operation } => verify::Assertion::OperationOccurred {
            operation: operation.to_did(),
        },
        Assertion::OperationMoved {
            operation,
            asset,
            amount,
        } => verify::Assertion::OperationMoved {
            operation: operation.to_did(),
            asset: asset.as_str().into(),
            amount: amount.get(),
        },
        Assertion::FlowWithin {
            asset,
            direction,
            from_sequence,
            to_sequence,
            bound,
        } => verify::Assertion::FlowWithin {
            asset: asset.as_str().into(),
            direction,
            from_sequence,
            to_sequence,
            bound,
        },
    }
}

/// The settlement layer as the core itself holds it: its final checkpoints, for
/// [`Ledger::verify_disclosure`]. It publishes no key: the caller names the clerk key.
struct OwnAnchors<'a, S: LeafStore, V: VaultBackend>(&'a Ledger<S, V>);

impl<S: LeafStore, V: VaultBackend> verify::AnchorSource for OwnAnchors<'_, S, V> {
    fn anchor(
        &self,
        venue: &verify::Venue,
        number: u64,
    ) -> Result<Option<verify::PublicAnchor>, alloc::string::String> {
        let l = self.0;
        let Ok(c) = l.final_checkpoint(Some(number)) else {
            return Ok(None);
        };
        // Only in the venue the checkpoint was anchored in (`F-027` T-027-04).
        if c.anchored_in() != *venue {
            return Ok(None);
        }
        let stored_at = c
            .coordinate
            .and_then(|k| l.state.settlement().ingested(&k))
            .map_or(0, |i| i.observation.at);
        let read_at = l.state.settlement().head().map_or(0, |h| h.height);
        Ok(Some(verify::PublicAnchor {
            commitment: c.commitment,
            stored_at,
            read_at,
        }))
    }

    fn attestation_key(
        &self,
        _venue: &verify::Venue,
    ) -> Result<Option<[u8; 32]>, alloc::string::String> {
        Ok(None)
    }
}

impl<S: LeafStore, V: VaultBackend> Ledger<S, V> {
    /// Step 1: the authority that issues it, and the justification on the legal path.
    fn disclosure_authority(
        &self,
        p: &Principal,
        r: &DisclosureRequest,
        now: Time,
    ) -> Result<(AuthorityUsed, Option<Justification>), Refusal> {
        if let Some(text) = &r.legal_obligation {
            let (used, j) = self
                .frame(p, OperatorOpKind::LegalDisclosure, text, now)
                .map_err(|e| match e {
                    Refusal::JustificationRequired => e,
                    _ => Refusal::NotDisclosable,
                })?;
            let acct = self
                .state
                .account(&r.subject)
                .ok_or(Refusal::NotDisclosable)?;
            // Operator authority over an account exists only while its custody is delegated
            // (`REQ-AU-6`); a legal obligation does not widen it.
            if acct.custody() != Custody::Delegated || acct.kind() == AccountKind::Issuance {
                return Err(Refusal::NotDisclosable);
            }
            return Ok((used, Some(j)));
        }
        let asset = match r.assertion {
            Assertion::HoldsAtLeast { asset, .. }
            | Assertion::OperationMoved { asset, .. }
            | Assertion::FlowWithin { asset, .. } => Some(asset),
            Assertion::OperationOccurred { .. } => None,
        };
        let d = eval::authorise_principal(
            &self.state,
            p,
            &Request {
                kind: OperationKind::Disclose,
                account: r.subject,
                asset,
                amount: None,
                counterparty: None,
            },
            now,
        )
        .map_err(|e| match e {
            Refusal::NotAuthorised
            | Refusal::UnknownAccount
            | Refusal::AuthorityRevoked
            | Refusal::BoundExceeded => Refusal::NotDisclosable,
            other => other,
        })?;
        // The subject's own authority, or one it granted: never an operator grant (`REQ-DS-5`).
        if !matches!(d.used.kind, AuthorityKind::Owner | AuthorityKind::Delegated) {
            return Err(Refusal::NotDisclosable);
        }
        Ok((d.used, None))
    }

    /// The postings of `op` at or below `s`, in sequence order.
    fn postings_of(&self, op: &OperationId, s: u64) -> Vec<&PostingEntry> {
        let journal = self.state.journal();
        self.posting_seqs_of(op)
            .into_iter()
            .filter(|q| *q <= s)
            .filter_map(|q| journal.get(q))
            .filter(|p| p.operation == *op)
            .collect()
    }

    /// The completing postings of `op` at or below `s`, which must touch `subject`; a reversal
    /// at or below `s` means it no longer stands. `ERR-DisclosureFalse` if there is none that
    /// touches the subject, `ERR-DisclosureUnsupported` for an opening load.
    fn completing(
        &self,
        op: &OperationId,
        subject: &AccountId,
        s: u64,
    ) -> Result<Vec<&PostingEntry>, Refusal> {
        let all = self.postings_of(op, s);
        if all.iter().any(|p| p.kind == PostingKind::Opening) {
            return Err(Refusal::DisclosureUnsupported);
        }
        if all.iter().any(|p| p.kind == PostingKind::Reversal) {
            return Err(Refusal::DisclosureFalse);
        }
        let done: Vec<&PostingEntry> = all.into_iter().filter(|p| completes(p.kind)).collect();
        if !done.iter().any(|p| p.touches(subject)) {
            return Err(Refusal::DisclosureFalse);
        }
        Ok(done)
    }

    /// Steps 3 and 4: the claims the assertion maps to, after its truth is checked at the
    /// checkpoint's entry sequence.
    fn claims_for(
        &self,
        subject: &AccountId,
        assertion: &Assertion,
        c: &Checkpoint,
    ) -> Result<Vec<Claim>, Refusal> {
        let s = c.entry_sequence;
        let root = c.kernel_root;
        let account = self.state.account(subject).ok_or(Refusal::NotDisclosable)?;
        let journal = self.state.journal();
        match *assertion {
            Assertion::HoldsAtLeast {
                asset,
                amount,
                kind,
            } => {
                if kind != BalanceKind::Available {
                    return Err(Refusal::DisclosureUnsupported);
                }
                let a = self.state.registry().get(&asset)?;
                let held = journal
                    .balance_before(subject, &asset, s.saturating_add(1))
                    .available;
                if held < i128::from(amount.get()) {
                    return Err(Refusal::DisclosureFalse);
                }
                Ok(vec![Claim::BalanceAtLeast {
                    account: account.sub_account(a).id().0,
                    layer: 0,
                    threshold: amount.get(),
                    state_root: root,
                }])
            }
            Assertion::OperationOccurred { operation } => {
                let done = self.completing(&operation, subject, s)?;
                Ok(done
                    .iter()
                    .map(|p| Claim::TransferExisted {
                        transfer_id: PostingMeta::new(p.kind, operation.to_ref(), p.ordinal)
                            .transfer_id_in(posting_home(&p.lines))
                            .0,
                        state_root: root,
                    })
                    .collect())
            }
            Assertion::OperationMoved {
                operation,
                asset,
                amount,
            } => {
                self.state.registry().get(&asset)?;
                let done = self.completing(&operation, subject, s)?;
                let mut moved: u64 = 0;
                let mut claims = Vec::new();
                for p in done {
                    let debited: BTreeSet<AccountId> = p
                        .lines
                        .iter()
                        .filter(|l| l.side == Direction::Debit)
                        .map(|l| l.account)
                        .collect();
                    let delivered: Vec<usize> = p
                        .lines
                        .iter()
                        .enumerate()
                        .filter(|(_, l)| {
                            l.asset == asset
                                && l.side == Direction::Credit
                                && !debited.contains(&l.account)
                        })
                        .map(|(i, _)| i)
                        .collect();
                    if delivered.is_empty() {
                        continue;
                    }
                    let id = PostingMeta::new(p.kind, operation.to_ref(), p.ordinal)
                        .transfer_id_in(posting_home(&p.lines));
                    let mut keys = KeySet::default();
                    keys.transfers.insert(id);
                    let stored = self
                        .book
                        .store()
                        .load(&keys)
                        .ok()
                        .and_then(|l| l.transfers.get(&id).cloned().flatten())
                        .ok_or(Refusal::DisclosureUnsupported)?;
                    if stored.entries.len() != p.lines.len() {
                        return Err(Refusal::DisclosureUnsupported);
                    }
                    for i in delivered {
                        let line = p.lines[i];
                        let entry = &stored.entries[i];
                        if entry.direction != cipher_clerk::Direction::Credit {
                            return Err(Refusal::DisclosureUnsupported);
                        }
                        let value = line.amount.minor().get();
                        moved = moved.checked_add(value).ok_or(Refusal::DisclosureFalse)?;
                        claims.push(Claim::TransferAmountEquals {
                            transfer_id: id.0,
                            value,
                            amount_commit: entry.amount.0,
                            state_root: root,
                        });
                    }
                }
                if moved != amount.get() || claims.is_empty() {
                    return Err(Refusal::DisclosureFalse);
                }
                Ok(claims)
            }
            Assertion::FlowWithin {
                asset,
                direction,
                from_sequence,
                to_sequence,
                bound,
            } => {
                let a = self.state.registry().get(&asset)?;
                if from_sequence > to_sequence || to_sequence > s {
                    return Err(Refusal::DisclosureUnsupported);
                }
                let mut flow: u128 = 0;
                let below = to_sequence.saturating_add(1);
                let mut from = from_sequence;
                loop {
                    let page = journal.position_postings(subject, &asset, from, below, 4_096);
                    let Some(last) = page.last().map(|p| p.seq) else {
                        break;
                    };
                    for p in &page {
                        let net: i128 = p
                            .lines
                            .iter()
                            .filter(|l| l.account == *subject && l.asset == asset)
                            .map(|l| l.delta())
                            .sum();
                        let part = match direction {
                            FlowDirection::In => net.max(0),
                            FlowDirection::Out => (-net).max(0),
                        };
                        flow += part.unsigned_abs();
                    }
                    from = last + 1;
                }
                let holds = match bound {
                    Bound::AtLeast(x) => flow >= u128::from(x),
                    Bound::AtMost(x) => flow <= u128::from(x),
                };
                if !holds {
                    return Err(Refusal::DisclosureFalse);
                }
                let relation = match bound {
                    Bound::AtLeast(x) => Relation::AtLeast(public_commitment(x)),
                    Bound::AtMost(x) => Relation::AtMost(public_commitment(x)),
                };
                Ok(vec![Claim::AggregateInWindow {
                    filter: Filter {
                        journal: None,
                        account: Some(account.sub_account(a).id().0),
                        direction: Some(match direction {
                            FlowDirection::In => 2,
                            FlowDirection::Out => 1,
                        }),
                    },
                    t_lo: from_sequence,
                    t_hi: to_sequence,
                    relation,
                    state_root: root,
                }])
            }
        }
    }

    /// `issue_disclosure` (SPEC §7.E, `CTR-DSC`). See the module documentation. `clerk` is the
    /// deployment's clerk key, reaching the core as a private input (`REQ-RT-6`); the disclosure's
    /// bytes are the reply.
    pub fn issue_disclosure(
        &mut self,
        ctx: &Ctx,
        request: DisclosureRequest,
        clerk: &dyn ClerkSigner,
    ) -> Result<Accepted<Vec<u8>>, Refusal> {
        let top = self.enter(|| crate::call::Call::IssueDisclosure {
            ctx: *ctx,
            request: request.clone(),
        });
        #[allow(clippy::redundant_closure_call)]
        let r: Result<Accepted<Vec<u8>>, Refusal> = (|| {
            let p = resolve(&self.state, &ctx.caller)?;
            let d = request_digest(&p, &request);
            let store = self.state.anchoring();
            if let Some(a) =
                self.replay(ctx, &p, &d, |r| store.issued(&r.op).map(<[u8]>::to_vec))?
            {
                return match a.value {
                    Some(value) => Ok(Accepted {
                        value,
                        record: a.record,
                        status: a.status,
                    }),
                    None => Err(Refusal::NotAuthorised),
                };
            }
            let command = Command::IssueDisclosure;
            let legal = request.legal_obligation.is_some();
            let (used, justification) = match self.disclosure_authority(&p, &request, ctx.now) {
                Ok(x) => x,
                Err(r) if legal => {
                    return Err(self.refused(
                        ctx,
                        &p,
                        command,
                        vec![AuditSubject::Account(request.subject)],
                        r,
                    ));
                }
                Err(r) => return Err(r),
            };
            let run = |l: &Self| -> Result<Vec<u8>, Refusal> {
                let c = l.final_checkpoint(request.checkpoint)?;
                let assurance = l
                    .config
                    .anchoring
                    .assurance
                    .clone()
                    .ok_or(Refusal::DisclosureUnsupported)?;
                let claims = l.claims_for(&request.subject, &request.assertion, &c)?;
                let mut envelope = verify::Disclosure {
                    version: verify::envelope::VERSION,
                    instance: c.instance,
                    // Where the anchor lives, so the verifier reads it there (`F-027` T-027-04).
                    venue: c.anchored_in(),
                    subject: request.subject.to_did(),
                    recipient: match request.recipient {
                        Recipient::Identity(i) => verify::Recipient::Identity(i.to_did()),
                        Recipient::Reference(b) => verify::Recipient::Reference(b),
                    },
                    assertion: envelope_assertion(&request.assertion),
                    checkpoint: verify::CheckpointRef {
                        number: c.number,
                        entry_sequence: c.entry_sequence,
                        kernel_root: c.kernel_root,
                        commitment: c.commitment,
                        construction: crate::anchor::construction(),
                    },
                    assurance: verify::Assurance {
                        declaration_digest: assurance.digest,
                        summary: format!("{SIGNED_ASSERTION_LABEL} {}", assurance.summary),
                        kind: verify::AssuranceKind::SignedAssertion,
                    },
                    issued_at: ctx.now,
                    claims: claims
                        .into_iter()
                        .map(|claim| SignedClaim {
                            claim,
                            signature: clerk.sign(&claim.public_bytes()),
                        })
                        .collect(),
                    signature: [0; 64],
                };
                envelope.signature = clerk.sign(&envelope.signed_bytes());
                Ok(envelope.to_bytes())
            };
            let bytes = match run(self) {
                Ok(b) => b,
                Err(r) if legal => {
                    return Err(self.refused(
                        ctx,
                        &p,
                        command,
                        vec![AuditSubject::Account(request.subject)],
                        r,
                    ));
                }
                Err(r) => return Err(r),
            };
            let record = OperationRecord {
                op: ctx.op,
                command,
                authorities: vec![used],
                accounts: vec![request.subject],
                postings: Vec::new(),
                at: ctx.now,
            };
            let mut subjects = Vec::new();
            if let Recipient::Identity(i) = request.recipient {
                subjects.push(AuditSubject::Identity(i));
            }
            let entry = if legal {
                Self::operator_entry(ctx, &p, d)
            } else {
                Self::request_entry(ctx, &p, d, None)
            };
            let writes = vec![
                Write::Anchor(AnchorWrite::Issued {
                    op: ctx.op,
                    bytes: bytes.clone(),
                }),
                Write::PutRecord(record.clone()),
                entry,
                Write::Audit(AuditNote {
                    op: ctx.op,
                    class: None,
                    action: None,
                    authorities: Vec::new(),
                    subjects,
                    justification,
                    outcome: AuditOutcome::Accepted,
                    at: ctx.now,
                }),
            ];
            self.check(&writes)?;
            self.commit(&writes)?;
            Ok(Accepted {
                value: bytes,
                record,
                status: None,
            })
        })();
        self.leave(top, || crate::call::Summary::of(&r));
        r
    }

    /// `verify(disclosure)` (SPEC §7.E): a query with no side effect. The same checks the offline
    /// verifier runs ([`verify::verify`]), against the checkpoints this deployment holds as final
    /// and the clerk key `clerk_key`. Any authenticated principal may ask (`REQ-PV-5`): the
    /// answer reveals nothing the bytes do not hold. Bytes that are not a disclosure are
    /// `ERR-DisclosureUnsupported`.
    pub fn verify_disclosure(
        &self,
        caller: &Caller,
        bytes: &[u8],
        clerk_key: [u8; 32],
    ) -> Result<verify::Report, Refusal> {
        resolve(&self.state, caller)?;
        verify::verify(bytes, Some(clerk_key), &OwnAnchors(self))
            .map_err(|_| Refusal::DisclosureUnsupported)
    }
}

/// The partition a recorded posting lives in: its first debit's ledger account's, which is where
/// the kernel placed its transfer (`REQ-RT-9`; every kernel account of a ledger account, its
/// holdings included, is in the ledger account's partition).
fn posting_home(lines: &[crate::query::journal::Line]) -> Partition {
    lines
        .iter()
        .find(|l| l.side == Direction::Debit)
        .map(|l| Partition::of(l.account.as_bytes()))
        .unwrap_or_default()
}
