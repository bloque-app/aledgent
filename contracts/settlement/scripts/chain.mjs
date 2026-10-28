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

import { ZERO_HEX, decodeChainHead, startOf } from './chain-head.mjs';

export { ZERO_HEX, decodeChainHead, startOf };

export const CHAIN_LABEL = stringToU8a('aledgent/settlement-entry/v1');
export const ZERO_DIGEST = new Uint8Array(32);

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

/// The contract's events in one block's records, in order: entries (`{ kind, seq, data, args }`),
/// the starts of a new chain (`{ kind: 'AnchorHeadSet' }`) and the change of its code
/// (`{ kind: 'ContractCodeUpdated' }`). `abi` decodes them. In a block that changes the contract's
/// code, its events before the change are the earlier code's, of other shapes: they are returned as
/// `{ kind: 'EarlierCode' }`, not decoded.
export function contractEventsOf(abi, address, records) {
  const ours = (event) => event.section === 'contracts' && event.data[0].toString() === address;
  const updated = records.findIndex(({ event }) => ours(event) && event.method === 'ContractCodeUpdated');
  const out = [];
  records.forEach((record, i) => {
    const { event } = record;
    if (!ours(event)) return;
    if (event.method === 'ContractCodeUpdated') {
      out.push({ kind: 'ContractCodeUpdated' });
      return;
    }
    if (event.method !== 'ContractEmitted') return;
    if (updated >= 0 && i < updated) {
      out.push({ kind: 'EarlierCode' });
      return;
    }
    const decoded = abi.decodeEvent(record);
    const kind = decoded.event.identifier.split('::').pop();
    const data = event.data[1].toU8a(true);
    const args = {};
    decoded.event.args.forEach((a, j) => {
      args[a.name] = decoded.args[j];
    });
    if (kind in ENTRY_EVENTS) out.push({ kind, seq: u64le(data, 0), data, args });
    else if (kind === 'AnchorHeadSet') out.push({ kind, args });
  });
  return out;
}

/// The fold of one contract's entries, kept as it advances: the last block folded, the position
/// and digest after it, where the chain started, and every entry of the current chain (`{ seq, at,
/// kind, args }`). A new chain (`AnchorHeadSet`) starts it again from `(0, zeros)`.
///
/// `fold` is what `newFold` returns, or the same object saved as JSON. With `skip` (`{ block,
/// until }`), the chain starts within block `block`, after its `until` event (`ContractCodeUpdated`:
/// an upgrade from code that kept no chain; `AnchorHeadSet`: a reset); what comes before it in that
/// block is not this chain's.
export function newFold({ number, position = 0n, digest = ZERO_HEX, start = position, skip = null }) {
  return { number, position: BigInt(position), digest, start: BigInt(start), entries: [], resets: 0, skip };
}

