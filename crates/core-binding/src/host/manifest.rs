// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The replica's manifest (`F-031` T-031-26, T-031-25's v2; `REQ-RT-3`, `INV-6`, `NFR-4`): a
//! replica that comes back after an unclean stop without a rebuild from genesis.
//!
//! T-031-25 keeps the replica across a clean stop. Any other stop (an OOM kill, `kill -9`, the host
//! lost with its volume) left stores that stand nowhere in particular: three files, each at its own
//! last synced commit, and the host rebuilt the replica from the archive, 39 to 50 minutes on the
//! production book. This module gives a running host a point to come back to.
//!
//! **Taken.** Every few inputs or seconds, between inputs, with nothing staged
//! ([`Driver::checkpoint`]): the replica's own archive synced, a savepoint taken in each of its
//! three files at that input ([`leaf_store::savepoint`]), then a record of where the replica
//! stands written beside them (`kept.json`, layout [`Kept::MANIFEST`]): what a clean stop's record
//! holds, the writer's code and mode as the Agent's hot row held them there, the book store's own
//! head, the three savepoint ids, and a digest over the rest. The record is written whole or not at
//! all (a temporary file, synced, renamed, the directory synced), so a crash at any point leaves
//! the previous manifest, whose savepoints each file still holds, or the new one. Each file keeps
//! the savepoint the current manifest names and the one being taken, never more.
//!
//! **Reopened** (`Driver::reopen_manifest`). The record is read and its digest checked. The
//! input it names must be one the host's log holds (extended from the Agent's ring, as a rebuild
//! extends it), with the same log hash, roots and next posting, and the Agent must stand at or
//! after it. Each file is put back to its savepoint and the replica's own archive cut where the
//! book store then stands, so the three files agree on that input again; the replica is rebuilt
//! over them as a clean stop's is (the service store from its leaves, checked leaf for leaf; the
//! audit records restored) and must reach the record's roots. Then every input after it is
//! replayed over it by the same step a rebuild from genesis takes (`replay_entry`), each
//! reaching its archived roots, and the driver is built only where the Agent's hot row stands.
//!
//! **What makes it equal to a rebuild, stated plainly.** A replay is deterministic in the state it
//! starts from and the inputs it is given. A reopened replica is checked to be the state a rebuild
//! has at the manifest's input: the kernel root, the service root and the audit chain's head equal
//! the archived entry's, the service store is rebuilt from the tree and checked leaf for leaf, and
//! the store's head is the record's. The writer's code and mode it continues under are the Agent's
//! own at that input. The inputs after it are then the same inputs through the same step, each
//! checked against its archived roots, and the result against the Agent's hot row. What this does
//! not re-check is what T-031-25 does not: the openings and the delivery records are taken as the
//! files hold them (both reach the roots through the next input's checks), and the archive's
//! history is not decided again by the running build (a roll's archive replay does that).
//!
//! **Any check that fails** gives the port back with why, and the host rebuilds from genesis as
//! before: no record, a torn or altered one, one from another history, one ahead of the Agent, a
//! savepoint missing, a store that does not reach the roots, an input that does not replay to its
//! roots. The replica is never served from a reopen that did not pass every check.

use std::path::Path;

use super::*;

/// What a replica manifest adds to a clean stop's record (`T-031-26`).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Manifest {
    /// The code version in force after the input, as the Agent's hot row held it.
    pub code: [u8; 32],
    /// The rules version in force after it.
    pub generation: u64,
    /// The scheduled upgrade after it.
    pub upgrade: Option<([u8; 32], u64)>,
    /// The digest of the upgrade-authority mode in force after it.
    pub authority: [u8; 32],
    /// The scheduled change of mode after it.
    pub authority_next: Option<([u8; 32], u64)>,
    /// The book store's last kernel commit, if it has one.
    pub book_applied: Option<u64>,
    /// The book store's log head.
    pub book_head: [u8; 32],
    /// The savepoint of the book's leaves at the input.
    pub book_savepoint: u64,
    /// The savepoint of the openings at the input.
    pub openings_savepoint: u64,
    /// The savepoint of the service tree at the input.
    pub service_savepoint: u64,
}

