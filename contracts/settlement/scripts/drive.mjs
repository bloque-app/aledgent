#!/usr/bin/env node
// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
// F-026 (T-026-04): deploy the settlement contract on Kreivo through the contracts store, and drive
// it end to end. See `contracts/settlement/README.md` for the exact commands.
//
//   node drive.mjs <step> [...]
//
// Steps, in the order a first run takes them:
//
//   preflight    the chain is Kreivo; the one `Assets` call the contract dispatches (`transfer`, for
//                a whole position) has the index and argument shape it encodes; the code fits
//                `MaxCodeLen`. Everything else moves through Kreivo's chain extension, which the
//                metadata does not describe: the `deposit` and `execute` steps exercise it.
//   community    register a community whose first admin is ADMIN (Kreivo's `CommunitiesManager`),
//                unless COMMUNITY_ID already exists, and fund the community account, which pays
//                for instantiation. Kreivo's `local` genesis already has community 1, "root",
//                administered by Alice, and asset `Here(1)` ("tUSD", sufficient, minimum 1), held
//                by Alice: the defaults use both.
//   publish      upload the code as an app of the contracts store (community origin).
//   license      request a license for the app (community origin).
//   instantiate  instantiate the contract through the store: `new(admin, [key], None)`.
//   bound        the admin sets a withdrawal bound for ASSET.
//   deposit      DEPOSITOR deposits to an account reference; the contract pulls it through Kreivo's
//                assets API, with no approval.
//   execute      KEY submits a batch: a withdrawal to PAYEE, one the bound refuses, and an anchor.
//   read         read the head and the chain's head (`chain_head`, amendment 0040) by contract
//                query and by raw child-trie storage at the finalized block (the adapter's path),
//                check they agree, and fold the contract's events from the chain's start to that
//                block: the fold must equal the chain's head. The contract stores no entry.
//   events       print the contract's events in the latest finalized blocks.
//   all          every step above, in order.
//
// Configuration, from the environment. Secret URIs are never printed.
//
//   KREIVO_WS        WebSocket endpoint (default ws://127.0.0.1:9944)
//   ADMIN_SURI       community admin and contract admin (default //Alice, dev chains only)
//   KEY_SURI         the contract's key (default //Bob)
//   DEPOSITOR_SURI   a depositor holding ASSET (default //Alice, who holds tUSD on a local chain)
//   PAYEE            address that receives the withdrawal (default //Dave's)
//   COMMUNITY_ID     u16 community id to use or register (default 1)
//   COMMUNITY_NAME   the name to register it under, if it does not exist (default "aledgent settlement")
//   ASSET            the asset, as Kreivo's FungibleAssetLocation JSON (default {"Here":1})
//   BUNDLE           the `.contract` bundle `cargo contract build` wrote
//   STATE            where ids and the contract address are kept between steps (default .state.json)
//   BLOCKS           `events`: how many blocks back to print; `read`, with no fold kept for the
//                    contract: how far back to look for the chain's start (default 20 and 256)

import { ApiPromise, Keyring, WsProvider } from '@polkadot/api';
import { Abi, ContractPromise } from '@polkadot/api-contract';
import { bnToU8a, compactToU8a, hexToU8a, stringToU8a, u8aConcat, u8aEq, u8aToHex } from '@polkadot/util';
import { blake2AsU8a, cryptoWaitReady, decodeAddress, encodeAddress } from '@polkadot/util-crypto';
import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { ENTRY_EVENTS, advance, contractEventsOf, decodeChainHead, expectFoldEquals, foldFromJson, foldToJson, locateStart, newFold, startOf } from './chain.mjs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const env = (name, fallback) => process.env[name] ?? fallback;

const WS = env('KREIVO_WS', 'ws://127.0.0.1:9944');
const COMMUNITY_ID = Number(env('COMMUNITY_ID', '1'));
const COMMUNITY_NAME = env('COMMUNITY_NAME', 'aledgent settlement');
const ASSET = JSON.parse(env('ASSET', '{"Here":1}'));
const BUNDLE = resolve(env('BUNDLE', resolve(here, '../target/ink/aledgent_settlement_contract.contract')));
const STATE = resolve(env('STATE', resolve(here, '.state.json')));

