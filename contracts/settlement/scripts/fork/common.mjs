// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
import { ApiPromise, WsProvider } from '@polkadot/api';
import { Abi, ContractPromise } from '@polkadot/api-contract';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { u8aToHex, hexToU8a, u8aConcat, bnToU8a, stringToU8a } from '@polkadot/util';
import { blake2AsU8a, encodeAddress, decodeAddress, cryptoWaitReady } from '@polkadot/util-crypto';
export const signedExtensions = { PassAuthenticate: { extrinsic: { authenticateParams: 'Option<Bytes>' }, payload: {} } };
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
const here = dirname(fileURLToPath(import.meta.url));
/// The bundles: this code (`BUNDLE`, default the contract's build) and v2 (`V2_BUNDLE`, required),
/// and where results go (`OUT`, default ./out).
const BUNDLES = {
  new: resolve(process.env.BUNDLE ?? resolve(here, '../../target/ink/aledgent_settlement_contract.contract')),
  v2: process.env.V2_BUNDLE ? resolve(process.env.V2_BUNDLE) : null,
};
const OUT = resolve(process.env.OUT ?? resolve(here, 'out'));
export const TEST = 'GJ4zVXnSXNbFJgo3cUXJXf9JYfVMGscukptNsnnfZLwUBr7';
export const PROD = 'Da1uhU5rnKTXtQxZDvkqDsEeS2SUaNBktAxvMa8CTKDjrdk';
export async function connect(port) {
  await cryptoWaitReady();
  const api = await ApiPromise.create({ provider: new WsProvider(`ws://127.0.0.1:${port}`), signedExtensions, noInitWarn: true });
  return api;
}
export const bundle = (which) => JSON.parse(readFileSync(BUNDLES[which] ?? (() => { throw new Error(`${which === 'v2' ? 'V2_BUNDLE' : 'BUNDLE'} is not set`); })(), 'utf8'));
export const abiOf = (api, which) => new Abi(bundle(which), api.registry.getChainProperties());
export function communityAccount(id) {
  const raw = u8aConcat(stringToU8a('modl'), stringToU8a('kv/cmtys'), bnToU8a(id, { bitLength: 16, isLe: true }));
  const p = new Uint8Array(32); p.set(raw); return encodeAddress(p, 2);
}
export const j = (x) => JSON.stringify(x, (_, v) => (typeof v === 'bigint' ? v.toString() : v), 1);
export const save = (f, x) => { mkdirSync(OUT, { recursive: true }); writeFileSync(resolve(OUT, f), j(x)); };
export { u8aToHex, hexToU8a, u8aConcat, bnToU8a, blake2AsU8a, encodeAddress, decodeAddress, ContractPromise, stringToU8a };