/// How a replica was reopened ([`Driver::reopen`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reopened {
    /// The input the record names.
    pub from: u64,
    /// How many inputs were replayed over it to reach the Agent.
    pub rolled: u64,
    /// Whether the record was a manifest (`T-031-26`) rather than a clean stop's (`T-031-25`).
    pub manifest: bool,
    /// How long the files took to open (and, for a manifest, to be put back to its savepoints),
    /// ms: after an unclean stop this is where the storage engine repairs them.
    pub stores_ms: u64,
    /// How long the replica took to be rebuilt over them, the service store from its leaves, ms.
    pub replica_ms: u64,
    /// How long the inputs after the manifest took to replay, ms.
    pub replay_ms: u64,
}

fn ms_since(t: std::time::Instant) -> u64 {
    u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// The points at which a manifest or a clean stop's record is written, in order: a test stops the
/// process's view of the files at each, as a kill there would (`T-031-26`).
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WritePoint {
    /// A manifest is due: nothing written yet.
    Begin,
    /// The replica's own archive synced.
    ArchiveSynced,
    /// The book's leaves saved.
    BookSaved,
    /// The openings saved.
    OpeningsSaved,
    /// The service tree saved.
    ServiceSaved,
    /// The record's temporary file written, not synced.
    RecordWritten,
    /// The temporary file synced, not renamed.
    RecordSynced,
    /// Renamed over the record, the directory not synced.
    RecordRenamed,
    /// The directory synced: the record is the new one.
    RecordDone,
    /// A clean stop's record written, and the savepoints deleted.
    SavepointsCleared,
}

type Hook = Box<dyn Fn(WritePoint, &Path) + Send + Sync>;

static HOOK: Mutex<Option<Hook>> = Mutex::new(None);

/// Call `hook` at every [`WritePoint`] of every replica, with the replica's directory, until it is
/// cleared (tests only: a kill at each point, simulated by copying the files there).
#[doc(hidden)]
pub fn set_write_hook(hook: Option<Hook>) {
    *HOOK.lock().unwrap_or_else(PoisonError::into_inner) = hook;
}

pub(super) fn at(point: WritePoint, dir: &Path) {
    if let Some(h) = HOOK.lock().unwrap_or_else(PoisonError::into_inner).as_ref() {
        h(point, dir);
    }
}

const DIGEST_DOMAIN: &[u8] = b"aledgent/replica-manifest/v2";

impl Kept {
    fn digest_of(&self) -> Result<[u8; 32], String> {
        let mut bare = self.clone();
        bare.digest = None;
        let bytes = serde_json::to_vec(&bare).map_err(|e| e.to_string())?;
        Ok(blake2b_256(DIGEST_DOMAIN, &[&bytes]))
    }

    /// The manifest with its digest set.
    ///
    /// # Errors
    ///
    /// The record does not encode.
    pub fn sealed(mut self) -> Result<Kept, String> {
        self.digest = Some(hex::encode(self.digest_of()?));
        Ok(self)
    }

    /// Whether a manifest is whole: its own part present and its digest over the rest.
    ///
    /// # Errors
    ///
    /// Why it is not.
    pub fn check_digest(&self) -> Result<(), String> {
        if self.manifest.is_none() {
            return Err("a manifest without its savepoints".into());
        }
        let want = hex::encode(self.digest_of()?);
        match &self.digest {
            Some(d) if *d == want => Ok(()),
            Some(_) => Err("the manifest does not match its digest".into()),
            None => Err("the manifest has no digest".into()),
        }
    }
}

/// Whether the audit store is behind what a kept replica had shipped: it then needs records the
/// kept outbox no longer holds, which a replay re-emits and a reopen cannot.
pub(super) fn audit_behind(audit_floor: Option<(u64, [u8; 32])>, kept: &Kept) -> bool {
    match (audit_floor, kept.audit_shipped) {
        (_, None) => false,
        (None, Some(_)) => true,
        (Some((f, _)), Some((k, _))) => f < k,
    }
}

impl ReplicaStores {
    /// The replica's directory, for stores on disk.
    pub fn dir(&self) -> Option<&Path> {
        match self {
            ReplicaStores::Memory => None,
            ReplicaStores::Disk { dir, .. } => Some(dir),
        }
    }

    /// Make the replica's own archive durable past record `since` (every record, for `None`):
    /// each segment that may hold a later record, and the directory.
    fn sync_book_archive_since(&self, since: Option<u64>) -> Result<(), DriveError> {
        let Some(dir) = self.dir() else {
            return Ok(());
        };
        let archive = dir.join("book").join("archive");
        let io = |e: std::io::Error| DriveError::Replica(format!("{}: {e}", archive.display()));
        let mut firsts: Vec<(u64, std::path::PathBuf)> = Vec::new();
        let entries = match std::fs::read_dir(&archive) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(io(e)),
        };
        for entry in entries {
            let path = entry.map_err(io)?.path();
            let first = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".seg"))
                .and_then(|n| n.parse::<u64>().ok());
            if let Some(first) = first {
                firsts.push((first, path));
            }
        }
        firsts.sort();
        let after = since.map_or(0, |s| s + 1);
        for (i, (_, path)) in firsts.iter().enumerate() {
            // A segment holds the records from its first up to the next segment's first.
            let ends_before = firsts.get(i + 1).is_some_and(|(next, _)| *next <= after);
            if !ends_before {
                std::fs::File::open(path)
                    .and_then(|f| f.sync_all())
                    .map_err(io)?;
            }
        }
        std::fs::File::open(&archive)
            .and_then(|d| d.sync_all())
            .map_err(io)
    }

    /// The stores of a kept replica, opened where they are, without wiping them (`T-031-25`); for
    /// a manifest, each put back to its savepoint first and the book's archive cut where its store
    /// then stands (`T-031-26`). The book store's archive must end exactly where the store stands,
    /// and for a manifest the store where the manifest says.
    fn reopen_stores(
        &self,
        manifest: Option<&Manifest>,
    ) -> Result<(BookStore, OpeningsStore, BookStore), DriveError> {
        let ReplicaStores::Disk { dir, cache_bytes } = self else {
            return Err(DriveError::Replica(
                "a replica in memory is not kept".into(),
            ));
        };
        let book = dir.join("book");
        let leaves =
            RedbEngine::open_cached(book.join("leaves.redb"), *cache_bytes).map_err(rep)?;
        let vault =
            OpeningsStore::open_cached(book.join("openings.redb"), *cache_bytes).map_err(rep)?;
        let service =
            RedbEngine::open_cached(dir.join("service").join("leaves.redb"), *cache_bytes)
                .map_err(rep)?;
        if let Some(m) = manifest {
            let restore = |what: &str, r: Result<(), String>| {
                r.map_err(|e| DriveError::Replica(format!("the {what} savepoint: {e}")))
            };
            restore(
                "book",
                leaves.restore_savepoint(m.book_savepoint).map_err(|e| e.0),
            )?;
            restore(
                "openings",
                vault
                    .restore_savepoint(m.openings_savepoint)
                    .map_err(|e| format!("{e:?}")),
            )?;
            restore(
                "service",
                service
                    .restore_savepoint(m.service_savepoint)
                    .map_err(|e| e.0),
            )?;
            leaf_store::archive::cut_after(book.join("archive"), m.book_applied).map_err(rep)?;
        }
        let archive = Archive::open(
            book.join("archive"),
            ArchiveConfig {
                fsync: false,
                ..ArchiveConfig::default()
            },
        )
        .map_err(rep)?;
        let last = archive.last();
        let store = BookStore::with_engine(
            leaves,
            Some(archive),
            StoreConfig {
                durability: Durability::Visible,
                verify_reads: true,
            },
        )
        .map_err(rep)?;
        // The store's own archive ends exactly where the store stands: no record missing, none
        // beyond.
        let head = store.applied_sequence().map(|s| (s, store.log_head()));
        if last != head {
            return Err(DriveError::Replica(format!(
                "the kept book's archive ends at {:?}, the store stands at {:?}",
                last.map(|l| l.0),
                head.map(|h| h.0)
            )));
        }
        if let Some(m) = manifest
            && head != m.book_applied.map(|s| (s, m.book_head))
        {
            return Err(DriveError::Replica(format!(
                "the book store stands at {:?}, the manifest at {:?}",
                head.map(|h| h.0),
                m.book_applied
            )));
        }
        let service = BookStore::with_engine(
            service,
            None,
            StoreConfig {
                durability: Durability::Visible,
                verify_reads: true,
            },
        )
        .map_err(rep)?;
        Ok((store, vault, service))
    }
}

