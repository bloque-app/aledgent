// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The movement cursor (`F-011` §5, `REQ-QR-5`, `REQ-PO-2`, `AC-H2.1`). Accepted proposal.
//!
//! A cursor is opaque bytes carrying `(last_sequence, filter_digest, view_sequence)`:
//!
//! - the next page starts after `last_sequence`, the last posting the previous page covered;
//! - it is read at `view_sequence`, the view the first page was read at, so postings committed
//!   since never shift a page, and the union of the pages is exactly the range as of that view;
//! - `filter_digest` binds it to the filter it was made for: the same cursor with another filter
//!   is refused, because the digest does not match.
//!
//! There is no offset anywhere. A cursor that is malformed, made for another filter, or from a
//! view the book has not reached cannot bound a range aledgent can serve: `ERR-QueryUnbounded`.
//! No cursor is stored: a server-side cursor table would be a side effect of a query
//! (`REQ-QR-3`).

use alloc::vec::Vec;

use crate::refusal::Refusal;

/// The cursor's format version.
const VERSION: u8 = 1;
/// Bytes of the filter digest kept in a cursor.
const DIGEST_LEN: usize = 16;
/// Encoded length.
const LEN: usize = 1 + 8 + 8 + DIGEST_LEN;

/// An opaque position in a movement list (module documentation).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Cursor(Vec<u8>);

/// A cursor's contents, once checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Position {
    /// The last posting the previous page covered.
    pub(crate) last: u64,
    /// The view the list is read at.
    pub(crate) view: u64,
}

impl Cursor {
    /// The cursor after `last`, for a list read at `view` under the filter whose digest is
    /// `filter`.
    pub(crate) fn new(last: u64, view: u64, filter: &[u8; 32]) -> Cursor {
        let mut b = Vec::with_capacity(LEN);
        b.push(VERSION);
        b.extend_from_slice(&last.to_be_bytes());
        b.extend_from_slice(&view.to_be_bytes());
        b.extend_from_slice(&filter[..DIGEST_LEN]);
        Cursor(b)
    }

    /// A cursor from bytes a caller holds. Nothing is checked until it is used.
    pub fn from_bytes(bytes: Vec<u8>) -> Cursor {
        Cursor(bytes)
    }

    /// Its bytes, to hand to the caller.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Its contents, if it was made for `filter` and its view is at or below `head`.
    /// `ERR-QueryUnbounded` otherwise.
    pub(crate) fn open(&self, filter: &[u8; 32], head: u64) -> Result<Position, Refusal> {
        let b = &self.0;
        if b.len() != LEN || b[0] != VERSION {
            return Err(Refusal::QueryUnbounded);
        }
        let mut last = [0u8; 8];
        last.copy_from_slice(&b[1..9]);
        let mut view = [0u8; 8];
        view.copy_from_slice(&b[9..17]);
        if b[17..] != filter[..DIGEST_LEN] {
            return Err(Refusal::QueryUnbounded);
        }
        let (last, view) = (u64::from_be_bytes(last), u64::from_be_bytes(view));
        if view > head || last >= view {
            return Err(Refusal::QueryUnbounded);
        }
        Ok(Position { last, view })
    }
}
