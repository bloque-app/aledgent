// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The archive's records (`T-005-15`): segments of the host's two logs, and checkpoints of the
//! Agent's runtime state.
//!
//! **A segment** is the next bytes of `archive.bin` and of `replies.jsonl`, exactly as the host
//! wrote them, after the bytes of the segment before: segment `n` starts where `n - 1` ended, in
//! both files, and names that segment's digest. Concatenated, a contiguous run of segments from
//! the first is a byte-exact copy of the host's two logs up to its end.
//!
//! **A restore point** is a segment that ends at a decision boundary (`cut`): the host had
//! written every entry and every reply of every decision up to there, as it does before it
//! answers. A large backlog is split into parts, entries first and replies after, and only its
//! last part is a restore point, so no restore ever holds a reply whose decision it does not
//! hold. Each segment carries the latest restore point at or before its end ([`Point`]).
//!
//! **A checkpoint** is the Agent's runtime state as the host saved it at a restore point
//! (`agent.state`), named by that segment, with its digest. It shortens a restore: the fresh
//! Agent starts from it and re-sends only the inputs after it. Without one, the Agent is founded
//! afresh and re-sent every input.
//!
//! Every record is `magic ‖ u32 header length ‖ header (JSON) ‖ payloads`. The header gives each
//! payload's offsets, length and SHA-256; a record's digest is the SHA-256 of all of it.

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

/// The format this build writes and reads.
pub const FORMAT: u32 = 1;

/// The most bytes of payload one segment carries.
pub const MAX_SEGMENT_PAYLOAD: u64 = 64 * 1024 * 1024;

/// The most bytes one record may take: a segment's payload, or a checkpoint (the Agent's state is
/// at most 4 MiB in VOS), and a header.
pub const MAX_RECORD_BYTES: u64 = MAX_SEGMENT_PAYLOAD + 1024 * 1024;

const SEGMENT_MAGIC: &[u8; 8] = b"ALGSEG1\n";
const CHECKPOINT_MAGIC: &[u8; 8] = b"ALGCKP1\n";

/// Where segments are named.
pub const SEGMENTS: &str = "segments/";
/// Where checkpoints are named.
pub const CHECKPOINTS: &str = "checkpoints/";

/// The name of segment `n`.
pub fn segment_name(n: u64) -> String {
    format!("{SEGMENTS}{n:016x}.seg")
}

/// The name of the checkpoint at segment `n`.
pub fn checkpoint_name(n: u64) -> String {
    format!("{CHECKPOINTS}{n:016x}.ckpt")
}

/// The number in a segment's or a checkpoint's name.
pub fn number_of(name: &str) -> Option<u64> {
    let base = name.rsplit('/').next()?;
    let (hex, ext) = base.split_once('.')?;
    if hex.len() != 16 || !matches!(ext, "seg" | "ckpt") {
        return None;
    }
    u64::from_str_radix(hex, 16).ok()
}

/// SHA-256, hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// A byte range of one of the host's logs, and the SHA-256 of its bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    /// Its first byte's offset.
    pub start: u64,
    /// One past its last byte.
    pub end: u64,
    /// The SHA-256 of its bytes, hex.
    pub sha256: String,
}

impl Span {
    /// Its length.
    pub fn len(&self) -> u64 {
        self.end.saturating_sub(self.start)
    }

    /// Whether it is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A restore point: where the core stood at a decision boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Point {
    /// The core's input number: every reply up to it is archived (`core_input_seq` in ingress's
    /// received-log).
    pub inputs: u64,
    /// The Agent's input number: every entry up to it is archived.
    pub entries: u64,
    /// The last entry's log hash, hex (zeros before the first).
    pub log_head: String,
    /// The last entry's kernel root, hex.
    pub kernel_root: String,
    /// The last entry's service root, hex.
    pub service_root: String,
    /// The last entry's audit digest, hex.
    pub audit: String,
    /// When the host committed the decision it ends at, in ms since the Unix epoch.
    pub at: u64,
    /// The bytes of `archive.bin` up to it.
    pub archive_bytes: u64,
    /// The bytes of `replies.jsonl` up to it.
    pub replies_bytes: u64,
}

/// A segment's header (module documentation).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentHeader {
    /// [`FORMAT`].
    pub format: u32,
    /// The book it belongs to: its genesis operation reference, hex.
    pub book: String,
    /// Its number, from 1.
    pub n: u64,
    /// The digest of segment `n - 1`, hex; empty for the first.
    pub prev: String,
    /// Its bytes of `archive.bin`.
    pub archive: Span,
    /// Its bytes of `replies.jsonl`.
    pub replies: Span,
    /// Whether its end is a restore point.
    pub cut: bool,
    /// The latest restore point at or before its end.
    pub point: Option<Point>,
    /// When it was written, in ms since the Unix epoch.
    pub taken_at: u64,
}

/// A checkpoint's header.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointHeader {
    /// [`FORMAT`].
    pub format: u32,
    /// The book.
    pub book: String,
    /// The segment whose end it stands at.
    pub n: u64,
    /// That segment's digest, hex.
    pub segment: String,
    /// The restore point it stands at.
    pub point: Point,
    /// The state's length.
    pub bytes: u64,
    /// The state's SHA-256, hex.
    pub sha256: String,
    /// When it was written.
    pub taken_at: u64,
}

/// A record that does not read back as written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Malformed(pub String);

