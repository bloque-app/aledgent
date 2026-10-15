// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Reading the archive back for a restore (`T-005-16`, `NFR-4`, `NFR-10`).
//!
//! [`fetch`] reads every segment from the first, and checks, before anything is laid out:
//!
//! - each record against itself (format, payload lengths and SHA-256) and against the one before
//!   it (its number, the previous digest it names, and its offsets in both logs: no byte missing,
//!   none twice), and that every segment is of the expected book;
//! - the entries: decoded, numbered from 1 without a gap, each naming the previous one's log
//!   hash (`REQ-RT-3`'s chain);
//! - the replies: one per core input number, from 1, without a gap, no key twice (`INV-18`);
//! - the restore point: the last segment that ends at a decision boundary, its point being the
//!   chain's last entry (number, log head, roots) and the replies' last input number.
//!
//! Bytes after the last restore point (the parts of a backlog still being sent) are left out.
//! A segment that does not read, or a gap in the numbering, ends the archive there: what follows
//! is not recoverable from it, and the report says so.
//!
//! [`lay_out`] then writes the host's two logs, the checkpoint's runtime state when one is used,
//! and the restore plan into an empty directory: what `aledgent-core-host restore` brings the
//! Agent forward from.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::Path;
use std::sync::{Condvar, Mutex, MutexGuard};

use core_binding::core::{ARCHIVE_FILE, REPLIES_FILE};
use core_binding::envelope::decode;
use core_binding::hot::RingEntry;
use ingress::boundary::{CoreReply, InputKey};
use serde::{Deserialize, Serialize};

use crate::archive::{ArchiveSource, contiguous_end};
use crate::record::{Checkpoint, Point, decode_checkpoint, decode_segment};
use crate::store::StoreError;

/// The restore plan's file in a laid-out state directory.
pub const RESTORE_FILE: &str = "restore.json";

/// What the archive gave back, checked (module documentation).
pub struct Fetched {
    /// The book.
    pub book: String,
    /// Segments read, up to the restore point.
    pub segments: u64,
    /// The restore point.
    pub point: Point,
    /// `archive.bin` up to it, byte for byte.
    pub archive: Vec<u8>,
    /// `replies.jsonl` up to it.
    pub replies: Vec<u8>,
    /// The entries.
    pub entries: Vec<RingEntry>,
    /// The replies, by core input number.
    pub replies_parsed: Vec<(InputKey, CoreReply)>,
    /// The latest checkpoint at or before the point that verifies, if any.
    pub checkpoint: Option<Checkpoint>,
    /// What was left out, and why: segments after the point, a gap, checkpoints that did not
    /// verify.
    pub notes: Vec<String>,
    /// Whether segments the archive should hold are missing or do not read: a gap, a record that
    /// does not verify or does not continue its predecessor. The point is then the last one
    /// before the damage, and what the damaged segments held is not recoverable from this
    /// archive; a restore refuses unless the operator accepts that (`opsctl restore core
    /// --accept-damaged-archive`), and the report records it.
    pub damaged: bool,
}

#[derive(Deserialize)]
struct ReplyLine {
    key: InputKey,
    reply: CoreReply,
}

/// How many segments [`fetch`] reads at once (`opsctl restore core --fetch-concurrency`). Each
/// read is one object-store request: 86 ms a segment from S3 with the CLI, one at a time, was about
/// 90 minutes for 64.8k segments (the 2026-10-15 restore drill, item 8).
pub const DEFAULT_CONCURRENCY: usize = 32;

/// Read and check the archive (module documentation), [`DEFAULT_CONCURRENCY`] segments at once.
/// `book`, when given, is the genesis operation reference the deployment's configuration names;
/// every segment must be of it.
pub fn fetch(source: &dyn ArchiveSource, book: Option<&str>) -> Result<Fetched, String> {
    fetch_with(source, book, DEFAULT_CONCURRENCY)
}

