// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
// Chopsticks fork of Kreivo mainnet at the newest finalized block whose CMLS bundle index is 0
// (adapted from deploy/canary/kreivo/fork/start.mjs).
//
//   node start.mjs <port> [block]    then, with BUNDLE and V2_BUNDLE set:
//   node rehearse.mjs <port> <test|prod>
import { readFileSync } from 'node:fs';
const endpoint = 'https://rpc.kreivo.bloque.sh';
const port = Number(process.argv[2] ?? 8131);
async function rpc(method, params) {
  for (let i = 1; ; i++) {
    try {
      const r = await fetch(endpoint, { method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify({ id: 1, jsonrpc: '2.0', method, params }) });
      const j = await r.json(); if (j.error) throw new Error(JSON.stringify(j.error)); return j.result;
    } catch (e) { if (i >= 5) throw e; await new Promise((r) => setTimeout(r, 3000)); }
  }
}
function bundleIndex(header) {
  for (const log of header.digest.logs) { const m = /^0x06434d4c53([0-9a-f]{2})02([0-9a-f]{2})/.exec(log); if (m) return parseInt(m[2], 16); }
  return null;
}
let block;
if (process.argv[3]) { const hash = await rpc('chain_getBlockHash', [Number(process.argv[3])]); const h = await rpc('chain_getHeader', [hash]); block = { number: Number(process.argv[3]), hash, index: bundleIndex(h), logs: h.digest.logs }; }
else {
  let hash = await rpc('chain_getFinalizedHead', []);
  for (let i = 0; i < 64; i++) { const h = await rpc('chain_getHeader', [hash]); const idx = bundleIndex(h); console.log(`[fork] #${parseInt(h.number, 16)} bundle index ${idx}`); if (idx === 0 || idx === null) { block = { number: parseInt(h.number, 16), hash, index: idx, logs: h.digest.logs }; break; } hash = h.parentHash; }
}
console.log(`[fork] forking at #${block.number} ${block.hash} (bundle index ${block.index})`);
console.log(`[fork] digest logs: ${JSON.stringify(block.logs)}`);
const { GenericExtrinsic } = await import('@polkadot/types');
const base = Object.getPrototypeOf(GenericExtrinsic.prototype);
const original = Object.getOwnPropertyDescriptor(base, 'signer');
Object.defineProperty(base, 'signer', { configurable: true, get() { if (this.isGeneral?.()) return { toString: () => 'general', toHex: () => '0x' }; return original.get.call(this); } });
const { fetchConfig, setupWithServer } = await import('@acala-network/chopsticks');
const { configSchema } = await import('@acala-network/chopsticks/schema/index');
const config = configSchema.parse({ ...(await fetchConfig(new URL('./kreivo.yml', import.meta.url).pathname)), block: block.number, port, db: new URL(`./db-${port}.sqlite`, import.meta.url).pathname });
const { close } = await setupWithServer(config);
console.log(`[fork] READY ws://127.0.0.1:${port} at #${block.number}`);
for (const sig of ['SIGINT', 'SIGTERM']) process.on(sig, async () => { await close().catch(() => {}); process.exit(0); });