// What the contract encodes (`src/kreivo.rs`). Preflight checks them against the live metadata.
const ASSETS_PALLET = 13;
const ASSETS_CALLS = { transfer: 8 };
// The contract's storage root keys (`src/lib.rs`, `ManualKey`).
// `anchors` (3) is the legacy map of every anchor by number, removed by the migration of the code
// of 2026-10-11; `latestAnchor` (8) is the only anchor that code keeps. `inbox` (4) is where v2
// stored each entry: this code stores none, and its reset removes what v2 left. `chain` (12) is the
// head of the chain of entries (amendment 0040), decoded only by `chain.mjs`.
const ROOT_KEYS = { registration: 1, head: 2, anchors: 3, inbox: 4, positions: 5, bounds: 6, bound: 7, latestAnchor: 8, anchorMigration: 9, anchorEpoch: 10, governance: 11, chain: 12 };

// Kreivo's transaction extensions (`runtime/kreivo/src/lib.rs`). `PassAuthenticate` comes first;
// its explicit part is `Option<AuthenticateParams>`, always `None` here (0x00), so the inner type
// never matters. The payment extensions forward `ChargeAssetTxPayment`'s identifier, which the API
// knows. VERIFY on the first real run: if the node rejects the transaction as a bad proof, compare
// `api.registry.signedExtensions` with the metadata's extension list.
const signedExtensions = {
  PassAuthenticate: { extrinsic: { passAuthenticate: 'Option<Bytes>' }, payload: {} },
};

const state = existsSync(STATE) ? JSON.parse(readFileSync(STATE, 'utf8')) : {};
const save = () => writeFileSync(STATE, JSON.stringify(state, null, 2));

function log(...args) {
  console.log('[drive]', ...args);
}

function fail(message) {
  throw new Error(message);
}

/// Kreivo's `Communities::community_account(id)`: `PalletId(*b"kv/cmtys").into_sub_account_truncating(id)`.
function communityAccount(id) {
  const raw = u8aConcat(stringToU8a('modl'), stringToU8a('kv/cmtys'), bnToU8a(id, { bitLength: 16, isLe: true }));
  const padded = new Uint8Array(32);
  padded.set(raw);
  return encodeAddress(padded);
}

/// A key of the contract's child trie: `Blake2_128Concat(root_key LE ++ SCALE(key))`, as
/// `pallet-contracts` hashes an ink! variable-length key.
function contractKey(rootKey, keyBytes = new Uint8Array()) {
  const raw = u8aConcat(bnToU8a(rootKey, { bitLength: 32, isLe: true }), keyBytes);
  return u8aConcat(blake2AsU8a(raw, 128), raw);
}

/// The API every `send` uses: the one `connect` made, or the one a caller hands `useApi`.
let currentApi;

/// Make `api` the one `send` (and so `callContract`) uses: for a caller that connects on its own
/// terms (no development keys, the network checked first).
function useApi(api) {
  currentApi = api;
}

const pause = (ms) => new Promise((r) => setTimeout(r, ms));

/// The dispatch error in `events` (an `ExtrinsicFailed`), as `section.name`, if any.
function dispatchFailure(registry, events) {
  const failed = events.find(({ event }) => event.section === 'system' && event.method === 'ExtrinsicFailed');
  if (!failed) return null;
  const err = failed.event.data[0];
  return err.isModule ? (({ section, name }) => `${section}.${name}`)(registry.findMetaError(err.asModule)) : err.toString();
}

/// Our extrinsic in the finalized chain, walking back from `head` at most `depth` blocks: the block
/// and the events of its application.
async function findFinalized(api, head, hash, depth) {
  let at = head;
  for (let i = 0; i < depth; i++) {
    const { block } = await api.rpc.chain.getBlock(at);
    const index = block.extrinsics.findIndex((x) => x.hash.toHex() === hash);
    if (index >= 0) {
      const records = await (await api.at(at)).query.system.events();
      const events = records.filter(({ phase }) => phase.isApplyExtrinsic && phase.asApplyExtrinsic.eqn(index));
      return { blockHash: at, events };
    }
    if (block.header.number.toNumber() === 0) break;
    at = block.header.parentHash;
  }
  return null;
}

