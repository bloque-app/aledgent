<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-028 — Credential schemes: passkeys and API keys

| | |
|---|---|
| Milestone | M-3 — Production V0 (holders migrating from Kreivo `Pass` hold passkeys today) |
| Track | B |
| Home | `crates/authn` (new), `crates/ledger-core` (`principal`, `custody`, `export`), `services/ingress` (`auth`, `surface`, `config`), `tools/pass-import` (new) |
| Depends on | `F-009`, `F-012`, `F-014`. `DEP-7`: passkeys are holder credentials, so they pass only with its switch on |
| Unblocks | `M-3`: holders reach owned custody with the passkeys they already have |
| Realises | `DEC-19` |

## Goal

A credential's verifier says which scheme proves possession of it. V0 ships three schemes: the existing
key, passkeys (WebAuthn) and API keys. A holder signs in with a passkey and gets a short-lived session.
A service or an agent presents an API key. Either way, the rest of ingress sees the same authenticated
caller it sees today, and the core goes on checking set membership and scope on every call. The passkeys
an operator's holders already registered as Kreivo `Pass` devices are read from the chain and imported into
their identities' sets through `import_credentials` (amendment 0010). Only their public material is read.

## Spec traces

- `REQ-AU-20`: a set per identity; add and remove by a member or a recovery; the holder may limit a
  credential to a subset of accounts; never the last one. The scheme changes none of this.
- `REQ-AU-3`, `REQ-AU-4`, `INV-22`: removal is immediate, also for sessions derived from the credential;
  a credential cannot widen itself.
- `REQ-AU-8`: no secret in a log, URL, query string, error or default. API keys are issued once and kept
  as hashes; sessions and challenges carry no secret in a URL.
- `REQ-AU-13`, `AC-A7.3`, `AC-A7.4`: a reclaim needs a proven holder credential, so a passkey must be able
  to prove itself.
- `REQ-AG-1`, `AC-G2.1`, `AC-G2.2`: an agent has its own credential, which may be an API key.
- `NFR-7`: every route is default-deny, including the two that authenticate.
- `REQ-PN-1`, `REQ-AU-7`: attribution names the credential, whatever its scheme.
- `REQ-MG-7`, `REQ-MG-8`, `US-N1`, `AC-N1.1`–`AC-N1.4` (amendment 0010): the operator imports
  credentials' public material into an identity's set, audited, never while an account of the identity
  is in owned custody.
- Errors: `ERR-CredentialInvalid`, `ERR-NotAuthorised`, `ERR-JustificationRequired`.

The owner ruled on this plan's open questions on 2026-10-02 (review of #110): "Owner rulings", at the end.

## Scope

**In:** the scheme tag on `Verifier` and its shape checks; the `authn` crate; passkey challenge,
assertion and registration at ingress; the accepted relying parties as configuration; durable sign
counts; session tokens; API keys; reclaim proofs by passkey; `import_credentials` (amendment 0010);
export format 7 carrying the scheme; `tools/pass-import`, which classifies Kreivo `Pass` accounts into
identities and ledger accounts.

**Out:** attestation trust (attestation statements are not verified: conveyance `none`); `EdDSA`
passkeys; substrate-key devices as credentials (an account with only those is a ledger account); an
index from a passkey's handle to its credential; holder-held kernel keys (`F-020`).

## Design

### 1. The scheme model

`Verifier` becomes `{ scheme, material }`. The scheme is a tag the export owns, so a tag is never reused.

| Scheme | Tag | Material | Shape checked by the core |
|---|---|---|---|
| `key` | 1 | The 32-byte Ristretto255 public key of CipherClerk's Schnorr signature | None, as today: a malformed key never verifies |
| `webauthn-es256` | 2 | Version; algorithm (ES256, COSE `-7`); handle (32 bytes); SEC1 uncompressed P-256 point (65 bytes); flags (backup eligible); sign count at registration; `rpId` | Every field, lengths and the `0x04` prefix. Ingress checks that the point is on the curve |
| `api-key` | 3 | SHA-256 of the secret (32 bytes) | Length |

