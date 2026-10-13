<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: key rotation

`NFR-8`, `REQ-SA-8`, `REQ-AU-3`, `REQ-AU-8`, guardrail 13 · **Owner:** security, with platform. **Lost**
(not rotated) keys: [key-loss](../key-loss.md).

**The rules that do not bend:**

- A secret never appears on a command line, in a log, a URL, a file in a repository, or an image.
- A new key is created in its boundary: a field of the signer's Vault secret (generated on a trusted
  terminal straight into `vault write`, never echoed, never in shell history), or an AWS KMS key for a
  lane opted into KMS. Holders' passkeys are theirs and never rotated here.
- **The signer rotates online** (`F-035`): `kill -HUP` makes it re-read its configuration whole. A file
  that does not load leaves the old configuration in place (its log says `reload refused`). A token or a
  grant removed is refused on the caller's very next request, even on a connection already open.
- Rotation never edits a store (`REQ-AT-3`).

## Preconditions

- [ ] A reason is recorded: scheduled, staff change, or suspected exposure. A suspected exposure is
  also an incident, under the operator's incident process.
- [ ] The latest backups are verified.
- [ ] For a contract-side change, the contract's admin key is available on the operator's terminal.

## Steps, by key

In the steps below, `<settle secret>` is the Vault secret the adapter's signer reads, and `<core secret>`
the one the core's signer reads; both are named in the signer's configuration (`signer.json`).

| Key | Steps | Online? |
|---|---|---|
| **Lane key** (PASS device) | (1) Write the new secret as a new field of `<settle secret>` (`lane-1-v2`) — or, for a lane opted into KMS, create a new `ECC_SECG_P256K1` key and grant the adapter's task role `kms:Sign` and `kms:GetPublicKey` on it. (2) Add it to the adapter's signer as a new key id (`{"id": "lane-1-v2", "scheme": "secp256k1", "source": {"vault": {"path": "<settle secret>", "field": "lane-1-v2"}}}`, or `{"kms": {"key_id": …}}`), grant it to the `settle` client, and `kill -HUP` the signer (it reads Vault again). (3) Register it as a device of the same PASS account (`add_device`). (4) In `settle.json`, point the lane's device at `{"signer": {"key": "lane-1-v2"}}` and restart the adapter. (5) Once the old device has no extrinsic in flight (the adapter's next reads settle each one), remove the old device, remove the old key id from the signer (`kill -HUP`), and remove the old field (or schedule the old KMS key for deletion) | Yes. Withdrawals wait a step |
| **Anchor key** | As for a lane key, on the anchor PASS account. One extrinsic of the anchor key is in flight at a time; rotate between anchors | Yes |
| **Lane under compromise** | **First** the contract's admin calls `set_keys` without that PASS account, or with **no keys at all**: every call stops, and nothing moves. Then rotate as above and restore the key set | No: external legs pause |
| **Clerk key** (anchors, disclosures) | (1) Write a new clerk key (a canonical Ristretto scalar) as the field `clerk-v2` of `<core secret>`, add it to the core's signer as `clerk-v2`, granted to `core`; `kill -HUP`. (2) The contract's admin calls `set_keys(keys, Some(new attestation key))` (the new key's public half: `aledgent-signer public <config> clerk-v2`). (3) Set `core_keys.clerk` to `clerk-v2` and restart the core host. (4) Verify with `aledgent-verify` that the record shows it ([anchoring](../anchoring.md)). (5) Remove `clerk` from the signer and its field from Vault. (6) Tell recipients: a disclosure signed under the old key verifies only against a record read before the change | Yes; disclosures pause for the core host's restart |
| **Key-ring root** | **Not rotatable.** It derives every kernel account key and blinding, so a new root is a new book: a re-genesis, which `NFR-17` forbids. Exposure is a security incident for the owner | — |
| **Agent authority and package keys** | **No rotation procedure is built**: both are bound into the Agent's identity (its descriptor's authority key; the runtime package's producer). They live in the signer, never in the core host, so exposure needs the signer's host. Exposure is a security incident for the owner | — |
| **Signer token** (a caller's: the core host's, the adapter's) | (1) Generate a new token in Vault (32 bytes or more of randomness). (2) Add its SHA-256 (`aledgent-signer token-sha256 <file>`) as a **second** entry of the caller's `token_sha256` in the signer's configuration; `kill -HUP`. (3) Write the new token to the caller's token file and restart the caller. (4) Remove the old hash; `kill -HUP`. The old token is refused on its next request | Yes; the caller restarts once |
| **Core link token** (ingress → core host) | (1) Generate a new token in Vault. (2) Add its SHA-256 as a second entry of `core_link.token_sha256` and restart the core host. (3) Set `ALEDGENT_CORE_LINK_TOKEN` to the new token and restart ingress. (4) Remove the old hash and restart the core host | Yes; decisions pause for each restart, acknowledgements continue |
| **The signer's Vault access** (its IAM-bound Vault roles) | Nothing to rotate in the signer: it holds no Vault secret, and each login's token lasts 60 s. To cut a signer off, unbind its IAM role from the Vault role it logs in with; it keeps the keys it has read until it restarts | Yes |
| **Bearer token** (operator, adapter, ops, a party) | (1) Generate a new token in Vault. (2) Add its SHA-256 as a **second** entry in `ingress.json` for the same principal and credential, and restart ingress. (3) Switch the client. (4) Remove the old entry and restart | Yes |
| **API key or credential** of a principal | Through the command surface: issue a new one, then `POST /v1/credentials/{credential}/remove`. Revocation takes effect within `NFR-20` | Yes |
| **Ingress authn key** | Set the new value in Vault and restart ingress. Every passkey session and outstanding challenge ends; holders sign in again | Yes, with sign-ins |
| **Ingress sealing key** | Rotate in Vault. Recipients re-register their notification secrets (`aledgent-ingress recipient`). The book is unchanged | Yes |
| **Backups' sealing key** | A new key in KMS, then **take a core backup at once** under it. Keep the old key until every backup sealed under it has expired | Yes |
| **Database role password** | Rotate on the database server and in Vault, then restart the process | Brief reconnection |
| **Recovery key** | `services/recovery/RUNBOOK.md` ("The key"). Change the core's configuration through its reviewed-change process. Pending recoveries need new approvals | Yes |
| **Relay transport key** (outcome A) | Draw a new host key and re-pin it at the peers. Sessions resume from `Hello` | Yes |

## Verification

- [ ] The adapter starts with its assertions passing (each key is a device of a PASS account in the
  contract's key list) and declares `serving`. A withdrawal or an anchor goes final under the new key.
- [ ] The signer's log shows `configuration reloaded`, then `key loaded` for every key with its origin
  (`vault`, or `kms` for an opted-in lane; never `file` in production), and `signed` lines under the new key id only.
- [ ] The old credential or token is refused: one request, expecting `401` or `ERR-NotAuthorised` (ingress),
  `refused … not authenticated` in the signer's log (a signer token), or `core link: a connection refused`
  in the core host's log (the link token).
- [ ] Clerk key: an internal disclosure verifies offline with `aledgent-verify` against the new record.
- [ ] No secret appears in any log during the change.

## Rollback

Keep the old key until verification passes. Each step above is reversed by restoring the previous
handle, entry or key set. A **compromised** key is never restored: stay paused (keys emptied) until the
new key verifies.

## Exercised

On every change: `services/signer/tests/signer.rs` (`nfr_8_tokens_and_keys_rotate_online`) rotates a
token and a key on a running signer. The contract-side steps are not exercised automatically yet; they are
exercised by hand in a staging deployment.
