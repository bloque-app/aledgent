// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The core's backup (`T-018-01`, accepted proposal: export-first).
//!
//! One backup is four artifacts, the first three taken under the core's lock at one point of the
//! input sequence ([`InProcessCore::checkpoint`]), the fourth from ingress's received-log after it:
//!
//! - `export.bin` — the `F-014` export of the whole book at posting sequence `S`. Taking it is a
//!   privileged read (`REQ-PV-4`): the backup operator needs the `PrivilegedRead` operator kind
//!   and a justification, and the read's record reaches the audit store before anything is
//!   returned. The job ships the core's audit outbox first, since an export refuses while the
//!   audit store is behind (`F-014` decision 8).
//! - `sidecar.json` — what the in-process core holds beside the book: the input number `s_b` the
//!   book stands at, the delivery records and the replies of the last inputs
//!   (`ingress::inprocess::Sidecar`). Public material.
//! - `openings.sealed` — the openings, which the export never holds (`REQ-RT-6`), sealed under the
//!   deployment's sealing key ([`crate::seal`]). The core's key root is not in any backup.
//! - `pass-through.jsonl` — the pass-through data of the operations the export holds (`REQ-PO-5`:
//!   "an export carries it, and an import restores it"), read from ingress's received-log, where it
//!   lives (owner ruling `pq8-export` (a), `F-009` `NOTES.md` decision 31); the core never holds it.
//!   One line per command row that carried data, in operation order, format
//!   [`PASS_THROUGH_FORMAT`]: `{"operation", "principal", "reference"}`, the data exactly as sent.
//!   The log is read after the checkpoint, so every operation the export holds is in it; a row the
//!   core had not decided by then is carried too, harmlessly (its own replay brings it back).
//!   An import restores it onto the restored log ([`crate::restore::restore_pass_through`]).
//!
//! **The anchor.** An export requires its `S` to be anchored with its book root (`REQ-PR-7`).
//! `F-017`'s anchors commit to the kernel root at an entry sequence, not to the book root at a
//! posting sequence that `F-014`'s check wants (`F-017`'s open point, the owner's), so the job
//! records a checkpoint in `F-014`'s in-process stand-in (`Checkpoints`) and keeps the root in the
//! manifest. A restore checks the import against that root, which proves nothing to a third party;
//! the drill's `INV-19` check is the one against the settlement layer's anchors.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use curve25519_dalek::scalar::Scalar;
use ingress::boundary::InputKey;
use ingress::inprocess::{InProcessCore, Sidecar};
use ingress::store::{Carried, Row, Status};
use ingress::wire::PassThrough;
use ledger_core::audit::AuditSink;
use ledger_core::export::{Checkpoints, Export, Openings};
use ledger_core::id::OperationId;
use ledger_core::ledger::Ctx;
use ledger_core::ledger::operator::openings_of;
use ledger_kernel::posting::OperationRef;
use ledger_kernel::store::LeafStore;
use ledger_kernel::vault::{LayerSums, RowKey, SideSum, VaultBackend};
use zeroize::Zeroize;

use super::{CorePoint, Manifest, StoreKind};
use crate::Error;
use crate::seal::SealingKey;

/// The export's artifact.
pub const EXPORT: &str = "export.bin";
/// The sidecar's artifact.
pub const SIDECAR: &str = "sidecar.json";
/// The sealed openings' artifact.
pub const OPENINGS: &str = "openings.sealed";
/// The pass-through data's artifact (`REQ-PO-5`, ruling `pq8-export` (a)).
pub const PASS_THROUGH: &str = "pass-through.jsonl";
/// Its line format.
pub const PASS_THROUGH_FORMAT: &str = "aledgent/pass-through/v1";

/// The pass-through data of the operations a core backup at input `s_b` holds, from the
/// received-log `rows` read after its checkpoint: every command row that carried data, except
/// those the log shows decided after `s_b`. In operation order.
pub fn pass_through_of(rows: &[Row], s_b: u64) -> Vec<Carried> {
    let mut out: Vec<Carried> = rows
        .iter()
        .filter(|r| !(r.status == Status::Decided && r.core_input_seq.is_some_and(|n| n > s_b)))
        .filter_map(Carried::of)
        .collect();
    out.sort_by_key(|c| c.op_ref.0);
    out
}

/// One line of the artifact.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct PassThroughLine {
    operation: String,
    principal: String,
    reference: PassThrough,
}

/// The artifact's bytes: one canonical JSON line per entry, each ending in `\n`.
pub fn encode_pass_through(entries: &[Carried]) -> Vec<u8> {
    let mut out = Vec::new();
    for c in entries {
        let line = PassThroughLine {
            operation: OperationId::from_ref(OperationRef(c.op_ref.0)).to_did(),
            principal: c.principal.clone(),
            reference: c.pass_through.clone(),
        };
        if let Ok(b) = serde_json::to_vec(&line) {
            out.extend_from_slice(&b);
            out.push(b'\n');
        }
    }
    out
}