- **The handle is `blake2_256(rawId)`.** That is exactly how Kreivo `Pass` names a WebAuthn device
  (`DeviceId`), so a native registration and an import look the same. The raw credential id is never on
  chain, so the handle is all an import can carry.
- **EdDSA is not included.** No `Pass` device can be EdDSA: `pass-authenticators` stores a 91-byte P-256
  SPKI and verifies with `p256` only, and every client requests `alg: -7` only. A later tag can add it.
- **What binds the scheme.** Request digests, the export (format 7) and the recovery approval message
  (`aledgent/approve-recovery/v2`) all bind it, so the same bytes under another scheme are another
  verifier.
- **`key` is unchanged.** Every verifier written before this feature decodes as `key`.

### 2. The WebAuthn flow

**Challenges.** `POST /v1/auth/challenge {purpose}` returns a challenge and its expiry. `purpose` is
`authenticate` or `register`. A challenge is `nonce(16) ‖ expires_at(8) ‖ purpose(1) ‖ HMAC-SHA256`,
keyed by the deployment's authentication key. Any replica can check it. A challenge is used once:
ingress's store records its nonce until it expires, so no replica accepts it twice.
TTL: 120 s. The key comes from `ALEDGENT_INGRESS_AUTHN_KEY`. It has no default: a deployment that
configures passkeys without it refuses to start (`REQ-AU-8`). The route reveals no ledger fact.

**Assertion verification** (`POST /v1/auth/passkey`), in this order. Every failure answers the same
`ERR-CredentialInvalid` with no detail, so there is no oracle:

