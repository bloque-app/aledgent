// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
// F-028: the conversion of a Kreivo Pass device into an aledgent passkey verifier.
import { describe, expect, test } from 'bun:test';
import {
  P256_SPKI_PREFIX,
  bytesToHex,
  convertDevice,
  encodePasskey,
  classify,
  hexToBytes,
  sec1FromSpki,
  sec1FromTruncatedSpki,
  validRpId,
} from './convert.ts';

// The P-256 generator, uncompressed: a point on the curve.
const G =
  '046b17d1f2e12c4247f8bce6e563a440f277037d812deb33a0f4a13945d898c296' +
  '4fe342e2fe1a7f9b8ee7eb4a7c0f9e162bce33576b315ececbb6406837bf51f5';

// The vector `crates/authn` decodes too (`dec_19_the_import_tools_vector_decodes`).
const VECTOR =
  '0101' +
  '11'.repeat(32) +
  G +
  '01' +
  '00000007' +
  '0e6170692e626c6f7175652e617070';

const spki = (point: string) => {
  const p = hexToBytes(point);
  const out = new Uint8Array(P256_SPKI_PREFIX.length + p.length);
  out.set(P256_SPKI_PREFIX);
  out.set(p, P256_SPKI_PREFIX.length);
  return out;
};

const record = (deviceId: string, point: string, signCount = 3) => ({
  device_id: `0x${deviceId}`,
  public_key: `0x${bytesToHex(spki(point))}`,
  sign_count: signCount,
  backup_eligible: true,
  backup_state: false,
});

describe('encodePasskey', () => {
  test('dec_19 writes the bytes ledger_core::scheme::PasskeyKey::encode writes', () => {
    const v = encodePasskey({
      handle: new Uint8Array(32).fill(0x11),
      publicKey: hexToBytes(G),
      rpId: 'api.bloque.app',
      backupEligible: true,
      signCount: 7,
    });
    expect(bytesToHex(v)).toBe(VECTOR);
  });

  test('dec_19 refuses an rpId that is not a lower-case DNS name', () => {
    expect(validRpId('api.bloque.app')).toBe(true);
    for (const bad of ['', 'API.bloque.app', 'https://api.bloque.app', 'a..b', '-a.b']) {
      expect(validRpId(bad)).toBe(false);
    }
  });
});

describe('sec1FromSpki', () => {
  test('dec_19 takes a 91-byte P-256 SPKI to its point, on the curve only', () => {
    expect(spki(G).length).toBe(91);
    expect(bytesToHex(sec1FromSpki(spki(G)) as Uint8Array)).toBe(G);
    const off = `${G.slice(0, -2)}f6`;
    expect(sec1FromSpki(spki(off))).toBeNull();
    const wrongPrefix = spki(G);
    wrongPrefix[10] ^= 1;
    expect(sec1FromSpki(wrongPrefix)).toBeNull();
    expect(sec1FromSpki(spki(G).slice(0, 90))).toBeNull();
  });
});

describe('sec1FromTruncatedSpki', () => {
  test('dec_19 recovers a key stored with a length prefix and its last two bytes cut', () => {
    // What Kreivo holds for such a device: 0x6d01, then the first 89 bytes of the SPKI.
    const field = new Uint8Array(91);
    field.set([0x6d, 0x01]);
    field.set(spki(G).slice(0, 89), 2);
    expect(bytesToHex(sec1FromTruncatedSpki(field) as Uint8Array)).toBe(G);
    // A head no point of this x has.
    const wrong = field.slice();
    wrong[90] ^= 1;
    expect(sec1FromTruncatedSpki(wrong)).toBeNull();
    // A whole SPKI is not a truncated one.
    expect(sec1FromTruncatedSpki(spki(G))).toBeNull();
  });
});

describe('convertDevice', () => {
  const id = 'ab'.repeat(32);

  test('req_au_20 a WebAuthn device becomes a passkey verifier', () => {
    const o = convertDevice(
      { account: 'acct', deviceId: hexToBytes(id), device: { type: 'WebAuthn', value: record(id, G) } },
      'api.bloque.app',
    );
    expect(o.kind).toBe('passkey');
    if (o.kind === 'passkey') {
      expect(o.verifier.startsWith(`0101${id}${G}01`)).toBe(true);
      expect(o.signCount).toBe(3);
    }
  });

  test('req_au_20 a substrate-key device is skipped, a broken one is reported', () => {
    expect(
      convertDevice(
        { account: 'acct', deviceId: hexToBytes(id), device: { type: 'SubstrateKey', value: `0x${'00'.repeat(32)}` } },
        'api.bloque.app',
      ).kind,
    ).toBe('skipped');
    expect(
      convertDevice(
        { account: 'acct', deviceId: hexToBytes(id), device: { type: 'WebAuthn', value: record('cd'.repeat(32), G) } },
        'api.bloque.app',
      ),
    ).toEqual({ kind: 'malformed', deviceId: id, reason: 'the record names another device' });
  });

  test('req_mg_7 accounts classify into identities and ledger accounts', () => {
    const a = '01'.repeat(32);
    const b = '02'.repeat(32);
    const k = '03'.repeat(32);
    const passkey = (d: string) =>
      convertDevice(
        { account: 'acct', deviceId: hexToBytes(d), device: { type: 'WebAuthn', value: record(d, G) } },
        'api.bloque.app',
      );
    const substrate = convertDevice(
      { account: 'keys', deviceId: hexToBytes(k), device: { type: 'SubstrateKey', value: 'HNZata7iMYWmk5RvZRTiAsSDhV8366zq2YGb3tLH5Upf74F' } },
      'api.bloque.app',
    );
    const out = classify([
      { account: 'acct', outcome: passkey(b) },
      { account: 'acct', outcome: passkey(a) },
      { account: 'keys', outcome: substrate },
    ]);
    expect(out.identities.length).toBe(1);
    expect(out.identities[0].open_identity.verifier.startsWith(`0101${a}`)).toBe(true);
    expect(out.identities[0].import_credentials?.credentials.length).toBe(1);
    expect(out.identities[0].import_credentials?.credentials[0].verifier.startsWith(`0101${b}`)).toBe(true);
    expect(out.ledger_accounts).toEqual([
      { kreivo_account: 'keys', substrate_keys: ['HNZata7iMYWmk5RvZRTiAsSDhV8366zq2YGb3tLH5Upf74F'] },
    ]);
    expect(out.unclassified.length).toBe(0);
  });
});