/// Sign and send `tx`, and settle it at finality.
///
/// Kreivo's collators build several blocks per relay-chain slot and reorganise short forks. The
/// first run against Kreivo 0.17 found both ways that goes wrong for a naive sender: a transaction
/// signed with a mortal era born at the best block became invalid when that block was retracted,
/// and the status subscription never reported it final. So:
///
/// - the era is born at the **finalized** head, which no reorganisation retracts;
/// - the outcome is read from the chain, not from the subscription: once the signer's nonce is
///   consumed at the finalized head, the extrinsic is found in the finalized chain and its events
///   decide;
/// - a transaction the pool drops (invalid, usurped, dropped) while the nonce is still unconsumed
///   at finality is signed again **with the same nonce**, so at most one of the signatures can
///   ever land. The same two proofs the settlement adapter needs before it releases.
async function send(tx, signer, label) {
  const api = currentApi;
  const nonce = (await api.rpc.system.accountNextIndex(signer.address)).toNumber();
  const hashes = [];
  for (let attempt = 1; attempt <= 3; attempt++) {
    const head = await api.rpc.chain.getFinalizedHead();
    const number = (await api.rpc.chain.getHeader(head)).number.toNumber();
    const signed = attempt === 1 ? tx : api.tx(tx.method);
    await signed.signAsync(signer, { nonce, blockHash: head, era: api.createType('ExtrinsicEra', { current: number, period: 64 }) });
    hashes.push(signed.hash.toHex());
    const outcome = await settle(api, signed, signer, nonce, hashes, label);
    if (outcome.result) {
      const failure = dispatchFailure(tx.registry, outcome.result.events);
      if (failure) fail(`${label}: dispatch failed: ${failure}`);
      log(`${label}: finalized in ${outcome.result.blockHash.toHex()}`);
      return outcome.result;
    }
    log(`${label}: the pool dropped it (${outcome.dropped}) and nonce ${nonce} is unconsumed at finality; signing it again with that nonce`);
  }
  fail(`${label}: dropped three times with nonce ${nonce} unconsumed`);
}

/// One signature of `send`: `{ result }` once final, or `{ dropped }` once the pool has dropped
/// it and the signer's nonce is still unconsumed at the finalized head.
async function settle(api, signed, signer, nonce, hashes, label) {
  let unsub;
  let dropped = null;
  let finalizedBy = null;
  signed
    .send(({ status, events }) => {
      if (status.isInBlock) log(`${label}: in block ${status.asInBlock.toHex()}`);
      else if (status.isRetracted) log(`${label}: its block ${status.asRetracted.toHex()} was retracted`);
      else if (status.isFinalized) finalizedBy = { events, blockHash: status.asFinalized };
      else if (status.isInvalid || status.isDropped || status.isUsurped || status.isFinalityTimeout) dropped = status.type;
    })
    .then((u) => {
      unsub = u;
    })
    .catch((error) => {
      dropped = `refused: ${error.message}`;
    });
  const deadline = Date.now() + Number(env('SEND_TIMEOUT_MS', '600000'));
  try {
    while (Date.now() < deadline) {
      await pause(3_000);
      if (finalizedBy) return { result: finalizedBy };
      const head = await api.rpc.chain.getFinalizedHead();
      const used = (await (await api.at(head)).query.system.account(signer.address)).nonce.toNumber();
      if (used > nonce) {
        for (const hash of hashes) {
          const found = await findFinalized(api, head, hash, 600);
          if (found) return { result: found };
        }
        fail(`${label}: nonce ${nonce} is consumed at finality, but by none of ${hashes.join(', ')}`);
      }
      if (dropped) return { dropped };
    }
    fail(`${label}: not final within ${env('SEND_TIMEOUT_MS', '600000')} ms`);
  } finally {
    unsub?.();
  }
}

function findEvent(events, section, method) {
  const found = events.find(({ event }) => event.section === section && event.method === method);
  return found?.event ?? fail(`no ${section}.${method} event`);
}

