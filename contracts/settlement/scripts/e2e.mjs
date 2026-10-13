#!/usr/bin/env node
// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
// The zombienet end-to-end suite for the settlement contract (T-026-04, issue #75) and the first
// fault-injection cases (T-027-06, issue #76). `.github/workflows/e2e-zombienet.yml` runs it against
// the network `e2e/network` spawns; `e2e/README.md` says how to run it by hand.
//
//   node e2e.mjs <phase>
//
// Phases, in the order CI runs them:
//
//   deploy   Deploy through the contracts store exactly as `drive.mjs` does (preflight, community,
//            publish, license, instantiate, bounds), from a test community of its own.
//   drive    The script driver: a deposit with an account reference, a withdrawal, a withdrawal the
//            bound refuses and one Kreivo refuses, an anchor, a full batch of withdrawals to accounts, the largest batch of withdrawals to another
//            deployment that fits, a
//            deposit of a non-sufficient asset, and a deposit Kreivo refuses. Records the weight
//            and storage deposit of a deposit and of a full batch. Writes what it did to
//            `expect.json`; it asserts nothing about outcomes itself.
//   check    Driver-independent: reads the inbox by raw child-trie storage at the finalized head,
//            the adapter's path, and checks every expectation in `expect.json` (one outcome per
//            reference, each with the right kind, amount and reason), that the inbox is gapless,
//            that each position equals the contract's holding, and that events agree with storage.
//            The aledgent driver (e2e/aledgent-driver) writes the same `expect.json`, so switching
//            drivers changes nothing here.
//   faults   Fault injection with the script driver: drop the connection mid-submit, kill the
//            submitter between signing and submitting, prune node state, and land a deposit in the
//            same gap as a withdrawal. Starving a light client of peers is the aledgent driver's
//            (`e2e/drivers/aledgent.sh faults`, which runs every case through the adapter).
//   summary  Render `report.json` as Markdown (`summary.md`), for the job summary.
//   all      deploy, drive, check, faults, summary.
//
// Configuration, from the environment (drive.mjs's variables apply too: KREIVO_WS, COMMUNITY_ID,
// BUNDLE, STATE, the *_SURI keys):
//
//   KREIVO_PRUNED_WS   the pruned Kreivo full node (T-027-06, "prune node state")
//   ZOMBIENET_BASE_DIR zombienet's directory for the network (for the record only)
//   PRUNED_STATE_BLOCKS how many blocks of state the pruned node keeps (default 16)
//   E2E_OUT            where report.json, expect.json and summary.md go (default ./.e2e)
//
// Every assertion that reads the chain reads it at a finalized block, and polls to a deadline
// rather than reading once: a submission's finality and the reader's view are not one clock.

import { ApiPromise, Keyring, WsProvider } from '@polkadot/api';
import { bnToU8a, hexToU8a, stringToU8a, u8aConcat, u8aEq, u8aToHex } from '@polkadot/util';
import { blake2AsU8a, decodeAddress } from '@polkadot/util-crypto';
import { spawnSync } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

import * as drive from './drive.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const env = (name, fallback) => process.env[name] ?? fallback;

const OUT = resolve(env('E2E_OUT', resolve(here, '.e2e')));
const REPORT = resolve(OUT, 'report.json');
const EXPECT = resolve(OUT, 'expect.json');
const PRUNED_WS = env('KREIVO_PRUNED_WS', '');
const PRUNED_STATE_BLOCKS = Number(env('PRUNED_STATE_BLOCKS', '16'));

const ASSET = drive.ASSET; // Here(1): sufficient, minimum balance 1 (e2e/network genesis)
const ASSET2 = JSON.parse(env('ASSET2', '{"Here":2}')); // non-sufficient, minimum balance 1
const DEPOSIT = 500_000n;
const WITHDRAW = 1_000n;
const BOUND = 1_000_000n;
const BOUND_PERIOD = 1_000;
const MAX_CALLS = 32; // `types::MAX_CALLS`
const U128_MAX = (1n << 128n) - 1n;

const { log, fail } = drive;
mkdirSync(OUT, { recursive: true });

// ---------------------------------------------------------------------------------------------
// Report and expectations
// ---------------------------------------------------------------------------------------------

const loadJson = (path, fallback) => (existsSync(path) ? JSON.parse(readFileSync(path, 'utf8')) : fallback);
const report = loadJson(REPORT, { scenarios: [], faults: [], measurements: {}, findings: {} });
const expectations = loadJson(EXPECT, []);
const saveReport = () => writeFileSync(REPORT, JSON.stringify(report, (_, v) => (typeof v === 'bigint' ? v.toString() : v), 2));
const saveExpect = () => writeFileSync(EXPECT, JSON.stringify(expectations, (_, v) => (typeof v === 'bigint' ? v.toString() : v), 2));

function record(list, name, status, detail) {
  const entry = { name, status, detail };
  const at = report[list].findIndex((e) => e.name === name);
  if (at >= 0) report[list][at] = entry;
  else report[list].push(entry);
  saveReport();
  log(`[${status}] ${name}${detail ? `: ${typeof detail === 'string' ? detail : JSON.stringify(detail)}` : ''}`);
}

/// Run one case; a thrown error is the case's failure, recorded, and the phase carries on so that
/// one broken case does not hide the others. The phase fails at the end if any case failed.
async function runCase(list, name, fn) {
  try {
    const detail = await fn();
    record(list, name, 'pass', detail);
    return true;
  } catch (error) {
    record(list, name, 'fail', String(error?.stack ?? error));
    return false;
  }
}

// ---------------------------------------------------------------------------------------------
// Reading the contract at a finalized block, as the adapter does
// ---------------------------------------------------------------------------------------------

const ref = () => u8aToHex(randomBytes(32));
/// An account as its public key in hex: entries and configuration print addresses with different
/// SS58 prefixes.
const pk = (address) => u8aToHex(decodeAddress(address));
const big = (codec) => BigInt(codec.toString());
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function finalized(api) {
  const hash = await api.rpc.chain.getFinalizedHead();
  const number = (await api.rpc.chain.getHeader(hash)).number.toNumber();
  return { hash, number };
}

/// Poll `fn` until it returns a truthy value or the deadline passes.
async function until(label, fn, timeoutMs = 180_000, everyMs = 3_000) {
  const deadline = Date.now() + timeoutMs;
  let last;
  for (;;) {
    last = await fn();
    if (last) return last;
    if (Date.now() > deadline) fail(`${label}: not reached within ${timeoutMs / 1000}s`);
    await sleep(everyMs);
  }
}

/// Decode raw storage bytes with the type a read message returns: `Result<T, LangError>`, and
/// `Option<T>` inside it for `inbox_entry` and `latest_anchor`. Prefixing `Ok` (and `Some`) lets the
/// ABI's own codec decode exactly what the contract stored.
function decodeAs(abi, message, raw, optional) {
  const m = abi.findMessage(message);
  const prefix = optional ? [0x00, 0x01] : [0x00];
  const value = abi.registry.createTypeUnsafe(m.returnType.lookupName || m.returnType.type, [u8aConcat(new Uint8Array(prefix), raw)], {
    isPedantic: true,
  });
  const ok = value.asOk ?? value;
  return optional ? ok.unwrap() : ok;
}

/// The contract's storage at one block, by child-trie key: `head`, `inbox[seq]`, `latest_anchor`,
/// `positions[asset]`. This is the adapter's path (F-026 NOTES, "How the adapter reads outcomes at
/// finality").
async function view(api, abi, address, hash) {
  const at = await api.at(hash);
  const info = (await at.query.contracts.contractInfoOf(address)).unwrap();
  const child = u8aToHex(u8aConcat(stringToU8a(':child_storage:default:'), info.trieId));
  const raw = async (key) => {
    const v = await api.rpc.childstate.getStorage(child, u8aToHex(key), hash);
    return v.isSome ? v.unwrap().toU8a(true) : null;
  };
  const K = drive.ROOT_KEYS;
  const u64 = (n) => bnToU8a(n, { bitLength: 64, isLe: true });
  return {
    hash,
    async head() {
      const bytes = await raw(drive.contractKey(K.head));
      if (!bytes) return { lastAnchor: 0, nextSeq: 0, prunedTo: 0 };
      const h = decodeAs(abi, 'head', bytes, false);
      return { lastAnchor: Number(h.lastAnchor.toString()), nextSeq: Number(h.nextSeq.toString()), prunedTo: Number(h.prunedTo.toString()) };
    },
    async entryBytes(seq) {
      return raw(drive.contractKey(K.inbox, u64(seq)));
    },
    async entry(seq) {
      const bytes = await raw(drive.contractKey(K.inbox, u64(seq)));
      return bytes ? decodeAs(abi, 'inboxEntry', bytes, true) : null;
    },
    async latestAnchor() {
      const bytes = await raw(drive.contractKey(K.latestAnchor));
      return bytes ? decodeAs(abi, 'latestAnchor', bytes, true) : null;
    },
    async position(asset) {
      const encoded = api.createType(api.tx.assets.transfer.meta.args[0].type.toString(), asset).toU8a();
      const bytes = await raw(drive.contractKey(K.positions, encoded));
      return bytes ? big(api.createType('u128', bytes)) : 0n;
    },
  };
}

