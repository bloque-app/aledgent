// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
// T-043-03: the gate's fold of the chain of entries, offline (`npm test`). Its digests are checked
// against the contract's: the pinned vector of `tests.rs` (`t_043_02_the_chain_digest_is_labelled_
// and_pinned`), and `chain-vectors.json`, which `vecgen/` prints from the contract's own event
// structs and its own `entry_digest` and `chain_digest`, one entry of every kind (a `Refused`
// carrying the largest call among them) and the first entry after a reset. The fold runs over a
// stub of the chain's RPC: blocks of `ContractEmitted` records.

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';

import { u8aConcat, u8aToHex } from '@polkadot/util';

import { ZERO_DIGEST, advance, chainDigest, contractEventsOf, decodeChainHead, entryDigest, locateStart, newFold, startOf } from '../chain.mjs';

const V = JSON.parse(readFileSync(new URL('./chain-vectors.json', import.meta.url), 'utf8'));
const bytes = (hex) => Uint8Array.from(hex.slice(2).match(/../g), (b) => parseInt(b, 16));
const ADDRESS = 'contract';
const OTHER = 'another';

/// A `ContractEmitted` record of `address` with `data`, and the stub ABI's name for it.
const kinds = new Map();
function emitted(kind, data, address = ADDRESS) {
  const d = typeof data === 'string' ? bytes(data) : data;
  const key = {};
  kinds.set(key, kind);
  return { key, event: { section: 'contracts', method: 'ContractEmitted', data: [{ toString: () => address }, { toU8a: () => d }] } };
}
const codeUpdated = (address = ADDRESS) => ({ event: { section: 'contracts', method: 'ContractCodeUpdated', data: [{ toString: () => address }] } });
const abi = {
  decodeEvent: (record) => {
    const kind = kinds.get(record.key);
    if (!kind) throw new Error('the earlier code\'s event was decoded');
    return { event: { identifier: `settlement::${kind}`, args: [] }, args: [] };
  },
};
/// The chain's RPC over `blocks` (`{ number: records }`).
const rpc = (blocks) => ({
  rpc: { chain: { getBlockHash: async (n) => n } },
  at: async (n) => ({ query: { system: { events: async () => blocks[n] ?? [] } } }),
});
const plain = (seq, at, kind) => ({ seq, at, kind });
const u64 = (n) => {
  const b = new Uint8Array(8);
  new DataView(b.buffer).setBigUint64(0, BigInt(n), true);
  return b;
};
/// The vectors' entries as blocks of records, from `from` on.
function entryBlocks() {
  const blocks = {};
  for (const e of V.entries) (blocks[e.at] ??= []).push(emitted(e.kind, e.data));
  return blocks;
}
const last = V.entries[V.entries.length - 1];
const headOfAll = { position: BigInt(V.entries.length), digest: last.chain, start: 0n };

test('the digest of tests.rs, pinned outside the crate', () => {
  // Anchor 1, operation reference [1; 32], commitment [0xA1; 32], at block 1, position 0.
  const data = u8aConcat(u64(0), new Uint8Array(32).fill(1), u64(1), new Uint8Array(32).fill(0xa1));
  const e0 = entryDigest(1, 'Anchored', data);
  assert.equal(u8aToHex(e0), '0x01306698c3faca4813e818fafd0fe09f2977d886b76582e0e1c5fd527195500a');
  assert.equal(u8aToHex(chainDigest(ZERO_DIGEST, 0n, e0)), '0x8cdf807deac993ffabca639a554c713011a2edb44f3ed1fca81602403b631c15');
});

test("every kind of entry, rebuilt from its event's data, digests as the contract's own code", () => {
  let d = ZERO_DIGEST;
  V.entries.forEach((e, n) => {
    const entry = entryDigest(e.at, e.kind, bytes(e.data));
    assert.equal(u8aToHex(entry), e.entry, `${e.kind} at ${n}`);
    d = chainDigest(d, BigInt(n), entry);
    assert.equal(u8aToHex(d), e.chain, `the chain after ${e.kind} at ${n}`);
  });
  assert.deepEqual([...new Set(V.entries.map((e) => e.kind))].sort(), ['Anchored', 'Bound', 'Deposited', 'Refused', 'Withdrawn']);
});

test('the head cell decodes in one place; absent, it reads as (next_seq, zeros, next_seq)', () => {
  assert.deepEqual(decodeChainHead(null, 13), { position: 13n, digest: `0x${'00'.repeat(32)}`, start: 13n });
  const cell = u8aConcat(u64(6), bytes(last.chain), u64(0));
  assert.deepEqual(decodeChainHead(cell), headOfAll);
  assert.throws(() => decodeChainHead(new Uint8Array(40)), /48/);
  assert.equal(startOf(headOfAll), 0n);
  assert.equal(startOf({ position: 1n, digest: last.chain }), null);
});