/// Read the artifact back. Any other form is refused.
pub fn decode_pass_through(bytes: &[u8]) -> Result<Vec<Carried>, Error> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| Error::Config(format!("{PASS_THROUGH} is not UTF-8")))?;
    text.lines()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let line: PassThroughLine = serde_json::from_str(l).map_err(|e| {
                Error::Config(format!("{PASS_THROUGH}: not {PASS_THROUGH_FORMAT}: {e}"))
            })?;
            let op = OperationId::parse(&line.operation).map_err(|_| {
                Error::Config(format!("{PASS_THROUGH}: an operation that does not parse"))
            })?;
            Ok(Carried {
                op_ref: InputKey(*op.as_bytes()),
                principal: line.principal,
                pass_through: line.reference,
            })
        })
        .collect()
}

/// The context the openings are sealed under: the artifact and its backup's identity, so a sealed
/// file cannot be passed off as another backup's.
pub fn openings_context(id: &str) -> Vec<u8> {
    [b"aledgent/ops/openings/v1/".as_slice(), id.as_bytes()].concat()
}

/// The openings' plaintext: a count, then per row the 17-byte row key and each side's value
/// (16 bytes, big-endian) and blinding (32 bytes). Wiped by the caller once sealed.
pub fn encode_openings(o: &Openings) -> Vec<u8> {
    let rows = o.rows();
    let mut out = Vec::with_capacity(8 + rows.len() * 113);
    out.extend_from_slice(&(rows.len() as u64).to_be_bytes());
    for (k, s) in rows {
        out.extend_from_slice(&k.to_bytes());
        for side in [&s.dr, &s.cr] {
            out.extend_from_slice(&side.value.to_be_bytes());
            out.extend_from_slice(side.blinding.as_bytes());
        }
    }
    out
}

/// Decode [`encode_openings`]. Any other form is refused.
pub fn decode_openings(bytes: &[u8]) -> Result<Openings, Error> {
    let bad = || Error::Restore("the openings do not decode".into());
    let (n, mut rest) = bytes.split_at_checked(8).ok_or_else(bad)?;
    let n = u64::from_be_bytes(n.try_into().map_err(|_| bad())?);
    let mut rows = BTreeMap::new();
    for _ in 0..n {
        let (key, r) = rest.split_at_checked(17).ok_or_else(bad)?;
        let key = RowKey::from_bytes(key.try_into().map_err(|_| bad())?);
        rest = r;
        let mut sides = [SideSum::ZERO; 2];
        for side in &mut sides {
            let (v, r) = rest.split_at_checked(16).ok_or_else(bad)?;
            let (b, r) = r.split_at_checked(32).ok_or_else(bad)?;
            rest = r;
            let blinding: [u8; 32] = b.try_into().map_err(|_| bad())?;
            side.value = u128::from_be_bytes(v.try_into().map_err(|_| bad())?);
            side.blinding = Option::from(Scalar::from_canonical_bytes(blinding)).ok_or_else(bad)?;
        }
        if rows
            .insert(
                key,
                LayerSums {
                    dr: sides[0],
                    cr: sides[1],
                },
            )
            .is_some()
        {
            return Err(bad());
        }
    }
    if !rest.is_empty() {
        return Err(bad());
    }
    Ok(Openings::from_rows(rows))
}

/// Who takes the backup, and why (`REQ-PV-4`: the privileged read records both).
pub struct CoreBackupJob<'a> {
    /// The backup operator, with the `PrivilegedRead` operator kind; the operation reference of
    /// its read; the time.
    pub ctx: Ctx,
    /// The justification recorded with the read.
    pub justification: &'a [u8],
    /// The key the openings are sealed under.
    pub sealing: &'a SealingKey,
    /// The backup's identity.
    pub id: &'a str,
}

/// A core backup taken at its checkpoint, not yet written: [`Taken::write`] adds the received-log's
/// pass-through data, read after the checkpoint (module documentation).
pub struct Taken {
    export: Export,
    sidecar: Sidecar,
    sealed: Vec<u8>,
    id: String,
    taken_at: u64,
}

impl Taken {
    /// The input number `s_b` the backup stands at.
    pub fn inputs(&self) -> u64 {
        self.sidecar.inputs
    }

    /// Write the backup into `root/core/<id>/`, with the pass-through data of the received-log
    /// `rows`, read after [`take`] returned. Returns the backup's directory and manifest.
    pub fn write(&self, root: &Path, rows: &[Row]) -> Result<(PathBuf, Manifest), Error> {
        write_bundle(
            root,
            &self.id,
            self.taken_at,
            &self.export,
            &self.sidecar,
            &self.sealed,
            &pass_through_of(rows, self.sidecar.inputs),
        )
    }
}