/// [`fetch`], reading up to `concurrency` segments at once (1: one at a time, on this thread).
///
/// Only the reading is concurrent. The check takes the segments one by one in their order, from 1,
/// exactly as a sequential read does: each record against itself and its predecessor, the book,
/// the offsets, then the chain. A segment that arrives early waits for the ones before it, and is
/// never looked at before them. The first segment that is missing, does not read or does not
/// continue its predecessor ends the archive there, as it does sequentially, and a store error on a
/// segment is the error it would be sequentially; whatever was read beyond it is dropped unread.
/// At most `2 × concurrency` segments are held ahead of the check.
pub fn fetch_with(
    source: &dyn ArchiveSource,
    book: Option<&str>,
    concurrency: usize,
) -> Result<Fetched, String> {
    let numbers = source.segments().map_err(|e| e.to_string())?;
    let end = contiguous_end(&numbers);
    let mut notes = Vec::new();
    let mut damaged = false;
    if numbers.last().copied().unwrap_or(0) > end {
        damaged = true;
        notes.push(format!(
            "segment {} is missing: the {} segments after it are not used",
            end + 1,
            numbers.iter().filter(|n| **n > end).count()
        ));
    }
    if end == 0 {
        return Err("the archive holds no segment".into());
    }
    let Read {
        mut archive,
        mut replies,
        the_book,
        point,
    } = if concurrency <= 1 {
        read_segments(end, book, &mut notes, &mut damaged, &mut |n| {
            source.get_segment(n)
        })?
    } else {
        let ahead = Ahead::new(end, concurrency);
        std::thread::scope(|s| {
            for _ in 0..concurrency.min(usize::try_from(end).unwrap_or(usize::MAX)) {
                s.spawn(|| ahead.work(source));
            }
            // Stops the readers however the check ends, before the scope waits for them.
            let _stop = StopOnDrop(&ahead);
            read_segments(end, book, &mut notes, &mut damaged, &mut |n| ahead.take(n))
        })?
    };
    let (segments, point, a_len, r_len, digest) =
        point.ok_or("the archive has no restore point")?;
    if (archive.len(), replies.len()) != (a_len, r_len) {
        notes.push(format!(
            "{} bytes after the restore point (a backlog still being sent) are not used",
            archive.len() - a_len + replies.len() - r_len
        ));
    }
    archive.truncate(a_len);
    replies.truncate(r_len);

    // The chain.
    let mut entries = Vec::new();
    let mut at = 0usize;
    let mut head = [0u8; 32];
    while at < archive.len() {
        let n = archive
            .get(at..at + 4)
            .map(|h| u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as usize)
            .ok_or("a torn entry")?;
        let body = archive.get(at + 4..at + 4 + n).ok_or("a torn entry")?;
        let e: RingEntry = decode(body).map_err(|e| format!("entry {}: {e}", entries.len() + 1))?;
        if e.seq != entries.len() as u64 + 1 || e.prev != head {
            return Err(format!(
                "the archived entries are not a chain at input {}",
                entries.len() + 1
            ));
        }
        head = e.log_hash();
        entries.push(e);
        at += 4 + n;
    }
    // The replies.
    let mut parsed = Vec::new();
    let mut keys = std::collections::HashSet::new();
    for line in replies.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        let l: ReplyLine =
            serde_json::from_slice(line).map_err(|e| format!("a reply line: {e}"))?;
        if l.reply.input_seq != parsed.len() as u64 + 1 {
            return Err(format!(
                "the archived replies skip from input {} to {}",
                parsed.len(),
                l.reply.input_seq
            ));
        }
        if !keys.insert(l.key) {
            return Err(format!("input {} answers a key twice", l.reply.input_seq));
        }
        parsed.push((l.key, l.reply));
    }
    // The point is the chain's end.
    let last = entries.last();
    let ok = point.entries == entries.len() as u64
        && point.inputs == parsed.len() as u64
        && point.log_head == hex::encode(head)
        && last.is_none_or(|e| {
            point.kernel_root == hex::encode(e.roots.kernel)
                && point.service_root == hex::encode(e.roots.service)
                && point.audit == hex::encode(e.roots.audit)
        });
    if !ok {
        return Err(format!(
            "segment {segments}'s point is not where its entries and replies end"
        ));
    }
    let book = the_book.unwrap_or_default();
    // The latest checkpoint that verifies, at or before the point.
    let mut checkpoint = None;
    let cps = source.checkpoints().map_err(|e| e.to_string())?;
    for &n in cps.iter().rev().filter(|n| **n <= segments) {
        let c = match source.get_checkpoint(n) {
            Ok(Some(rec)) => decode_checkpoint(&rec).map_err(|e| e.to_string()),
            Ok(None) => Err("vanished".into()),
            Err(e) => Err(e.to_string()),
        };
        let c = match c {
            Ok(c) => c,
            Err(e) => {
                notes.push(format!("checkpoint {n} is not used: {e}"));
                continue;
            }
        };
        let p = &c.header.point;
        let on_chain = c.header.book == book
            && usize::try_from(p.entries).is_ok_and(|k| k <= entries.len())
            && (p.entries == 0
                || hex::encode(entries[p.entries as usize - 1].log_hash()) == p.log_head)
            && (n != segments || c.header.segment == digest);
        if on_chain {
            checkpoint = Some(c);
            break;
        }
        notes.push(format!(
            "checkpoint {n} is not used: it is not on this archive's chain"
        ));
    }
    Ok(Fetched {
        book,
        segments,
        point,
        archive,
        replies,
        entries,
        replies_parsed: parsed,
        checkpoint,
        notes,
        damaged,
    })
}

