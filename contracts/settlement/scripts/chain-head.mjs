// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
// F-043 (T-043-03): the one decoding of the settlement contract's head cell (amendment 0040), with
// no dependencies, so that every reader (`chain.mjs`, `drive.mjs`, `e2e.mjs`, `fork/rehearse.mjs`)
// shares it. The head's third field, `start`, is not yet ruled by the owner: dropping it changes
// this file only. Readers ask `startOf(head)`, never `head.start`.

const hex = (bytes) => `0x${Array.from(bytes, (b) => b.toString(16).padStart(2, '0')).join('')}`;

/// The 32 zero bytes, as hex: the digest of a chain with no entry yet.
export const ZERO_HEX = `0x${'00'.repeat(32)}`;

const u64le = (bytes, offset) => {
  let n = 0n;
  for (let i = 7; i >= 0; i--) n = (n << 8n) | BigInt(bytes[offset + i]);
  return n;
};

/// `ChainHead`: `position` (u64), `digest` ([u8; 32]), `start` (u64), in that order, SCALE.
/// `bytes` is the cell as stored, or the value of the `chain_head` message (the same layout).
/// Absent (`null`), the cell reads as the contract reads it: `{ position: next_seq, digest: zeros,
/// start: next_seq }`, with `nextSeq` the head's.
export function decodeChainHead(bytes, nextSeq) {
  if (bytes === null || bytes === undefined) {
    if (nextSeq === undefined) throw new Error('decodeChainHead: no cell and no next_seq to read it from');
    return { position: BigInt(nextSeq), digest: ZERO_HEX, start: BigInt(nextSeq) };
  }
  if (bytes.length !== 48) throw new Error(`decodeChainHead: ${bytes.length} bytes, a ChainHead is 48`);
  return { position: u64le(bytes, 0), digest: hex(bytes.subarray(8, 40)), start: u64le(bytes, 40) };
}

/// Where a head says its chain started (a decoded head, or the message's value as JSON), or
/// `null` if it carries no `start`.
export const startOf = (head) => (head.start === undefined || head.start === null ? null : BigInt(head.start));
