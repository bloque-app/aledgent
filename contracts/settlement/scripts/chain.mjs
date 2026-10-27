// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
// F-043 (T-043-03): the settlement contract's chain of entries (amendment 0040), as a reader
// rebuilds it. The contract stores no entry; it keeps one head cell, `(position, digest, start)`,
// and emits every entry in an event. A reader folds those events and compares the result with the
// head. `drive.mjs read` and the zombienet suite (`e2e.mjs`) both read through this module.
//
//   e_n = blake2b-256(SCALE(InboxEntry { at, event }))
//   d_n = blake2b-256("aledgent/settlement-entry/v1" ‖ d_(n-1) ‖ n as u64 big-endian ‖ e_n)
//
// with `d_(start-1)` the 32 zero bytes.
//
// **Rebuilding an entry from its event.** Each entry's event (`Deposited`, `Withdrawn`,
// `Anchored`, `Refused`, `Bound`) is the entry's own fields, in the order of its `InboxEvent`
// variant, after the position `seq` (ink! 5 encodes every field of an event in its data, topics
// included). So `SCALE(InboxEntry)` is the block number (u32, little-endian), the variant's index,
// and the event's data less its first 8 bytes. Nothing is decoded and re-encoded, so nothing can
// be re-encoded differently.

import { bnToU8a, stringToU8a, u8aConcat, u8aEq, u8aToHex } from '@polkadot/util';
import { blake2AsU8a } from '@polkadot/util-crypto';

export const CHAIN_LABEL = stringToU8a('aledgent/settlement-entry/v1');
export const ZERO_DIGEST = new Uint8Array(32);
const ZERO_HEX = u8aToHex(ZERO_DIGEST);

/// The events that carry an entry, by the index of their `InboxEvent` variant (`src/types.rs`).
export const ENTRY_EVENTS = { Deposited: 0, Withdrawn: 1, Anchored: 2, Refused: 3, Bound: 4 };

/// The root key of the head cell (`ManualKey<0x0000_000C>`).
export const CHAIN_ROOT_KEY = 0x0c;

const fail = (message) => {
  throw new Error(message);
};

const u64le = (bytes, offset) => {
  let n = 0n;
  for (let i = 7; i >= 0; i--) n = (n << 8n) | BigInt(bytes[offset + i]);
  return n;
};

/// **The one decoding of the head cell** (`ChainHead`): `position` (u64), `digest` ([u8; 32]),
/// `start` (u64), in that order, SCALE. `bytes` is the cell as stored, or the value of the
/// `chain_head` message (the same layout). Absent (`null`), the cell reads as the contract reads it:
/// `{ position: next_seq, digest: zeros, start: next_seq }`, with `nextSeq` the head's.
///
/// The head's third field, `start`, is not yet ruled by the owner: dropping it changes this
/// function, and nothing else here reads the cell.
export function decodeChainHead(bytes, nextSeq) {
  if (bytes === null || bytes === undefined) {
    if (nextSeq === undefined) fail('decodeChainHead: no cell and no next_seq to read it from');
    return { position: BigInt(nextSeq), digest: ZERO_HEX, start: BigInt(nextSeq) };
  }
  if (bytes.length !== 48) fail(`decodeChainHead: ${bytes.length} bytes, a ChainHead is 48`);
  return { position: u64le(bytes, 0), digest: u8aToHex(bytes.subarray(8, 40)), start: u64le(bytes, 40) };
}

/// `e_n` of the entry an event carries: `blake2b-256(at as u32 LE ‖ variant ‖ data[8..])`.
export function entryDigest(at, kind, data) {
  const variant = ENTRY_EVENTS[kind] ?? fail(`not an entry event: ${kind}`);
  const encoded = u8aConcat(bnToU8a(at, { bitLength: 32, isLe: true }), new Uint8Array([variant]), data.subarray(8));
  return blake2AsU8a(encoded, 256);
}

/// `d_n = blake2b-256(CHAIN_LABEL ‖ previous ‖ n (u64 BE) ‖ e_n)`.
export function chainDigest(previous, n, entry) {
  return blake2AsU8a(u8aConcat(CHAIN_LABEL, previous, bnToU8a(n, { bitLength: 64, isLe: false }), entry), 256);
}

/// The contract's events in one block's records, in order: entries (`{ kind, seq, data, args,
/// names }`) and the starts of a new chain (`{ kind: 'AnchorHeadSet' }`). `abi` decodes them.
export function contractEventsOf(abi, address, records) {
  const out = [];
  for (const record of records) {
    const { event } = record;
    if (event.section === 'contracts' && event.method === 'ContractCodeUpdated' && event.data[0].toString() === address) {
      out.push({ kind: 'ContractCodeUpdated' });
      continue;
    }
    if (event.section !== 'contracts' || event.method !== 'ContractEmitted' || event.data[0].toString() !== address) continue;
    const decoded = abi.decodeEvent(record);
    const kind = decoded.event.identifier.split('::').pop();
    const data = event.data[1].toU8a(true);
    const args = {};
    decoded.event.args.forEach((a, i) => {
      args[a.name] = decoded.args[i];
    });
    if (kind in ENTRY_EVENTS) out.push({ kind, seq: u64le(data, 0), data, args });
    else if (kind === 'AnchorHeadSet') out.push({ kind, args });
  }
  return out;
}