test('the fold of the events equals the head; another contract\'s events are not folded', async () => {
  const blocks = entryBlocks();
  blocks[11].push(emitted('Deposited', V.entries[0].data, OTHER));
  const fold = newFold({ number: 9 });
  await advance(rpc(blocks), abi, ADDRESS, fold, 14, plain);
  assert.equal(fold.position, BigInt(V.entries.length));
  assert.equal(fold.digest, last.chain);
  assert.deepEqual(fold.entries.map((e) => e.kind), V.entries.map((e) => e.kind));
});

test('a skipped, repeated or reordered position fails', async () => {
  for (const mutate of [(b) => b[11].splice(0, 1), (b) => b[11].push(b[11][1]), (b) => b[11].reverse()]) {
    const blocks = entryBlocks();
    mutate(blocks);
    await assert.rejects(advance(rpc(blocks), abi, ADDRESS, newFold({ number: 9 }), 14, plain), /skipped, repeated or out of order/);
  }
});

test('a reset: the chain it ends is checked against the head before it, then a new chain from zeros', async () => {
  const blocks = entryBlocks();
  blocks[20] = [emitted('AnchorHeadSet', new Uint8Array(4)), emitted('Anchored', V.afterReset.data)];
  const heads = { 19: headOfAll };
  const fold = newFold({ number: 9 });
  await advance(rpc(blocks), abi, ADDRESS, fold, 20, plain, async (n) => heads[n] ?? assert.fail(`no head at #${n}`));
  assert.equal(fold.resets, 1);
  assert.equal(fold.position, 1n);
  assert.equal(fold.start, 0n);
  assert.equal(fold.digest, V.afterReset.chain);
  assert.deepEqual(fold.entries, [{ seq: 0, at: 20, kind: 'Anchored' }]);

  // The chain it ends does not match the head before it: its last entry was hidden.
  const hidden = entryBlocks();
  hidden[20] = blocks[20];
  hidden[14] = [];
  await assert.rejects(advance(rpc(hidden), abi, ADDRESS, newFold({ number: 9 }), 20, plain, async () => headOfAll), /the chain the reset at #20 ends/);
  // With no head to check it against, a fold does not cross a reset.
  await assert.rejects(advance(rpc(blocks), abi, ADDRESS, newFold({ number: 9 }), 20, plain), /spans a reset/);
  // An entry of the old chain before the reset in its own block is unsupported, and says so.
  const same = entryBlocks();
  same[14].push(emitted('AnchorHeadSet', new Uint8Array(4)));
  await assert.rejects(advance(rpc(same), abi, ADDRESS, newFold({ number: 9 }), 14, plain, async () => headOfAll), /unsupported/);
});

test('an upgrade: the chain starts after ContractCodeUpdated, in its block; the earlier code\'s events are not decoded', async () => {
  // v2 left next_seq = 0 here (the vectors' chain starts at 0): one v2 event, the upgrade, then
  // the first entry of the new chain, in one block.
  const blocks = entryBlocks();
  blocks[10] = [emitted(undefined, new Uint8Array(8)), codeUpdated(), ...blocks[10]];
  const fold = newFold({ number: 9, position: 0n, skip: { block: 10, until: 'ContractCodeUpdated' } });
  await advance(rpc(blocks), abi, ADDRESS, fold, 14, plain);
  assert.equal(fold.digest, last.chain);
  // A change of code anywhere else fails.
  const later = entryBlocks();
  later[12].push(codeUpdated());
  await assert.rejects(advance(rpc(later), abi, ADDRESS, newFold({ number: 9 }), 14, plain), /code changed at #12/);
  // A reader with no fold kept finds the start at the upgrade's block.
  const located = await locateStart(rpc(blocks), abi, ADDRESS, 14, headOfAll, 64, async () => assert.fail('start is known'));
  assert.deepEqual([located.number, located.position, located.skip], [9, 0n, { block: 10, until: 'ContractCodeUpdated' }]);
  // Without `start`, from the head's next position at that block less the entries after the change.
  const noStart = await locateStart(rpc(blocks), abi, ADDRESS, 14, { position: headOfAll.position, digest: headOfAll.digest }, 64, async () => 1n);
  assert.equal(noStart.position, 0n);
});

test('contractEventsOf marks an upgrade block\'s earlier events, undecoded', () => {
  const records = [emitted(undefined, new Uint8Array(8)), codeUpdated(), emitted('Anchored', V.entries[3].data)];
  assert.deepEqual(contractEventsOf(abi, ADDRESS, records).map((e) => e.kind), ['EarlierCode', 'ContractCodeUpdated', 'Anchored']);
});