/// Fold every finalized block after `fold.number` up to `toNumber`, in place. Fails if an entry's
/// position is not the next one (an entry skipped, repeated or out of order). `plain(seq, at, kind,
/// args)` turns an entry into what is kept in `fold.entries`.
///
/// **A reset** (`AnchorHeadSet`) starts a new chain from `(0, zeros)`. The chain it ends is checked
/// first: the fold up to the block before the reset must equal the head at that block, which
/// `headAt(n)` reads (a decoded head, `decodeChainHead`'s). With no `headAt`, a fold that spans a
/// reset fails rather than drop a chain it never compared. An entry of the old chain in the reset's
/// own block, before the reset, is unsupported (no head is readable between two extrinsics of one
/// block) and fails with that reason.
///
/// **A change of the contract's code** is folded only where the fold starts (`skip`): a chain from
/// before an upgrade is folded from the upgrade's block, at the head it left. Anywhere else it fails.
export async function advance(api, abi, address, fold, toNumber, plain, headAt = null) {
  let position = BigInt(fold.position);
  let digest = typeof fold.digest === 'string' ? hexBytes(fold.digest) : fold.digest;
  for (let n = Number(fold.number) + 1; n <= toNumber; n++) {
    const hash = await api.rpc.chain.getBlockHash(n);
    const records = await (await api.at(hash)).query.system.events();
    let events = contractEventsOf(abi, address, records);
    if (fold.skip && Number(fold.skip.block) === n) {
      const at = events.map((e) => e.kind).lastIndexOf(fold.skip.until);
      if (at < 0) fail(`#${n}: the fold starts after its ${fold.skip.until}, and it has none`);
      events = events.slice(at + 1);
      fold.skip = null;
    }
    const resets = events.flatMap((e, i) => (e.kind === 'AnchorHeadSet' ? [i] : []));
    const reset = resets.length ? resets[0] : -1;
    for (let k = 1; k < resets.length; k++) {
      if (events.slice(resets[k - 1], resets[k]).some((e) => e.kind in ENTRY_EVENTS)) fail(`#${n}: entries between two resets in one block: unsupported, no head is readable between them`);
    }
    if (reset >= 0) {
      if (events.slice(0, reset).some((e) => e.kind in ENTRY_EVENTS)) {
        fail(`#${n}: entries of the old chain in the block that resets it, before the reset: unsupported, no head is readable between them and the reset`);
      }
      if (!headAt) fail(`#${n}: the fold spans a reset, and no head before it was given to check the chain it ends`);
      const before = await headAt(n - 1);
      expectFoldEquals({ ...fold, position, digest: u8aToHex(digest) }, before, `the chain the reset at #${n} ends, at #${n - 1}`);
    }
    for (const e of events) {
      if (e.kind === 'ContractCodeUpdated' || e.kind === 'EarlierCode') fail(`the contract's code changed at #${n}: fold from that block, at the head it left`);
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

/// Whether the fold equals a head (`decodeChainHead`'s): position and digest, and where the chain
/// started if the head says.
export function foldMatches(fold, head) {
  const start = startOf(head);
  return BigInt(fold.position) === head.position && fold.digest === head.digest && (start === null || BigInt(fold.start) === start);
}

/// Fail unless the fold equals the head, naming both.
export function expectFoldEquals(fold, head, label) {
  if (!foldMatches(fold, head)) {
    fail(`${label}: the events fold to (${fold.position}, ${fold.digest}, start ${fold.start}) at #${fold.number}, the head is (${head.position}, ${head.digest}, start ${startOf(head)})`);
  }
}

/// Walk back from block `toNumber` to where the current chain started, for a reader that kept no
/// fold. Returns a fold positioned just before the chain's first entry, ready for `advance`. Stops
/// at the block of the newest `AnchorHeadSet` (a new chain from 0, after it), at the block the
/// contract's code changed (the chain starts after the change, at the head's `start`, or without
/// it at `positionAfter(n)` less the entries after the change in that block), or, with `start`, at
/// the block holding the entry at `start`; fails after `maxBlocks`.
export async function locateStart(api, abi, address, toNumber, head, maxBlocks, positionAfter) {
  const start = startOf(head);
  if (start !== null && head.position === start) return newFold({ number: toNumber, position: start });
  for (let n = toNumber, i = 0; n > 0 && i < maxBlocks; n--, i++) {
    const hash = await api.rpc.chain.getBlockHash(n);
    const events = contractEventsOf(abi, address, await (await api.at(hash)).query.system.events());
    if (events.some((e) => e.kind === 'AnchorHeadSet')) return newFold({ number: n - 1, position: 0n, skip: { block: n, until: 'AnchorHeadSet' } });
    if (events.some((e) => e.kind === 'ContractCodeUpdated')) {
      const after = events.filter((e) => e.kind in ENTRY_EVENTS).length;
      const position = start ?? (await positionAfter(n)) - BigInt(after);
      return newFold({ number: n - 1, position, skip: { block: n, until: 'ContractCodeUpdated' } });
    }
    if (start !== null && events.some((e) => e.seq === start)) return newFold({ number: n - 1, position: start });
  }
  return fail(`the chain's start is not within ${maxBlocks} blocks of #${toNumber}`);
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
