<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-041 — The core records the approval (Feature B, a stub)

`[PENDING AMENDMENT 0037]`. **This is a stub, not a plan to build from.** It is **blocked** by two things: Feature A
(`F-040`) shipping, and the throughput measurement of `T-041-00`, against a threshold the owner sets before the run.
Nothing in it starts, and no code merges, until both are done and the owner has approved the amendment that
`T-041-01` writes. It is here so that A freezes formats B can live with, and so the cost B puts on the book is
written down before anyone asks for it.

| | |
|---|---|
| Milestone | Not assigned. Delivered as the next re-genesis's genesis code or as a core upgrade; the owner decides after `T-041-00` (amendment 0037, Question 10) |
| Home | `crates/ledger-core` (the record, the check, the refusal), `crates/core-binding` and `crates/core-archive` (checkpoints, export, import), `agents/core` and `agents/core-host` (the trusted set, the host's side), `services/ingress` (attaching the record), `crates/conformance` |
| Depends on | `F-040` (the formats frozen, ingress asking for approvals), `F-035` (ingress's signing key sits in the key boundary), `F-037` (a core upgrade, if that is the delivery), amendment 0037 |
| Unblocks | Proof in the book that a person approved each command |
| Realises | `REQ-AU-23` `[PENDING AMENDMENT 0037]`, `REQ-AU-7`, `REQ-AU-3`, `REQ-AU-20`, `REQ-RC-3`, `REQ-PR-10`, and a requirement amendment B adds in `T-041-01` (not named until then) |

## Goal

The core refuses a command that needs an approval and has no matching approval record, and writes the record into
the book, so the book itself, and not only ingress, shows a person approved each such command.

## What changes in the core (a proposal, from reading the code and not from a run)

- Each command in scope carries an **approval record** (amendment 0037, F3): the approving credential, the command
  digest, the digest of the assertion, ingress's signature and a key id. Ingress signs it with a key whose public
  part the core already trusts.
- For each such command the core checks that the record's digest equals this command's digest, that the signature
  is ingress's under a key in the trusted set, and that the credential is an active member of the acting
  identity's set. It then writes the record into the book. A command with no matching record is refused, and the
  refusal is recorded like any refusal.
- **What the core must be given.** The digest of F1 names the client key; the core receives only the operation
  reference derived from it. B adds the client key to what the core receives (amendment 0037, G7). The proposal
  for the canonical text: ingress sends it, and the core hashes exactly the bytes it was given and then parses
  them. The core never re-serialises a command to hash it.
- **The hash.** F1's digest is SHA-256 unless the owner picks the BLAKE2b the core already uses (Question 6). The
  core does not link SHA-256 today. If SHA-256 stays, this feature adds it to the core program, a small
  dependency whose size and time `T-041-00` includes.
- **The signature.** The proposal: CipherClerk's Schnorr signature over Ristretto255, which the core already verifies
  for `key` credentials, so the program gains no new cryptography. A proposal to confirm in `T-041-02`; it fixes
  the record's last 64 bytes.
- **The key.** Ingress's signing key sits in the key boundary like the feed key (`F-035`). Rotating it needs a
  trusted set with ids; the record's key id names one.
- **The refusal.** Under either option of amendment 0037, R2, the refusal is a line in the core's list. If
  `T-040-10` kept it outside, this feature brings it in.

## What it costs (an estimate; nothing is measured yet)

These come from reading the code and are not from a run. `T-041-00` replaces every one with a number.

- Each approved command carries about 150 more bytes (the record's 149, plus framing), and the digest's
  canonical text if it is sent separately. A batch is limited to what one invocation carries, about 16 KiB
  (`MAX_COMMAND_BYTES`, `notes/FIT-VOS-CIPHERCLERK.md`), so fewer commands fit in a batch. The figure is to be
  confirmed.
- One signature check per command inside the program, on top of today's work.
- One more write to the book per command, so the book grows with every approved transaction, and keeps growing.
- Checkpoints, exports and the roots all include the records, so they change, and so do their sizes and times.
- One more signature at ingress per approved command, through the signer (`F-035`), in the path of `NFR-1`.

## The throughput measurement comes first

**`T-041-00`** is the first task and everything else waits for it. On x86 or CI, never on a laptop:

1. **Baseline.** Commands per second and the 50th and 99th percentile of latency with today's code, at several batch
   sizes.
2. **The same run with a stand-in approval record** on each command, checked and written, in the real format of F3.
3. **The report.** The difference at each batch size, the batch size at which the batch limit bites, the growth of the
   book per million commands, and the change in a checkpoint's and an export's size and time. A recommendation, set
   against the owner's threshold.

The owner sets the threshold of "acceptable" **before the run**, in the task file and in a commit that precedes the
run's. Without it, the number means nothing (amendment 0037, Question 9).

## Order

1. `T-041-00` runs as soon as `F-040`'s formats are frozen (`T-040-02`). It does not need A to ship.
2. If the cost is acceptable, `T-041-01` (the amendment), `T-041-02`, `T-041-03`, `T-041-04`, `T-041-05`.
3. `T-041-06` after the measurement, with the owner.

Because `F-037` is merged, B can be delivered as an upgrade of the core after production is founded, so B need not
block the next re-genesis. Only amendment 0037 and F1 to F3 must be settled before it.

## Out of scope

- Everything of Feature A. B must not change F1 to F3's bytes; if it must, it is a version 2.
- Verifying the passkey inside the core. `DEC-19` keeps P-256 out of the core program, so the record proves
  that ingress said a credential approved a digest, and not, to a third party looking only at the book, that a
  passkey signed. Keeping the assertion elsewhere is how anyone shows the rest.
- Making the device show the person the amount (amendment 0037, G3).