async function connect() {
  await cryptoWaitReady();
  const api = await ApiPromise.create({ provider: new WsProvider(WS), signedExtensions });
  currentApi = api;
  const keyring = new Keyring({ type: 'sr25519' });
  const accounts = {
    admin: keyring.addFromUri(env('ADMIN_SURI', '//Alice')),
    key: keyring.addFromUri(env('KEY_SURI', '//Bob')),
    depositor: keyring.addFromUri(env('DEPOSITOR_SURI', '//Alice')),
  };
  accounts.payee = env('PAYEE', keyring.addFromUri('//Dave').address);
  const bundle = JSON.parse(readFileSync(BUNDLE, 'utf8'));
  const abi = new Abi(bundle, api.registry.getChainProperties());
  return { api, accounts, bundle, abi };
}

function contractOf(api, abi) {
  const address = state.contract ?? fail('no contract yet: run `instantiate` first');
  return new ContractPromise(api, abi, address);
}

/// What `execute` keeps back from each call's frame beyond what the batch uses, as the contract's
/// constants add up (`src/types.rs`: `CALL_FRAME_COST`, `REFUSAL_RESERVE`, `BATCH_FINISH_RESERVE`).
/// A batch sent with exactly its dry run's gas could find its last call short of that reserve and
/// refuse it as `CallFailed(OutOfGas)`; this much more never does (F-026 NOTES, decision 23).
const EXECUTE_MARGIN = { refTime: 2_000_000_000n, proofSize: 96n * 1024n };

/// Dry-run a message, fail on a revert or a `LangError`, then send it with the gas it needs
/// (`execute`: plus `EXECUTE_MARGIN`).
async function callContract(api, contract, signer, message, args, label) {
  const gasLimit = api.registry.createType('WeightV2', api.consts.system.blockWeights.maxBlock);
  const query = await contract.query[message](signer.address, { gasLimit, storageDepositLimit: null }, ...args);
  if (query.result.isErr) fail(`${label}: dry run failed: ${query.result.asErr.toString()}`);
  if (query.result.asOk.flags.isRevert) fail(`${label}: dry run reverted: ${JSON.stringify(query.output?.toHuman())}`);
  log(`${label}: dry run returns`, JSON.stringify(query.output?.toHuman()));
  const required = query.gasRequired;
  const sent =
    message === 'execute'
      ? api.registry.createType('WeightV2', {
          refTime: BigInt(required.refTime.toString()) + EXECUTE_MARGIN.refTime,
          proofSize: BigInt(required.proofSize.toString()) + EXECUTE_MARGIN.proofSize,
        })
      : required;
  const tx = contract.tx[message]({ gasLimit: sent, storageDepositLimit: null }, ...args);
  // The dry run is returned too: its `gasRequired` and `storageDeposit` are what the suite records.
  return { ...(await send(tx, signer, label)), output: query.output, query };
}

/// The contract's child trie at block `hash`: `raw(key)` reads one cell by its unhashed key.
async function childTrie(api, address, hash) {
  const info = (await (await api.at(hash)).query.contracts.contractInfoOf(address)).unwrap();
  const child = u8aToHex(u8aConcat(stringToU8a(':child_storage:default:'), info.trieId));
  return {
    info,
    async raw(key) {
      const v = await api.rpc.childstate.getStorage(child, u8aToHex(key), hash);
      return v.isSome ? v.unwrap().toU8a(true) : null;
    },
  };
}

/// Run a read-only message at block `hash` (not the best block), and return its value's bytes:
/// the `Result<T, LangError>` it returns, less its `Ok` byte.
async function messageAt(api, abi, address, hash, message, args = []) {
  const data = abi.findMessage(message).toU8a(args);
  const r = await (await api.at(hash)).call.contractsApi.call(address, address, 0, null, null, data);
  if (r.result.isErr) fail(`${message} at ${hash.toHex?.() ?? hash}: ${r.result.asErr.toString()}`);
  const out = r.result.asOk.data.toU8a(true);
  if (r.result.asOk.flags.isRevert || out[0] !== 0) fail(`${message} at ${hash.toHex?.() ?? hash}: reverted (${u8aToHex(out)})`);
  return out.subarray(1);
}

/// `Head` (three u64s: last anchor, next position, pruned watermark) from its cell or its
/// message's value.
function decodeHead(bytes) {
  if (!bytes) return { lastAnchor: 0n, nextSeq: 0n, prunedTo: 0n };
  const v = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  return { lastAnchor: v.getBigUint64(0, true), nextSeq: v.getBigUint64(8, true), prunedTo: v.getBigUint64(16, true) };
}