/// The replica a kept record names, rebuilt over its reopened stores (`T-031-25`, `T-031-26`): the
/// service store from the service tree's leaves, checked leaf for leaf; the audit records the
/// record holds restored, checked as a chain; the records the audit store holds dropped from the
/// outbox. It must reach the record's roots and next posting.
pub(super) fn open_kept(
    stores: &ReplicaStores,
    keys: &HostKeys,
    genesis: &Genesis,
    kept: &Kept,
    audit_floor: Option<(u64, [u8; 32])>,
) -> Result<(Replica, ServiceStore, Recorder, (u64, u64)), DriveError> {
    let t = std::time::Instant::now();
    let (store, vault, service) = stores.reopen_stores(kept.manifest.as_ref())?;
    let stores_ms = ms_since(t);
    let t = std::time::Instant::now();
    let service = ServiceStore { store: service };
    let (recorder, ring) = Recorder::new(keys);
    let book = Book::open(RecStore::new(store), RecVault::new(vault), ring)
        .map_err(|e| DriveError::Replica(format!("the kept book: {e:?}")))?;
    // Every leaf of the tree is read: the kinds the store keeps, and the host's delivery records,
    // cover the tree.
    if service.holds_foreign_kinds()? {
        return Err(DriveError::Replica(
            "the kept service tree holds leaves of a kind the store does not keep".into(),
        ));
    }
    let leaves = service.all_leaves()?;
    let audit = ledger_core::export::AuditHead {
        emitted: kept.audit_emitted,
        head: kept.audit_head,
        shipped: kept.audit_shipped,
    };
    let mut state = State::from_whole_leaves(leaves, &audit)
        .map_err(|e| DriveError::Replica(format!("the kept service store: {e:?}")))?;
    let mut outbox = Vec::with_capacity(kept.audit_outbox.len());
    for h in &kept.audit_outbox {
        let record = hex::decode(h)
            .ok()
            .and_then(|b| ledger_core::audit::AuditRecord::decode(&b))
            .ok_or_else(|| DriveError::Replica("a kept audit record".into()))?;
        outbox.push(record);
    }
    state
        .restore_audit_outbox(outbox)
        .map_err(|e| DriveError::Replica(format!("the kept audit records: {e:?}")))?;
    let mut replica = Ledger::from_parts(book, state, genesis.lifecycle.clone());
    let _ = replica.state().take_touched();
    // What the audit store holds already leaves the outbox, as a replay drops it.
    if let Some(f) = audit_floor {
        replica
            .acknowledge_audit(f)
            .map_err(|e| DriveError::Replica(format!("the audit store's head: {e:?}")))?;
    }
    replica.record_calls(true);
    let (roots, next) = replica_roots(&replica, &service)?;
    if roots != kept.roots || next != kept.next_posting {
        return Err(DriveError::Replica(
            "the kept stores do not reach the kept roots".into(),
        ));
    }
    Ok((replica, service, recorder, (stores_ms, ms_since(t))))
}

