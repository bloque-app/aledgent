<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-044 — The archive sealed by aledgent (Vault Transit and an offline recovery key)

| | |
|---|---|
| Milestone | M-3 — Production V0 (the archive is `NFR-4`'s backup; nothing here is on `M-3`'s critical path) |
| Track | B |
| Home | `crates/core-archive` (the seal, its age code, the spec, the archiver's read-back), `.github/workflows/ci.yml` (one line in the `vault transit (F-042)` job). Follow-ups in `ops/` (runbooks, `opsctl archive copy`, the restore's Vault login) and the infrastructure repository (bucket, Transit key, copy) |
| Depends on | `F-005` (`T-005-15`–`T-005-18`, the archive), `F-042` (Vault Transit on test and prod), infrastructure#26 (archive decoupled from the node) |
| Realises | `NFR-4`, `NFR-10`, `NFR-22`, `NFR-21` (c), `REQ-AU-8` |
| Status | **Proposed, and built in the pull request that carries this file.** The decisions D1–D9 at the end are proposals for the owner; the code follows the recommended option of each and changes if a ruling differs. Owner rulings already made and followed: object lock 180 days for now, retention forever (David, 2026-10-25). No `SPEC.md` change: the spec has no requirement on how backups are encrypted, and `NFR-8` / `REQ-AR-3` concern settlement-layer key material, which this does not touch. It is an amendment to the plan: `F-005`'s "S3 with Object Lock and KMS" becomes "S3 with Object Lock, sealed by aledgent, KMS optional" (a row for `F-044` in `PLAN.md` is left to the merge, see T-044-09) |

## Why

Today every archived object is encrypted by S3 under the deployment's AWS KMS key (SSE-KMS). Two facts make
that the wrong place for the archive's only encryption:

