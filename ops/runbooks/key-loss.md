<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: key loss

`F-018` T-018-08 · `NFR-8`, `REQ-SA-8`, `REQ-AU-8`, guardrail 13

**Owner:** security, with platform. **Rules that do not bend:** key material never lives in the process
that holds the book, and never in these backups (`ops/backup/schedule.toml`, `excluded.keys`). A lost key
is recovered from Vault's own backup (the signer's keys, `F-035`) or the KMS's (an opted-in lane, the
backups' sealing key), never rebuilt from the book, and never replaced by an
edit of any store (`REQ-AT-3`).

## Which key

| Key | Holds | If lost |
|---|---|---|
| The core's key-ring root (the signer's `keyring`, in Vault, `F-035`; `ALEDGENT_CORE_ROOT` for the in-process core only) | every kernel account key, the registrar key, every blinding and signing nonce (`F-001`) | Restore its Vault secret from Vault's backup. Without it the book cannot be imported (`F-014` refuses foreign keys) and no posting can be signed. There is no other way back: the book is unreadable and unwritable, by design. |
| A settlement lane's signer (`F-016`) | one nonce lane on the settlement layer | Stop that lane: remove it from the adapter's configuration and restart the adapter; other lanes continue. Register a new key for the instance on the layer (the pallet's admin), then add the lane. An extrinsic already signed under the lost key is still on the layer or provably not: the adapter's next reads settle it. |
| The backups' sealing key (`ALEDGENT_OPS_SEALING_KEY`) | the openings of every core backup | Restore it from the KMS. Without it, every core backup's openings stay sealed and no core backup can be restored: the live core keeps running from its own journal, and a new core backup under a new sealing key must be taken at once. Until one is, a lost core is restored by the received-log's full replay from genesis: its journal rebuilt from the log alone (`ops::restore::rebuild_journal`), slower than a backup and needing the log from its first row. |
| The clerk key (the signer's `clerk`, in Vault, `F-035`; `ALEDGENT_CLERK_KEY` for the in-process core only; `F-017`) | the signature on every disclosure | Restore its Vault secret from Vault's backup; the instance's published attestation key is unchanged and disclosures verify again. If it cannot be restored, generate a new one into the signer's Vault secret, publish its public key on the instance record (`anchoring.md`, "Setting the instance's attestation key"), and tell recipients that disclosures signed under the old key verify only against a record read before the change. |
| The audit store's writer credential | appending to the audit trail | Reading needs none. Initialise a new writer credential only through the audit store's own procedure; never edit its files. |
| Ingress's recipients' sealing key | notification secrets at rest | Recipients re-register their secrets (`aledgent-ingress recipient`). Nothing in the book changes. |

## Procedure

1. Page security. Rotate every credential that could have been exposed with the key (`REQ-AU-3` takes
   effect at once).
2. Restore the key from Vault's backup into the signer's Vault secret (or the KMS key, for an opted-in
   lane), and `kill -HUP` the signer — never into an environment, never onto a disk the book's backups
   reach, never onto a command line.
3. Verify: for the core root, `opsctl restore core` on an isolated host against the latest backup passes
   (`restore.md`); for a lane, the adapter's start-up assertions pass and it declares `serving`.

## Exercised

The refusals are exercised in CI: `F-014`'s import refuses foreign keys
(`crates/ledger-core/tests/export_import.rs`); a core backup does not open under another sealing key and
holds no key or blinding in the clear (`ops/tests/restore.rs`); the adapter's key hygiene
(`services/settle/tests/key_hygiene.rs`). A KMS restore runs in staging: deferred.
