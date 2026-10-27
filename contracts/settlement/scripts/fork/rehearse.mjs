// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
// T-043-03, Chopsticks part: on a LOCAL fork only (mock signatures), the upgrade of one deployed
// instance to this code through its community's real referendum, the full reset through the
// same route, and every call refused as any other origin.
//
//   node rehearse.mjs <port> <test|prod>
import { blake2AsU8a, bnToU8a, connect, ContractPromise, abiOf, bundle, communityAccount, decodeAddress, encodeAddress, hexToU8a, j, PROD, save, TEST, u8aConcat, u8aToHex } from './common.mjs';

const port = Number(process.argv[2]);
const which = process.argv[3];
const CFG = {
  test: { C: TEST, community: 1200, other: 1201, app: 3, licence: 0, membership: 10022 },
  prod: { C: PROD, community: 1201, other: 1200, app: 4, licence: 0, membership: 10026 },
}[which];
const HUF7 = 'HUf7Nvfhp85yFZm31zV2ux8h1946Bzzmos2jqGGhp3bWXD4';
const RANDOM = encodeAddress(blake2AsU8a('f043-random-account'), 2);
const MOCK = `0x01deadbeef${'cd'.repeat(60)}`;
const out = { which, cfg: CFG, log: [] };
const say = (...a) => { const s = a.map((x) => (typeof x === 'string' ? x : j(x))).join(' '); out.log.push(s); console.log(`[${which}]`, s); };

const api = await connect(port);
const abiNew = abiOf(api, 'new');
const abiV2 = abiOf(api, 'v2');
const codeNew = bundle('new').source.wasm;
const rpc = (m, p) => api.rpc(m, ...p).catch(() => api._rpcCore.provider.send(m, p));
const send_ = (m, p) => api._rpcCore.provider.send(m, p);
const newBlock = (count = 1) => send_('dev_newBlock', [{ count }]);
const best = async () => { const h = await api.rpc.chain.getHeader(); return { number: h.number.toNumber(), hash: h.hash.toHex() }; };
const mockSigner = { signPayload: async () => ({ id: 1, signature: MOCK }) };
const C = CFG.C;
const caccount = communityAccount(CFG.community);
const otherAccount = communityAccount(CFG.other);
const origin = {
  root: { system: 'Root' },
  signed: (a) => ({ system: { Signed: a } }),
  community: (id) => ({ Communities: { communityId: id, subset: null } }),
};

function errText(err) {
  if (err?.isModule) { const m = api.registry.findMetaError(err.asModule); return `${m.section}.${m.name}`; }
  return err?.toString();
}

async function fund(address, planck = 100_000_000_000_000n) {
  const cur = await api.query.system.account(address);
  await send_('dev_setStorage', [{ System: { Account: [[[address], { nonce: cur.nonce.toNumber(), consumers: cur.consumers.toNumber(), providers: Math.max(cur.providers.toNumber(), 1), sufficients: cur.sufficients.toNumber(), data: { free: planck.toString(), reserved: cur.data.reserved.toString(), frozen: cur.data.frozen.toString(), flags: cur.data.flags.toString() } }]] } }]);
  await newBlock(1);
}

/// Sign with a mock signature as `address`, submit, make a block, return its events.
async function send(tx, address, label) {
  await tx.signAsync(address, { signer: mockSigner, nonce: -1 });
  const hash = tx.hash.toHex();
  await api.rpc.author.submitExtrinsic(tx);
  await newBlock(1);
  const b = await best();
  const block = await api.rpc.chain.getBlock(b.hash);
  const index = block.block.extrinsics.findIndex((x) => x.hash.toHex() === hash);
  if (index < 0) throw new Error(`${label}: not in block #${b.number}`);
  const records = await (await api.at(b.hash)).query.system.events();
  const events = records.filter((r) => r.phase.isApplyExtrinsic && r.phase.asApplyExtrinsic.eqn(index));
  const failed = events.find((r) => r.event.section === 'system' && r.event.method === 'ExtrinsicFailed');
  const success = events.find((r) => r.event.section === 'system' && r.event.method === 'ExtrinsicSuccess');
  const r = { label, block: b.number, ok: !failed, error: failed ? errText(failed.event.data[0]) : null, weight: success ? success.event.data[0].weight.toJSON() : null, events: events.map((e) => `${e.event.section}.${e.event.method}`) };
  say(`${label}: #${b.number} ${r.ok ? 'ok' : `FAILED ${r.error}`}`);
  return { ...r, records: events, hash: b.hash };
}