1. **The archive is kept forever (`NFR-22`) and locked; the KMS key is not.** A KMS key scheduled for deletion
   (it nearly happened: an apply of `main` during infrastructure#26's window would have scheduled it) makes every
   locked object unreadable, and Object Lock then guarantees the unreadable bytes are kept. The archive's
   readability must not depend on one cloud key's lifecycle.
2. **Whoever can read the bucket with `kms:Decrypt` reads the book.** SSE-KMS is transparent to any principal
   with both grants; the archive's confidentiality is then a property of two IAM policies in one account.

So aledgent seals each object itself, before it leaves the core host, to two independent ways of opening it:
a **Vault Transit key** (operational restores, audited by Vault, never exported) and an **offline recovery
key** (break-glass: opens the archive with no Vault, no AWS and nothing of aledgent's). KMS may stay as an
outer layer; nothing depends on it.

**What it protects against:** reading the bucket (a leaked AWS role, a policy mistake, a copy of the bucket);
the loss of the KMS key; the loss of Vault (the recovery key); the loss of the recovery key (Vault).
**What it does not:** the core host's own disk holds the whole book in plaintext (`archive.bin` and
`replies.jsonl` from genesis), so a compromised node is out of scope here; and anyone with the public keys
can write an object that opens. Integrity is, as before, the hash chain, the anchors and `fetch`'s checks
(`T-005-16`), not the seal.

## The design

### Where it sits

A layer over any object store (`core_archive::seal::Sealed`, under `Archive`). It seals the exact bytes the
store would have held, under the same name. **Records, their format (2) and their hash chain do not change**:
`prev` is still the SHA-256 of the plaintext record, `fetch` checks a sealed archive exactly as an unsealed
one, and a resealed copy of today's archive keeps its chain. `core_archive::open(spec, access)` builds the layer
from the spec, so `serve.rs`, `chainspec.rs` and `opsctl` need no code change.

### The envelope (format 1)

```text
aledgent-seal/v1\n
{"name":"segments/…seg","bytes":N,"sha256":"…","clear":K}\n     the seal line
<the first K bytes of the plaintext, in clear>
<an age v1 file of the whole plaintext>
```

The age file (`age-encryption.org/v1`, C2SP) carries:

| Stanza | What it is | Who opens it |
|---|---|---|
| `-> aledgent-transit <key> <version>` | the 16-byte file key, RSA-OAEP-SHA-256 (MGF1-SHA-256) to the public half of Transit key `<key>` at `<version>` | Vault's `<mount>/decrypt/<key>`, as `vault:v<version>:<base64>` (verified against Vault 2.0.3) |
| `-> X25519 <share>` (one per recovery key) | the file key, exactly as `age` wraps it | the recovery identity, with the stock `age` tool |
| `-> aledgent-seal <sha256 of the seal line>` | nothing; it is there so the header's MAC (keyed by the file key) covers the seal line | — (`age` ignores it) |

Then the payload: a fresh 16-byte nonce and STREAM over ChaCha20-Poly1305 in 64 KiB chunks, as `age` writes it.
The code is ours (`core_archive::age`, about 450 lines, RustCrypto primitives), checked against the reference
Rust implementation (rage) both ways in the tests; and the Go reference CLI (`age` v1.2.1) opened a sealed
segment's age part, cut with the recipe below, with the recovery identity (2026-10-26, by hand).

- **Versioned header:** the magic line (`aledgent-seal/v1`) and age's own version line.
- **Key id and version:** the Transit stanza's arguments; an X25519 stanza names no key, and a reader tries each
  recovery identity it holds.
- **Nonce:** age's per-file payload nonce; the stanza nonces are age's.
- **AEAD:** ChaCha20-Poly1305 (payload, X25519 wrap); RSA-OAEP for the Transit wrap; HMAC-SHA-256 over the
  header.
- **Read** (with a key): the seal line's name must be the name asked for; the header must bind the seal line; the MAC must
  verify; the plaintext must have the seal line's length and SHA-256 and begin with the clear bytes.
- **Recovery with stock tools** (no aledgent binary): `n=$(head -2 obj | wc -c); k=$(sed -n 2p obj | jq .clear);
  tail -c +$((n+k+1)) obj | age -d -i recovery.txt > record`.

### In clear, and why

The writer (the core host) reads its archive back in three places: the archiver's resume after a restart (the
last segment's header and digest, and the last checkpoint's header), the write-once comparison after a refused
put, and `verify-genesis --archive` (the report kept beside the chainspec). It holds no key that opens the
archive (D2), so what it reads back is kept **in clear**:

| Object | In clear | Sealed only |
|---|---|---|
| segment | its record header: the book, the offsets and SHA-256 of both payloads, the restore point (input and entry numbers, log head, kernel, service and audit roots, times, byte counts), when it was written | the log's entries and the replies |
| checkpoint | its record header (the same kind of fields) | the Agent's state |
| `chainspec/<digest>.verified.json` | all of it: verified, the digest, chunk and record counts, the roots, a duration | — |
| `chainspec/<digest>.bin` and anything else | nothing | all of it |

**What a keyless reader can trust of it.** For a reader with a key, the clear bytes are authenticated: the header's
MAC covers the seal line, and the decrypted plaintext must have the seal line's SHA-256 and begin with the clear
bytes. **For the keyless writer they are not.** A segment's or checkpoint's clear header is taken as the store
returns it, and so is the seal line beside it (`Sealed::head` checks only its name and bounds); anyone who can
write an object could write any seal line and any header. The whole of a `verify-genesis` report is in clear,
and a keyless read checks it against the seal line's SHA-256, which catches corruption but not a forgery, since
that seal line is no more authenticated. What keeps these bytes honest for the writer is the store: write-once
(`If-None-Match`), Object Lock, and a writer role that is the only one allowed to put.