impl core::fmt::Display for Malformed {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

fn frame(magic: &[u8; 8], header: &impl Serialize, payloads: &[&[u8]]) -> Vec<u8> {
    let h = serde_json::to_vec(header).unwrap_or_else(|_| unreachable!("a header serialises"));
    let mut out =
        Vec::with_capacity(12 + h.len() + payloads.iter().map(|p| p.len()).sum::<usize>());
    out.extend_from_slice(magic);
    out.extend_from_slice(&(h.len() as u32).to_be_bytes());
    out.extend_from_slice(&h);
    for p in payloads {
        out.extend_from_slice(p);
    }
    out
}

fn unframe<'a, H: for<'de> Deserialize<'de>>(
    magic: &[u8; 8],
    bytes: &'a [u8],
) -> Result<(H, &'a [u8]), Malformed> {
    let bad = |s: &str| Malformed(s.to_owned());
    let rest = bytes
        .strip_prefix(magic.as_slice())
        .ok_or_else(|| bad("not a record of this kind"))?;
    let (len, rest) = rest.split_at_checked(4).ok_or_else(|| bad("truncated"))?;
    let len = u32::from_be_bytes(len.try_into().map_err(|_| bad("truncated"))?) as usize;
    let (h, payload) = rest.split_at_checked(len).ok_or_else(|| bad("truncated"))?;
    let header: H = serde_json::from_slice(h).map_err(|e| Malformed(format!("header: {e}")))?;
    Ok((header, payload))
}

/// A segment, read back and checked: its header, and its two payloads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Segment {
    /// Its header.
    pub header: SegmentHeader,
    /// Its bytes of `archive.bin`.
    pub archive: Vec<u8>,
    /// Its bytes of `replies.jsonl`.
    pub replies: Vec<u8>,
    /// Its digest, hex.
    pub digest: String,
}

/// Write a segment record.
pub fn encode_segment(header: &SegmentHeader, archive: &[u8], replies: &[u8]) -> Vec<u8> {
    frame(SEGMENT_MAGIC, header, &[archive, replies])
}

/// Read a segment record and check it against itself: the format, each payload's length and
/// SHA-256.
pub fn decode_segment(bytes: &[u8]) -> Result<Segment, Malformed> {
    let (header, payload): (SegmentHeader, _) = unframe(SEGMENT_MAGIC, bytes)?;
    if header.format != FORMAT {
        return Err(Malformed(format!(
            "format {} is not {FORMAT}",
            header.format
        )));
    }
    let a = usize::try_from(header.archive.len()).map_err(|_| Malformed("length".into()))?;
    let r = usize::try_from(header.replies.len()).map_err(|_| Malformed("length".into()))?;
    if payload.len() != a + r || header.archive.end < header.archive.start {
        return Err(Malformed(format!(
            "segment {}: the payload is not the header's length",
            header.n
        )));
    }
    let (archive, replies) = payload.split_at(a);
    if sha256_hex(archive) != header.archive.sha256 || sha256_hex(replies) != header.replies.sha256
    {
        return Err(Malformed(format!(
            "segment {}: a payload's SHA-256 is not the header's",
            header.n
        )));
    }
    Ok(Segment {
        archive: archive.to_vec(),
        replies: replies.to_vec(),
        digest: sha256_hex(bytes),
        header,
    })
}

/// A checkpoint, read back and checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checkpoint {
    /// Its header.
    pub header: CheckpointHeader,
    /// The Agent's runtime state.
    pub state: Vec<u8>,
}

/// Write a checkpoint record.
pub fn encode_checkpoint(header: &CheckpointHeader, state: &[u8]) -> Vec<u8> {
    frame(CHECKPOINT_MAGIC, header, &[state])
}

/// Read a checkpoint record and check it against itself.
pub fn decode_checkpoint(bytes: &[u8]) -> Result<Checkpoint, Malformed> {
    let (header, state): (CheckpointHeader, _) = unframe(CHECKPOINT_MAGIC, bytes)?;
    if header.format != FORMAT {
        return Err(Malformed(format!(
            "format {} is not {FORMAT}",
            header.format
        )));
    }
    if state.len() as u64 != header.bytes || sha256_hex(state) != header.sha256 {
        return Err(Malformed(format!(
            "checkpoint {}: the state is not the header's",
            header.n
        )));
    }
    Ok(Checkpoint {
        header,
        state: state.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(start: u64, bytes: &[u8]) -> Span {
        Span {
            start,
            end: start + bytes.len() as u64,
            sha256: sha256_hex(bytes),
        }
    }

    #[test]
    fn t_005_15_records_read_back_and_a_changed_byte_does_not() {
        let h = SegmentHeader {
            format: FORMAT,
            book: "ab".into(),
            n: 3,
            prev: "00".into(),
            archive: span(10, b"entries"),
            replies: span(20, b"replies\n"),
            cut: true,
            point: None,
            taken_at: 7,
        };
        let rec = encode_segment(&h, b"entries", b"replies\n");
        let s = decode_segment(&rec).unwrap();
        assert_eq!((s.header, s.archive.as_slice()), (h, &b"entries"[..]));
        assert_eq!(s.digest, sha256_hex(&rec));
        let mut bad = rec.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(decode_segment(&bad).is_err());
        assert!(decode_segment(&rec[..rec.len() - 1]).is_err());
        assert!(
            decode_checkpoint(&rec).is_err(),
            "a segment is not a checkpoint"
        );
        assert_eq!(number_of(&segment_name(0x2a)), Some(0x2a));
        assert_eq!(number_of(&checkpoint_name(7)), Some(7));
        assert_eq!(number_of("segments/2a.seg"), None);
    }
}