/// What the segments gave, up to where they end (the check's first part).
struct Read {
    archive: Vec<u8>,
    replies: Vec<u8>,
    the_book: Option<String>,
    /// The last restore point: its segment, the point, how much of each log it covers, its digest.
    point: Option<(u64, Point, usize, usize, String)>,
}

/// Segments `1..=end`, taken in order from `take` and checked one by one (module documentation).
fn read_segments(
    end: u64,
    book: Option<&str>,
    notes: &mut Vec<String>,
    damaged: &mut bool,
    take: &mut dyn FnMut(u64) -> Result<Option<Vec<u8>>, StoreError>,
) -> Result<Read, String> {
    let mut archive = Vec::new();
    let mut replies = Vec::new();
    let mut prev = String::new();
    let mut the_book: Option<String> = book.map(str::to_owned);
    // The last restore point, and how much of each log and of the chain it covers.
    let mut point: Option<(u64, Point, usize, usize, String)> = None;
    for n in 1..=end {
        let rec = match take(n) {
            Ok(Some(r)) => r,
            Ok(None) => {
                *damaged = true;
                notes.push(format!("segment {n} vanished: the archive ends before it"));
                break;
            }
            Err(e) => return Err(format!("segment {n}: {e}")),
        };
        let s = match decode_segment(&rec) {
            Ok(s) => s,
            Err(e) => {
                *damaged = true;
                notes.push(format!(
                    "segment {n} does not read ({e}): the archive ends before it"
                ));
                break;
            }
        };
        let h = &s.header;
        let joins = h.n == n
            && h.prev == prev
            && h.archive.start == archive.len() as u64
            && h.replies.start == replies.len() as u64;
        if !joins {
            *damaged = true;
            notes.push(format!(
                "segment {n} does not continue segment {}: the archive ends before it",
                n - 1
            ));
            break;
        }
        match &the_book {
            Some(b) if *b != h.book => {
                return Err(format!(
                    "segment {n} is of book {}, not {b}: another deployment's archive",
                    h.book
                ));
            }
            Some(_) => {}
            None => the_book = Some(h.book.clone()),
        }
        archive.extend_from_slice(&s.archive);
        replies.extend_from_slice(&s.replies);
        prev = s.digest.clone();
        if h.cut {
            let p = h
                .point
                .clone()
                .ok_or_else(|| format!("segment {n} is a restore point without its point"))?;
            if p.archive_bytes != archive.len() as u64 || p.replies_bytes != replies.len() as u64 {
                return Err(format!("segment {n}: its point is not its end"));
            }
            point = Some((n, p, archive.len(), replies.len(), s.digest));
        }
    }
    Ok(Read {
        archive,
        replies,
        the_book,
        point,
    })
}

/// One read of a segment.
type Got = Result<Option<Vec<u8>>, StoreError>;

/// Segments read ahead of the check by a pool of readers ([`fetch_with`]): each reader takes the
/// next number not yet taken, at most `window` past the one the check waits for, and leaves what it
/// read by number. The check takes them in order.
struct Ahead {
    end: u64,
    window: u64,
    state: Mutex<AheadState>,
    moved: Condvar,
}

