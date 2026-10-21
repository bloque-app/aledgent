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
| Milestone | Not assigned. Ships in the next core, as a core upgrade (`F-037`), after `T-041-00` (owner's ruling of 2026-10-21), not in the next re-genesis |
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
- **What the core must be given.** The digest of F1 holds the deployment, the client key, the initiator, the
  acted-for identity, a payload digest and the command's canonical text, and not the operation reference. The core
  already knows the deployment and the caller; it does not receive the client key (amendment 0037, G4). B adds to
  what the core receives: the client key, the canonical text and the payload digest. The core hashes exactly the bytes
  it was given, then **parses them, writes them again by the canonical rules and refuses any difference** from
  the command it decides, so a buggy or compromised ingress cannot send text that says one thing while the core
  decides another.
- **The hash.** F1's digest is SHA-256 (ruled). The core links only BLAKE2b today, so this feature adds SHA-256 to
  the core program, a small dependency whose size and time `T-041-00` includes.
- **The signature.** The proposal: CipherClerk's Schnorr signature over Ristretto255, which the core already verifies
  for `key` credentials, so the program gains no new cryptography. A proposal to confirm in `T-041-02`; it fixes
  the record's last 64 bytes.
- **The key.** Ingress's signing key sits in the key boundary like the feed key (`F-035`). Rotating it needs a
  trusted set with ids; the record's key id names one.
- **The refusal.** `ERR-CommandApprovalRequired` joins the core's own list with the next core (amendment 0037, R2,
  ruled), which removes the named exception `T-040-10` added to the conformance check.

## An open design fork (the owner's; not decided here)

Amendment 0037 (Q-A) records a fork this stub does not settle, and which `T-041-01` waits for. The owner's ruling `w14` is that both alternatives are measured, for security and throughput, before one is chosen (see "Measurement design" at the end). Either:

1. **The core trusts a digest of ingress's approval.** The core keeps accepting direct proofs only from `key`
   credentials. Every other scheme, present or future, is approved by ingress, which is a principal of its own, and
   the core trusts a digest of that approval, as it already trusts the passkey check ingress did for
   `reclaim_custody`; or
2. **The core records an ingress-signed approval record for each command**, which is this feature as drafted.

What each gives, plainly. The record of 2 is signed by ingress, so it cannot detect a compromised ingress. It adds
evidence in the book of who approved what, and protects against a path that bypasses ingress. In both designs ingress
vouches for the passkey. Only the core verifying the assertion itself would protect against a compromised ingress, and
`DEC-19` rejected that. Everything between here and "Measurement design" describes design 2; the measurement covers both.

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

