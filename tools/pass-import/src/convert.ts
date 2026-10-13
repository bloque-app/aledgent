// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
// F-028 (DEC-19): a Kreivo `Pass` WebAuthn device as an aledgent passkey verifier.
//
// Pure functions: no network, no key, no signer. `main.ts` reads the chain; this file turns what it
// read into the verifier bytes the core accepts (`ledger_core::scheme::PasskeyKey::encode`), byte
// for byte. The Rust side pins the same vector (`crates/authn`, test
// `dec_19_the_import_tools_vector_decodes`).

import { p256 } from '@noble/curves/nist.js';

/** The DER prefix of a P-256 SubjectPublicKeyInfo with an uncompressed point (91 bytes in all). */
export const P256_SPKI_PREFIX = Uint8Array.from([
  0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
  0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
]);

/** The verifier's version byte and its one algorithm (ES256), as `PasskeyKey` writes them. */
const PASSKEY_VERSION = 1;
const ALG_ES256 = 1;
const FLAG_BACKUP_ELIGIBLE = 0b0000_0001;

export function hexToBytes(hex: string): Uint8Array {
  const h = hex.startsWith('0x') ? hex.slice(2) : hex;
  if (h.length % 2 !== 0 || !/^[0-9a-fA-F]*$/.test(h)) {
    throw new Error('not hex');
  }
  const out = new Uint8Array(h.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = Number.parseInt(h.slice(2 * i, 2 * i + 2), 16);
  return out;
}

export function bytesToHex(b: Uint8Array): string {
  return Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
}

/** Bytes from whatever polkadot-api hands back for a byte array: hex, a Uint8Array, or a Binary. */
export function bytesOf(v: unknown): Uint8Array {
  if (v instanceof Uint8Array) return v;
  if (typeof v === 'string') return hexToBytes(v);
  if (v && typeof v === 'object') {
    const o = v as { asBytes?: () => Uint8Array; asHex?: () => string };
    if (typeof o.asBytes === 'function') return o.asBytes();
    if (typeof o.asHex === 'function') return hexToBytes(o.asHex());
  }
  throw new Error('not a byte array');
}

/** The SEC1 uncompressed point of a 91-byte P-256 SPKI, checked to be on the curve. */
export function sec1FromSpki(spki: Uint8Array): Uint8Array | null {
  if (spki.length !== P256_SPKI_PREFIX.length + 65) return null;
  for (let i = 0; i < P256_SPKI_PREFIX.length; i++) {
    if (spki[i] !== P256_SPKI_PREFIX[i]) return null;
  }
  const point = spki.slice(P256_SPKI_PREFIX.length);
  if (point[0] !== 0x04) return null;
  try {
    p256.Point.fromBytes(point).assertValidity();
  } catch {
    return null;
  }
  return point;
}

/**
 * The point of a key some clients stored truncated: their codec wrote the 91-byte SPKI as a
 * length-prefixed byte string (`0x6d01`, compact 91) into Pass's fixed 91-byte field, so the field
 * holds the prefix, the SPKI header, `0x04`, x, and only the first 30 bytes of y. y follows from x
 * up to its sign, so the point is recovered: of the two points with this x, the one whose y starts
 * with the 30 stored bytes. `null` if neither does.
 */
export function sec1FromTruncatedSpki(field: Uint8Array): Uint8Array | null {
  if (field.length !== 91 || field[0] !== 0x6d || field[1] !== 0x01) return null;
  for (let i = 0; i < P256_SPKI_PREFIX.length; i++) {
    if (field[2 + i] !== P256_SPKI_PREFIX[i]) return null;
  }
  if (field[28] !== 0x04) return null;
  const x = field.slice(29, 61);
  const yHead = field.slice(61, 91);
  for (const tag of [0x02, 0x03]) {
    const compressed = new Uint8Array(33);
    compressed[0] = tag;
    compressed.set(x, 1);
    try {
      const point = p256.Point.fromBytes(compressed).toBytes(false);
      if (point.slice(33, 63).every((b, i) => b === yHead[i])) return point;
    } catch {
      // Not a point: try the other sign.
    }
  }
  return null;
}

/** A lower-case DNS name of 1 to 253 bytes, as `ledger_core::scheme::valid_rp_id` requires. */
export function validRpId(rpId: string): boolean {
  if (rpId.length === 0 || rpId.length > 253) return false;
  return rpId
    .split('.')
    .every(
      (l) => l.length > 0 && l.length <= 63 && /^[a-z0-9-]+$/.test(l) && !l.startsWith('-') && !l.endsWith('-'),
    );
}

/** A passkey's public material, as `PasskeyKey` holds it. */
export interface PasskeyKey {
  /** blake2_256(rawId): Kreivo Pass's DeviceId. */
  handle: Uint8Array;
  /** SEC1 uncompressed P-256 point. */
  publicKey: Uint8Array;
  rpId: string;
  backupEligible: boolean;
  signCount: number;
}

/** `PasskeyKey::encode`: version, algorithm, handle, point, flags, counter (big-endian), rpId. */
export function encodePasskey(k: PasskeyKey): Uint8Array {
  if (k.handle.length !== 32) throw new Error('a handle is 32 bytes');
  if (k.publicKey.length !== 65 || k.publicKey[0] !== 0x04) throw new Error('a SEC1 point is 65 bytes');
  if (!validRpId(k.rpId)) throw new Error(`rpId ${k.rpId} is not a lower-case DNS name`);
  if (!Number.isInteger(k.signCount) || k.signCount < 0 || k.signCount > 0xffff_ffff) {
    throw new Error('a sign count is a u32');
  }
  const rp = new TextEncoder().encode(k.rpId);
  const out = new Uint8Array(2 + 32 + 65 + 1 + 4 + 1 + rp.length);
  let at = 0;
  out[at++] = PASSKEY_VERSION;
  out[at++] = ALG_ES256;
  out.set(k.handle, at);
  at += 32;
  out.set(k.publicKey, at);
  at += 65;
  out[at++] = k.backupEligible ? FLAG_BACKUP_ELIGIBLE : 0;
  new DataView(out.buffer).setUint32(at, k.signCount, false);
  at += 4;
  out[at++] = rp.length;
  out.set(rp, at);
  return out;
}

/** One `Pass.Devices` entry, as read: the account, the device id, and the composite device. */
export interface DeviceEntry {
  account: string;
  deviceId: Uint8Array;
  /** The composite device enum: `{ type, value }`. */
  device: { type: string; value: unknown };
}

/** What one device came to. */
export type Outcome =
  | {
      kind: 'passkey';
      deviceId: string;
      /** The key was stored truncated on chain and recovered (`sec1FromTruncatedSpki`). */
      recovered: boolean;
      verifier: string;
      signCount: number;
      backupEligible: boolean;
      backupState: boolean;
    }
  | { kind: 'skipped'; deviceId: string; authenticator: string; key: string }
  | { kind: 'malformed'; deviceId: string; reason: string };

/** A WebAuthn `CredentialRecord`'s fields, as polkadot-api decodes them. */
interface CredentialRecord {
  device_id: unknown;
  public_key: unknown;
  sign_count: number;
  backup_eligible: boolean;
  backup_state: boolean;
}

/**
 * One device to an outcome. Only WebAuthn devices are passkeys. A substrate-key device is never a
 * credential: an account with only those is a ledger account (the owner's ruling, 2026-10-02).
 */
export function convertDevice(e: DeviceEntry, rpId: string): Outcome {
  const deviceId = bytesToHex(e.deviceId);
  if (e.device.type.toLowerCase() !== 'webauthn') {
    const v = e.device.value;
    const key = typeof v === 'string' ? v : (() => {
      try {
        return `0x${bytesToHex(bytesOf(v))}`;
      } catch {
        return '';
      }
    })();
    return { kind: 'skipped', deviceId, authenticator: e.device.type, key };
  }
  // `Dev<CredentialRecord, …>` is a one-field wrapper; polkadot-api may hand either back.
  const v = e.device.value as CredentialRecord | [CredentialRecord];
  const rec: CredentialRecord = Array.isArray(v) ? v[0] : v;
  try {
    const recordId = bytesOf(rec.device_id);
    if (bytesToHex(recordId) !== deviceId) {
      return { kind: 'malformed', deviceId, reason: 'the record names another device' };
    }
    const field = bytesOf(rec.public_key);
    const whole = sec1FromSpki(field);
    const point = whole ?? sec1FromTruncatedSpki(field);
    if (!point) return { kind: 'malformed', deviceId, reason: 'not a P-256 SPKI on the curve' };
    const verifier = encodePasskey({
      handle: recordId,
      publicKey: point,
      rpId,
      backupEligible: rec.backup_eligible,
      signCount: rec.sign_count,
    });
    return {
      kind: 'passkey',
      deviceId,
      recovered: whole === null,
      verifier: bytesToHex(verifier),
      signCount: rec.sign_count,
      backupEligible: rec.backup_eligible,
      backupState: rec.backup_state,
    };
  } catch (err) {
    return { kind: 'malformed', deviceId, reason: (err as Error).message };
  }
}

/** The most credentials one import carries: Pass's and the core's own bound. */
export const MAX_IMPORTED_CREDENTIALS = 100;

/**
 * An identity (the owner's ruling, 2026-10-02): a `Pass` account with a passkey. Its first passkey
 * is its primary credential, the verifier `open_identity` opens it with; any others are imported
 * with `import_credentials` (amendment 0010, `REQ-MG-7`). For an identity aledgent already holds,
 * every passkey goes through the import.
 */
export interface IdentityEntry {
  kreivo_account: string;
  /** The body of `POST /v1/identities`: the primary passkey. */
  open_identity: { kind: 'person'; scheme: 'webauthn-es256'; verifier: string };
  /** The body of `POST /v1/ops/identities/{identity}/credentials` for the other passkeys, if any. */
  import_credentials: {
    credentials: Array<{ scheme: 'webauthn-es256'; verifier: string }>;
    justification: string;
  } | null;
  devices: Outcome[];
}

/**
 * A ledger account (the glossary's): a `Pass` account whose only devices are substrate keys. It is
 * input to an initial opening load, never a credential set.
 */
export interface LedgerAccountEntry {
  kreivo_account: string;
  substrate_keys: string[];
}

/** What the accounts came to. */
export interface Classified {
  identities: IdentityEntry[];
  ledger_accounts: LedgerAccountEntry[];
  /** Accounts with neither a usable passkey nor a substrate key: every device malformed. */
  unclassified: Array<{ kreivo_account: string; devices: Outcome[] }>;
}

export const IMPORT_JUSTIFICATION = 'Migration of Kreivo Pass passkeys (F-028, REQ-MG-7)';

/** Classify the accounts, in account order, devices in id order. */
export function classify(entries: Array<{ account: string; outcome: Outcome }>): Classified {
  const by = new Map<string, Outcome[]>();
  for (const { account, outcome } of entries) {
    const list = by.get(account) ?? [];
    list.push(outcome);
    by.set(account, list);
  }
  const out: Classified = { identities: [], ledger_accounts: [], unclassified: [] };
  for (const account of [...by.keys()].sort()) {
    const devices = (by.get(account) ?? []).sort((a, b) => a.deviceId.localeCompare(b.deviceId));
    const passkeys = devices.filter(
      (d): d is Extract<Outcome, { kind: 'passkey' }> => d.kind === 'passkey',
    );
    const keys = devices.filter(
      (d): d is Extract<Outcome, { kind: 'skipped' }> => d.kind === 'skipped',
    );
    const [first, ...rest] = passkeys;
    if (first) {
      const more = rest.slice(0, MAX_IMPORTED_CREDENTIALS);
      out.identities.push({
        kreivo_account: account,
        open_identity: { kind: 'person', scheme: 'webauthn-es256', verifier: first.verifier },
        import_credentials:
          more.length > 0
            ? {
                credentials: more.map((d) => ({ scheme: 'webauthn-es256', verifier: d.verifier })),
                justification: IMPORT_JUSTIFICATION,
              }
            : null,
        devices,
      });
    } else if (keys.length > 0) {
      out.ledger_accounts.push({
        kreivo_account: account,
        substrate_keys: keys.map((k) => k.key),
      });
    } else {
      out.unclassified.push({ kreivo_account: account, devices });
    }
  }
  return out;
}