struct AheadState {
    /// The next number a reader takes.
    next: u64,
    /// The number the check waits for.
    want: u64,
    /// Read, not yet taken by the check.
    ready: BTreeMap<u64, Got>,
    /// The check is over: readers stop.
    stop: bool,
}

impl Ahead {
    fn new(end: u64, concurrency: usize) -> Ahead {
        Ahead {
            end,
            window: (concurrency as u64).saturating_mul(2),
            state: Mutex::new(AheadState {
                next: 1,
                want: 1,
                ready: BTreeMap::new(),
                stop: false,
            }),
            moved: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, AheadState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn wait<'a>(&self, g: MutexGuard<'a, AheadState>) -> MutexGuard<'a, AheadState> {
        self.moved.wait(g).unwrap_or_else(|p| p.into_inner())
    }

    /// A reader: until the check is over or every segment is taken.
    fn work(&self, source: &dyn ArchiveSource) {
        loop {
            let n = {
                let mut st = self.lock();
                loop {
                    if st.stop || st.next > self.end {
                        return;
                    }
                    if st.next < st.want.saturating_add(self.window) {
                        break;
                    }
                    st = self.wait(st);
                }
                st.next += 1;
                st.next - 1
            };
            // A reader that panics still leaves an answer, or the check would wait forever.
            let got =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| source.get_segment(n)))
                    .unwrap_or_else(|_| {
                        Err(StoreError::Unavailable(format!(
                            "the read of segment {n} panicked"
                        )))
                    });
            self.lock().ready.insert(n, got);
            self.moved.notify_all();
        }
    }

    /// Segment `n`'s read, once it is there. The check calls it with `1, 2, …`, in order.
    fn take(&self, n: u64) -> Got {
        let mut st = self.lock();
        st.want = n;
        self.moved.notify_all();
        loop {
            if let Some(got) = st.ready.remove(&n) {
                return got;
            }
            st = self.wait(st);
        }
    }
}

/// Ends the readers when the check ends, however it ends.
struct StopOnDrop<'a>(&'a Ahead);

impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.lock().stop = true;
        self.0.moved.notify_all();
    }
}

/// The restore plan `aledgent-core-host restore` follows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestorePlan {
    /// The book.
    pub book: String,
    /// Where it came from (no credential).
    pub source: String,
    /// The restore point.
    pub point: Point,
    /// Segments used.
    pub segments: u64,
    /// The checkpoint the Agent starts from, by segment, and its Agent input number; none: the
    /// Agent is founded afresh.
    pub checkpoint: Option<(u64, u64)>,
    /// When the plan was laid out, in ms.
    pub at: u64,
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut f = File::create(path).map_err(|e| format!("{}: {e}", path.display()))?;
    f.write_all(bytes)
        .and_then(|_| f.sync_all())
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Lay a restore out in `dir`, which must be absent or empty: the two logs, the checkpoint's
/// runtime state under `state_file` (when `use_checkpoint` and the archive has one), and the plan.
/// Returns the plan.
pub fn lay_out(
    dir: &Path,
    fetched: &Fetched,
    source: &str,
    state_file: &str,
    use_checkpoint: bool,
    at: u64,
) -> Result<RestorePlan, String> {
    if dir.exists()
        && fs::read_dir(dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?
            .next()
            .is_some()
    {
        return Err(format!(
            "{} is not empty: a restore writes only into an empty directory (REQ-AT-3)",
            dir.display()
        ));
    }
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    write_synced(&dir.join(ARCHIVE_FILE), &fetched.archive)?;
    write_synced(&dir.join(REPLIES_FILE), &fetched.replies)?;
    let checkpoint = match (&fetched.checkpoint, use_checkpoint) {
        (Some(c), true) => {
            write_synced(&dir.join(state_file), &c.state)?;
            Some((c.header.n, c.header.point.entries))
        }
        _ => None,
    };
    let plan = RestorePlan {
        book: fetched.book.clone(),
        source: source.to_owned(),
        point: fetched.point.clone(),
        segments: fetched.segments,
        checkpoint,
        at,
    };
    let json = serde_json::to_vec_pretty(&plan).map_err(|e| e.to_string())?;
    write_synced(&dir.join(RESTORE_FILE), &json)?;
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(plan)
}