impl<P: AgentPort> Driver<P> {
    /// Take a replica manifest at the last committed input (`T-031-26`, module documentation):
    /// the replica's archive synced, a savepoint of each of its files, and the record written.
    /// Only between inputs, with nothing staged, and with the host's log holding every committed
    /// input durably. `prev` is the manifest in force: its savepoints are kept until this one is
    /// written, every older one is deleted.
    ///
    /// # Errors
    ///
    /// Stores in memory, inputs staged, the replica or the Agent not where the last committed input
    /// left them, or a store that could not be saved. The manifest in force stays then, with its
    /// savepoints.
    pub fn checkpoint(
        &mut self,
        replica: &Replica,
        stores: &ReplicaStores,
        prev: Option<&Kept>,
    ) -> Result<Kept, DriveError> {
        let Some(dir) = stores.dir() else {
            return Err(DriveError::Replica(
                "a replica in memory has no manifest".into(),
            ));
        };
        if !self.staged.is_empty() {
            return Err(DriveError::Replica("inputs are staged".into()));
        }
        at(WritePoint::Begin, dir);
        let (roots, next_posting) = replica_roots(replica, &self.service)?;
        let after = self.last_entry.as_ref().map_or(self.genesis, |e| e.roots);
        if roots != after {
            return Err(DriveError::Replica(
                "the replica is not where the last committed input left it".into(),
            ));
        }
        // The writer's code and mode are the Agent's own at this input: what a rebuild would have
        // reached, and what the inputs after it are replayed under.
        let hot = self
            .port
            .hot()
            .map_err(DriveError::Port)?
            .ok_or(DriveError::Refused(AgentRefusal::NotFounded))?;
        if hot.inputs != self.seq
            || hot.log_head != self.log_head
            || hot.roots() != roots
            || hot.next_posting != next_posting
        {
            return Err(DriveError::Replica(format!(
                "the Agent stands at input {}, the replica at {}",
                hot.inputs, self.seq
            )));
        }
        let prev = prev.and_then(|k| k.manifest.as_ref());
        let keep = |id: Option<u64>| id.into_iter().collect::<Vec<u64>>();
        let book = replica.book().store().inner();
        stores.sync_book_archive_since(prev.and_then(|m| m.book_applied))?;
        at(WritePoint::ArchiveSynced, dir);
        let book_savepoint = book
            .engine()
            .savepoint(&keep(prev.map(|m| m.book_savepoint)))
            .map_err(rep)?;
        at(WritePoint::BookSaved, dir);
        let openings_savepoint = replica
            .book()
            .vault()
            .backend()
            .inner()
            .savepoint(&keep(prev.map(|m| m.openings_savepoint)))
            .map_err(rep)?;
        at(WritePoint::OpeningsSaved, dir);
        let service_savepoint = self
            .service
            .store
            .engine()
            .savepoint(&keep(prev.map(|m| m.service_savepoint)))
            .map_err(rep)?;
        at(WritePoint::ServiceSaved, dir);
        let audit = replica.state().audit();
        let kept = Kept {
            version: Kept::MANIFEST,
            seq: self.seq,
            log_head: self.log_head,
            roots,
            next_posting,
            genesis_roots: self.genesis,
            audit_emitted: audit.emitted(),
            audit_head: audit.head_hash(),
            audit_shipped: audit.shipped(),
            audit_outbox: audit.outbox().map(|r| hex::encode(r.encode())).collect(),
            manifest: Some(Manifest {
                code: hot.code,
                generation: hot.generation,
                upgrade: hot.upgrade,
                authority: hot.authority,
                authority_next: hot.authority_next,
                book_applied: book.applied_sequence(),
                book_head: book.log_head(),
                book_savepoint,
                openings_savepoint,
                service_savepoint,
            }),
            digest: None,
        }
        .sealed()
        .map_err(DriveError::Replica)?;
        stores.write_kept(&kept)?;
        Ok(kept)
    }