What the archiver's resume does catch, with no key: the clear header's book must be this host's; the bytes of
`archive.bin` and `replies.jsonl` at the header's offsets must have the header's SHA-256 (else "not this host's
history" and the archiver stops writing); and the local logs must not be shorter than the header's ends. What it
does not catch: a header that matches the local logs but carries another digest (the seal line's SHA-256, taken
as the segment's) or another restore point. The archiver then continues the chain from the wrong digest; nothing
is lost on the node, and `fetch` (which decrypts) refuses the chain at that segment at the next restore or
drill, so the damage shows there, not at write time. Closing that gap cheaply: the writer keeps the last
segment's number and digest in a local file and compares it on resume (not built; a fresh node after a restore
has no such file, so `restore` would lay it out).

> **Trade-off, stated plainly.** A reader of the bucket learns the archive's volume and rhythm (inputs and
> entries per segment, when each was written, byte counts) and the roots, which anchoring publishes anyway.
> Object sizes and S3 timestamps already show most of it. They learn no identity, account, amount or reply.
> The alternative that hides the headers is a local write-ahead file of the last heads, laid out by `restore`
> too (D2 (b)); it adds a file whose loss blocks the archiver until a restore, and a state the drill must cover.

### Transit: the public key, not `datakey` or `encrypt`

The writer wraps file keys to the **public half** of a Transit `rsa-4096` key (pinned name, version and PEM in a
seal file of public keys). Vault only ever decrypts. Rejected:

- **`encrypt` of each object:** a segment is up to 64 MiB, over Vault's default 32 MiB request; the whole book
  would pass through Vault; and Vault would be on the write path.
- **`datakey` per object or per epoch:** Vault on the core host's write path, and a Vault role and token on the
  core host. `F-042` keeps the core host away from Vault (its metadata service is locked to it, and its build has
  no Transit engine). A Vault outage would stall the archive.

With the public key, the archive keeps writing through a Vault outage, and a stolen core host token opens
nothing in the archive.

### File keys by epoch

A writer seals under one random file key for an **epoch**: at most 4096 objects or one hour, and a new one at
every start. Each object still has its own payload nonce, so its own payload key. A reader remembers each file
key it opened (by the digest of the wrap stanzas), so a restore asks Vault **once an epoch, not once a
segment**: the 2026-10-15 drill read 64.8k segments, which is 64.8k RSA-4096 decryptions against the Vault the
signers use for every signature (`F-042`), at `fetch`'s 32 concurrent reads. An epoch's file key opens that
epoch's objects; whoever has it already had Vault or the recovery key.

### Write once