/// `DryRunApi.dry_run_call(origin, call)`: ok, or the dispatch error.
async function dryRun(o, call) {
  const res = await api.call.dryRunApi.dryRunCall(o, call, 5);
  if (res.isErr) return { ok: false, error: `dry run error: ${res.asErr.toString()}` };
  const eff = res.asOk;
  const ex = eff.executionResult;
  if (ex.isOk) return { ok: true, actualWeight: ex.asOk.actualWeight.toJSON(), events: eff.emittedEvents.map((e) => `${e.section}.${e.method}`) };
  return { ok: false, error: errText(ex.asErr.error), events: eff.emittedEvents.map((e) => `${e.section}.${e.method}`) };
}

/// Raw cells: ink raw key (root key LE ‖ SCALE(key)) → value hex, via ContractsApi.get_storage.
async function cell(rawKeyHex, at) {
  const r = at ? await (await api.at(at)).call.contractsApi.getStorage(C, rawKeyHex) : await api.call.contractsApi.getStorage(C, rawKeyHex);
  if (r.isErr) throw new Error(`get_storage ${rawKeyHex}: ${r.asErr.toString()}`);
  return r.asOk.isSome ? r.asOk.unwrap().toHex() : null;
}
const u64le = (n) => u8aToHex(bnToU8a(BigInt(n), { bitLength: 64, isLe: true })).slice(2);
async function cells(n) {
  const keys = ['00000000', '01000000', '02000000', '03000000', '08000000', '09000000', '0a000000', '0b000000', '0c000000'];
  for (let s = 0; s < n + 2; s++) keys.push(`04000000${u64le(s)}`);
  const o = {};
  for (const k of keys) o[k] = await cell(`0x${k}`);
  // Every key of the child trie, too, to see nothing else is there.
  const info = (await api.query.contracts.contractInfoOf(C)).unwrap();
  const child = u8aToHex(u8aConcat(new TextEncoder().encode(':child_storage:default:'), info.trieId));
  let all = [];
  try { all = (await api.rpc.childstate.getKeysPaged(child, '0x', 1000, null)).map((k) => k.toHex()); } catch (e) { all = [`unreadable: ${e.message}`]; }
  return { cells: o, childKeys: all.length, info: { codeHash: info.codeHash.toHex(), items: info.storageItems.toNumber(), bytes: info.storageBytes.toNumber(), itemDeposit: info.storageItemDeposit.toString(), byteDeposit: info.storageByteDeposit.toString(), baseDeposit: info.storageBaseDeposit.toString() } };
}

const gasMax = () => api.registry.createType('WeightV2', { refTime: 400_000_000_000n, proofSize: 4_000_000n });
async function query(abi, caller, message, args = []) {
  const c = new ContractPromise(api, abi, C);
  const q = await c.query[message](caller, { gasLimit: gasMax(), storageDepositLimit: null }, ...args);
  const res = { result: q.result.isOk ? (q.result.asOk.flags.isRevert ? 'revert' : 'ok') : `err ${q.result.asErr.toString()}`, output: q.output?.toJSON(), gasRequired: q.gasRequired.toJSON(), gasConsumed: q.gasConsumed.toJSON(), storageDeposit: q.storageDeposit.toJSON(), q };
  return res;
}