**The threshold (owner's ruling of 2026-10-21).** The measured throughput with approval records must not fall more
than 1.5 standard deviations of the baseline's run-to-run variation below the baseline mean. Latency (p50, p99), the
book's growth and the checkpoint figures are reported and are not gated.

**To be confirmed by the owner before the run.** 1.5 standard deviations is close to "no measurable slowdown", and
it is more permissive than it sounds: see "The acceptance rule" in the measurement design. And a standard deviation means little from a few runs, so the baseline needs **at least 10
runs** on one machine (more if the variation is large), each long enough to be stable, and the run with records the
same number. The threshold is written in `tasks.md` and committed before the run, so the number cannot be chosen
after the result (amendment 0037, Q-F).

## Order

1. `T-041-00` runs as soon as `F-040`'s formats are frozen (`T-040-02`). It does not need A to ship.
2. If the cost is acceptable, `T-041-01` (the amendment), `T-041-02`, `T-041-03`, `T-041-04`, `T-041-05`.
3. `T-041-06` after the measurement, with the owner.

B ships as a core upgrade in the next core (`F-037`), so it does not block the next re-genesis. Only amendment 0037
and F1 to F3 must be settled before it.

## Out of scope

- Everything of Feature A. B must not change F1 to F3's bytes; if it must, it is a version 2.
- Verifying the passkey inside the core. `DEC-19` keeps P-256 out of the core program, so the record proves
  that ingress said a credential approved a digest, and not, to a third party looking only at the book, that a
  passkey signed. Keeping the assertion elsewhere is how anyone shows the rest.
- Making the device show the person the amount (amendment 0037, G3).

## Measurement design: baseline, option (b) and option (c)

Owner's ruling `w14` (2026-10-21): Feature A, the front door only, goes first. Before the core design is chosen,
**both** alternatives are measured, for security and for throughput. The owner's rule is that the non-functional
requirements are preserved: "my call is that NFRs should be preserved". Nothing here picks a design; it says what
is measured and what would make each pass.

The four variants, named once:

| Name | What it is |
|---|---|
| **Baseline** | Today's code. The core is told nothing about approvals |
| **(a) Front door only** | Feature A (`F-040`). Ingress asks for and checks the approval; the core is unchanged. Not a separate core variant: for the core it is the baseline |
| **(b) Record per command** | Ingress signs an approval record for each command (amendment 0037, F3); the core checks the signature against its trusted set of ingress keys, checks the record matches the command and the credential is an active member of the identity's set, and writes the record into the book. A command with no matching record is refused |
| **(c) A principal of its own** | The core keeps accepting direct proofs only from `key` credentials. Every other scheme, present or future, is approved by ingress, which is a principal of its own, and the core trusts a digest of that approval carried with the command, as it trusts the `PasskeyChecked` mark of `reclaim_custody` today. The link that carries it is already authenticated and signed (`F-038`, `DEC-27` (f)). Whether the digest is written into the book is part of what is measured (the two sub-variants below) |

Variant (c) has two sub-variants, because the answer changes the cost and the evidence: **(c1)** the core checks the
digest against the command and discards it; **(c2)** the core also writes the digest into the book.

### 1. The throughput and latency protocol

On x86 or CI only (`x86-bench.yml`), never on the 16 GB Mac.

- **One machine, one run order.** All variants run on the same machine type in the same job, interleaved
  (baseline, b, c1, c2, baseline, ...), so that a noisy neighbour hits every variant alike.
- **Repetitions.** At least **10 runs of each variant**, so that a standard deviation means something, and the number
  is **fixed before the first run**: more runs added after seeing a result would let the data choose its own test.
  The first runs of each variant are warm-up, named in the note and discarded.
- **Batch sizes.** The sizes of `notes/MEASUREMENTS-F031.md`, with 1, 8 and 32 among them. Variants are compared at
  the **same batch size**. The largest batch the invocation limit allows for variant (b) is smaller than the
  baseline's, so that cell is reported on its own and is not compared with the baseline's largest.
- **The mix.** The stress suite's mix of commands and its number of concurrent principals (`NFR-18`), with the share
  of commands that need an approval stated and varied (all, half, none), because the cost is paid only by those.
  **The gating cell** is one: all commands approved, at the production batch size the owner names. The other cells
  are reported with their intervals and gate nothing, so the verdict is one test and not a family of them.
- **What is recorded per run.** Commands per second; p50 and p99 latency of acknowledgement (`NFR-1`) and of a
  decision (`NFR-16`); the growth of the book per million commands; the size and the build time of a checkpoint
  and of an export; the time to bring a new node up to date at the same book size (`NFR-21`).
- **The stand-in approval is real in format.** It uses the real record bytes (F3), a real signature and, for (c), a
  real digest of the real length, so the costs are the real costs. The assertion itself is not verified in the
  core in any variant.

**The acceptance rule, the owner's number.** For each of (b), (c1) and (c2), in the gating cell: the mean throughput
over its runs must not fall more than **1.5 standard deviations of the baseline's run-to-run variation** below the
baseline's mean. Latency, book growth and checkpoint and export figures are reported and are gated only by the NFR
table below. The owner confirms the rule, and the number of runs (at least 10), in a commit **before** the first
run; the rule is not changed after seeing a result.

**Why that number is more permissive than it sounds (a correction).** With 10 runs of each variant, the measured
difference between two means has a standard error of about 0.45 of the baseline's standard deviation. So a harmless
change almost never fails the rule (less than one time in a thousand). The fault is the other way: a real slowdown of
**one** standard deviation passes the rule about **87 times in 100**, and one of 1.5 standard deviations passes about
half the time. And the rule gets looser as the machine gets noisier: if the baseline's standard deviation is 5% of its
mean, a real loss of about 7% is tolerated; if a noisy runner makes it 10%, a loss of 15% is. The standard deviation
from 10 runs is itself known only to within about a quarter, so the line moves with luck.

**A sounder rule, proposed next to the owner's number (Q-F of the amendment; not adopted).**
1. The owner states a **margin** in percent, the largest loss of throughput that is acceptable, from a need: capacity
   against `NFR-2` and `NFR-18`, and latency headroom against `NFR-1`, `NFR-15` and `NFR-16`.
2. The number of runs is **chosen by a power calculation** for that margin and the baseline's measured variation, and
   fixed before the run.
3. Runs are interleaved in blocks (baseline, then each variant) and compared as **paired differences**, which cancels
   most of a noisy neighbour.
4. The gate is a **one-sided 95% confidence bound** on the loss: the variant passes if the upper bound of its loss is
   below the margin. The interval is reported, not only pass or fail.

The owner decides which rule governs. Until then both are reported, the owner's number as the gate it was ruled to be.

### 2. The non-functional requirements either option touches

Every `NFR-*` of `SPEC.md` §11 was read. Each is listed once, with whether (b) or (c) touches it, how it is checked
and what passes. "Pass" always means: the requirement holds as written, with no relaxing of its number (§11: a
plan must not relax them silently).

| NFR | Touched by | How it is checked | Pass when |
|---|---|---|---|
| `NFR-1` acknowledge within 50 ms p99 | (a), (b), (c): ingress does an assertion check and two durable writes before it logs; (b) and (c) add a signature or a digest on the same path | The stress suite's acknowledgement latency for transfer, reservation and withdrawal, approvals on, per variant (`T-040-07` for (a)) | p99 at or under 50 ms for all three commands, at the suite's concurrency. **This measurement does not cover `NFR-1` for (b).** The stand-ins sign with a fixed test key and no signer and verify no assertion, so they measure the core's cost only. In production the path adds a round trip to the signer (`F-035`) for (b), plus the assertion check, the live-set read and two writes at ingress; `T-040-07` and a rerun of it with the signer in the path (`T-041-05`) carry that |
| `NFR-2` 2.5 million operations a month, and the entries they imply | (b), (c2): the book grows by a record per command | Book growth per million commands measured; projected at 2.5 million operations a month for a year | The growth fits the deployment's storage plan with a margin the owner states as a number (Q-F), which is not set yet; no redesign of the book is needed to sustain the volume |
| `NFR-3` balance read 5 ms p99 at 150 million entries | (b), (c2) only if records are in the same store as entries and slow the reads | The read benchmark at 150 million entries with records present | p99 at or under 5 ms, and not more than the owner's regression margin above the baseline's p99 (Q-F) |
| `NFR-4` nothing acknowledged is lost; restorable | (b), (c2): records are part of what is restored | The restore drill with records in the book | A restore reproduces the book and its records; every balance re-derived from entries alone (`INV-6`) |
| `NFR-5`, `NFR-17` deployable and upgradable without an outage | (b), (c): a new core with new code, a new trusted set of ingress keys, a changed input | The upgrade runs on test under load with the fence and its way out (`F-037`), and a rotation of the ingress key is made online | No request refused for the upgrade or the rotation; every command decided as usual or refused under a named condition; the old and the new ingress both work during the roll |
| `NFR-6` audit trail retained and queryable | (b), (c2): the record is audit evidence; (c1) and (a) leave ingress's note as the only trace | The audit trail query for an approved command | The query names the approving credential and the digests (a note at ingress for (a) and (c1); the book for (b) and (c2)) |
| `NFR-7` no unauthenticated route; default deny | (a), (b), (c): a new route that returns the challenge | The default-deny test covers the new route; the stream carries the approval over the authenticated link only | A request without a session to the approval route is refused; no route reaches a ledger fact unauthenticated |
| `NFR-8` no single process can move value without bound; key material in a separate boundary | (b), (c): ingress gets a signing key for approvals | The key sits in the signer (`F-035`), never in the process environment, a log, a URL or an error; the key's reach is bounded | No key bytes in the ingress or core process; the audit of the boundary lists the new key; a compromised ingress can sign approvals only for commands that reach it (see the security table: this is the honest limit) |
| `NFR-9` daily reconciliation, three axes | (b), (c): none expected | The reconciliation job runs on a book with records | All three axes report zero difference; records move no balance |
| `NFR-10` restore drill | (b), (c2) | As `NFR-4` | As `NFR-4` |
| `NFR-11` observable signals | (a), (b), (c): new refusals and the rollout switch need signals | The signals list gains: approvals asked, accepted, refused by reason class, and report-only would-refuse counts | The six existing signals are unchanged and the new ones are visible, with no credential, assertion or digest of a credential in a label |
| `NFR-14` conformance suite exercises every invariant and every error | (a), (b), (c): a new refusal | The two-way check between §10 and the refusal list (`T-040-10`) | Every new error and invariant has a case; the named exception is gone once the core carries the refusal |
| `NFR-15` complete within 30 s p99 | (b), (c) | The stress suite's completion latency | p99 at or under 30 s, and not more than the owner's regression margin above the baseline's p99 (Q-F) |
| `NFR-16` answer with the decision within 1 s p99 | (a), (b), (c) | The stress suite's decided-answer latency, approvals on | p99 at or under 1 s for every command answered with its decision, including the approved account and security commands |
| `NFR-18` throughput measured, scale by partitions | (b), (c) | This measurement, under the stress suite's mix; and a sharded run | Reported with the mix and the principals; the design still scales by adding partitions (records do not tie two partitions together) |
| `NFR-19` credit lag 2 s p99 | (b), (c): none expected | The stress suite's credit lag | p99 at or under 2 s, and not more than the owner's regression margin above the baseline's p99 (Q-F) |
| `NFR-20` revocation lag 1 s p99 | (b), (c): the core's check that the approving credential is still in the set | A revocation of a credential, then an approval from it | The approval is refused within 1 s of the revocation's commit; for (c) the same, because ingress reads the set through the stream (`F-038`) |
| `NFR-21` scale out, replace and lose a node; bring a new node up to date in 15 minutes by design | (b), (c2): the book is larger, checkpoints and exports include the records | Time to admit and catch up a new node at the book's size, with records; the decision rate while a node is admitted | The time stays within the design value (15 minutes), or any miss is reported as `NFR-21` says, with `NFR-1`, `NFR-15` and `NFR-16` holding throughout |
| `NFR-22` retention of what replay needs | (b), (c2): the records are inputs' content; the trusted key set is part of what replay needs | A replay from genesis on a book with records, under every code generation that wrote them | Replay re-derives the same roots; the trusted key set at each input is recoverable from the inputs |

`NFR-12` and `NFR-13` are not in `SPEC.md`'s list (a gap in the sequence, not an omission of this table).

### 3. The security comparison

Honest reading first. In every variant ingress is what verifies the passkey, because the core carries no P-256
(`DEC-19`). So **an approval signed or marked by ingress cannot detect a compromised ingress**: someone who controls
ingress can approve anything. Only the core verifying the assertion itself would stop that, and `DEC-19` rejected it.
The options differ in what else they give.

| Threat | Baseline | (a) Front door only | (b) Record per command | (c) A principal of its own |
|---|---|---|---|---|
| **A stolen session token** | Moves value and adds a device-bound credential for 15 minutes | Prevented: a write needs an approval from a credential the thief does not hold | Prevented, and the book holds the record | Prevented |
| **A compromised ingress** | Can log any command | Not prevented: it can skip the check | **Not detected:** it can sign records for anything. The book shows a record, which proves nothing about the person | **Not detected:** it can mark anything approved |
| **Theft of ingress's signing key, without compromising ingress** | No such key | No such key | Forges approvals for any command, from anywhere that can reach the core, until the key is rotated. The key is in the signer (`F-035`), which limits who can use it, not whether a thief with the key can | No new key; the link's authentication (`F-038`) is what a thief would need |
| **A path that bypasses ingress** (a second door to the core, a rogue feeder) | Not prevented | Not prevented: the core does not ask | Prevented only if the signer signs for no caller but ingress (`F-035`'s grant model) and the core refuses a command with no record signed by a trusted key | Prevented for a command with no mark; but the mark rides the authenticated link, so what is stopped is bypass of the link, not of ingress's code |
| **Replay of an approval** | Not applicable | Prevented: one-time number, expiry, the sign count | Prevented at ingress, and the core also refuses a record whose command it has already decided (idempotency) | As (a) at ingress; the core's idempotency covers the command |
| **An approval for another command** | Not applicable | Prevented: the digest binds the command | Prevented at ingress, and again in the core, which parses the text it was given, writes it again by the canonical rules, compares it to the command it decides and only then hashes it | Prevented at ingress; in the core by the same parse-compare-hash check in (c1) and (c2) |
| **A revoked credential, while the core cannot be read** | Not applicable | Prevented: the approval is refused (`ERR-DecisionUnavailable`) rather than checked against a stored copy; the cost is availability | Same at ingress; the core checks membership again when it decides | Same at ingress; the core checks again when it decides |
| **A compromised client application** (it shows one amount and asks for another) | Not applicable | Not prevented: the device shows no amount | Not prevented | Not prevented |
| **Issued-secret (API key) abuse** | Moves value; can change a credential set | Not prevented, by ruling: an issued secret signs nothing and is exempt. A leaked one can still add the attacker's device-bound credential (amendment Q-I) | Same | Same |
| **A configured token or the adapter's inputs** | Accepted | Exempt by scheme (an `api-key`, or the adapter's facts); a token naming a `key` credential needs that key's signature | Same | Same |
| **A future credential scheme** | Core verifies each scheme or not at all | Ingress must learn each scheme's approval | Ingress signs for any scheme; the record format does not change | The design's point: the core never needs to learn a new scheme, only to trust ingress's digest |
| **Evidence in the book of who approved** | None | Ingress's note only, outside the book | Yes: the record, signed by ingress | (c2) yes, a digest; (c1) no |
| **Cost on the core** | None | None | SHA-256 and a signature check in the program; a record per command in the book; the client key, the text and the payload digest added to the input | A digest check; a digest per command in the book for (c2) |

What this does not say: that (b) is "more secure" than (c). Both depend on ingress's honesty for the passkey.
(b) adds a signature scheme and a trusted key set to the core, which is more code in the place where code is
hardest to change, and a key whose compromise lets the holder write approvals. (c) adds less, and gives the core
no way to tell a good mark from a forged one except that the link is authenticated. The measurement answers the
cost; the owner decides whether the extra evidence is worth it.

### 4. The stand-in implementation plan

No stand-in is merged. Each lives on its own branch off `develop`, with a draft pull request that is closed, not
merged, when the measurement ends. The commit trailers and signing rules of the repository apply. Only the code
below is written, and only as much as the measurement needs.

| Branch | Holds | Minimal code |
|---|---|---|
| `bench/approval-baseline` | Today's code with the measurement harness's hooks | A run mode in the stress suite that can mark a share of commands "needs approval", so all variants see the same mix. Nothing in the core changes |
| `bench/approval-b-record` | Variant (b) | The 149-byte record (F3) as a type in `ledger-core`; the SHA-256 digest; the signature check against a trusted key set held as a constant; the client key and canonical text added to the core input; the write of the record to the book; ingress signs the record with a fixed test key (no signer). Checkpoint, export and import carry the record |
| `bench/approval-c1-mark` | Variant (c1) | A mark on the core input carrying the 32-byte digest, as `PasskeyChecked` is carried; the core recomputes the digest from the command and refuses a mismatch, then discards it; the link is as today |
| `bench/approval-c2-mark` | Variant (c2), on `c1` | As (c1), and the core writes the 32-byte digest into the book; checkpoint, export and import carry it |

Each variant's branch also carries the same two helpers the measurement uses: a generator of the approval bytes at
ingress's side (fixed test keys, real lengths) and a counter of the bytes added per command. Neither is shipped.
The harness is `x86-bench.yml`'s `compare` mode, run once per variant by branch name; the report is a note in
`notes/`, written by `T-041-08`. The stand-ins verify no passkey assertion: the measurement is of the core's cost,
and a real assertion is checked at ingress in every variant alike (`T-040-07` measures that cost).