1. The credential the body names exists, is active, has the `webauthn-es256` scheme, and its identity is
   active (the core's lookup). `blake2_256(rawId)` equals its handle.
2. `authenticatorData` is at least 37 bytes. Its `rpIdHash` equals `SHA-256(rpId)` of the verifier, and
   that `rpId` is one the deployment accepts (§6).
3. Flags: UP is set. UV is set unless the deployment turns it off; Kreivo required UV at registration,
   so imported passkeys have it. BE equals the verifier's. BS is free.
4. `clientDataJSON`: `type` is `webauthn.get`. `challenge` decodes to a challenge this deployment issued
   for `authenticate`, not expired and not seen before. `origin` is one of the origins configured for
   the verifier's `rpId`, by exact match. `crossOrigin` is absent or false.
5. The signature is DER ECDSA P-256 over `authenticatorData ‖ SHA-256(clientDataJSON)`, under the
   verifier's key.
6. **Sign count.** If both the stored count and the presented count are zero, the assertion passes:
   synced passkeys always report zero. Otherwise the presented count must be greater than the stored one,
   or the assertion is refused as a possible clone. The count is **durable**: each sign-in moves it in
   ingress's store, with the verifier's count as its floor, by a write that only ever raises it. Two
   replicas racing on one assertion cannot both win.

**Result.** A session token, derived for this sign-in: `principal ‖ credential ‖ kind ‖ expires_at`, MAC'd
with the same key, TTL 15 min (Kreivo's own session bound). It serves any number of actions until then. It is presented as a bearer token, like any other. It cannot be
renewed: a new session needs a new assertion. Removing the credential ends the session at the next call,
because the core resolves the credential on every command and query (`REQ-AU-3`).

**Where verification runs: ingress (recommended, `DEC-19`).** The alternative is the core. That would put
P-256, JSON parsing and an origin policy into a guest that already exceeds the pinned program limit. It
would also need a clock for challenge expiry and add work to every invocation. Ingress already
authenticates every caller (`NFR-7`), and the core already trusts it to. The core keeps what only it can
decide: membership, status, scope and authority.

**Registration of a new passkey** (`register_credential` with `scheme: webauthn`), by a caller who is a
member of the set (`REQ-AU-20`). Ingress parses `attestationObject` and checks:

- `type` is `webauthn.create`, the challenge was issued for `register`, and the origin and `rpIdHash`
  are accepted;
- UP and UV are set, and so is AT;
- the credential id is shorter than 1023 bytes;
- the COSE key is EC2, P-256, `alg -7`, and on the curve.

It builds the verifier and forwards the command. The attestation statement is not verified, whatever
its format: aledgent trusts the holder's device, not a manufacturer.

### 3. API keys

- **Issue.** `register_credential` with `scheme: api-key`. Ingress draws 32 bytes from the OS, forwards
  only their SHA-256 as the verifier, and returns
  `aledgent_ak1.<credential id, base64url>.<secret, base64url>` once. The received-log holds only the
  hash. A repeat under the same client key returns the credential, not the key: a key lost in transit
  is removed and issued again.
- **Use.** The key is presented as a bearer token. Ingress reads the credential id from it, looks up the
  verifier, and compares hashes in constant time.
- **Scope and kind.** The scope is the set member's scope. Limiting a key to one account is the typical
  use of `REQ-AU-20`'s subset. The kind is `service` if the identity is a configured service principal,
  `agent` for an agent, and `holder` otherwise, which needs the `DEP-7` switch.
- **Rotate and revoke.** Rotation is a new key, then `remove_credential` on the old one: two commands,
  each effective at once. Revocation is `remove_credential`.
- **Never in a log.** The token type has no `Debug`, and the access log keeps no header (`REQ-AU-8`).

### 4. How it maps to `REQ-AU-20`

| Rule | With schemes |
|---|---|
| Add | `register_credential` takes a verifier of any scheme. The presenter is an active member, and the new scope is within the presenter's (`REQ-AU-4`, `INV-22`) |
| Remove | `remove_credential`, unchanged: immediate, never the last member. Sessions and API keys stop at the next call |
| Subset of accounts | The member's `scope`, unchanged and scheme-independent |
| Recovery reissue | `initiate_recovery` carries a verifier of any scheme. For a passkey, the holder's new device runs the registration ceremony first, and its verifier goes into the recovery. The recovery authority's approval binds the scheme |
| Reclaim proof (`REQ-AU-13`) | `key`: the core verifies the signature, as today. `webauthn-es256`: ingress verifies an assertion whose challenge is `reclaim_challenge(account, epoch)` and forwards the proof marked as checked at the boundary. The core accepts that mark only for a passkey. `api-key`: never |
| Import (`REQ-MG-7`, amendment 0010) | `import_credentials`, on the operator surface: one or more verifiers into an existing identity's set, each marked `Imported`, with a justification and an audit record, shown to every authority holder. Refused while any account of the identity is owned (`REQ-MG-8`). An imported passkey still proves itself before owned custody (`AC-N1.4`) |
| Holder-held recovery credential | Any scheme, marked as before |

### 5. Migration from Kreivo `Pass`

**Storage.** `Pass.Devices` is a double map: Kreivo account, then `DeviceId`, then the composite device.

- **Device kinds.** The device is `WebAuthn(CredentialRecord)` or `SubstrateKey(AccountId32)`. The
  variant is the authenticator kind.
- **`CredentialRecord`** holds `device_id` (`blake2_256(rawId)`), `public_key` (91-byte DER SPKI, P-256),
  `sign_count`, `backup_eligible` and `backup_state`.
- **The account** is derived from a hashed user id the registering application chooses.
- **Not on chain:** the raw credential id, the `rpId` and the origin.
- **What the import tool must expect on mainnet:**
  - one device per account, most of them substrate keys;
  - passkeys all ES256, with sign counts at 0;
  - SPKI keys that a client stored truncated: when the SPKI was written with a length prefix
    (`0x6d01`), the last two bytes of y are missing, and Kreivo cannot verify those devices.
    y follows from x up to its sign, so the import tool recovers them.
  - `Pass.RegisteredAccounts` may be empty, in which case the chain does not say whose app registered a
    device.

**The `rpId`.** Kreivo checks `rpIdHash = SHA-256(host of the origin)` once, at registration, and never on
an assertion: it accepts a passkey of any `rpId`. That is deliberate. It is finding `GUV-2` of
Guvenkaya's Pallet Pass review, accepted because binding to one origin would centralise the protocol.
aledgent is private and binds (`DEC-19`).

- **An operator's existing passkeys** may have been registered on a page whose `rp.id` was set to the
  page's exact host (say `api.example.com`). They are bound to that host. aledgent keeps that `rpId`
  accepted, with its origin, for sign-in and import only.
- **New passkeys use the registrable domain** (say `example.com`). That covers both the host where the
  SDK signs in and the host where signup creates the key.
- **Only accepted passkeys are imported.** The import file states each passkey's `rpId`, because the chain
  cannot, and ingress refuses an import whose `rpId` is not configured (`REQ-MG-7`).
- **Fallback.** A holder whose old passkey cannot reach the old host registers a new-domain passkey
  through it there, a member adding a member (`REQ-AU-20`), or recovers (`REQ-AU-15`).

**Classification.** A `Pass` account with a passkey device is an **identity**, and that passkey is its
primary credential. An account whose only device is a substrate key is a **ledger
account** (§2's glossary). It is input to the opening load (`REQ-RC-4`, `DEC-14`), not to any
credential set. Substrate keys are never migrated as credentials.

**Landing.**

- **Through `import_credentials`** (`REQ-MG-7`).
  - **A new identity** is opened under the operator's policy authority (`DEP-8`), with its primary
    passkey as its first credential. Any further passkeys are then imported.
  - **An identity aledgent already holds** takes all its passkeys through the import.
  - **The custody condition.** The identity's accounts are delegated, so the import is allowed
    (`REQ-MG-8`).
- **Not through `F-014`.** `F-014`'s import rebuilds a whole book with identical state, and is not a
  merge. It does carry verifiers with their scheme (format 7), so passkeys survive an export and import.
- **The tool.** `tools/pass-import` is TypeScript on `polkadot-api`.
  It is read-only and never signs. It writes two lists:
  - **identities**, each with the `open_identity` body of its primary passkey and the
    `import_credentials` body of any others. A passkey verifier holds the SEC1 point from the SPKI,
    the handle, the sign count, BE, and the `rpId` from `--rp-id`;
  - **ledger accounts**, each with its substrate keys, for the opening load.
- **Only the operator's accounts.** Run it with `--accounts` and the operator's users' Kreivo addresses,
  so that its `rpId` is stated only for its own devices.
- **Mapping.** The landing step maps each Kreivo account to its aledgent identity or ledger account
  through the opening extract.

### 6. Ingress routes

| Route | Authenticated by | Does |
|---|---|---|
| `POST /v1/auth/challenge` | nothing; reveals no ledger fact | issues a challenge |
| `POST /v1/auth/passkey` | the assertion in its body | verifies it and returns a session |
| any other route | a bearer: configured token, session, or API key | unchanged |
| `POST /v1/credentials` | a member of the set | `register_credential` with `scheme` |
| `POST /v1/accounts/{account}/custody/reclaim` | a member of the set | the proof may be an assertion |
| `POST /v1/ops/identities/{identity}/credentials` | the operator's import authority | `import_credentials` (`REQ-MG-7`) |

**Configuration, not code.**

- **The block.** `passkeys: { relying_parties: [{ rp_id, origins, registrations }],
  require_user_verification, challenge_ttl_ms, session_ttl_ms }`, with the key from
  `ALEDGENT_INGRESS_AUTHN_KEY`.
- **Sign-in and import only.** A party with `registrations: false` accepts sign-ins and imports, but no
  new passkeys.
- **Which party a registration uses.** The one whose `rpId` hashes to the attestation's `rpIdHash`, and
  whose origins include the ceremony's origin.

An example (`deploy/templates/ingress.json` has the same shape):

| `rp_id` | `origins` | `registrations` |
|---|---|---|
| `example.com` | `https://auth.example.com`, `https://api.example.com` | `true` |
| `api.example.com` | `https://api.example.com` | `false` |

**Durable state at ingress.** Ingress's store gains two tables: each passkey's sign count, and the
challenges already used, each kept until it expires.

### 7. Crates, pinned

| Crate | Version | For | Why |
|---|---|---|---|
| `p256` (RustCrypto) | `=0.13.2` | ECDSA P-256 verification, SEC1 and SPKI points | The same crate Kreivo's verifier uses. `no_std`, widely reviewed |
| `coset` (Google) | `=0.3.8` | COSE keys in the attestation | On `ciborium` `0.2.2`. It is also what `pass-authenticators` parses with |
| `sha2`, `hmac`, `blake2` | `0.10`, `0.12`, `0.10.6` | Hashes, challenge and session MACs, the handle | Already in the workspace |
| `base64` | `0.22` | base64url in client data and tokens | Already in the workspace |

`webauthn-rs` was considered and not taken. It links OpenSSL, it would decide the flow for us, and it
does not take Kreivo's `DeviceId` or a SPKI import.

## Test plan

- **`authn`.**
  - Assertions and attestations from a software authenticator, accepted.
  - Each check of §2, broken one at a time and refused: `rpIdHash`, UP, UV, BE, type, challenge (forged,
    expired, wrong purpose, reused), origin, signature, sign count.
  - A real Kreivo `Pass` record (SPKI) converted and verified. Malformed CBOR, COSE and DER refused.
- **Ingress.**
  - Challenge to session to command, end to end.
  - A session refused after `remove_credential` (`REQ-AU-3`).
  - An API key issued once, used, rotated and revoked.
  - Holders refused with the `DEP-7` switch off (`NFR-7`).
  - No secret in the access log or an error (`REQ-AU-8`).
  - Reclaim by passkey (`AC-A7.3`), and a bad or missing proof (`AC-A7.4`).
- **Core.**
  - Shape checks.
  - A scoped API key cannot register a wider credential (`INV-22`, `AC-A5.4`).
  - `import_credentials`: `AC-N1.1`–`AC-N1.4`.
  - Export and import round trip with every scheme (`AC-K5.2`).
  - The recovery approval binds the scheme.
- **`tools/pass-import`.** Decoding of a captured `Pass.Devices` entry; SPKI to SEC1; truncated keys
  recovered; accounts classified into identities and ledger accounts.
- **Store.** Sign counts only rise, and a challenge is used once, on both stores.
- **Conformance.** The new cases are registered in the suite under the ids they name.

## Risks

- **`rpId` lock-in (`RISK-10`).** See §5. The operator must keep the old host able to serve the ceremony.
- **Synced passkeys report a sign count of zero**, so the count detects clones only for device-bound
  ones. Passkeys on Kreivo mainnet report zero today.
- **Trust in ingress.** The core accepts ingress's check of a passkey reclaim proof. That is the same
  trust it already places in ingress's authentication of every command, but it is now written down.
- **An identity with an owned account cannot take an import** (`REQ-MG-8`). Imports run before any account
  of the identity is made owned.
- **The registering application may be inconsistent about its `rpId`** across environments. Imports
  must state the `rpId` the registration code actually used.

## Owner rulings (review of #110, 2026-10-02)

1. **`rpId` and origins.** Kreivo accepting any `rpId` is `GUV-2`, an accepted risk. aledgent binds:
   - a configured set of accepted `rpId`s and origins;
   - imports only from that set;
   - the registrable domain for new passkeys, and the old exact host kept for the existing ones.
2. **Import.** It goes through a spec requirement, not `open_identity`: amendment 0010, `REQ-MG-7` and
   `REQ-MG-8`, Epic N. No `INV-*` forbids it.
3. **API keys never prove a reclaim.**
4. **Durable sign counts, per sign-in.** A sign-in grants a session, a short-lived token derived per
   sign-in, good for several actions.
5. **Substrate-key-only `Pass` accounts are ledger accounts**, input to the opening load, never credentials. Only
   accounts with a passkey are identities, with that passkey as their primary credential.
6. **`OQ-12` / `DEP-7`.** Ingress authenticates both service principals and holder credentials. Amendment
   0009 closes `OQ-12`.