/// The fold of one contract's entries, kept as it advances: the last block folded, the position
/// and digest after it, where the chain started, and every entry of the current chain (`{ seq, at,
/// kind, args }`). A new chain (`AnchorHeadSet`) starts it again from `(0, zeros)`.
///
/// `fold` is what `newFold` returns, or the same object saved as JSON.
export function newFold({ number, position = 0n, digest = ZERO_HEX, start = position }) {
  return { number, position: BigInt(position), digest, start: BigInt(start), entries: [], resets: 0 };
}

/// Fold every finalized block after `fold.number` up to `toNumber`, in place. Fails if an entry's
/// position is not the next one (an entry skipped, repeated or out of order), or if the
/// contract's code changes mid-way (a chain from before an upgrade is folded from the upgrade's
/// block, `newFold` at the head it left). `plain(seq, at, kind, args)` turns an entry into what is
/// kept in `fold.entries`.
export async function advance(api, abi, address, fold, toNumber, plain) {
  let position = BigInt(fold.position);
  let digest = typeof fold.digest === 'string' ? hexBytes(fold.digest) : fold.digest;
  for (let n = Number(fold.number) + 1; n <= toNumber; n++) {
    const hash = await api.rpc.chain.getBlockHash(n);
    const records = await (await api.at(hash)).query.system.events();
    for (const e of contractEventsOf(abi, address, records)) {
      if (e.kind === 'ContractCodeUpdated') fail(`the contract's code changed at #${n}: fold from that block, at the head it left`);
      if (e.kind === 'AnchorHeadSet') {
        position = 0n;
        digest = ZERO_DIGEST;
        fold.start = 0n;
        fold.entries = [];
        fold.resets += 1;
        continue;
      }
      if (e.seq !== position) fail(`#${n}: a ${e.kind} event at position ${e.seq}, the chain is at ${position}: an entry is skipped, repeated or out of order`);
      digest = chainDigest(digest, e.seq, entryDigest(n, e.kind, e.data));
      fold.entries.push(plain(Number(e.seq), n, e.kind, e.args));
      position += 1n;
    }
    fold.number = n;
    fold.position = position;
    fold.digest = u8aToHex(digest);
  }
  fold.position = position;
  fold.digest = u8aToHex(digest);
  return fold;
}

/// Whether the fold equals a head (`decodeChainHead`'s): position and digest; `start` too once
/// the fold knows where its chain started.
export function foldMatches(fold, head) {
  return BigInt(fold.position) === head.position && fold.digest === head.digest && BigInt(fold.start) === head.start;
}

/// Fail unless the fold equals the head, naming both.
export function expectFoldEquals(fold, head, label) {
  if (!foldMatches(fold, head)) {
    fail(`${label}: the events fold to (${fold.position}, ${fold.digest}, start ${fold.start}) at #${fold.number}, the head is (${head.position}, ${head.digest}, start ${head.start})`);
  }
}

/// Walk back from block `toNumber` to where the current chain started, for a reader that kept no
/// fold: `start` (from the head) is the first position of the chain. Returns a fold positioned just
/// before the chain's first entry, ready for `advance`. Stops at the block of the newest
/// `AnchorHeadSet`, or at the block holding the entry at `start`; fails after `maxBlocks`.
export async function locateStart(api, abi, address, toNumber, head, maxBlocks) {
  if (head.position === head.start) return newFold({ number: toNumber, position: head.start, start: head.start });
  for (let n = toNumber, i = 0; n > 0 && i < maxBlocks; n--, i++) {
    const hash = await api.rpc.chain.getBlockHash(n);
    const events = contractEventsOf(abi, address, await (await api.at(hash)).query.system.events());
    if (events.some((e) => e.kind === 'AnchorHeadSet')) return newFold({ number: n - 1, position: 0n, start: 0n });
    if (events.some((e) => e.seq === head.start)) return newFold({ number: n - 1, position: head.start, start: head.start });
  }
  return fail(`the chain's start (position ${head.start}) is not within ${maxBlocks} blocks of #${toNumber}`);
}

/// Hex to bytes, for a digest kept as JSON.
export function hexBytes(hex) {
  const clean = hex.startsWith('0x') ? hex.slice(2) : hex;
  return Uint8Array.from(clean.match(/../g) ?? [], (b) => parseInt(b, 16));
}

/// The fold as JSON (BigInts as strings), and back.
export const foldToJson = (fold) => JSON.parse(JSON.stringify(fold, (_, v) => (typeof v === 'bigint' ? v.toString() : v)));
export const foldFromJson = (json) => ({ ...json, position: BigInt(json.position), start: BigInt(json.start) });

export { u8aEq };