/// The head and the chain's head at block `hash`, each read twice: by raw child-trie key (the
/// adapter's path) and by its message run at that block. Fails if the two disagree.
async function chainHeadAt(api, abi, address, hash) {
  const trie = await childTrie(api, address, hash);
  const headCell = await trie.raw(contractKey(ROOT_KEYS.head));
  const head = decodeHead(headCell);
  const viaHeadMessage = decodeHead(await messageAt(api, abi, address, hash, 'head'));
  if (JSON.stringify(viaHeadMessage, big) !== JSON.stringify(head, big)) fail(`head: storage ${JSON.stringify(head, big)}, query ${JSON.stringify(viaHeadMessage, big)}`);
  const cell = await trie.raw(contractKey(ROOT_KEYS.chain));
  const chain = decodeChainHead(cell, head.nextSeq);
  const viaQuery = decodeChainHead(await messageAt(api, abi, address, hash, 'chainHead'));
  if (JSON.stringify(viaQuery, big) !== JSON.stringify(chain, big)) fail(`chain head: storage ${JSON.stringify(chain, big)}, query ${JSON.stringify(viaQuery, big)}`);
  if (chain.position !== head.nextSeq) fail(`the chain's head is at ${chain.position}, the head's next position is ${head.nextSeq}`);
  return { head, chain, cellPresent: cell !== null, codeHash: trie.info.codeHash.toHex() };
}

const big = (_, v) => (typeof v === 'bigint' ? v.toString() : v);

/// An entry of the fold as plain values, from its event's decoded fields: `{ seq, at, kind,
/// operationRef, accountRef, from, to, asset, amount, reason, call, number, commitment }`.
function plainEntry(seq, at, kind, args) {
  const out = { seq, at, kind };
  const put = (name, f) => {
    const v = args[name];
    if (v !== undefined && v !== null) out[name] = f(v);
  };
  put('operationRef', (v) => v.toHex());
  put('accountRef', (v) => v.toHex());
  put('from', (v) => u8aToHex(v.toU8a()));
  put('to', (v) => (v.type === 'Account' ? v.value.toHex() : JSON.stringify(v.toJSON())));
  put('asset', (v) => JSON.stringify(v.toJSON()));
  put('amount', (v) => v.toString());
  put('reason', (v) => renderEnum(v));
  put('call', (v) => JSON.stringify(v.toJSON()));
  put('number', (v) => Number(v.toString()));
  put('commitment', (v) => v.toHex());
  return out;
}

/// `Refusal` (and nested enums) as `BoundExceeded` or `Kreivo(Assets(CannotTransfer))`.
function renderEnum(codec) {
  const isEnum = (c) => c && typeof c.type === 'string' && 'isBasic' in c;
  if (!isEnum(codec)) return codec?.toString();
  if (codec.isBasic) return codec.type;
  const inner = codec.value;
  // Not `isEmpty`: an enum whose own value is a unit variant reports itself empty too.
  if (isEnum(inner)) return `${codec.type}(${renderEnum(inner)})`;
  if (!inner || inner.toRawType?.() === 'Null') return codec.type;
  return `${codec.type}(${inner.toString()})`;
}