async function assetBalance(api, hash, asset, who) {
  const account = await (await api.at(hash)).query.assets.account(asset, who);
  return account.isSome ? big(account.unwrap().balance) : 0n;
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

/// `{"here":1}` (a codec's JSON) against `{"Here":1}` (the configuration's).
const sameAsset = (json, asset) => json.toLowerCase() === JSON.stringify(asset).toLowerCase();

/// An inbox entry as plain values: `{ seq, at, kind, operationRef, accountRef, from, to, asset,
/// amount, reason, number, commitment }`.
function plainEntry(seq, entry) {
  const ev = entry.event;
  const body = ev.value;
  const get = (name) => (body.get ? body.get(name) : body[name]);
  const out = { seq, at: Number(entry.at.toString()), kind: ev.type };
  const opt = (name, f) => {
    const v = get(name);
    if (v !== undefined && v !== null) out[name] = f(v);
  };
  opt('operationRef', (v) => v.toHex());
  opt('accountRef', (v) => v.toHex());
  opt('from', (v) => v.toHex());
  opt('to', (v) => (v.type === 'Account' ? v.value.toHex() : JSON.stringify(v.toJSON())));
  opt('asset', (v) => JSON.stringify(v.toJSON()));
  opt('amount', (v) => big(v));
  opt('reason', (v) => renderEnum(v));
  opt('number', (v) => Number(v.toString()));
  opt('commitment', (v) => v.toHex());
  return out;
}

/// Every unpruned inbox entry at `hash`, in order, failing on a gap (`REQ-SL-9`).
async function inbox(v) {
  const head = await v.head();
  const entries = [];
  for (let seq = head.prunedTo; seq < head.nextSeq; seq++) {
    const e = await v.entry(seq);
    if (!e) fail(`inbox[${seq}] is missing below next_seq ${head.nextSeq}: the inbox has a gap`);
    entries.push(plainEntry(seq, e));
  }
  return { head, entries };
}

// ---------------------------------------------------------------------------------------------
// Submitting
// ---------------------------------------------------------------------------------------------

const gasMax = (api) => api.registry.createType('WeightV2', api.consts.system.blockWeights.maxBlock);

/// A signed-ready `execute(envelopes)` extrinsic, with the gas its dry run needs.
async function executeTx(ctx, envelopes) {
  const { api, accounts, abi } = ctx;
  const contract = drive.contractOf(api, abi);
  const q = await contract.query.execute(accounts.key.address, { gasLimit: gasMax(api), storageDepositLimit: null }, envelopes);
  if (q.result.isErr) fail(`execute dry run failed: ${q.result.asErr.toString()}`);
  if (q.result.asOk.flags.isRevert) fail(`execute dry run reverted: ${JSON.stringify(q.output?.toHuman())}`);
  // Half again the dry run's gas: the dry run prices the batch against the state it ran on, and a
  // deposit landing first in the same block can turn a refusal into a transfer, which costs more
  // (found by the first run where both raced: `Contracts.OutOfGas`). Recorded measurements use
  // the dry run's own figure.
  // At least the reserve `execute` keeps back from its frames (drive.mjs `EXECUTE_MARGIN`), which
  // half again covers for any batch but the smallest. Never more than a normal extrinsic may weigh
  // with the call's own weight on top (about 11 KB of proof), or the pool refuses the transaction
  // ("would exhaust the block limits", found when a batch of 32 was capped at the maximum itself).
  const max = api.consts.system.blockWeights.perClass.normal.maxExtrinsic.unwrap();
  const headroom = { refTime: 50_000_000_000n, proofSize: 64n * 1024n };
  const margin = (w, part) => {
    const need = BigInt(w.toString());
    const half = (need * 3n) / 2n;
    const reserve = need + drive.EXECUTE_MARGIN[part];
    const m = half > reserve ? half : reserve;
    const c = BigInt(max[part].toString()) - headroom[part];
    return m < c ? m : c;
  };
  const gasLimit = api.registry.createType('WeightV2', { refTime: margin(q.gasRequired.refTime, 'refTime'), proofSize: margin(q.gasRequired.proofSize, 'proofSize') });
  return { tx: contract.tx.execute({ gasLimit, storageDepositLimit: null }, envelopes), query: q };
}

const withdrawal = (operationRef, to, asset, amount) => ({ operationRef, call: { Withdraw: { to: { Account: to }, asset, amount: amount.toString() } } });
const toDeployment = (operationRef, contract, accountRef, asset, amount) => ({
  operationRef,
  call: { Withdraw: { to: { Deployment: { contract, accountRef } }, asset, amount: amount.toString() } },
});

/// A dry run of `execute(envelopes)` with `gasLimit` (default: the block's), failing on a revert.
/// Returns what it needs and each call's outcome, rendered (`Applied`, `Refused(BoundExceeded)`).
async function dryRunExecute(ctx, envelopes, gasLimit = gasMax(ctx.api)) {
  const { api, accounts, abi } = ctx;
  const contract = drive.contractOf(api, abi);
  const q = await contract.query.execute(accounts.key.address, { gasLimit, storageDepositLimit: null }, envelopes);
  if (q.result.isErr) return { failed: q.result.asErr.toString(), gasRequired: q.gasRequired };
  if (q.result.asOk.flags.isRevert) fail(`execute dry run reverted: ${JSON.stringify(q.output?.toHuman())}`);
  const result = q.output.asOk ?? q.output;
  if (result.isErr) fail(`execute refused the batch: ${JSON.stringify(result.asErr.toHuman())}`);
  const outcomes = result.asOk.map((o) => (o.isApplied ? 'Applied' : `Refused(${renderEnum(o.asRefused.reason)})`));
  return {
    refTime: big(q.gasRequired.refTime),
    proofSize: big(q.gasRequired.proofSize),
    storageDeposit: q.storageDeposit.isCharge ? big(q.storageDeposit.asCharge) : -big(q.storageDeposit.asRefund),
    outcomes,
  };
}

/// Whether the deployed code runs each call in a frame of its own (`apply`, F-026 decision 23).
const isolatesCalls = (abi) => abi.messages.some((m) => m.method === 'apply');

/// A second deployment of the same code, through the contracts store (another licence of the same
/// app), to receive withdrawals to a `Deployment`. Its admin and key are the suite's. The suite's own
/// contract stays the one `drive.state` names.
async function peerDeployment(ctx) {
  if (report.deploy?.peer) return report.deploy.peer;
  const saved = { licenseId: drive.state.licenseId, contract: drive.state.contract };
  try {
    await drive.steps.license(ctx);
    await drive.steps.instantiate(ctx);
    report.deploy = { ...(report.deploy ?? {}), peer: drive.state.contract, peerLicenseId: drive.state.licenseId };
    saveReport();
    return report.deploy.peer;
  } finally {
    Object.assign(drive.state, saved);
    drive.save();
  }
}
const anchorCall = (operationRef, number, commitment) => ({ operationRef, call: { Anchor: { number, commitment } } });

/// What a submitted extrinsic cost: its dry run and the events of its inclusion.
function costOf(api, query, events) {
  const out = {
    dryRun: {
      refTime: big(query.gasRequired.refTime),
      proofSize: big(query.gasRequired.proofSize),
      storageDeposit: query.storageDeposit.isCharge ? `charge ${query.storageDeposit.asCharge.toString()}` : `refund ${query.storageDeposit.asRefund.toString()}`,
    },
  };
  for (const { event } of events) {
    if (event.section === 'system' && event.method === 'ExtrinsicSuccess') {
      const info = event.data[0];
      out.weight = { refTime: big(info.weight.refTime), proofSize: big(info.weight.proofSize) };
    }
    if (event.section === 'transactionPayment' && event.method === 'TransactionFeePaid') out.fee = big(event.data[1]);
    if (event.section === 'contracts' && event.method === 'StorageDepositTransferredAndHeld') {
      out.storageDepositHeld = (out.storageDepositHeld ?? 0n) + big(event.data[2]);
    }
    if (event.section === 'contracts' && event.method === 'StorageDepositTransferredAndReleased') {
      out.storageDepositReleased = (out.storageDepositReleased ?? 0n) + big(event.data[2]);
    }
  }
  const max = api.consts.system.blockWeights.perClass.normal.maxExtrinsic;
  if (max.isSome) {
    const m = max.unwrap();
    out.maxExtrinsic = { refTime: big(m.refTime), proofSize: big(m.proofSize) };
    // Percentages in tenths, with integers only (INV-7 has no floating point in the repository).
    out.shareOfMaxExtrinsic = {
      refTimePermille: Number((out.dryRun.refTime * 1000n) / out.maxExtrinsic.refTime),
      proofSizePermille: Number((out.dryRun.proofSize * 1000n) / out.maxExtrinsic.proofSize),
    };
  }
  return out;
}

async function connectPruned() {
  if (!PRUNED_WS) return null;
  return ApiPromise.create({ provider: new WsProvider(PRUNED_WS), signedExtensions: drive.signedExtensions, noInitWarn: true });
}

// ---------------------------------------------------------------------------------------------
// Phases
// ---------------------------------------------------------------------------------------------

const phases = {
  async deploy(ctx) {
    const { api } = ctx;
    report.network = {
      runtime: `${api.runtimeVersion.specName.toString()} ${api.runtimeVersion.specVersion.toNumber()}`,
      kreivoRelease: env('KREIVO_RELEASE_TAG', ''),
      kreivoWasmSha256: env('KREIVO_WASM_SHA256', ''),
      polkadotSdk: env('PSDK_RELEASE', ''),
      communityId: Number(env('COMMUNITY_ID', '1')),
      // Everything the adapter would need to know about the chain's rules.
      maxExtrinsicNormal: api.consts.system.blockWeights.perClass.normal.maxExtrinsic.toHuman(),
      contractsDepositPerItem: api.consts.contracts.depositPerItem.toString(),
      contractsDepositPerByte: api.consts.contracts.depositPerByte.toString(),
      maxCodeLen: api.consts.contracts.maxCodeLen.toNumber(),
    };
    saveReport();
    // A mainnet deployment uses a community that already exists. A fresh
    // network has none, so it is registered first, by the same step taking its other branch; then
    // the deployment runs exactly the steps, in the order and with the code, that the mainnet
    // deployment runs (contracts/settlement/README.md): `community` finds the community and funds
    // it, then publish, license, instantiate and bound.
    const communityExisted = (await api.query.communities.info(report.network.communityId)).isSome;
    if (!communityExisted) {
      log('--- setup: register the test community (mainnet already has its own)');
      await drive.steps.community(ctx);
    }
    const mainnetSteps = ['preflight', 'community', 'publish', 'license', 'instantiate', 'bound'];
    for (const step of mainnetSteps) {
      log(`--- ${step} (as on mainnet)`);
      await drive.steps[step](ctx);
    }
    // Beyond the mainnet steps: a bound for the non-sufficient asset the suite also deposits.
    const contract = drive.contractOf(api, ctx.abi);
    await drive.callContract(api, contract, ctx.accounts.admin, 'setWithdrawalBound', [ASSET2, { amount: BOUND.toString(), period: BOUND_PERIOD }], `set_withdrawal_bound ${JSON.stringify(ASSET2)}`);
    record('scenarios', 'deploy through the contracts store with the mainnet steps', 'pass', `${communityExisted ? 'community existed' : 'community registered first'}; then ${mainnetSteps.join(', ')}`);
    report.deploy = {
      appId: drive.state.appId,
      licenseId: drive.state.licenseId,
      contract: drive.state.contract,
      community: drive.state.community,
      codeBytes: hexToU8a(ctx.bundle.source.wasm).length,
      codeHash: ctx.bundle.source.hash,
    };
    saveReport();
  },

  async drive(ctx) {
    const { api, accounts, abi } = ctx;
    const contract = drive.contractOf(api, abi);
    expectations.length = 0;
    let ok = true;

    // The operator's script first, as it is run against mainnet: its deposit, its batch (a
    // withdrawal, one the bound refuses, an anchor), and its own storage-against-query and events
    // reads. The `check` phase then covers its entries with the global checks.
    ok &= await runCase('scenarios', 'drive.mjs deposit, execute, read, events (the operator steps)', async () => {
      for (const step of ['deposit', 'execute', 'read', 'events']) {
        log(`--- drive.mjs ${step}`);
        await drive.steps[step](ctx);
      }
    });

    ok &= await runCase('scenarios', 'deposit with an account reference', async () => {
      const accountRef = ref();
      const { query, events, blockHash } = await drive.callContract(api, contract, accounts.depositor, 'deposit', [accountRef, ASSET, DEPOSIT.toString()], 'deposit');
      report.measurements.deposit = costOf(api, query, events);
      expectations.push({ kind: 'Deposited', accountRef, from: accounts.depositor.address, asset: ASSET, amount: DEPOSIT, finalizedIn: blockHash.toHex() });
      saveExpect();
      return `finalized in ${blockHash.toHex()}; cost ${JSON.stringify(report.measurements.deposit, (_, v) => (typeof v === 'bigint' ? v.toString() : v))}`;
    });

    ok &= await runCase('scenarios', 'withdrawal, refusal by the bound and anchor in one batch', async () => {
      const head = await (await view(api, abi, contract.address, (await finalized(api)).hash)).head();
      const w = ref();
      const r = ref();
      const a = ref();
      const commitment = u8aToHex(randomBytes(32));
      const number = head.lastAnchor + 1;
      const { tx } = await executeTx(ctx, [withdrawal(w, accounts.payee, ASSET, WITHDRAW), withdrawal(r, accounts.payee, ASSET, U128_MAX), anchorCall(a, number, commitment)]);
      const { blockHash } = await drive.send(tx, accounts.key, 'execute');
      expectations.push(
        { kind: 'Withdrawn', operationRef: w, to: accounts.payee, asset: ASSET, amount: WITHDRAW },
        { kind: 'Refused', operationRef: r, reason: 'BoundExceeded' },
        { kind: 'Anchored', operationRef: a, number, commitment },
      );
      saveExpect();
      return `finalized in ${blockHash.toHex()}`;
    });

    ok &= await runCase('scenarios', 'withdrawal Kreivo refuses (frozen account)', async () => {
      // Alice owns the asset (e2e/network genesis) and so can freeze the contract's account in it.
      await drive.send(api.tx.assets.freeze(ASSET, { Id: contract.address.toString() }), accounts.admin, 'assets.freeze');
      const k = ref();
      try {
        const { tx } = await executeTx(ctx, [withdrawal(k, accounts.payee, ASSET, WITHDRAW)]);
        await drive.send(tx, accounts.key, 'execute (frozen)');
      } finally {
        await drive.send(api.tx.assets.thaw(ASSET, { Id: contract.address.toString() }), accounts.admin, 'assets.thaw');
      }
      expectations.push({ kind: 'Refused', operationRef: k, reason: 'Kreivo(Assets(CannotTransfer))' });
      saveExpect();
    });

    ok &= await runCase('scenarios', `full batch of ${MAX_CALLS} withdrawals`, async () => {
      const refs = Array.from({ length: MAX_CALLS }, ref);
      const { tx, query } = await executeTx(ctx, refs.map((r) => withdrawal(r, accounts.payee, ASSET, WITHDRAW)));
      const { events, blockHash } = await drive.send(tx, accounts.key, `execute (${MAX_CALLS} withdrawals)`);
      report.measurements.executeFullBatch = costOf(api, query, events);
      for (const r of refs) expectations.push({ kind: 'Withdrawn', operationRef: r, to: accounts.payee, asset: ASSET, amount: WITHDRAW });
      saveExpect();
      return `finalized in ${blockHash.toHex()}`;
    });

    // What one more call costs, by destination: dry runs of a batch of one and of `MAX_CALLS`, at
    // the block's gas. The difference over `MAX_CALLS - 1` is the cost of a call; the rest is the
    // batch's own. Nothing is submitted.
    ok &= await runCase('scenarios', 'cost of one call, by destination (dry runs)', async () => {
      const peer = await peerDeployment(ctx);
      const kinds = {
        account: (r) => withdrawal(r, accounts.payee, ASSET, WITHDRAW),
        deployment: (r) => toDeployment(r, peer, ref(), ASSET, WITHDRAW),
      };
      const out = {};
      report.measurements.perCall = out;
      // `MAX_CALLS` withdrawals to accounts; six to another deployment (32 of those exceed even the
      // block's own proof size in a dry run).
      for (const [kind, make, n] of [['account', kinds.account, MAX_CALLS], ['deployment', kinds.deployment, 6]]) {
        const one = await dryRunExecute(ctx, [make(ref())]);
        const full = await dryRunExecute(ctx, Array.from({ length: n }, () => make(ref())));
        for (const [calls, d] of [[1, one], [n, full]]) {
          if (d.failed || d.outcomes.some((o) => o !== 'Applied')) fail(`${kind}, ${calls} calls: ${d.failed ?? d.outcomes.join(', ')}`);
        }
        const per = (f) => (full[f] - one[f]) / BigInt(n - 1);
        const perCall = { refTime: per('refTime'), proofSize: per('proofSize'), storageDeposit: per('storageDeposit') };
        out[kind] = {
          one: { refTime: one.refTime, proofSize: one.proofSize, storageDeposit: one.storageDeposit },
          full: { calls: n, refTime: full.refTime, proofSize: full.proofSize, storageDeposit: full.storageDeposit },
          perCall,
          batch: { refTime: one.refTime - perCall.refTime, proofSize: one.proofSize - perCall.proofSize },
        };
        saveReport();
      }
      return JSON.stringify(out, (_, v) => (typeof v === 'bigint' ? v.toString() : v));
    });

    // The worst case, sized as the adapter sizes a batch (`F-027`, T-027-09): the most withdrawals
    // to another deployment, each in its own frame, whose dry run with half again on top fits a
    // normal extrinsic less the call's own weight. `MAX_CALLS` is only the contract's bound; this
    // is what bounds a batch of the costliest kind (F-026 decision 23, "The price").
    ok &= await runCase('scenarios', 'largest batch of withdrawals to another deployment, sized by proof size', async () => {
      const per = report.measurements.perCall?.deployment ?? fail('no measurement of a withdrawal to another deployment');
      const max = BigInt(api.consts.system.blockWeights.perClass.normal.maxExtrinsic.unwrap().proofSize.toString());
      const room = ((max - 64n * 1024n) * 2n) / 3n;
      const n = Number((room - per.batch.proofSize) / per.perCall.proofSize);
      if (n < 1) fail(`not even one withdrawal to another deployment fits: ${room} of room`);
      const peer = await peerDeployment(ctx);
      const refs = Array.from({ length: n }, ref);
      const { tx, query } = await executeTx(ctx, refs.map((r) => toDeployment(r, peer, ref(), ASSET, WITHDRAW)));
      const { events, blockHash } = await drive.send(tx, accounts.key, `execute (${n} withdrawals to another deployment)`);
      report.measurements.executeLargestBatchToDeployment = { calls: n, ...costOf(api, query, events) };
      for (const r of refs) expectations.push({ kind: 'Withdrawn', operationRef: r, toDeployment: peer, asset: ASSET, amount: WITHDRAW });
      saveExpect();
      return `${n} calls, dry run ${big(query.gasRequired.proofSize)} B of proof size (${max} per extrinsic); finalized in ${blockHash.toHex()}`;
    });

    // F-026 decision 23: a call that fails in its own frame is refused under its reference, and the
    // rest of the batch applies. The second of two withdrawals to another deployment is given just
    // too little gas to finish: the largest proof size at which its dry run still fails, found by
    // bisection, so the frame runs out at its very end, after it moved the value and the receiving
    // deployment recorded it. All of that is rolled back with the frame.
    ok &= await runCase('scenarios', 'a call that runs out of gas in its own frame is refused; the rest applies', async () => {
      if (!isolatesCalls(abi)) return 'skipped: this code runs every call in the batch frame';
      const peer = await peerDeployment(ctx);
      const [a, b] = [ref(), ref()];
      const envelopes = [toDeployment(a, peer, ref(), ASSET, WITHDRAW), toDeployment(b, peer, ref(), ASSET, WITHDRAW)];
      const full = await dryRunExecute(ctx, envelopes);
      if (full.outcomes?.join() !== 'Applied,Applied') fail(`both calls should apply with the block's gas: ${JSON.stringify(full)}`);
      const refTime = full.refTime * 2n;
      const at = (proofSize) => dryRunExecute(ctx, envelopes, api.registry.createType('WeightV2', { refTime, proofSize }));
      const shape = (d) => d.failed ?? d.outcomes.join();
      // Bisect for the least proof size that applies both: `hi` applies both, `lo` does not.
      const both = 'Applied,Applied';
      const target = 'Applied,Refused(CallFailed(OutOfGas))';
      let lo = full.proofSize / 4n;
      let hi = full.proofSize + 200_000n;
      if (shape(await at(lo)) === both) fail(`both calls apply with ${lo}`);
      if (shape(await at(hi)) !== both) fail(`at ${hi}: ${shape(await at(hi))}`);
      while (hi - lo > 256n) {
        const mid = (lo + hi) / 2n;
        if (shape(await at(mid)) === both) hi = mid;
        else lo = mid;
      }
      // A little under the boundary, so that the real run's frame still runs out at its very end
      // (the last charge of the frame is its `Withdrawn` event, about 6 KB of proof size).
      lo -= 1_024n;
      if (shape(await at(lo)) !== target) fail(`at ${lo}: ${shape(await at(lo))}, expected ${target}`);
      const peerView = async () => view(api, abi, peer, (await finalized(api)).hash);
      const peerBefore = await (await peerView()).head();
      const holdingBefore = await assetBalance(api, (await finalized(api)).hash, ASSET, peer);
      const contract = drive.contractOf(api, abi);
      const gasLimit = api.registry.createType('WeightV2', { refTime, proofSize: lo });
      const { blockHash } = await drive.send(contract.tx.execute({ gasLimit, storageDepositLimit: null }, envelopes), accounts.key, 'execute (second call starved)');
      const pv = await view(api, abi, peer, blockHash);
      const peerAfter = await pv.head();
      const holdingAfter = await assetBalance(api, blockHash, ASSET, peer);
      if (peerAfter.nextSeq - peerBefore.nextSeq !== 1) fail(`the receiving deployment wrote ${peerAfter.nextSeq - peerBefore.nextSeq} entries, expected 1`);
      if (holdingAfter - holdingBefore !== WITHDRAW) fail(`the receiving deployment got ${holdingAfter - holdingBefore}, expected ${WITHDRAW}`);
      expectations.push(
        { kind: 'Withdrawn', operationRef: a, toDeployment: peer, asset: ASSET, amount: WITHDRAW },
        { kind: 'Refused', operationRef: b, reason: 'CallFailed(OutOfGas)' },
      );
      saveExpect();
      report.findings.callIsolation = { proofSizeGiven: lo, proofSizeToApplyBoth: hi, dryRunBoth: full.proofSize, peerEntries: peerAfter.nextSeq - peerBefore.nextSeq };
      return `with ${lo} of proof size the second call is refused as CallFailed(OutOfGas) (${hi} applies both); the receiver recorded and holds only the first`;
    });

    // The same, with the weights known ahead of time (the owner, 2026-09-29): each call's limit is
    // a dry run of it alone, and `execute_with_limits` gives each frame that limit plus an eighth.
    // Every call applies; then the same batch with the second call's limit halved: that call runs
    // out in its frame and is refused, and the others still apply.
    ok &= await runCase('scenarios', 'per-call limits from dry runs: a call given too little runs out alone', async () => {
      if (!abi.messages.some((m) => m.method === 'executeWithLimits')) return 'skipped: this code takes no per-call limits';
      const peer = await peerDeployment(ctx);
      const contract = drive.contractOf(api, abi);
      const make = () => [withdrawal(ref(), accounts.payee, ASSET, WITHDRAW), toDeployment(ref(), peer, ref(), ASSET, WITHDRAW), withdrawal(ref(), accounts.payee, ASSET, WITHDRAW)];
      const limitsOf = async (envelopes) => {
        const out = [];
        for (const e of envelopes) {
          const d = await dryRunExecute(ctx, [e]);
          if (d.failed || d.outcomes[0] !== 'Applied') fail(`dry run of one call: ${d.failed ?? d.outcomes}`);
          out.push({ refTime: d.refTime, proofSize: d.proofSize });
        }
        return out;
      };
      const run = async (envelopes, limits, label) => {
        const q = await contract.query.executeWithLimits(accounts.key.address, { gasLimit: gasMax(api), storageDepositLimit: null }, envelopes, limits);
        if (q.result.isErr || q.result.asOk.flags.isRevert) fail(`${label}: dry run failed: ${q.result.isErr ? q.result.asErr.toString() : JSON.stringify(q.output?.toHuman())}`);
        const need = (w, part) => {
          const n = BigInt(w.toString());
          const m = n + drive.EXECUTE_MARGIN[part];
          return (n * 3n) / 2n > m ? (n * 3n) / 2n : m;
        };
        const gasLimit = api.registry.createType('WeightV2', { refTime: need(q.gasRequired.refTime, 'refTime'), proofSize: need(q.gasRequired.proofSize, 'proofSize') });
        await drive.send(contract.tx.executeWithLimits({ gasLimit, storageDepositLimit: null }, envelopes, limits), accounts.key, label);
        return q;
      };
      const all = make();
      const limits = await limitsOf(all);
      await run(all, limits, 'execute_with_limits (dry-run limits)');
      expectations.push(
        { kind: 'Withdrawn', operationRef: all[0].operationRef, to: accounts.payee, asset: ASSET, amount: WITHDRAW },
        { kind: 'Withdrawn', operationRef: all[1].operationRef, toDeployment: peer, asset: ASSET, amount: WITHDRAW },
        { kind: 'Withdrawn', operationRef: all[2].operationRef, to: accounts.payee, asset: ASSET, amount: WITHDRAW },
      );
      const starved = make();
      const short = await limitsOf(starved);
      short[1] = { refTime: short[1].refTime, proofSize: short[1].proofSize / 2n };
      await run(starved, short, 'execute_with_limits (second call given half)');
      expectations.push(
        { kind: 'Withdrawn', operationRef: starved[0].operationRef, to: accounts.payee, asset: ASSET, amount: WITHDRAW },
        { kind: 'Refused', operationRef: starved[1].operationRef, reason: 'CallFailed(OutOfGas)' },
        { kind: 'Withdrawn', operationRef: starved[2].operationRef, to: accounts.payee, asset: ASSET, amount: WITHDRAW },
      );
      saveExpect();
      report.findings.perCallLimits = { dryRunLimits: limits, halved: short[1] };
      return `limits ${JSON.stringify(limits, (_, v) => (typeof v === 'bigint' ? v.toString() : v))}: all applied; with the second halved it is refused as CallFailed(OutOfGas) and the others apply (checked at finality)`;
    });

    ok &= await runCase('scenarios', 'deposit of a non-sufficient asset', async () => {
      const accountRef = ref();
      const before = (await api.query.system.account(contract.address)).data;
      const { events, blockHash } = await drive.callContract(api, contract, accounts.depositor, 'deposit', [accountRef, ASSET2, DEPOSIT.toString()], 'deposit (non-sufficient)');
      const after = (await (await api.at(blockHash)).query.system.account(contract.address)).data;
      const account = (await (await api.at(blockHash)).query.assets.account(ASSET2, contract.address)).unwrap();
      report.findings.nonSufficientAsset = {
        existenceReason: account.reason.toHuman(),
        contractNativeReservedBefore: before.reserved.toString(),
        contractNativeReservedAfter: after.reserved.toString(),
        assetEvents: events.filter(({ event }) => event.section === 'assets').map(({ event }) => `${event.section}.${event.method}`),
      };
      expectations.push({ kind: 'Deposited', accountRef, from: accounts.depositor.address, asset: ASSET2, amount: DEPOSIT });
      saveExpect();
      return report.findings.nonSufficientAsset;
    });

    ok &= await runCase('scenarios', 'deposit that would leave the depositor below the minimum is refused', async () => {
      const fin = await finalized(api);
      const held = await assetBalance(api, fin.hash, ASSET, accounts.depositor.address);
      const q = await contract.query.deposit(accounts.depositor.address, { gasLimit: gasMax(api), storageDepositLimit: null }, ref(), ASSET, held.toString());
      if (q.result.isErr) fail(`dry run failed: ${q.result.asErr.toString()}`);
      const output = q.output.asOk ?? q.output;
      if (!output.isErr) fail(`a deposit of the depositor's whole balance (${held}) was accepted: ${JSON.stringify(output.toHuman())}`);
      const error = renderEnum(output.asErr);
      if (error !== 'DepositRefused(Assets(CannotDeposit))') fail(`refused as ${error}, expected DepositRefused(Assets(CannotDeposit))`);
      report.findings.depositBelowMinimum = error;
      return `the whole balance (${held}) is refused as ${error}; the message reverts, so nothing is recorded`;
    });

    report.driver = 'script';
    saveReport();
    if (!ok) fail('a drive case failed; see report.json');
  },

  async check(ctx) {
    // `check final` after the faults: the same checks over the final state, recorded separately.
    const tag = process.argv[3] === 'final' ? ' (final state)' : '';
    const { api, abi } = ctx;
    const address = drive.state.contract ?? fail('no contract: run deploy first');
    // Poll to a deadline: a driver can finish on what it saw at the best block (the aledgent
    // driver's refusal is reported as soon as it is observed), and finality follows. Read until
    // every expected reference is in the inbox at the finalized head, or three minutes pass; the
    // cases below then say what is missing.
    const expected = (entries) =>
      expectations.every((x) =>
        x.kind === 'Deposited'
          ? entries.filter((e) => e.accountRef === x.accountRef).length >= expectations.filter((y) => y.kind === 'Deposited' && y.accountRef === x.accountRef).length
          : entries.some((e) => e.operationRef === x.operationRef),
      );
    let fin;
    let v;
    let head;
    let entries;
    const deadline = Date.now() + 180_000;
    for (;;) {
      fin = await finalized(api);
      v = await view(api, abi, address, fin.hash);
      ({ head, entries } = await inbox(v));
      if (expected(entries) || Date.now() > deadline) break;
      await sleep(6_000);
    }
    report[tag ? 'inboxFinal' : 'inbox'] = { finalized: fin.number, head, entries: entries.length };
    let ok = true;

    ok &= await runCase('scenarios', `every expectation has exactly one outcome, at finality${tag}`, async () => {
      // A driver that stopped early leaves nothing to check; that is a failure, not a pass.
      if (!expectations.length) fail("expect.json is empty: the driver recorded no operation");
      const byOp = new Map();
      for (const e of entries) {
        if (!e.operationRef) continue;
        if (byOp.has(e.operationRef)) fail(`operation reference ${e.operationRef} has two entries: ${byOp.get(e.operationRef).seq} and ${e.seq}`);
        byOp.set(e.operationRef, e);
      }
      const problems = [];
      // Deposits, by account reference: as many entries as deposits made to it, each amount once
      // (the aledgent driver deposits to one ledger account's reference several times).
      const deposits = new Map();
      for (const x of expectations.filter((x) => x.kind === 'Deposited')) deposits.set(x.accountRef, [...(deposits.get(x.accountRef) ?? []), x]);
      for (const [accountRef, xs] of deposits) {
        const found = entries.filter((e) => e.kind === 'Deposited' && e.accountRef === accountRef);
        if (found.length !== xs.length) {
          problems.push(`deposit ${accountRef}: ${found.length} entries for ${xs.length} deposits`);
          continue;
        }
        const byAmount = (a, b) => (BigInt(a.amount) < BigInt(b.amount) ? -1 : BigInt(a.amount) > BigInt(b.amount) ? 1 : 0);
        const want = [...xs].sort(byAmount);
        const got = [...found].sort(byAmount);
        want.forEach((x, i) => {
          const e = got[i];
          if (e.amount !== BigInt(x.amount) || !sameAsset(e.asset, x.asset) || e.from !== pk(x.from))
            problems.push(`deposit ${accountRef}: ${JSON.stringify(e, (_, v2) => (typeof v2 === 'bigint' ? v2.toString() : v2))}`);
        });
      }
      for (const x of expectations) {
        if (x.kind === 'Deposited') continue;
        const e = byOp.get(x.operationRef);
        if (!e) {
          problems.push(`${x.kind} ${x.operationRef}: no entry`);
          continue;
        }
        if (e.kind !== x.kind) problems.push(`${x.operationRef}: ${e.kind}, expected ${x.kind}${e.reason ? ` (${e.reason})` : ''}`);
        // A withdrawal to another deployment renders `to` as `{"deployment":{"contract":..}}`.
        const toOk = () => (x.toDeployment ? e.to?.includes(pk(x.toDeployment)) || e.to?.includes(x.toDeployment) : e.to === pk(x.to));
        if (x.kind === 'Withdrawn' && (e.amount !== BigInt(x.amount) || !sameAsset(e.asset, x.asset) || !toOk()))
          problems.push(`${x.operationRef}: ${e.amount} of ${e.asset} to ${e.to}, expected ${x.amount} of ${JSON.stringify(x.asset)} to ${x.to ?? x.toDeployment}`);
        if (x.kind === 'Refused' && e.reason !== x.reason) problems.push(`${x.operationRef}: refused as ${e.reason}, expected ${x.reason}`);
        // A binding of a book-only asset (F-029, amendment 0011): its asset and amount.
        if (x.kind === 'Bound' && (e.amount !== BigInt(x.amount) || !sameAsset(e.asset, x.asset)))
          problems.push(`${x.operationRef}: bound ${e.amount} of ${e.asset}, expected ${x.amount} of ${JSON.stringify(x.asset)}`);
        // Every anchor's entry carries its number and commitment; only the latest is also kept in
        // place (owner's ruling of 2026-10-11).
        if (x.kind === 'Anchored' && (e.number !== Number(x.number) || e.commitment !== x.commitment))
          problems.push(`${x.operationRef}: anchor ${e.number} with ${e.commitment}, expected ${x.number} with ${x.commitment}`);
        if (x.kind === 'Anchored' && Number(x.number) === head.lastAnchor) {
          const rec = await v.latestAnchor();
          if (!rec) problems.push(`anchor ${x.number}: no latest anchor`);
          else if (Number(rec.number.toString()) !== Number(x.number) || rec.commitment.toHex() !== x.commitment || Number(rec.seq.toString()) !== e.seq) problems.push(`anchor ${x.number}: latest ${JSON.stringify(rec.toHuman())} does not match entry ${e.seq}`);
        }
      }
      if (problems.length) fail(problems.join('; '));
      return `${expectations.length} expectations, ${entries.length} entries, head ${JSON.stringify(head)}`;
    });

    ok &= await runCase('scenarios', `each position equals the contract holding${tag}`, async () => {
      const out = {};
      for (const asset of [ASSET, ASSET2]) {
        const position = await v.position(asset);
        const holding = await assetBalance(api, fin.hash, asset, address);
        if (position !== holding) fail(`${JSON.stringify(asset)}: position ${position}, holding ${holding}`);
        out[JSON.stringify(asset)] = position.toString();
      }
      return out;
    });

    ok &= await runCase('scenarios', `events agree with storage at finality${tag}`, async () => {
      // Events at each entry's block, from the collator (default pruning keeps 256 blocks).
      const byBlock = new Map();
      for (const e of entries) byBlock.set(e.at, [...(byBlock.get(e.at) ?? []), e]);
      let matched = 0;
      const problems = [];
      for (const [number, list] of byBlock) {
        const hash = await api.rpc.chain.getBlockHash(number);
        const records = await (await api.at(hash)).query.system.events();
        const emitted = new Map();
        for (const record of records) {
          const { event } = record;
          if (event.section !== 'contracts' || event.method !== 'ContractEmitted' || event.data[0].toString() !== address) continue;
          const decoded = abi.decodeEvent(record);
          const name = decoded.event.identifier.split('::').pop();
          if (!['Deposited', 'Withdrawn', 'Anchored', 'Refused', 'Bound'].includes(name)) continue;
          emitted.set(Number(decoded.args[0].toString()), name);
        }
        for (const e of list) {
          if (emitted.get(e.seq) === e.kind) matched++;
          else problems.push(`inbox[${e.seq}] ${e.kind} at #${number}: event ${emitted.get(e.seq) ?? 'missing'}`);
        }
      }
      report.findings.eventsVersusStorage = { entries: entries.length, eventsMatching: matched, problems };
      if (problems.length) fail(problems.join('; '));
      return `${matched} of ${entries.length} entries have their event, at the entry's block`;
    });

    saveReport();
    if (!ok) fail('a check failed; see report.json');
  },

  async faults(ctx) {
    const { api, accounts, abi } = ctx;
    const contract = drive.contractOf(api, abi);
    const address = contract.address.toString();
    const key = accounts.key;
    let ok = true;

    // The cases withdraw from the position: make sure it holds enough, whichever driver ran step 3
    // (the aledgent driver's deposit is spent on its own withdrawal).
    {
      const fin = await finalized(api);
      const position = await (await view(api, abi, address, fin.hash)).position(ASSET);
      if (position < 10n * WITHDRAW) {
        const float = ref();
        await drive.callContract(api, contract, accounts.depositor, 'deposit', [float, ASSET, DEPOSIT.toString()], 'deposit (a float for the fault cases)');
        expectations.push({ kind: 'Deposited', accountRef: float, from: accounts.depositor.address, asset: ASSET, amount: DEPOSIT });
        saveExpect();
      }
    }

    // Both proofs of "not executed" (owner, 2026-09-24): the inbox at finality has no entry for
    // the reference, and the signing key's nonce the extrinsic used is still unconsumed.
    const proofs = async (operationRef, nonce) => {
      const fin = await finalized(api);
      const { entries } = await inbox(await view(api, abi, address, fin.hash));
      const entry = entries.find((e) => e.operationRef === operationRef) ?? null;
      const finalizedNonce = (await (await api.at(fin.hash)).query.system.account(key.address)).nonce.toNumber();
      return { fin, entry, nonceConsumed: finalizedNonce > nonce, finalizedNonce };
    };
    const payeeBalance = async () => assetBalance(api, (await finalized(api)).hash, ASSET, accounts.payee);
    const expectRejected = async (hex, label) => {
      try {
        await api.rpc.author.submitExtrinsic(hex);
      } catch (error) {
        return String(error.message ?? error);
      }
      fail(`${label}: the node accepted the same signed bytes again`);
    };

    ok &= await runCase('faults', 'drop the connection mid-submit', async () => {
      const results = [];
      // (a) The node has the extrinsic (its pool said so) when the connection drops.
      // (b) The connection drops right after the bytes are sent, with no answer at all.
      for (const variant of ['after the pool accepted it', 'before any answer']) {
        const r = ref();
        const payeeBefore = await payeeBalance();
        const nonce = (await api.rpc.system.accountNextIndex(key.address)).toNumber();
        const { tx } = await executeTx(ctx, [withdrawal(r, accounts.payee, ASSET, WITHDRAW)]);
        // Mortal for 32 blocks, born at the finalized head (a retracted birth block would make
        // it invalid for a reason that has nothing to do with the case).
        const birth = await finalized(api);
        const signed = await tx.signAsync(key, { nonce, blockHash: birth.hash, era: api.createType('ExtrinsicEra', { current: birth.number, period: 32 }) });
        const hex = signed.toHex();
        const provider = new WsProvider(env('KREIVO_WS', 'ws://127.0.0.1:9944'), false);
        await provider.connect();
        await provider.isReady;
        if (variant === 'after the pool accepted it') {
          await new Promise((resolvePromise, reject) => {
            provider
              .subscribe('author_extrinsicUpdate', 'author_submitAndWatchExtrinsic', [hex], (error, status) => {
                // The first status is the pool's answer: `ready` or `future` means it holds the
                // extrinsic; anything else means it refused it, and the case cannot run.
                if (error) reject(error);
                else if (status === 'ready' || status === 'future') resolvePromise();
                else reject(new Error(`the pool answered ${JSON.stringify(status)}`));
              })
              .catch(reject);
          });
        } else {
          provider.send('author_submitExtrinsic', [hex]).catch(() => {});
        }
        await provider.disconnect();
        // The submitter has lost track. Recover from the chain alone, at finality: wait until the
        // nonce is consumed or the extrinsic's mortality has passed.
        const start = (await finalized(api)).number;
        const settled = await until(`${variant}: the nonce is consumed or the extrinsic expires`, async () => {
          const p = await proofs(r, nonce);
          return p.nonceConsumed || p.fin.number > start + 40 ? p : null;
        }, 360_000);
        const payeeAfter = await payeeBalance();
        const credited = payeeAfter - payeeBefore;
        if (settled.nonceConsumed) {
          if (!settled.entry) fail(`${variant}: the nonce was consumed but the inbox has no entry for the reference`);
          if (settled.entry.kind !== 'Withdrawn' || credited !== WITHDRAW) fail(`${variant}: ${settled.entry.kind}, payee credited ${credited}`);
        } else {
          if (settled.entry) fail(`${variant}: an entry exists but the nonce is unconsumed`);
          if (credited !== 0n) fail(`${variant}: the payee was credited ${credited} with no entry`);
        }
        const rejected = await expectRejected(hex, variant);
        await sleep(12_000);
        const again = await proofs(r, nonce);
        const count = (await inbox(await view(api, abi, address, again.fin.hash))).entries.filter((e) => e.operationRef === r).length;
        if (count > 1) fail(`${variant}: ${count} entries for one reference`);
        if (settled.entry) expectations.push({ kind: 'Withdrawn', operationRef: r, to: accounts.payee, asset: ASSET, amount: WITHDRAW });
        results.push(
          `${variant}: ${settled.nonceConsumed ? `executed once (inbox[${settled.entry.seq}], payee +${credited})` : 'provably not executed (no entry, nonce unconsumed, expired)'}; resubmitting the same bytes is refused (${rejected})`,
        );
      }
      saveExpect();
      return results.join(' | ');
    });

    ok &= await runCase('faults', 'kill the submitter between signing and submitting', async () => {
      const file = resolve(OUT, 'signed-not-submitted.json');
      const child = spawnSync(process.execPath, [fileURLToPath(import.meta.url), 'sign-only', file], { env: process.env, stdio: 'inherit', timeout: 120_000 });
      if (child.signal !== 'SIGKILL') fail(`the signer was meant to die by SIGKILL after signing, not ${child.signal ?? `exit ${child.status}`}`);
      const { hex, nonce, operationRef } = JSON.parse(readFileSync(file, 'utf8'));
      const payeeBefore = await payeeBalance();

      // Both proofs hold, but "not executed" is not final yet: the signed bytes are still valid.
      const p1 = await proofs(operationRef, nonce);
      if (p1.entry || p1.nonceConsumed) fail(`before any submission: entry ${JSON.stringify(p1.entry)}, nonce consumed ${p1.nonceConsumed}`);
      const best = await api.rpc.chain.getBlockHash();
      // Would the lost bytes still apply? A dry run at the best block says, without submitting
      // them. (`TaggedTransactionQueue::validate_transaction` traps on Kreivo 0.16.9 when called
      // over `state_call` with these bytes; the first run recorded it.)
      let stillValid;
      try {
        const applied = await api.rpc.system.dryRun(hex, best);
        stillValid = applied.isOk && applied.asOk.isOk;
      } catch (error) {
        stillValid = `unknown (${String(error.message ?? error).slice(0, 120)})`;
      }

      // Consume the nonce with something that moves nothing, as a recovering submitter must before
      // it releases: then the lost bytes can never land.
      await drive.send(api.tx.system.remarkWithEvent('aledgent e2e: consume the lost nonce'), key, 'system.remark_with_event (consume the nonce)');
      const p2 = await proofs(operationRef, nonce);
      if (!p2.nonceConsumed) fail('the remark did not consume the nonce');
      const rejected = await expectRejected(hex, 'the lost bytes after the nonce was consumed');
      await sleep(12_000);
      const p3 = await proofs(operationRef, nonce);
      if (p3.entry) fail(`the lost call landed: ${JSON.stringify(p3.entry)}`);
      const credited = (await payeeBalance()) - payeeBefore;
      if (credited !== 0n) fail(`the payee was credited ${credited}`);
      report.findings.signedNotSubmitted = { stillValidBeforeNonceConsumed: stillValid, rejectionAfter: rejected };
      return `before: no entry and nonce ${nonce} unconsumed, and the signed bytes ${stillValid === true ? 'still validate' : stillValid === false ? 'no longer validate' : `validate: ${stillValid}`}; after consuming the nonce with a remark they are refused (${rejected}) and never land`;
    });

    // Needs a light client to starve: aledgent's adapter (services/settle over smoldot). The script
    // driver submits through a node's RPC and has none.
    record('faults', 'starve the light client of peers', 'pending', 'n/a with the script driver, which has no light client: the aledgent driver runs it (e2e/README.md, "Fault cases")');

    ok &= await runCase('faults', 'prune node state', async () => {
      const pruned = await connectPruned();
      if (!pruned) fail('KREIVO_PRUNED_WS is not set');
      try {
        const collatorFin = await finalized(api);
        const { entries } = await inbox(await view(api, abi, address, collatorFin.hash));
        const oldest = entries.reduce((a, e) => (a && a.at <= e.at ? a : e), null) ?? fail('the inbox is empty');
        // Wait until the pruned node has finalized well past its window over the oldest entry.
        const target = oldest.at + PRUNED_STATE_BLOCKS + 8;
        const pf = await until(`pruned node finalizes #${target}`, async () => {
          const f = await finalized(pruned);
          return f.number >= target ? f : null;
        }, 600_000, 6_000);
        const oldHash = await pruned.rpc.chain.getBlockHash(oldest.at);
        let eventsAtOldBlock;
        try {
          const evs = await (await pruned.at(oldHash)).query.system.events();
          eventsAtOldBlock = `readable (${evs.length} records)`;
        } catch (error) {
          eventsAtOldBlock = `unreadable: ${String(error.message ?? error).slice(0, 160)}`;
        }
        let eventsOnCollator;
        try {
          const evs = await (await api.at(await api.rpc.chain.getBlockHash(oldest.at))).query.system.events();
          eventsOnCollator = `readable (${evs.length} records)`;
        } catch (error) {
          eventsOnCollator = `unreadable: ${String(error.message ?? error).slice(0, 160)}`;
        }
        // The adapter's path on the pruned node: head and the whole range at its finalized head.
        const pv = await view(pruned, abi, address, pf.hash);
        const cv = await view(api, abi, address, pf.hash);
        const ph = await pv.head();
        let same = 0;
        for (let seq = ph.prunedTo; seq < ph.nextSeq; seq++) {
          const a = await pv.entryBytes(seq);
          const b = await cv.entryBytes(seq);
          if (!a || !b || !u8aEq(a, b)) fail(`inbox[${seq}] differs between the pruned node and the collator at #${pf.number}`);
          same++;
        }
        report.findings.prunedNode = { stateBlocks: PRUNED_STATE_BLOCKS, oldestEntryBlock: oldest.at, prunedFinalized: pf.number, eventsAtOldBlock, eventsOnCollator, inboxEntriesReadAtHead: same };
        if (eventsAtOldBlock.startsWith('readable')) fail(`the pruned node still serves #${oldest.at}'s events at #${pf.number}: pruning did not take effect`);
        return report.findings.prunedNode;
      } finally {
        await pruned.disconnect();
      }
    });

    ok &= await runCase('faults', 'a deposit in the same gap as a withdrawal', async () => {
      const f0 = await finalized(api);
      const v0 = await view(api, abi, address, f0.hash);
      const cursor = (await v0.head()).nextSeq;
      const position0 = await v0.position(ASSET);
      const d = ref();
      const w = ref();
      const depositQuery = await contract.query.deposit(accounts.depositor.address, { gasLimit: gasMax(api), storageDepositLimit: null }, d, ASSET, DEPOSIT.toString());
      const depositTx = contract.tx.deposit({ gasLimit: depositQuery.gasRequired, storageDepositLimit: null }, d, ASSET, DEPOSIT.toString());
      const { tx: withdrawTx } = await executeTx(ctx, [withdrawal(w, accounts.payee, ASSET, WITHDRAW)]);
      // Submitted together, from two signers, so they land between the same two finalized reads.
      const [dep, wd] = await Promise.all([drive.send(depositTx, accounts.depositor, 'deposit (same gap)'), drive.send(withdrawTx, key, 'execute (same gap)')]);
      const f1 = await finalized(api);
      const v1 = await view(api, abi, address, f1.hash);
      const head = await v1.head();
      const range = [];
      for (let seq = cursor; seq < head.nextSeq; seq++) {
        const e = await v1.entry(seq);
        if (!e) fail(`gap at inbox[${seq}]`);
        range.push(plainEntry(seq, e));
      }
      const kinds = range.map((e) => e.kind).sort();
      if (range.length !== 2 || kinds[0] !== 'Deposited' || kinds[1] !== 'Withdrawn') fail(`one range read from the cursor returned ${JSON.stringify(range.map((e) => e.kind))}`);
      if (range.find((e) => e.kind === 'Deposited').accountRef !== d || range.find((e) => e.kind === 'Withdrawn').operationRef !== w) fail('the entries carry other references');
      const position1 = await v1.position(ASSET);
      if (position1 !== position0 + DEPOSIT - WITHDRAW) fail(`position ${position0} -> ${position1}, expected +${DEPOSIT} -${WITHDRAW}`);
      const holding = await assetBalance(api, f1.hash, ASSET, address);
      if (holding !== position1) fail(`holding ${holding} differs from position ${position1}`);
      const sameBlock = range[0].at === range[1].at;
      expectations.push(
        { kind: 'Deposited', accountRef: d, from: accounts.depositor.address, asset: ASSET, amount: DEPOSIT },
        { kind: 'Withdrawn', operationRef: w, to: accounts.payee, asset: ASSET, amount: WITHDRAW },
      );
      saveExpect();
      return `inbox[${cursor}..${head.nextSeq}) = ${range.map((e) => `${e.kind}@#${e.at}`).join(', ')} (${sameBlock ? 'same block' : 'different blocks'}; finalized in ${dep.blockHash.toHex().slice(0, 10)} and ${wd.blockHash.toHex().slice(0, 10)}); position ${position0} -> ${position1}`;
    });

    saveReport();
    if (!ok) fail('a fault case failed; see report.json');
  },

  async summary() {
    const lines = ['## Settlement contract on zombienet', ''];
    const n = report.network ?? {};
    lines.push(`Runtime **${n.runtime ?? '?'}** (Kreivo release \`${n.kreivoRelease || '?'}\`, wasm sha256 \`${(n.kreivoWasmSha256 || '?').slice(0, 16)}…\`), ${n.polkadotSdk || '?'}; driver **${report.driver ?? '?'}**.`);
    if (report.deploy) lines.push(`Contract \`${report.deploy.contract}\` (app ${report.deploy.appId}, licence ${report.deploy.licenseId}, community ${report.deploy.community?.id}), code ${report.deploy.codeBytes} bytes of ${n.maxCodeLen}.`);
    const icon = { pass: 'pass', fail: '**FAIL**', pending: '*pending*' };
    for (const [title, list] of [['Scenarios', report.scenarios], ['Fault injection (T-027-06)', report.faults]]) {
      lines.push('', `### ${title}`, '', '| Case | Result | Detail |', '|---|---|---|');
      for (const c of list) {
        const detail = (typeof c.detail === 'string' ? c.detail : JSON.stringify(c.detail ?? '')).replace(/\n/g, ' ').replace(/\|/g, '\\|').slice(0, 600);
        lines.push(`| ${c.name} | ${icon[c.status] ?? c.status} | ${detail} |`);
      }
    }
    lines.push('', '### Weights and storage deposits', '', '| Message | Dry-run ref time | Dry-run proof size | Of max extrinsic (ref time / proof size, ‰) | Storage deposit (dry run) | Held (events) | Fee |', '|---|---|---|---|---|---|---|');
    for (const [name, m] of Object.entries(report.measurements ?? {})) {
      if (name === 'perCall') continue;
      lines.push(
        `| ${name} | ${m.dryRun?.refTime} | ${m.dryRun?.proofSize} | ${m.shareOfMaxExtrinsic ? `${m.shareOfMaxExtrinsic.refTimePermille} / ${m.shareOfMaxExtrinsic.proofSizePermille}` : '?'} | ${m.dryRun?.storageDeposit} | ${m.storageDepositHeld ?? 0} | ${m.fee ?? '?'} |`,
      );
    }
    const per = report.measurements?.perCall;
    if (per) {
      lines.push('', '### Cost of one call (dry runs)', '', '| Withdrawal to | 1 call (ref time, proof size) | Full batch | Per call | Batch alone |', '|---|---|---|---|---|');
      for (const [kind, c] of Object.entries(per)) {
        lines.push(`| ${kind} | ${c.one.refTime}, ${c.one.proofSize} | ${c.full.calls} calls: ${c.full.refTime}, ${c.full.proofSize} | ${c.perCall.refTime}, ${c.perCall.proofSize} | ${c.batch.refTime}, ${c.batch.proofSize} |`);
      }
    }
    lines.push('', '### Findings', '', '```json', JSON.stringify(report.findings ?? {}, (_, v) => (typeof v === 'bigint' ? v.toString() : v), 2), '```');
    if (report.inbox) lines.push('', `Inbox at finalized #${report.inbox.finalized}: ${report.inbox.entries} entries, head ${JSON.stringify(report.inbox.head)}.`);
    writeFileSync(resolve(OUT, 'summary.md'), `${lines.join('\n')}\n`);
    console.log(lines.join('\n'));
  },
};

// ---------------------------------------------------------------------------------------------
// The aledgent driver's chain setup (e2e/aledgent-driver)
// ---------------------------------------------------------------------------------------------

// Kreivo's Pass authority: `PalletId(*b"kreivo_p")` padded with zeros (`AuthorityFromPalletId`).
const PASS_AUTHORITY = u8aConcat(stringToU8a('kreivo_p'), new Uint8Array(24));

/// Register the PASS account of `user` with the secp256k1 device key `secret`, as
/// `pallet-pass::register` verifies it: the device signs `SignedMessage { context, challenge,
/// authority }` over `challenge = blake2_256(block_hash(context) ‖ account)`, the account being the
/// extrinsic context a registration uses. Two orders of the signed bytes exist: `context ‖ authority
/// ‖ challenge` (pass-authenticators 0.1.0, which Kreivo 0.16.9 locks) and `context ‖ challenge ‖
/// authority` (1.0.0 and later, which `services/settle` speaks). Both are tried, newest first, and
/// the one the runtime accepted is recorded.
async function registerPass(ctx, lane) {
  const { api, accounts } = ctx;
  const account = u8aToHex(blake2AsU8a(u8aConcat(new Uint8Array(32), hexToU8a(`0x${lane.user}`))));
  if (account !== `0x${lane.account}`) fail(`PASS account of ${lane.user}: ${account}, the adapter expects 0x${lane.account}`);
  const device = new Keyring({ type: 'ecdsa' }).addFromSeed(hexToU8a(`0x${lane.secret}`), {}, 'ecdsa');
  if (u8aToHex(blake2AsU8a(device.publicKey)) !== `0x${lane.device}`) fail(`device of ${lane.user} does not match the adapter's`);
  const errors = [];
  for (const order of ['challenge-then-authority (1.x)', 'authority-then-challenge (0.1.0)']) {
    const fin = await finalized(api);
    const context = fin.number;
    const challenge = blake2AsU8a(u8aConcat(fin.hash.toU8a(), hexToU8a(account)));
    const ctxBytes = bnToU8a(context, { bitLength: 32, isLe: true });
    const message = order.startsWith('challenge') ? u8aConcat(ctxBytes, challenge, PASS_AUTHORITY) : u8aConcat(ctxBytes, PASS_AUTHORITY, challenge);
    const signature = device.sign(message); // ECDSA over blake2_256(message), 65 bytes, recoverable
    const attestation = {
      SubstrateKey: {
        message: { context, challenge: u8aToHex(challenge), authorityId: u8aToHex(PASS_AUTHORITY) },
        public: `0x${lane.device}`,
        signature: { Ecdsa: u8aToHex(signature) },
      },
    };
    try {
      await drive.send(api.tx.pass.register(`0x${lane.user}`, attestation), accounts.admin, `pass.register ${lane.user.slice(0, 8)} (${order})`);
      return order;
    } catch (error) {
      errors.push(`${order}: ${error.message}`);
    }
  }
  fail(`pass.register ${lane.user}: ${errors.join('; ')}`);
}

phases['aledgent-setup'] = async (ctx) => {
  const { api, accounts, abi } = ctx;
  const keys = JSON.parse(readFileSync(resolve(OUT, 'aledgent-keys.json'), 'utf8'));
  const passes = [...keys.lanes, keys.anchor];
  const orders = [];
  for (const lane of passes) {
    orders.push(await registerPass(ctx, lane));
    // Fees and the contract's storage deposits are paid by the PASS account.
    await drive.send(api.tx.balances.transferKeepAlive(`0x${lane.account}`, env('PASS_FUNDING', '100000000000000')), accounts.admin, `fund PASS ${lane.account.slice(0, 8)}`);
  }
  report.findings.passRegistrationOrder = [...new Set(orders)].join(', ');
  // The contract's keys: the script's key, the adapter's lanes and its anchor key; and the core's
  // clerk key as the attestation key its anchors are checked against.
  const contract = drive.contractOf(api, abi);
  const keyList = [accounts.key.address, ...passes.map((l) => `0x${l.account}`)];
  await drive.callContract(api, contract, accounts.admin, 'setKeys', [keyList, `0x${keys.attestationKey}`], 'set_keys (aledgent lanes)');
  writeFileSync(
    resolve(OUT, 'aledgent-env.json'),
    JSON.stringify({
      contract: u8aToHex(decodeAddress(contract.address.toString())),
      genesis: api.genesisHash.toHex(),
      specVersion: api.runtimeVersion.specVersion.toNumber(),
      depositor: u8aToHex(decodeAddress(accounts.depositor.address)),
    }),
  );
  report.driver = 'aledgent';
  saveReport();
  record('scenarios', 'aledgent setup: PASS accounts registered and made keys', 'pass', `${passes.length} PASS accounts (signed-message order accepted: ${report.findings.passRegistrationOrder})`);
};

// The aledgent driver's helpers (e2e/aledgent-driver): each prints one `RESULT <json>` line.

// `deposit-to <account-ref> <amount>`: the customer's deposit, which the aledgent driver asks for.
phases['deposit-to'] = async (ctx) => {
  const [accountRef, amount] = process.argv.slice(3);
  const contract = drive.contractOf(ctx.api, ctx.abi);
  const r = await drive.callContract(ctx.api, contract, ctx.accounts.depositor, 'deposit', [accountRef, ASSET, amount], `deposit ${amount} to ${accountRef.slice(0, 10)}`);
  const block = (await ctx.api.rpc.chain.getHeader(r.blockHash)).number.toNumber();
  console.log(`RESULT ${JSON.stringify({ block, blockHash: r.blockHash.toHex(), depositor: ctx.accounts.depositor.address })}`);
};

// `nonexecution <operation-ref> <account> <nonce>`: both proofs of "not executed" (the owner's
// ruling of 2026-09-24) at the finalized head: the inbox's entry for the reference, if any, and
// whether the account's nonce has passed `nonce`.
phases.nonexecution = async (ctx) => {
  const [operationRef, account, nonce] = process.argv.slice(3);
  const { api, abi } = ctx;
  const address = drive.contractOf(api, abi).address.toString();
  const fin = await finalized(api);
  const { entries } = await inbox(await view(api, abi, address, fin.hash));
  const entry = entries.find((e) => e.operationRef === operationRef.toLowerCase()) ?? null;
  const finalizedNonce = (await (await api.at(fin.hash)).query.system.account(account)).nonce.toNumber();
  console.log(`RESULT ${JSON.stringify({ finalized: fin.number, entry, finalizedNonce, nonceConsumed: finalizedNonce > Number(nonce) }, (_, v) => (typeof v === 'bigint' ? v.toString() : v))}`);
};

// `pruned-events <block>`: wait until the pruned node has finalized past `block` by its state
// window, then say whether that block's events can still be read there, and on the collator.
phases['pruned-events'] = async (ctx) => {
  const block = Number(process.argv[3]);
  const pruned = (await connectPruned()) ?? fail('KREIVO_PRUNED_WS is not set');
  try {
    const target = block + PRUNED_STATE_BLOCKS + 8;
    const pf = await until(`pruned node finalizes #${target}`, async () => {
      const f = await finalized(pruned);
      return f.number >= target ? f : null;
    }, 900_000, 6_000);
    const readEvents = async (api, hash) => {
      try {
        return `readable (${(await (await api.at(hash)).query.system.events()).length} records)`;
      } catch (error) {
        return `unreadable: ${String(error.message ?? error).slice(0, 160)}`;
      }
    };
    const hash = await ctx.api.rpc.chain.getBlockHash(block);
    const result = { block, prunedFinalized: pf.number, eventsOnPruned: await readEvents(pruned, hash), eventsOnCollator: await readEvents(ctx.api, hash) };
    console.log(`RESULT ${JSON.stringify(result)}`);
  } finally {
    await pruned.disconnect();
  }
};

// `record-fault <name> <pass|fail|pending> <detail>`: a fault case the aledgent driver ran.
phases['record-fault'] = async () => {
  const [name, status, detail] = process.argv.slice(3);
  record('faults', name, status, detail);
};

phases.all = async (ctx) => {
  for (const phase of ['deploy', 'drive', 'check', 'faults']) {
    log(`=== ${phase}`);
    await phases[phase](ctx);
  }
};

// `sign-only <file>`: the "kill the submitter between signing and submitting" child. It signs one
// withdrawal with the key's next nonce, writes the bytes, and dies by SIGKILL before submitting.
async function signOnly(ctx, file) {
  const operationRef = ref();
  const nonce = (await ctx.api.rpc.system.accountNextIndex(ctx.accounts.key.address)).toNumber();
  const { tx } = await executeTx(ctx, [withdrawal(operationRef, ctx.accounts.payee, ASSET, WITHDRAW)]);
  const signed = await tx.signAsync(ctx.accounts.key, { nonce, era: 0 });
  writeFileSync(file, JSON.stringify({ hex: signed.toHex(), nonce, operationRef }));
  log(`signed with nonce ${nonce}; dying before submitting`);
  process.kill(process.pid, 'SIGKILL');
}

const phase = process.argv[2];
if (!phases[phase] && phase !== 'sign-only') {
  console.error(`usage: node e2e.mjs <${[...Object.keys(phases), 'sign-only'].join('|')}>`);
  process.exit(2);
}
// The aledgent driver's helpers print one result and leave the summary alone.
const helpers = ['deposit-to', 'nonexecution', 'pruned-events'];
if (phase === 'summary' || phase === 'record-fault') {
  await phases[phase]();
} else {
  const ctx = await drive.connect();
  try {
    if (phase === 'sign-only') await signOnly(ctx, process.argv[3]);
    else await phases[phase](ctx);
  } finally {
    await ctx.api.disconnect();
    if (!helpers.includes(phase)) await phases.summary();
  }
}