/// Take a backup of the running core at one checkpoint (module documentation). `sink` is the
/// deployment's audit store. Write it with [`Taken::write`] once the received-log is read.
pub fn take<S, V>(
    core: &InProcessCore<S, V>,
    job: &CoreBackupJob<'_>,
    sink: &mut dyn AuditSink,
) -> Result<Taken, Error>
where
    S: LeafStore + Send + Sync + 'static,
    V: VaultBackend + Send + Sync + 'static,
{
    let taken = core
        .checkpoint(|ledger, sidecar| -> Result<_, Error> {
            ledger
                .ship_audit(sink)
                .map_err(|e| Error::Audit(format!("the outbox did not ship: {e:?}")))?;
            let mut anchors = Checkpoints::new();
            anchors
                .anchor(ledger)
                .ok_or_else(|| Error::Core("the book cannot be read".into()))?;
            let export = ledger
                .export_book(&job.ctx, &anchors, job.justification, sink)
                .map_err(|e| Error::Export(format!("{e:?}")))?;
            let mut plain = encode_openings(&openings_of(ledger));
            let sealed = job.sealing.seal(&plain, &openings_context(job.id));
            plain.zeroize();
            Ok((export, sidecar.clone(), sealed?))
        })
        .ok_or_else(|| Error::Core("the core is not running".into()))?;
    let (export, sidecar, sealed) = taken?;
    Ok(Taken {
        export,
        sidecar,
        sealed,
        id: job.id.to_owned(),
        taken_at: job.ctx.now,
    })
}

/// Write a core backup's four artifacts and its manifest.
pub fn write_bundle(
    root: &Path,
    id: &str,
    taken_at: u64,
    export: &Export,
    sidecar: &Sidecar,
    sealed_openings: &[u8],
    pass_through: &[Carried],
) -> Result<(PathBuf, Manifest), Error> {
    let m = &export.manifest;
    let point = CorePoint {
        inputs: sidecar.inputs,
        sequence: m.sequence,
        kernel_root: hex::encode(m.kernel_root),
        service_root: hex::encode(m.service_root),
        book_root: hex::encode(m.book_root()),
    };
    let side = serde_json::to_vec_pretty(sidecar).map_err(|e| Error::Config(e.to_string()))?;
    super::write(
        root,
        StoreKind::Core,
        id,
        taken_at,
        &[
            (EXPORT, &export.to_bytes()),
            (SIDECAR, &side),
            (OPENINGS, sealed_openings),
            (PASS_THROUGH, &encode_pass_through(pass_through)),
        ],
        Some(point),
    )
}

/// A core backup, read back and verified: the export, the sidecar and the unsealed openings.
pub struct CoreBundle {
    /// The manifest.
    pub manifest: Manifest,
    /// The export.
    pub export: Export,
    /// The sidecar.
    pub sidecar: Sidecar,
    /// The openings (secret: never logged, `Debug` shows only a count).
    pub openings: Openings,
    /// The pass-through data of its operations (`REQ-PO-5`): empty for a backup taken before the
    /// artifact existed.
    pub pass_through: Vec<Carried>,
}

/// Read and verify the core backup in `dir`: every checksum, the export's layout, the sidecar's
/// point against the manifest's, the openings under `sealing`.
pub fn read(dir: &Path, sealing: &SealingKey) -> Result<CoreBundle, Error> {
    let manifest = super::verify(dir)?;
    if manifest.store != StoreKind::Core {
        return Err(Error::Config("not a core backup".into()));
    }
    let point = manifest
        .point
        .clone()
        .ok_or_else(|| Error::Config("a core backup names its point".into()))?;
    let export = Export::from_bytes(&super::read_artifact(dir, &manifest, EXPORT)?)
        .map_err(|e| Error::Export(format!("{e:?}")))?;
    let sidecar: Sidecar = serde_json::from_slice(&super::read_artifact(dir, &manifest, SIDECAR)?)
        .map_err(|e| Error::Config(e.to_string()))?;
    let m = &export.manifest;
    if sidecar.inputs != point.inputs
        || m.sequence != point.sequence
        || hex::encode(m.book_root()) != point.book_root
    {
        return Err(Error::Config(
            "the backup's artifacts are not of one point".into(),
        ));
    }
    let sealed = super::read_artifact(dir, &manifest, OPENINGS)?;
    let mut plain = sealing.open(&sealed, &openings_context(&manifest.id))?;
    let openings = decode_openings(&plain);
    plain.zeroize();
    let pass_through = match manifest.artifact(PASS_THROUGH) {
        Some(_) => decode_pass_through(&super::read_artifact(dir, &manifest, PASS_THROUGH)?)?,
        None => Vec::new(),
    };
    Ok(CoreBundle {
        manifest,
        export,
        sidecar,
        openings: openings?,
        pass_through,
    })
}