/// The community's referendum: note, submit, decision deposit, one aye from a member, then blocks
/// until the scheduler dispatches the proposal. Returns the timeline and the enacting block's events.
async function referendum(call, label, until) {
  const bytes = call.method.toU8a();
  const hash = u8aToHex(blake2AsU8a(bytes));
  await send(api.tx.preimage.notePreimage(u8aToHex(bytes)), HUF7, `${label}: note_preimage (${bytes.length} B, ${hash.slice(0, 10)})`);
  const r = await send(api.tx.communityReferenda.submit(origin.community(CFG.community), { Lookup: { hash, len: bytes.length } }, { After: 1 }), HUF7, `${label}: submit`);
  const submitted = r.records.find((e) => e.event.section === 'communityReferenda' && e.event.method === 'Submitted');
  const index = submitted.event.data[0].toNumber();
  await send(api.tx.communityReferenda.placeDecisionDeposit(index), HUF7, `${label}: place_decision_deposit(${index})`);
  await send(api.tx.communities.vote(CFG.membership, index, { Standard: true }), HUF7, `${label}: vote aye (membership ${CFG.membership})`);
  const relay0 = (await api.query.parachainSystem.lastRelayChainBlockNumber()).toNumber();
  const timeline = [];
  let enacted = null;
  for (let i = 0; i < 40 && !enacted; i++) {
    await newBlock(1);
    const b = await best();
    const relay = (await api.query.parachainSystem.lastRelayChainBlockNumber()).toNumber();
    const records = await (await api.at(b.hash)).query.system.events();
    for (const rec of records) {
      const { section, method, data } = rec.event;
      if (['communityReferenda', 'scheduler', 'contractsStore'].includes(section) || (section === 'contracts' && method !== 'ContractEmitted')) timeline.push(`#${b.number} relay +${relay - relay0}: ${section}.${method} ${JSON.stringify(data.toHuman()).slice(0, 200)}`);
    }
    if (until(records)) enacted = { block: b.number, hash: b.hash, records };
  }
  say(`${label}: referendum ${index} ${enacted ? `enacted in #${enacted.block}` : 'NOT enacted in 40 blocks'}`);
  const info = (await api.query.communityReferenda.referendumInfoFor(index)).toHuman();
  return { index, preimage: hash, len: bytes.length, timeline, enacted, info: Object.keys(info ?? {})[0] };
}

const isEvent = (records, section, method, pred = () => true) => records.some((r) => r.event.section === section && r.event.method === method && pred(r.event));
const zeros = `0x${'00'.repeat(32)}`;

try {
  const b0 = await best();
  out.fork = { block: b0.number, spec: api.runtimeVersion.specVersion.toNumber() };
  for (const a of [HUF7]) await fund(a);
  // --- before
  const reg = (await query(abiV2, C, 'registration')).output.ok;
  const head0 = (await query(abiV2, C, 'head')).output.ok;
  const gov0 = (await query(abiV2, C, 'governance')).output.ok;
  const admin = encodeAddress(decodeAddress(reg.admin), 2);
  const key = encodeAddress(decodeAddress(reg.keys[0]), 2);
  out.before = { registration: reg, head: head0, governance: gov0, admin, key, communityAccount: caccount, app: (await api.query.contractsStore.apps(CFG.app)).toJSON(), contractAccount: (await api.query.contractsStore.contractAccount([CFG.app, CFG.licence])).toHuman() };
  const n = Number(head0.nextSeq);
  const cellsBefore = await cells(n);
  out.before.cells = cellsBefore;
  say('before', { head: head0, governance: gov0, admin, key, n, items: cellsBefore.info.items, childKeys: cellsBefore.childKeys });

  // --- publish_upgrade and upgrade, as each origin (dry runs at the same state)
  const publish = api.tx.contractsStore.publishUpgrade(CFG.app, codeNew);
  const upgrade = api.tx.contractsStore.upgrade(CFG.app, CFG.licence);
  const batch = api.tx.utility.batchAll([publish.method, upgrade.method]);
  const origins = {
    [`Communities{${CFG.community}} (required)`]: origin.community(CFG.community),
    [`Communities{${CFG.other}} (the other community)`]: origin.community(CFG.other),
    Root: origin.root,
    'signed: contract admin': origin.signed(admin),
    'signed: contract key': origin.signed(key),
    'signed: a random account': origin.signed(RANDOM),
    "signed: the community's own account": origin.signed(caccount),
    'signed: HUf7 (a member)': origin.signed(HUF7),
  };
  out.upgradeOrigins = {};
  for (const [name, o] of Object.entries(origins)) {
    out.upgradeOrigins[name] = { publish_upgrade: await dryRun(o, publish.method), upgrade: await dryRun(o, upgrade.method), batch: await dryRun(o, batch.method) };
    const x = out.upgradeOrigins[name];
    say(`upgrade origin ${name}: publish_upgrade ${x.publish_upgrade.ok ? 'ok' : x.publish_upgrade.error}; upgrade ${x.upgrade.ok ? 'ok' : x.upgrade.error}; batch ${x.batch.ok ? 'ok' : x.batch.error}`);
  }
  // A real signed attempt as the admin (mock-signed), to see the extrinsic itself fail.
  await fund(admin);
  out.upgradeSignedByAdmin = (({ records, ...r }) => r)(await send(api.tx.utility.batchAll([publish.method, upgrade.method]), admin, 'batch_all(publish_upgrade, upgrade) signed by the admin'));

  // `upgrade` alone, once a new version is published: `publish_upgrade` enacted alone by the
  // community's referendum, then `upgrade` dry-run as each origin. Without
  // this, `upgrade` alone reads `AppInstanceUpToDate` as the community and an origin error as
  // anyone else, which does not show the order of the two checks.
  const pub = await referendum(publish, 'publish_upgrade alone', (recs) => isEvent(recs, 'contractsStore', 'AppUpdated'));
  out.upgradeAfterPublish = { publish: pub.enacted ? `enacted by referendum ${pub.index} in #${pub.enacted.block}` : 'not enacted' };
  for (const [name, o] of Object.entries(origins)) out.upgradeAfterPublish[name] = await dryRun(o, upgrade.method);
  say('upgrade after publish_upgrade', Object.fromEntries(Object.entries(out.upgradeAfterPublish).map(([k, v]) => [k, typeof v === 'string' ? v : v.ok ? 'ok' : v.error])));

  // --- the real enactment: the community's referendum (it publishes once more and upgrades)
  const up = await referendum(batch, 'upgrade', (recs) => isEvent(recs, 'contracts', 'ContractCodeUpdated', (e) => e.data[0].toString() === C || encodeAddress(e.data[0], 2) === C));
  out.upgradeReferendum = { ...up, enacted: up.enacted && { block: up.enacted.block, events: up.enacted.records.map((r) => `${r.event.section}.${r.event.method}`) } };
  const info1 = (await api.query.contracts.contractInfoOf(C)).unwrap();
  out.afterUpgrade = { codeHash: info1.codeHash.toHex(), expected: bundle('new').source.hash, app: (await api.query.contractsStore.apps(CFG.app)).toJSON() };
  const cellsAfter = await cells(n);
  const diff = Object.keys(cellsBefore.cells).filter((k) => cellsBefore.cells[k] !== cellsAfter.cells[k]);
  out.afterUpgrade.cellsDiffer = diff;
  out.afterUpgrade.childKeys = [cellsBefore.childKeys, cellsAfter.childKeys];
  out.afterUpgrade.head = (await query(abiNew, C, 'head')).output.ok;
  out.afterUpgrade.chainHead = (await query(abiNew, C, 'chainHead')).output.ok;
  const ch = out.afterUpgrade.chainHead;
  out.afterUpgrade.chainHeadIsNZerosN = Number(ch.position) === n && ch.digest === zeros && Number(ch.start) === n;
  out.afterUpgrade.inboxEntrySelectorGone = (await api.call.contractsApi.call(C, C, 0, null, null, '0x00000302' + u64le(0))).result.toHuman();
  say('after upgrade', { codeHash: out.afterUpgrade.codeHash, cellsDiffer: diff, chainHead: ch, ok: out.afterUpgrade.chainHeadIsNZerosN });

  // --- governance (prod: the admin names the community's account first)
  let gov = (await query(abiNew, C, 'governance')).output.ok;
  if (!gov) {
    const q = await query(abiNew, admin, 'setGovernance', [caccount]);
    const tx = api.tx.contracts.call(C, 0, q.q.gasRequired, q.q.storageDeposit.isCharge ? q.q.storageDeposit.asCharge : null, abiNew.findMessage('set_governance').toU8a([caccount]));
    out.setGovernance = { dry: { result: q.result, output: q.output, gasRequired: q.gasRequired, storageDeposit: q.storageDeposit }, ...(({ records, ...r }) => r)(await send(tx, admin, 'set_governance(community account) signed by the admin')) };
    gov = (await query(abiNew, C, 'governance')).output.ok;
  }
  out.governance = encodeAddress(decodeAddress(gov), 2);
  if (out.governance !== caccount) throw new Error(`governance is ${out.governance}, not ${caccount}`);

  // --- set_anchor_head as each caller (contract dry runs)
  const last = Number(out.afterUpgrade.head.lastAnchor);
  const book = `0x${'b4'.repeat(32)}`;
  const callers = { [`community ${CFG.community}'s account (governance)`]: caccount, admin, key, 'a random account': RANDOM, [`community ${CFG.other}'s account`]: otherAccount, 'HUf7 (a member)': HUF7 };
  out.resetCallers = {};
  for (const [name, who] of Object.entries(callers)) {
    const q = await query(abiNew, who, 'setAnchorHead', [last, 0, book]);
    out.resetCallers[name] = { result: q.result, output: q.output, gasRequired: q.gasRequired, gasConsumed: q.gasConsumed, storageDeposit: q.storageDeposit };
    say(`set_anchor_head as ${name}: ${q.result} ${j(q.output)}`);
  }
  const stale = await query(abiNew, caccount, 'setAnchorHead', [last + 1, 0, book]);
  out.resetStale = { expected_last: last + 1, result: stale.result, output: stale.output };
  say(`set_anchor_head stale (expected_last ${last + 1}): ${j(stale.output)}`);
  const g = await query(abiNew, caccount, 'setAnchorHead', [last, 0, book]);
  const data = abiNew.findMessage('set_anchor_head').toU8a([last, 0, book]);
  const deposit = g.q.storageDeposit.isCharge ? g.q.storageDeposit.asCharge : null;
  const inner = api.tx.contracts.call(C, 0, g.q.gasRequired, deposit, data);
  const routed = api.tx.communities.dispatchAsAccount(inner.method);
  out.resetRoutes = {
    [`dispatch_as_account(Contracts.call) as Communities{${CFG.community}} (required)`]: await dryRun(origin.community(CFG.community), routed.method),
    [`dispatch_as_account(Contracts.call) as Communities{${CFG.other}}`]: await dryRun(origin.community(CFG.other), routed.method),
    'Contracts.call as Root': await dryRun(origin.root, inner.method),
    'dispatch_as_account as Root': await dryRun(origin.root, routed.method),
    'Contracts.call signed by the admin': await dryRun(origin.signed(admin), inner.method),
    'Contracts.call signed by a key': await dryRun(origin.signed(key), inner.method),
    'Contracts.call signed by a random account': await dryRun(origin.signed(RANDOM), inner.method),
  };
  for (const [k, v] of Object.entries(out.resetRoutes)) say(`reset route ${k}: ${v.ok ? `ok (actual weight ${j(v.actualWeight)})` : v.error}`);
  await fund(key);
  out.resetSignedByKey = (({ records, ...r }) => r)(await send(api.tx.contracts.call(C, 0, g.q.gasRequired, deposit, data), key, 'set_anchor_head signed by a key'));

  // --- the reset, enacted through the community's referendum
  const community0 = (await api.query.system.account(caccount)).data.toJSON();
  const rs = await referendum(routed, 'reset', (recs) => isEvent(recs, 'scheduler', 'Dispatched', (e) => e.data[1].isSome));
  const recs = rs.enacted?.records ?? [];
  const released = recs.filter((r) => r.event.section === 'contracts' && r.event.method === 'StorageDepositTransferredAndReleased').map((r) => ({ from: encodeAddress(r.event.data[0], 2), to: encodeAddress(r.event.data[1], 2), amount: r.event.data[2].toString() }));
  const held = recs.filter((r) => r.event.section === 'contracts' && r.event.method === 'StorageDepositTransferredAndHeld').map((r) => ({ from: encodeAddress(r.event.data[0], 2), to: encodeAddress(r.event.data[1], 2), amount: r.event.data[2].toString() }));
  const dispatched = recs.filter((r) => r.event.section === 'scheduler' && r.event.method === 'Dispatched').map((r) => r.event.data.toHuman());
  const emitted = recs.filter((r) => r.event.section === 'contracts' && r.event.method === 'ContractEmitted' && encodeAddress(r.event.data[0], 2) === C);
  out.resetReferendum = { index: rs.index, preimage: rs.preimage, timeline: rs.timeline, info: rs.info, enactedIn: rs.enacted?.block, dispatched, released, held, contractEvents: emitted.map((r) => { try { return abiNew.decodeEvent(r).event.identifier; } catch { return 'undecodable'; } }) };
  const community1 = (await api.query.system.account(caccount)).data.toJSON();
  out.resetReferendum.communityBalance = { before: community0, after: community1 };
  out.resetDry = { gasRequired: g.gasRequired, gasConsumed: g.gasConsumed, storageDeposit: g.storageDeposit, routedActualWeight: out.resetRoutes[Object.keys(out.resetRoutes)[0]].actualWeight };
  const cellsReset = await cells(n);
  out.afterReset = {
    entriesGone: Array.from({ length: n + 2 }, (_, s) => `04000000${u64le(s)}`).every((k) => cellsReset.cells[k] === null),
    cells: cellsReset.cells, childKeys: cellsReset.childKeys, info: cellsReset.info,
    head: (await query(abiNew, C, 'head')).output.ok, chainHead: (await query(abiNew, C, 'chainHead')).output.ok,
    anchorEpoch: (await query(abiNew, C, 'anchorEpoch')).output.ok, latestAnchor: (await query(abiNew, C, 'latestAnchor')).output.ok,
  };
  say('after reset', { entriesGone: out.afterReset.entriesGone, head: out.afterReset.head, chainHead: out.afterReset.chainHead, released, held, items: cellsReset.info.items });

  // --- one entry after the reset, folded from its event
  const commitment = `0x${'a1'.repeat(32)}`;
  const env = [{ operationRef: `0x${'0f'.repeat(32)}`, call: { Anchor: { number: 1, commitment } } }];
  const qe = await query(abiNew, key, 'execute', [env]);
  const gas = api.registry.createType('WeightV2', { refTime: BigInt(qe.q.gasRequired.refTime.toString()) + 2_000_000_000n, proofSize: BigInt(qe.q.gasRequired.proofSize.toString()) + 96n * 1024n });
  const ex = await send(api.tx.contracts.call(C, 0, gas, null, abiNew.findMessage('execute').toU8a([env])), key, 'execute([Anchor 1]) signed by a key');
  const evs = ex.records.filter((r) => r.event.section === 'contracts' && r.event.method === 'ContractEmitted' && encodeAddress(r.event.data[0], 2) === C);
  const anchored = evs.map((r) => ({ name: abiNew.decodeEvent(r).event.identifier, data: r.event.data[1].toU8a(true) })).find((e) => e.name.endsWith('Anchored'));
  const entry = u8aConcat(bnToU8a(ex.block, { bitLength: 32, isLe: true }), new Uint8Array([2]), anchored.data.slice(8));
  const e0 = blake2AsU8a(entry, 256);
  const d0 = blake2AsU8a(u8aConcat(new TextEncoder().encode('aledgent/settlement-entry/v1'), new Uint8Array(32), bnToU8a(0n, { bitLength: 64, isLe: false }), e0), 256);
  const chAfter = (await query(abiNew, C, 'chainHead')).output.ok;
  out.fold = { block: ex.block, outcome: qe.output, eventSeq: Number(Buffer.from(anchored.data.slice(0, 8)).readBigUInt64LE()), entry: u8aToHex(entry), e0: u8aToHex(e0), d0: u8aToHex(d0), chainHead: chAfter, equal: chAfter.digest === u8aToHex(d0) && Number(chAfter.position) === 1 && Number(chAfter.start) === 0 };
  say('fold after reset', { d0: out.fold.d0, chainHead: chAfter, equal: out.fold.equal });
} catch (e) {
  out.error = e.stack ?? String(e);
  console.error(e);
} finally {
  save(`results-${which}.json`, out);
  await api.disconnect();
  process.exit(0);
}
