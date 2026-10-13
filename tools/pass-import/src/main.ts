// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
// F-028 (DEC-19, plan §5): read every Kreivo `Pass` device at one finalized block and write an
// aledgent import file: the identities (accounts with a passkey, their primary credential and the
// rest to import) and the ledger accounts (accounts with only substrate keys, for an initial load).
//
// Read-only. It holds no key and builds no transaction: it opens a WebSocket, reads storage at a
// pinned block, and closes. Only public material is read (`REQ-AU-8`).
//
//   bun run src/main.ts --rp-id <the passkeys' rp id> --out pass-import.json \
//       [--endpoint wss://kreivo.io] [--accounts accounts.txt] [--at <block hash>]
//
// The rpId is the operator's statement, not the chain's: Kreivo stores no rpId, so the operator
// names the relying party its passkeys were registered under (`RISK-10`). `--accounts` limits the file to a list of SS58 addresses, one a line.

import { readFileSync, writeFileSync } from 'node:fs';
import { createClient } from 'polkadot-api';
import { getWsProvider } from 'polkadot-api/ws';
import { type Outcome, bytesOf, classify, convertDevice, validRpId } from './convert.ts';

const DEFAULT_ENDPOINT = 'wss://kreivo.io';

function arg(name: string): string | undefined {
  const i = process.argv.indexOf(`--${name}`);
  return i >= 0 ? process.argv[i + 1] : undefined;
}

function fail(msg: string): never {
  console.error(`pass-import: ${msg}`);
  process.exit(2);
}

async function main() {
  const rpId = arg('rp-id') ?? fail('--rp-id is required: the host the passkeys were registered on');
  if (!validRpId(rpId)) fail(`--rp-id ${rpId} is not a lower-case DNS name`);
  const out = arg('out') ?? fail('--out is required');
  const endpoint = arg('endpoint') ?? DEFAULT_ENDPOINT;
  const only = arg('accounts');
  const wanted = only
    ? new Set(
        readFileSync(only, 'utf8')
          .split('\n')
          .map((l) => l.trim())
          .filter((l) => l.length > 0 && !l.startsWith('#')),
      )
    : null;

  const client = createClient(getWsProvider(endpoint));
  try {
    // One block for the whole read, so the file is a consistent snapshot.
    const block = arg('at') ? { hash: arg('at') as string, number: undefined } : await client.getFinalizedBlock();
    const at = block.hash;
    // The unsafe API decodes from the chain's own metadata: no descriptors to keep in step with
    // Kreivo's runtime, and nothing here writes.
    const api = client.getUnsafeApi();
    const genesis = await client.getChainSpecData().then((s) => s.genesisHash);
    const raw = (await api.query.Pass.Devices.getEntries({ at })) as Array<{
      keyArgs: [string, unknown];
      value: { type: string; value: unknown };
    }>;
    const entries: Array<{ account: string; outcome: Outcome }> = [];
    for (const r of raw) {
      const [account, deviceId] = r.keyArgs;
      if (wanted && !wanted.has(account)) continue;
      entries.push({
        account,
        outcome: convertDevice({ account, deviceId: bytesOf(deviceId), device: r.value }, rpId),
      });
    }
    const classified = classify(entries);
    const count = (k: Outcome['kind']) => entries.filter((e) => e.outcome.kind === k).length;
    const file = {
      format: 'aledgent-pass-import/v2',
      source: { chain: 'kreivo', endpoint, genesis, block: { hash: at, number: block.number } },
      rp_id: rpId,
      totals: {
        accounts: classified.identities.length + classified.ledger_accounts.length + classified.unclassified.length,
        identities: classified.identities.length,
        ledger_accounts: classified.ledger_accounts.length,
        unclassified: classified.unclassified.length,
        passkeys: count('passkey'),
        recovered: entries.filter((e) => e.outcome.kind === 'passkey' && e.outcome.recovered).length,
        substrate_keys: count('skipped'),
        malformed: count('malformed'),
      },
      ...classified,
    };
    writeFileSync(out, `${JSON.stringify(file, null, 2)}\n`);
    console.log(
      `pass-import: ${file.totals.identities} identities (${file.totals.passkeys} passkeys, ` +
        `${file.totals.recovered} recovered), ${file.totals.ledger_accounts} ledger accounts, ` +
        `${file.totals.unclassified} unclassified, at ${at} → ${out}`,
    );
  } finally {
    client.destroy();
  }
}

await main();