/// The fold of `address`'s entries up to finalized block `fin` (`{ hash, number }`), checked
/// against the chain's head there: `{ head, chain, fold }`. The fold is kept in the state between
/// steps (`state.folds[address]`), so each call folds only the blocks after the last; `instantiate`
/// starts it, and an upgrade or a caller with no fold kept starts it where the chain did
/// (`chain.mjs` `locateStart`).
async function foldAt(api, abi, address, fin) {
  const at = await chainHeadAt(api, abi, address, fin.hash);
  state.folds ??= {};
  let fold = state.folds[address] ? foldFromJson(state.folds[address]) : null;
  // A block before the fold kept is read from a fold of its own, and the one kept stays.
  const keep = !fold || fold.number <= fin.number;
  if (!fold || !keep) {
    const positionAfter = async (n) => (await chainHeadAt(api, abi, address, await api.rpc.chain.getBlockHash(n))).head.nextSeq;
    fold = await locateStart(api, abi, address, fin.number, at.chain, Number(env('BLOCKS', '256')), positionAfter);
    log(`fold of ${address}: ${keep ? 'no fold kept' : `#${fin.number} is before the fold kept`}; the chain starts after #${fold.number}`);
  }
  // A reset ends a chain: the fold up to the block before it is checked against the head there.
  const headAt = async (n) => (await chainHeadAt(api, abi, address, await api.rpc.chain.getBlockHash(n))).chain;
  await advance(api, abi, address, fold, fin.number, plainEntry, headAt);
  expectFoldEquals(fold, at.chain, `${address} at #${fin.number}`);
  if (keep) {
    state.folds[address] = foldToJson(fold);
    save();
  }
  return { ...at, fold };
}

/// Start the fold of `address` in block `number`, and fold that block: at its instantiation (a
/// chain from `(0, zeros)`), or, with `upgrade`, at an upgrade from code that kept no chain (a chain
/// from the head's `start`, after the `ContractCodeUpdated` event; the earlier code's events before
/// it in that block are of other shapes and not this chain's). Entries in that block after the
/// start are folded and checked against the head at the block. Returns that head.
async function startFold(api, abi, address, number, { upgrade = false } = {}) {
  const hash = await api.rpc.chain.getBlockHash(number);
  const { chain } = await chainHeadAt(api, abi, address, hash);
  let fold;
  if (upgrade) {
    const events = contractEventsOf(abi, address, await (await api.at(hash)).query.system.events());
    if (!events.some((e) => e.kind === 'ContractCodeUpdated')) fail(`#${number} does not change the code of ${address}`);
    const after = BigInt(events.filter((e) => e.kind in ENTRY_EVENTS).length);
    const start = startOf(chain) ?? chain.position - after;
    fold = newFold({ number: number - 1, position: start, skip: { block: number, until: 'ContractCodeUpdated' } });
  } else {
    fold = newFold({ number: number - 1, position: 0n });
  }
  await advance(api, abi, address, fold, number, plainEntry);
  expectFoldEquals(fold, chain, `${address} at #${number}, where its chain starts`);
  state.folds ??= {};
  state.folds[address] = foldToJson(fold);
  save();
  return chain;
}

async function read(api, contract, message, args = []) {
  const gasLimit = api.registry.createType('WeightV2', api.consts.system.blockWeights.maxBlock);
  const query = await contract.query[message](contract.address, { gasLimit, storageDepositLimit: null }, ...args);
  if (query.result.isErr) fail(`${message}: ${query.result.asErr.toString()}`);
  // `MessageResult<T>`: unwrap the ink! `LangError` layer.
  return query.output.asOk ?? query.output;
}

const steps = {
  async preflight({ api, bundle }) {
    const version = api.runtimeVersion;
    log(`runtime ${version.specName.toString()} spec ${version.specVersion.toNumber()}`);
    if (version.specName.toString() !== 'kreivo-parachain') fail('not a Kreivo runtime');
    for (const [call, index] of Object.entries(ASSETS_CALLS)) {
      const [pallet, callIndex] = api.tx.assets[call].callIndex;
      if (pallet !== ASSETS_PALLET || callIndex !== index) {
        fail(`assets.${call} is [${pallet}, ${callIndex}], the contract encodes [${ASSETS_PALLET}, ${index}]`);
      }
    }
    // The contract's own byte layout for `transfer`, built independently and compared with the
    // runtime's: `[13, 8] ++ asset ++ Id(target) ++ Compact(amount)`.
    const target = decodeAddress(encodeAddress(new Uint8Array(32).fill(0x22)));
    const assetType = api.tx.assets.transfer.meta.args[0].type.toString();
    const ours = u8aConcat(
      new Uint8Array([ASSETS_PALLET, ASSETS_CALLS.transfer]),
      api.createType(assetType, ASSET).toU8a(),
      new Uint8Array([0]),
      target,
      compactToU8a(1000),
    );
    const theirs = api.tx.assets.transfer(ASSET, { Id: target }, 1000).method.toU8a();
    if (!u8aEq(ours, theirs)) fail(`transfer encodes differently:\n ours   ${u8aToHex(ours)}\n theirs ${u8aToHex(theirs)}`);
    const code = hexToU8a(bundle.source.wasm);
    const maxCodeLen = api.consts.contracts.maxCodeLen.toNumber();
    if (code.length > maxCodeLen) fail(`code is ${code.length} bytes, MaxCodeLen is ${maxCodeLen}`);
    if (!api.tx.contractsStore) fail('no ContractsStore pallet');
    log(`ok: Assets at ${ASSETS_PALLET}, call shapes match, code ${code.length} bytes of ${maxCodeLen}`);
  },

  async community({ api, accounts }) {
    const { admin } = accounts;
    if ((await api.query.communities.info(COMMUNITY_ID)).isSome) {
      // ADMIN must be the community's admin: `EnsureCommunity` accepts a signed origin that is.
      log(`community ${COMMUNITY_ID} exists; using it`);
    } else {
      const tx = api.tx.communitiesManager.register(COMMUNITY_ID, COMMUNITY_NAME, { Id: admin.address }, null, null);
      await send(tx, admin, 'communitiesManager.register');
    }
    const account = communityAccount(COMMUNITY_ID);
    // The community account pays the contract's existential deposit and storage deposit.
    const fund = api.tx.balances.transferKeepAlive(account, env('COMMUNITY_FUNDING', '100000000000000'));
    await send(fund, admin, 'fund the community account');
    state.community = { id: COMMUNITY_ID, account };
    save();
    log(`community ${COMMUNITY_ID}, account ${account}`);
  },

  async publish({ api, accounts, bundle }) {
    const tx = api.tx.contractsStore.publish(bundle.source.wasm, null, null);
    const { events } = await send(tx, accounts.admin, 'contractsStore.publish');
    state.appId = findEvent(events, 'contractsStore', 'AppPublished').data[0].toNumber();
    save();
    log(`app ${state.appId}`);
  },

  async license({ api, accounts }) {
    const appId = state.appId ?? fail('no app yet: run `publish` first');
    const { events } = await send(api.tx.contractsStore.requestLicense(appId), accounts.admin, 'contractsStore.requestLicense');
    state.licenseId = findEvent(events, 'contractsStore', 'AppLicenseEmitted').data[1].toString();
    save();
    log(`license ${state.licenseId}`);
  },

  async instantiate({ api, accounts, abi }) {
    const { appId, licenseId } = state;
    if (appId === undefined || licenseId === undefined) fail('run `publish` and `license` first');
    // `toU8a` returns the call data already length-prefixed, which is how the API reads a
    // `Uint8Array` given for `Bytes`. The salt is raw, so it goes as hex: as a `Uint8Array` the API
    // would read its first byte as a length prefix (found by the first zombienet run).
    const data = abi.findConstructor('new').toU8a([accounts.admin.address, [accounts.key.address], null]);
    const salt = u8aToHex(stringToU8a(`aledgent-${Date.now()}`));
    const tx = api.tx.contractsStore.instantiate(appId, licenseId, 0, data, salt);
    const { blockHash } = await send(tx, accounts.admin, 'contractsStore.instantiate');
    const address = (await api.query.contractsStore.contractAccount([appId, licenseId])).unwrap().toString();
    state.contract = address;
    save();
    // The chain starts empty at instantiation, `(0, zeros, 0)`: the fold starts at its block.
    const number = (await api.rpc.chain.getHeader(blockHash)).number.toNumber();
    if (abi.messages.some((m) => m.method === 'chainHead')) await startFold(api, abi, address, number);
    log(`contract ${address}, instantiated at #${number}`);
  },

  async bound({ api, accounts, abi }) {
    const contract = contractOf(api, abi);
    const bound = { amount: env('BOUND', '1000000'), period: Number(env('BOUND_PERIOD', '100')) };
    await callContract(api, contract, accounts.admin, 'setWithdrawalBound', [ASSET, bound], 'set_withdrawal_bound');
  },

  async deposit({ api, accounts, abi }) {
    const contract = contractOf(api, abi);
    const amount = env('DEPOSIT', '500000');
    // No approval: the contract pulls from its caller through Kreivo's assets API, which keeps
    // DEPOSITOR's asset account at or above the asset's minimum balance.
    const accountRef = u8aToHex(blake2AsU8a(`account-ref-${Date.now()}`));
    const { output } = await callContract(api, contract, accounts.depositor, 'deposit', [accountRef, ASSET, amount], 'deposit');
    log(`deposit recorded at ${JSON.stringify(output.toHuman())}, account reference ${accountRef}`);
  },

  async execute({ api, accounts, abi }) {
    const contract = contractOf(api, abi);
    const head = (await read(api, contract, 'head')).toJSON();
    const ref = (label) => u8aToHex(blake2AsU8a(`${label}-${Date.now()}`));
    const envelopes = [
      { operationRef: ref('withdraw'), call: { Withdraw: { to: { Account: accounts.payee }, asset: ASSET, amount: env('WITHDRAW', '1000') } } },
      // Past any bound the `bound` step set: refused, recorded, and the rest still applies.
      { operationRef: ref('refused'), call: { Withdraw: { to: { Account: accounts.payee }, asset: ASSET, amount: '340282366920938463463374607431768211455' } } },
      { operationRef: ref('anchor'), call: { Anchor: { number: Number(head.lastAnchor) + 1, commitment: u8aToHex(blake2AsU8a('commitment')) } } },
    ];
    const { output } = await callContract(api, contract, accounts.key, 'execute', [envelopes], 'execute');
    log('outcomes', JSON.stringify(output.toHuman()));
  },

  async read({ api, abi }) {
    const contract = contractOf(api, abi);
    const hash = await api.rpc.chain.getFinalizedHead();
    const number = (await api.rpc.chain.getHeader(hash)).number.toNumber();
    // The adapter's path: the head and the chain's head by raw child-trie key at a finalized
    // block, each against its message run at that block; then the events folded to it.
    const { head, chain, cellPresent, fold } = await foldAt(api, abi, contract.address.toString(), { hash, number });
    log(`head ${JSON.stringify(head, big)}; chain head ${JSON.stringify(chain, big)} (${cellPresent ? 'stored' : 'no cell: read as (next_seq, zeros, next_seq)'}); storage and query agree (finalized #${number} ${hash.toHex()})`);
    for (const e of fold.entries.slice(-Number(env('ENTRIES', '8')))) log(`entry[${e.seq}] #${e.at} ${JSON.stringify(e)}`);
    log(`the events fold to the chain's head: ${fold.entries.length} entries since the chain started at ${fold.start}, ${fold.resets} reset(s) folded`);
  },

  async events({ api, abi }) {
    const address = state.contract ?? fail('no contract yet');
    const finalized = await api.rpc.chain.getFinalizedHead();
    let hash = finalized;
    for (let i = 0; i < Number(env('BLOCKS', '20')); i++) {
      const at = await api.at(hash);
      for (const record of await at.query.system.events()) {
        const { event } = record;
        if (event.section === 'contracts' && event.method === 'ContractEmitted' && event.data[0].toString() === address) {
          const decoded = abi.decodeEvent(record);
          log(`${hash.toHex()} ${decoded.event.identifier}`, JSON.stringify(decoded.args.map((a) => a.toHuman())));
        }
      }
      hash = (await api.rpc.chain.getHeader(hash)).parentHash;
    }
  },
};

steps.all = async (ctx) => {
  for (const step of ['preflight', 'community', 'publish', 'license', 'instantiate', 'bound', 'deposit', 'execute', 'read', 'events']) {
    log(`--- ${step}`);
    await steps[step](ctx);
  }
};

// What `e2e.mjs` (the zombienet suite) reuses: the same connection, deployment steps and reads, so
// the suite deploys and drives exactly as this script does.
export {
  ASSET,
  EXECUTE_MARGIN,
  ROOT_KEYS,
  big,
  callContract,
  chainHeadAt,
  childTrie,
  decodeHead,
  foldAt,
  messageAt,
  plainEntry,
  renderEnum,
  startFold,
  communityAccount,
  connect,
  contractKey,
  contractOf,
  fail,
  findEvent,
  log,
  read,
  save,
  send,
  signedExtensions,
  state,
  steps,
  useApi,
};

// Run as a command only when invoked directly, not when imported.
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const step = process.argv[2];
  if (!steps[step]) {
    console.error(`usage: node drive.mjs <${Object.keys(steps).join('|')}>`);
    process.exit(2);
  }
  const ctx = await connect();
  try {
    await steps[step](ctx);
  } finally {
    await ctx.api.disconnect();
  }
}