    /// Delete every savepoint of the replica's files (`T-031-26`): a clean stop's, after its
    /// record is written.
    pub(super) fn clear_savepoints(&self, replica: &Replica) -> Result<(), DriveError> {
        replica
            .book()
            .store()
            .inner()
            .engine()
            .clear_savepoints()
            .map_err(rep)?;
        replica
            .book()
            .vault()
            .backend()
            .inner()
            .clear_savepoints()
            .map_err(rep)?;
        self.service.store.engine().clear_savepoints().map_err(rep)
    }

    /// [`Driver::reopen`] of a replica manifest (`T-031-26`, module documentation).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn reopen_manifest(
        stores: &ReplicaStores,
        mut port: P,
        keys: HostKeys,
        genesis: &Genesis,
        kept: &Kept,
        archive: &mut Vec<RingEntry>,
        audit_floor: Option<(u64, [u8; 32])>,
    ) -> Result<(Self, Replica, Reopened), Reopen<P>> {
        let unusable = |port: P, why: String| Reopen::Unusable(port, why);
        let Some(m) = kept.manifest.clone() else {
            return Err(unusable(port, "not a manifest".into()));
        };
        let hot = match port.hot() {
            Ok(Some(h)) => h,
            Ok(None) => return Err(unusable(port, "the Agent is not founded".into())),
            Err(e) => return Err(unusable(port, format!("{e:?}"))),
        };
        // The log, with what the Agent committed beyond it, as a rebuild takes it.
        if archive.len() as u64 > hot.inputs {
            return Err(unusable(
                port,
                format!(
                    "the host archived {} inputs and the Agent committed {}",
                    archive.len(),
                    hot.inputs
                ),
            ));
        }
        for seq in archive.len() as u64 + 1..=hot.inputs {
            match port.entry(seq) {
                Ok(Some(e)) => archive.push(e),
                Ok(None) => {
                    return Err(unusable(
                        port,
                        format!("input {seq} is neither archived nor in the Agent's ring"),
                    ));
                }
                Err(e) => return Err(unusable(port, format!("{e:?}"))),
            }
        }
        // The manifest is an input of this history: the Agent at or past it, the log holding it
        // with the same log hash, roots and next posting.
        if kept.seq > hot.inputs {
            return Err(unusable(
                port,
                format!(
                    "the manifest is at input {}, ahead of the Agent at {}",
                    kept.seq, hot.inputs
                ),
            ));
        }
        let (log_head, roots, next_posting) = match kept.seq.checked_sub(1) {
            None => ([0; 32], kept.genesis_roots, kept.next_posting),
            Some(i) => {
                let e = &archive[i as usize];
                (e.log_hash(), e.roots, e.next_posting)
            }
        };
        if log_head != kept.log_head || roots != kept.roots || next_posting != kept.next_posting {
            return Err(unusable(
                port,
                format!(
                    "the manifest at input {} is not the log's input {}",
                    kept.seq, kept.seq
                ),
            ));
        }
        if audit_behind(audit_floor, kept) {
            return Err(unusable(
                port,
                "the audit store is behind what the manifest had shipped".into(),
            ));
        }
        let rolled = (|| {
            let (mut replica, mut service, recorder, timings) =
                open_kept(stores, &keys, genesis, kept, audit_floor)?;
            let t = std::time::Instant::now();
            // From here the replica decides as a replay that reached this input would: under the
            // writer's code and mode the Agent had there.
            replica.set_code(m.code, m.authority);
            replica.set_authority_next(m.authority_next);
            let mut at = Replaying {
                writer: WriterCode {
                    code: m.code,
                    generation: m.generation,
                    upgrade: m.upgrade,
                },
                next: kept.seq + 1,
                prev: kept.log_head,
            };
            let mut floor = audit_floor;
            let clerk = recorder.clerk();
            for e in &archive[kept.seq as usize..] {
                replay_entry(&mut replica, &mut service, &mut at, &clerk, e)?;
                // As a replay drops them: what the audit store holds leaves the outbox as it is
                // emitted.
                if let Some(f) = floor
                    && replica.acknowledge_audit(f).is_err()
                {
                    floor = None;
                }
            }
            Ok::<_, DriveError>((replica, service, recorder, timings, ms_since(t)))
        })();
        let (replica, service, recorder, (stores_ms, replica_ms), replay_ms) = match rolled {
            Ok(x) => x,
            Err(e) => return Err(unusable(port, format!("{e:?}"))),
        };
        let reopened = Reopened {
            from: kept.seq,
            rolled: hot.inputs - kept.seq,
            manifest: true,
            stores_ms,
            replica_ms,
            replay_ms,
        };
        Self::assemble(
            port,
            keys,
            hot,
            replica,
            service,
            recorder,
            kept.genesis_roots,
            archive,
        )
        .map(|(d, r)| (d, r, reopened))
        .map_err(|(port, e)| Reopen::Unusable(port, format!("the manifest's driver: {e:?}")))
    }
}
