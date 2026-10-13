#!/usr/bin/env node
// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
// The issuer's side of a binding (F-029, amendment 0011, REQ-AS-8), on the suite's Kreivo: a plain
// `Assets.transfer` of `amount` of `asset` from //Alice (the genesis assets' owner and issuer) to the
// deployment's contract account. A plain transfer writes no inbox entry, so no account of the book
// is credited; the contract's `Bind` then checks that its holding is its recorded position plus the
// book's outstanding balance, exactly.
//
//   KREIVO_WS=ws://127.0.0.1:9944 node fund-contract.mjs <contract-account-hex> <asset-index> <amount>
//
// <asset-index> is n of `Here(n)`. Prints `RESULT {json}` on success; exits non-zero on any
// failure. Development keys only: refuses an endpoint that is not on this machine.

import { sr25519CreateDerive } from '@polkadot-labs/hdkd';
import { DEV_MINI_SECRET } from '@polkadot-labs/hdkd-helpers';
import { AccountId } from 'polkadot-api';
import { getPolkadotSigner } from 'polkadot-api/signer';
import { fromHex } from 'polkadot-api/utils';
import { createWsClient } from 'polkadot-api/ws';

const fail = (m) => {
  console.error(`fund-contract: ${m}`);
  process.exit(1);
};

const [contractHex, indexText, amountText] = process.argv.slice(2);
const endpoint = process.env.KREIVO_WS;
if (!endpoint) fail('KREIVO_WS is required');
if (!/^wss?:\/\/(127\.0\.0\.1|localhost|\[::1\])(:\d+)?(\/.*)?$/.test(endpoint)) fail(`${endpoint} is not on this machine: development keys only`);
if (!/^(0x)?[0-9a-fA-F]{64}$/.test(contractHex ?? '')) fail('the contract account: 32 bytes of hex');
if (!/^[0-9]+$/.test(indexText ?? '')) fail('the asset index: n of Here(n)');
if (!/^[0-9]+$/.test(amountText ?? '')) fail('the amount: a non-negative integer, in minor units');

const ss58 = AccountId(2);
const contract = ss58.dec(fromHex(contractHex.startsWith('0x') ? contractHex : `0x${contractHex}`));
const id = { type: 'Here', value: Number(indexText) };
const amount = BigInt(amountText);
const alice = sr25519CreateDerive(fromHex(`0x${DEV_MINI_SECRET}`))('//Alice');
const signer = getPolkadotSigner(alice.publicKey, 'Sr25519', alice.sign);

const client = createWsClient(endpoint);
try {
  const api = client.getUnsafeApi();
  const before = (await api.query.Assets.Account.getValue(id, contract, { at: 'finalized' }))?.balance ?? 0n;
  let block = null;
  if (amount > 0n) {
    const tx = api.tx.Assets.transfer({ id, target: { type: 'Id', value: contract }, amount });
    const r = await tx.signAndSubmit(signer, { mortality: { mortal: true, period: 64 } });
    if (!r.ok) fail(`Assets.transfer failed in block #${r.block.number}: ${JSON.stringify(r.dispatchError)}`);
    block = { number: r.block.number, hash: r.block.hash };
  }
  const after = (await api.query.Assets.Account.getValue(id, contract, { at: 'finalized' }))?.balance ?? 0n;
  if (after !== before + amount) fail(`the contract holds ${after}, expected ${before + amount}`);
  console.log(`RESULT ${JSON.stringify({ contract, asset: `Here(${indexText})`, amount: amount.toString(), holding: after.toString(), block })}`);
} finally {
  client.destroy();
}