Sealing is randomised, so an exact retry seals different bytes and the store refuses it (S3's 412, the
directory's existing name). The layer then reads what is there and compares **seal lines**: the same name,
length and SHA-256 is the same object (`created: false`); anything else is a conflict, as before. An object
written before the seal is compared byte for byte.

### Reading

A reader opens with whichever it is given, recovery identities first (local), then Transit:

- `vault=https://…&transit-mount=<mount>` in the spec, and the token in the file `ALEDGENT_ARCHIVE_VAULT_TOKEN_FILE`
  names (kept fresh by Vault Agent's token sink; read again before every call, and again once after a 403);
- the recovery identity file `ALEDGENT_ARCHIVE_RECOVERY_IDENTITY` names (an `age-keygen` file).

Neither key is ever in the spec, the environment itself, a log or an error (`REQ-AU-8`). **A writer is given
neither**, whatever the environment names (`open` gives `Access::Write` no keys): the core host never holds a
key or a token that opens the archive. Vault not answering is
`Unavailable` (retried); Vault refusing is `Sealed` (not retried), naming the key and version, never the token.
With neither key, only what is in clear reads, and anything else says what would open it.

Objects written before the seal read as they are (`unsealed=accept`, the default) or are refused
(`unsealed=refuse`, for the new bucket once the copy is checked, D6).

### The spec

```text
s3://<bucket>/<prefix>?region=<r>[&kms-key-id=<k>]&lock-mode=compliance&retain-days=180
    &seal=/etc/aledgent/archive-seal.json                  writer: seal to these public keys
    &vault=https://vault…&transit-mount=<mount>            reader: Vault Transit
    &unsealed=accept|refuse
fs:<dir>[?seal=…&vault=…&transit-mount=…&unsealed=…]
```

A writer over S3 needs Object Lock and **the seal or KMS** (both are allowed). Requiring the seal outright is
D5: it would make a core release depend on the node change.

The seal file (public keys only; the writer refuses it without a recovery key, with an RSA key under 3072 bits,
or with an unknown field):

```json
{ "transit": { "key": "archive", "version": 1, "public_key": "-----BEGIN PUBLIC KEY-----…" },
  "recovery": ["age1…"] }
```

### Object Lock

The default retention is **180 days, compliance** (David, 2026-10-25, relaxing 400). Retention stays forever
(`NFR-22`): no lifecycle rule deletes current versions. If the archive enters PCI DSS scope, 365 days
(requirement 10.5.1); Colombia's ten years later. Each is `retain-days` in the spec, not a code change.

**The running deployment is unchanged by this release.** Its node passes `kms-key-id`, `lock-mode=COMPLIANCE`
and `retain-days=400` explicitly (infrastructure, `app_aledgent_node.tf`, from the `services` outputs), and no
seal option, so it keeps writing as today: the layer passes objects through unsealed. The new default only
applies to a spec without `retain-days`. The old bucket's policy also denies any PUT not under its KMS key
(`ArchiveKeyOnly`), so a sealed writer into the old bucket must keep `kms-key-id`; the new bucket has no such
statement (D9).

### Rotation

- **Transit:** `rotate` gives version N+1. The operator puts its public key and version in the seal file and
  restarts the core host (the next start's epoch uses it). Objects sealed to N keep naming N. **Vault must keep
  every version decryptable:** `min_decryption_version` is never raised, the key is never deleted
  (`deletion_allowed=false`); the test shows that raising it loses the old versions through Vault.
  `auto_rotate_period` stays off: the writer pins a version.
- **Recovery key:** a new `age1…` in the seal file; objects already sealed open only with the identity they were
  sealed to, so **every recovery identity ever used is kept**, as long as the archive (forever). Locked objects
  cannot be re-wrapped in place.
- **Either key lost:** the other opens everything. Both lost: the archive is unreadable, and locked.

### Objects already written (SSE-KMS)

They stay readable as they are (the layer passes unsealed objects through). The move to the new bucket copies
them **resealed**, under the same names: `core_archive::seal::copy` reads each object of the old archive (with a
role that has `kms:Decrypt`), seals it, publishes it, and decrypts it back and compares it byte for byte; any
read-back that fails is an error. Run again, it skips what is there: with a reader key (Transit or the recovery
identity) it decrypts and checks each such object too; without one it lists them as **unverified**. A source
that already holds sealed objects is refused (read it through a `Sealed` reader), so nothing is sealed twice.
**Retiring the old bucket requires a run with a reader key that leaves no object unverified**, then the drill. The new bucket then holds no unsealed object, and the old bucket and its KMS key are retired only
after the copy has been fetched and checked with each key on its own (D7). The CLI around it (`opsctl archive
copy`) is a follow-up in `ops/`.

## What is built (this pull request)

- `core_archive::age`: age v1, header, X25519 stanza, STREAM, Bech32 keys.
- `core_archive::seal`: the envelope, `Recipients` (the seal file), `TransitKey`, `Keys`, `VaultTransit`
  (`decrypt` with a token file, bounded retries), `Sealed` (the layer), `copy`.
- `store::Head` and `ObjectStore::head` (default: the whole object), `StoreError::Sealed`.
- `Archive::segment_head` / `checkpoint_head`; the archiver resumes from them.
- `open` / `open_with`: the seal's options; `S3Config::check_writer(sealed)`; `DEFAULT_RETAIN_DAYS = 180`.
- Tests: the seal over a directory (either key alone, no key, write once, tampering, a moved object, a
  rewritten seal line, rotation, epochs, unsealed objects, the copy, the spec); the core host's archiver over a
  sealed store, restarted with a new writer and resuming, then restored with the recovery key alone and with
  Transit alone, rolled forward to the same roots; S3 (MinIO, Object Lock) sealed, its 412 path; Vault 2.0.3:
  the writer's public-key wrap opened by Vault per version after a rotation, a decrypt-only policy, a denied
  token, `min_decryption_version`; rage opening the seal's age part and the seal opening rage's.

## Follow-ups (not in this pull request; not in `crates/core-archive`)

1. **`ops/`:** `opsctl archive copy --from <old> --to <new>` over `seal::copy`; `opsctl archive verify` to report
   sealed and unsealed objects and the Transit versions in use; the runbooks (`core-archive.md`,
   `total-loss-recovery.md`, `restore-drill.md`) for the seal file, the reader's token file and recovery
   identity, the stock-`age` recovery, and `kms:Decrypt` no longer needed for a sealed archive.
2. **The restore's Vault login:** today a token file (Vault Agent's sink). `F-042`'s session
   (`signer`, feature `vault-session`, AWS IAM login) could log `opsctl` and `aledgent-core-host restore` in
   directly under an `archive-restore` role (D8).
3. **Docs:** `agents/README.md` and `deploy/` show `kms-key-id` in `--archive`; they gain `seal=`.
4. **The node change** (bloque-ledgent#289): the seal file on the node, and the new bucket in `--archive`.
5. **Single-flight unwraps:** concurrent reads that miss the same epoch each ask Vault (up to `fetch`'s 32 at the
   first segment of an epoch). Harmless; a single-flight map would make it one.

## Decisions for the owner

| | Question | Options | Recommended (built) |
|---|---|---|---|
| D1 | How Transit is used | (a) public-key wrap, Vault only decrypts; (b) `datakey` on the core host; (c) `encrypt` of each object | **(a)**: no Vault on the core host or its write path; (c) cannot carry 64 MiB segments |
| D2 | How the keyless writer reads back | (a) record headers and the verify report in clear; (b) a local write-ahead file of the last heads, laid out by restore; (c) the core host decrypts through Vault (a role and a token on it) | **(a)**: no new state on the node; the metadata it shows is in the table above |
| D3 | Recovery key custody | who generates it (offline, `age-keygen`), how many copies, where (safes, escrow), split (e.g. Shamir 2-of-3) or whole, who may use it, how often it is tested | Generated offline, two printed copies in two places, tested at every restore drill with Vault switched off for the drill; splitting is your call |
| D4 | Transit placement | (a) a new mount `transit-aledgent-archive`; (b) a key in `transit-aledgent-signer` | **(a)**: the archive's policy and audit apart from signing; the signer's roles never see it |
| D5 | Is the seal mandatory for an S3 writer? | (a) the seal or KMS (transition); (b) the seal, always | **(a)** now; **(b)** in the release after the cutover, so a core release does not wait on the node change |
| D6 | Unsealed objects in the new bucket | (a) accept; (b) refuse (`unsealed=refuse`) | **(b)** in the new bucket's specs once the copy is checked; (a) is the default for the old bucket |
| D7 | Old objects | (a) reseal into the new bucket; (b) copy as they are (KMS-SSE), read as unsealed | **(a)**: the new bucket depends on no KMS key; retire the old bucket and key only after both keys have opened the copy |
| D8 | The restore's Vault login | (a) token file from Vault Agent (built); (b) `F-042`'s AWS IAM session in `opsctl` and `restore` | (a) now, (b) as a follow-up |
| D9 | Keep SSE-KMS on the new bucket? | (a) no: SSE-S3 (AES-256) as the bucket default; (b) yes, as an outer layer | **(a)**: the point is not to depend on it; (b) costs a key and grants and buys little over the seal |

Also for the owner: **the file key epoch** (4096 objects or one hour). Shorter makes a restore call Vault more;
longer widens what one file key opens. And the core host's process holds each epoch's file key in memory; it
holds the plaintext book already, so this adds nothing a compromised host does not have.
