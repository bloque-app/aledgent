#!/usr/bin/env node
// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
// The Kusama relay chain's light-client spec for the adapter, from a pinned, checksummed
// source, cut to one boot node.
//
//   node fetch-kusama.mjs <out.json>
//
// Source: `@polkadot-api/known-chains` 0.12.4 on npm (published 2026-09-28, with npm provenance),
// `dist/specs/kusama/kusama.js`: Kusama's spec with its genesis state root and a `lightSyncState`
// checkpoint at block 35 434 907. The checkpoint is what makes a cold start take seconds: without
// one, smoldot warp-syncs from Kusama's genesis, which takes longer than the three minutes after
// which the adapter restarts itself as blind (services/settle/src/main.rs, `BLIND_RESTART`).
// polkadot-sdk's own `kusama.json` carries no checkpoint, so it is not used.
//
// Both the tarball and the spec inside it are pinned by SHA-256, and the spec's genesis state root
// gives Kusama's genesis hash 0xb0a8d493…3dafe (checked when the pin was taken; the adapter's
// light client then follows Kreivo's heads from this relay chain, so a wrong relay spec syncs no
// Kreivo block at all). A newer checkpoint means a new pin: bump VERSION and the two digests
// together, in a reviewed commit.
//
// Node 22 or later, and `tar` on the PATH. Nothing else is installed.

import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const VERSION = '0.12.4';
const TARBALL = `https://registry.npmjs.org/@polkadot-api/known-chains/-/known-chains-${VERSION}.tgz`;
const TARBALL_SHA256 = '8fdc263d7c1a1556673d92c30d1131ed74c9a582e46adfc0422176740af8203b';
const SPEC_PATH = 'package/dist/specs/kusama/kusama.js';
const SPEC_SHA256 = '0a6e64743fab1462e8c5705e1fc1552d147fbca65de998b2524a40a02042c298';
const GENESIS_STATE_ROOT = '0xb0006203c3a6e6bd2c6a17b1d4ae8ca49a31da0f4579da950b127774b44aef6b';
// One boot node, Parity's (the suite and a deployment run one per chain). smoldot
// finds further peers through it.
const BOOT_NODE = '/dns/kusama-bootnode-0.polkadot.io/tcp/30333/p2p/12D3KooWSueCPH3puP2PcvqPJdNaDNF3jMZjtJtDiSy35pWrbt5h';

const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex');
const fail = (message) => {
  console.error(`fetch-kusama: ${message}`);
  process.exit(1);
};

const out = process.argv[2] ?? fail('usage: node fetch-kusama.mjs <out.json>');

const response = await fetch(TARBALL);
if (!response.ok) fail(`${TARBALL}: HTTP ${response.status}`);
const tarball = Buffer.from(await response.arrayBuffer());
if (sha256(tarball) !== TARBALL_SHA256) fail(`${TARBALL}: SHA-256 ${sha256(tarball)}, pinned ${TARBALL_SHA256}`);

const dir = mkdtempSync(join(tmpdir(), 'kusama-spec-'));
try {
  writeFileSync(join(dir, 'known-chains.tgz'), tarball);
  execFileSync('tar', ['-xzf', join(dir, 'known-chains.tgz'), '-C', dir, SPEC_PATH]);
  const module = readFileSync(join(dir, SPEC_PATH), 'utf8');
  // The module is `const chainSpec = \`<the JSON>\`; export { chainSpec };`.
  const text = module.slice(module.indexOf('`') + 1, module.lastIndexOf('`'));
  if (sha256(text) !== SPEC_SHA256) fail(`${SPEC_PATH}: SHA-256 ${sha256(text)}, pinned ${SPEC_SHA256}`);
  const spec = JSON.parse(text);
  if (spec.id !== 'ksmcc3') fail(`the spec is ${spec.id}, not Kusama (ksmcc3)`);
  if (!spec.lightSyncState) fail('the spec carries no lightSyncState checkpoint');
  if (spec.genesis?.stateRootHash !== GENESIS_STATE_ROOT) fail(`genesis state root ${spec.genesis?.stateRootHash}`);
  if (!spec.bootNodes.includes(BOOT_NODE)) fail(`${BOOT_NODE} is not among the spec's boot nodes`);
  spec.bootNodes = [BOOT_NODE];
  writeFileSync(out, `${JSON.stringify(spec, null, 2)}\n`);
  console.log(`fetch-kusama: ${out}: Kusama (ksmcc3), known-chains ${VERSION}, one boot node`);
} finally {
  rmSync(dir, { recursive: true, force: true });
}
