<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-033 — R-03: who may send a message to another of the book's Agents

An experiment (2026-10-04, R-03), then the owner's rulings of
2026-10-05 (`rq-01`, `rq-02`), built as `T-033-13`. Nothing here changes `SPEC.md`, `PLAN.md` or a DEC.

## Report mode (owner rulings of 2026-10-05, `T-033-13`)

**The rulings.**
- `rq-01`: the relay is isolated, temporarily, with a credential admitted for stream steps only; the
  checks run, but do not gate.
- `rq-02`: one voter's key is the root of an Agent's messages for the MVP. That key is in Vault and is
  used only by `aledgent-signer`; it is not in KMS.

**What changed from the experiment.**
- **No VOS accessor is needed for what we run.** Every Agent `aledgent-core-host` runs is a **Local**
  Agent that the host drives through the runtime ABI (`driver.rs`, `sharded.rs`); no VOS Shared host,
  Raft or share ledger is on any path. The host is each Agent's one voter, so the quorum-1 certificate is
  the host's own signature over the Agent's committed reply, made with the Agent's voter key through the
  signer. Proposal ask 2 (an ordered entry's certificate and control image from VOS's Shared host) is for
  the day a VOS Shared host commits instead of ours: `F-021`, three voters. No fork branch, no pin bump.
- **The check is in the shard actor's transition**, not in the runtime's composite engine: the shard
  Agents run `shards::agent` under VOS's standard runtime, and the check is guest code there
  (`decide::run_round`), within calls 9 to 14 like the rest of the actor.
- **The chains live in the Agents' rows**, not the control image: `c`/`i`/`v` per peer in a shard, `g`/`w`
  for the feed (`kv.rs`). The feed's chain is the registry's running digest `D`, which the principals Agent
  already commits (amendment 0015): nothing new on the sender's side.
- **The claim carries the heads**, as the prototype's did. The heads come from the round's reply (`out`),
  which the host's replica has just matched byte for byte.

**How it runs.**
- **Pins.** The driver pins each shard (`Envelope::Pin`): the mode, the runtime program, and every
  sender's committee. The relay never can. An Agent never pinned is `off` and behaves as before R-03.
- **Mode.** `off` checks nothing. `report`, the deployed default (`provenance::DEPLOYED`), checks every
  delivery and applies it whatever it finds. `enforce` applies only what chains to a certified head; the
  rest waits, and its relay delivers up to a certified head. The chains are kept in every mode, so a mode
  can change on a running book.
- **Certificates.** After a round that sent credits, and after a principals invocation that changed the
  registry, the host certifies the commit (`shards::attest::Attestor`). The relay carries, with each
  sender's delivery, the latest certificate whose head the delivery reaches.
- **Verdicts** (`provenance::judge`). `Verified { through }`; `Pending` (the head is past what is
  applied); `Stale`; `Unattested` (no certificate yet: normal while a delivery stops inside a sender's
  commit, and covered by the next one, since the chain is cumulative); `Failed(reason)`.
- **What the host raises.** Each round's reply carries its checks. The host counts them
  (`attest::Report`), keeps the failing streams and the age of the oldest unverified message, and writes
  one JSON line per failure to standard error (`r03_check_failed`, with `"applied_anyway": true` in
  `report`). The metrics `aledgent_r03_failing_streams`, `aledgent_r03_unverified_age_ms`,
  `aledgent_r03_checks_total` and `aledgent_r03_failures_total` are deferred like the sharded profile's
  lags; their rules page security (`AledgentR03CheckFailed` at once, `AledgentR03Unverified` after five
  minutes over a minute), through their own route. Runbook: `ops/runbooks/release/r03-provenance.md`.

**Tests** (`crates/shards/tests/r03_report.rs`): an honest relay under every fault (duplicates,
reordering, dropped heads, lost acknowledgements) verifies every stream to its head with no failure; a
forging relay's credit is applied and reported in `report` (the trial balance then breaks too), refused in
`enforce` (the genuine credit applies once, the trial balance holds), unseen in `off`; a relay restart
certifies the heads again; unattested credits are verified by a later certificate; another committee,
another program and a changed feed entry are reported; the sender's chain equals the prototype's
`Outbox`. `ops/tests/alerts.rs`: both rules page security through their route.

**Measured.** See [Report mode's gas](#report-modes-gas).

### Report mode's gas

All on hosted runners, `stress.yml`, `core=shards-recompiler`, VOS `c54fc1ff` (the pin), the JAM-compatible
standard runtime. Gas is exact; times are one runner each.

**The check alone** (run **37274515150**, `shards --r03-gas 1,16,64`): the same deliveries to shard 1 pinned
`off` and pinned `report`, one sender, one voter. Both keep the chains, so the difference is the check:
the certificate's Ed25519 verification, the claim's hash and the judge.

| Delivery | Actor gas, `off` | Actor gas, `report` | The check |
|---|---:|---:|---:|
| The feed, one entry, its certificate | 13 855 099 | 16 831 274 | **2.98 M** |
| 1 credit, its certificate | 55 250 053 | 58 345 119 | **3.10 M** |
| 16 credits, one certificate | 667 059 990 | 668 987 789 | 1.93 M |
| 64 credits, one certificate | 2 711 732 490 | 2 713 299 144 | 1.57 M |

- **About 3 M actor gas per sender commit** at quorum 1, against the experiment's 4.2 M estimate (native
  timings scaled by a curve operation's gas). The work per certificate is the same whatever the delivery's
  size; the smaller differences on large deliveries are the PVM's memory price and the allocator, not
  less verification. Take 3.1 M as the figure.
- **Per message: not measured apart.** The chains are kept in every mode, so the probe cannot see them. A
  link is two BLAKE2b blocks, about 10 k gas at 4 794 a block, against about 42 M of actor gas for applying
  one credit (the 64-credit row): about 0.02 %.
- **Against a round.** A shard round of 64 commands costs 4.2 to 4.9 G of actor gas, and a receiver pays one
  certificate per sender per round: 3 M is under 0.1 % of a round.

**Trims** (`rq-05`, run **37341044640**, the same probe with a second sender: shard 0 pinned `off` and
`report` take the same trim of the `k` credits, with the receiver's certificate; EPYC 7763; after v2 roots
became the default, `DEC-25`, so the deliveries themselves cost less than in 37274515150):

| Credits trimmed | Trim, `off` | Trim, `report` | The check |
|---:|---:|---:|---:|
| 1 | 2 736 701 | 5 871 368 | **3.13 M** |
| 16 | 4 558 926 | 7 742 911 | **3.18 M** |
| 64 | 12 982 264 | 16 259 879 | **3.28 M** |

- **About 3.2 M actor gas per trim**, one certificate, the same order as a credit or feed delivery's check
  (3.09 M and 2.97 M in the same run). It grows a little with the trim, by about 2 k a credit.
- A trim is one message per peer per round, carried only when the peer applied something new, so a
  shard pays at most one trim check per peer per round on top of the credits' checks.
- The sender keeps one more row per credit until it is trimmed: its chain (32 bytes), removed with the
  credit.

**Whole runs** (2 and 4 shards, 2 000 transfers, batch 64): the same workload `off` (run **37272475463**),
`report` (run **37272478074**) and on `main` before this change (run **37272481224**).

| | `main` | `off` | `report` |
|---|---:|---:|---:|
| Actor gas per round, 2 / 4 shards (M) | 4 871 / 4 215 | 4 872 / 4 215 | 4 862 / 4 227 |
| Decided per second, 2 / 4 shards | 42 / 41 | 52 / 53 | 41 / 40 |
| R-03 checks (all verified), 2 / 4 shards | — | 0 / 0 | 35 / 109 |
| Failures | — | 0 | 0 |
| Runner CPU | EPYC 7763 | EPYC 9V74 | EPYC 7763 |
| Trial balance, exactly once, anchor | holds, yes | holds, yes, same anchors | holds, yes, same anchors |

- The runs cannot resolve 3 M: per-round actor gas moves by tenths of a percent between runs of the same
  workload, in both directions. That is why the probe exists.
- The rate difference is the runner: `off` drew an EPYC 9V74, `report` and `main` an EPYC 7763, and on the
  same CPU `report` decides within 2 % of `main`.
- The experiment's estimate for three voters (8.3 to 8.7 M, two signatures) is not re-measured: no Agent
  has three voters before `F-021`.


**Trims, checked before go-live** (owner ruling `rq-05`, 2026-10-05; `T-033-15`). A trim is shard D
telling shard S how far it has applied S's credits; S then forgets them. A forged trim makes S forget a
credit D never applied. Now a trim carries D's certificate like any other message:
- D's claim carries, beside its outbound heads, its **applied head** of each source: `trim/<D>` towards
  S, the sequence D applied through and D's chain through it (`RoundReply::acks`). The host certifies a
  commit that applied credits, as it does one that sent them.
- S keeps the chain of every credit it sent until it is trimmed (row `h`). It checks the certificate,
  then that the certified head reaches the trim and that D's chain there is S's own: D applied exactly
  what S sent (`provenance::judge_trim`). A trim past its certificate, or a chain S never sent, fails
  as `forged`.
- The mode is the receiver's, as for every stream: `report` trims anyway and reports, `enforce` trims
  only what is verified. The relay carries, with a trim, D's certificate whose applied head reaches it.

**Not covered yet** (the owner's rulings of 2026-10-05):
- the cross-host relay (`net`) builds its own rounds and pins nothing, so outcome A runs `off`. It must
  carry the certificates before multi-host goes live: a release gate (`rq-04`, `T-033-17`);
- a stream that failed keeps failing: an operator's re-base of the receiver's chain after an incident
  is post-freeze work (`rq-03`, `T-033-16`, `PLAN.md` §4);
- `settle`, `evidence`, `audit` and `anchor` take the same path once `T-033-11` builds their Agents.


## The gap

`DEC-23`'s Agents exchange numbered streams through a relay: `credit/<n>`, `feed`, `settle/<n>`,
`evidence/<n>`, `audit/<w>`, `anchor`. The receiving runtime applies each message once and in sequence
(`REQ-RT-13`, `INV-18`), but a stream step is authorised by the receipt of whoever invokes the receiver:
the relay's principal. The runtime cannot tell who wrote a message. VOS at `perf/agent-stack` `c54fc1ff`
(on `saga/agents` `e6f2bb45`) has no cross-Agent message in the runtime ABI and no consensus-committed
outbox (`docs/extensions.md:88-92`).

## What a forged message does, stream by stream (option (d), today)

| Stream | A forged message | Which `NFR-9` axis sees it | When |
|---|---|---|---|
| `credit/<n>` | Value appears at a payee; the receiver's in-transit position passes what the source sent. Each shard still balances on its own (`tests/provenance.rs`, `..._only_reconciliation_sees_it`) | Second (in transit at a consistent cut) | Next reconciliation (daily) |
| `feed` | A grant lets a principal debit any payer up to the forged limit; the transfers it then makes balance. Also a forged opening, revocation or `Binding` | None: the transfers balance, and no shard's applied feed is compared with the registry's digest | Never by reconciliation |
| `evidence/<n>` | A leg settles that the settlement layer never finalised: a deposit credited from the issuance counterparty, a reservation captured or released (`INV-10`, `INV-11`) | Third (book against the settlement layer) | Next reconciliation |
| `settle/<n>` | The settlement Agent queues a call no shard decided: value leaves on chain (`DEC-18`) | Third | After the value has left |
| `audit/<w>`, `anchor` | The anchor commits to roots and heads no writer had, or a forged anchor or bind is published (`INV-19`, `REQ-PR-*`) | None (a binding's amount: third) | When a verifier checks a disclosure |

So "only reconciliation catches it" holds for three of the six streams, and after the fact. The relay
is today a single process that can move value without bound, which `NFR-8` forbids.

## Who can forge

- **The relay host** (the process holding the relay's principal; in `exp-multihost-relay`, each host's
  node, whose Noise sessions authenticate hosts but not the Agent that wrote a message).
- **A compromised execution node**: an observer (reads state, answers queries) or a voter (signs and
  commits its Agent's entries). One voter per Agent at go-live (`DEC-22`), three after `DEP-11`.
- **An operator**: whoever holds deployment credentials and node keys.

A compromised node of the *receiver* can apply anything to its own Agent whatever the receiver
checks; that is the Agent's own integrity (transition proofs, `F-030`), out of R-03's scope.

## The options

| | (a) Sender's commit certificate + its outbox, checked by the receiver's runtime | (b) A relay principal per sender, receipts scoped per stream | (c) A signature on each message by the sender Agent's key | (d) Reconciliation only (today) |
|---|---|---|---|---|
| Proves | A quorum of the sender's voters committed these messages, in this order, under the pinned runtime | The invoker holds the credential admitted for that sender's streams | Key in the runtime: nothing (every replica and observer can read the key). Key in the host: one node saw the row | Nothing at apply time |
| Who can still forge | A quorum of the sender's voters (at go-live, the one voter's key holder) | Whoever holds that sender's relay credential, on its host | Anyone who reads the Agent's state (in runtime); one node (in host) | Relay, any voter, operator |
| Relay host alone | No | Only the streams of the senders whose credentials it holds | Yes (in runtime) / No (in host) | Yes, every stream |
| Gas at the receiver | 4.2 M per signature (8.3 M at quorum 2 of 3) per sender commit, plus 10 to 30 k per message (measured natively, scaled; below) | None extra per step, but **one invocation per sender** instead of one composite for all: about 265 M each | 4.2 M per message (64 credits: 270 M) | None |
| Latency | One share round among the sender's voters after apply (none with one voter); about 2 ms of guest time per sender commit at 3.8 G gas/s | None | None | Detection: daily |
| Needs from VOS | The Shared host's per-entry certificate and the control image at an ordered index, to the relay (the share ledger exists upstream: `shared_raft.rs` `sign_local`, `record_remote_share`, `certificate`); a committee handover the guest can check (`DEP-11`) | Nothing: role claims exist; the stream-to-role rule is our runtime's | Nothing | Nothing |
| Ours to build | Outbound heads in the control image (beside `SST1`), the check in the composite engine | Per-stream rule in our runtime; a credential per sender | — | An hourly transit check at each anchor would shorten detection for credits only |
| Within JAM's host calls | Yes: guest code; the certificate is a caller blob | Yes | Yes | Yes |

**Why (a) is adapted.** The task's version proves one message by a Merkle path into the sender's
committed outbox. VOS's commit claim (`OrderedCommitClaim`, `vos/src/agent/shared_commit.rs`) covers the
Linear lane only by the `BlobRef` of its whole image, up to 4 MiB: an inclusion proof would be the image,
about 157 M gas of BLAKE2b at 4 794 gas a block, or need state blocks, whose fetch is host call `0x180`,
outside JAM. The control image is small and already ours (`SST1`). So the sender's runtime keeps, per
outbound stream, a head `(seq, chain)` with `chain_s = H(stream ‖ s ‖ chain_{s-1} ‖ message_s)`, and the
receiver, which applies in sequence anyway, recomputes the chain over each delivered segment and accepts it
only if it lands on the certified head: one hash per message, one certificate per sender commit, no path.
A Merkle mountain range over the same messages can be added if someone needs to verify one message alone
(a third party following a credit's provenance).

**Why not (b) as the defence.** It narrows who can forge from "the relay" to "the relay of that sender",
proves nothing about a commit, and costs a full invocation per sender per round because a VOS invocation
carries one receipt. It is worth having as defence in depth where hosts already invoke for their own
shards.

**Why (c) is dropped.** A key in Agent state is readable by every replica and observer and would sit in
every proof's witness; a key in each host is a one-node certificate per message, dearer than (a) and
weaker once there are three voters. Signing one digest per commit instead turns it into (a).

## For the MVP (owner ruling of 2026-10-04: freeze, no more iterations)

**Ships at go-live:**
- `NFR-9`, as it is.
- The relay as an isolated process. Its credential is admitted for stream steps only, and it holds no
  Agent node key.
- This check, built but off.

**The most secure option buildable now within JAM: (a) at quorum 1.**
- Go-live has one voter per Agent, so the certificate is that voter's single signature.
- The check runs in our composite engine, with the heads in the control image beside `SST1`. It is guest
  code, using calls 9 to 14.
- It needs one VOS accessor: an ordered entry's certificate and control image, from the Shared host.
- Turn it on for `settle`, `feed` and `evidence` first, as soon as that accessor exists.

**Deferred to VOS/Dani:**
- quorum-of-three certificates and committee handover (`DEP-11`)
- a consensus-committed cross-Agent outbox, which retires the relay

## The prototype

`crates/shards/src/provenance.rs` (feature `provenance`): `Outbox` (the sender's heads), `Claim` and
`Certificate` (shaped as VOS's `OrderedCommitClaim` and `ReplicaQuorumCertificate`: Ed25519
`verify_strict`, signatures ordered by signer, quorum a majority), `Receiver` (pinned committee and runtime
program per sender, verified head per stream; `accept` moves nothing on refusal). The claim carries the
heads directly; in VOS it would carry the control image's reference and the evidence would add the image.

`crates/shards/tests/provenance.rs` drives real shards: shard 0 decides transfers to shard 1, its
credits are chained as enqueued, and shard 1 applies only what the receiver lets through. Refused: a
changed amount, payee or operation, a reordering, an invented credit (past the certified head it waits),
a relay signature in a voter's place, one voter's signature, a repeated signer, a signer outside the
committee, the relay's own committee, a claim changed after signing, another Agent's genuine certificate,
another runtime program, a gap, a delivery that stops short. A property test flips any one bit of any
credit and finds it refused. Accepted: the genuine segment, repeats as duplicates, a stale certificate as
a no-op, a resend from an earlier sequence. Without the check (today), a forged credit is applied and only
the cross-shard in-transit equality sees it.

## Measured (an estimate of guest gas)

Natively on an Apple M2 Pro, release, three runs, under load (average 9 to 13), against the operation
whose PVM gas `curve-bench` measured: a Ristretto scalar multiplication with decompression and compression,
3.92 M gas, 34.3 to 34.6 µs here. The ratio scales the rest (curve25519-dalek's u64 backend on both).

| | Native | Guest gas, estimated |
|---|---:|---:|
| Certificate, quorum 1 of 1 (go-live) | 36.6 µs | 4.2 M |
| Certificate, quorum 2 of 3 | 72.7 to 76.4 µs | 8.3 to 8.7 M |
| Per credit in a 64-credit segment (one link, 37-byte credit) | 192 to 265 ns | 21 to 30 k (two BLAKE2b blocks at 4 794: about 10 k) |

For scale (`proposals/vos/0012`): 265 M of runtime gas per invocation, about 21 M per composite step,
138 M of actor gas per reservation. A shard that receives from seven peers in a round pays at most seven
certificates, 58 M at quorum 2, against 1.86 G for seven separate invocations under (b).

## What is left

- The engine: outbound heads in the control image, filled from what a step enqueues; the check in
  `wire::composite_engine` for stream steps; the relay fetching certificates and control images.
- Committee handover: `NFR-21` (b) replaces nodes, so a receiver must follow each sender's committee
  through evidence the old committee signed (`DEP-11`; VOS `gap/3-change-replicas`).
- Native decide (`F-034`, `nd-d02`): messages leave only after the PVM confirms the round, which is
  when the commit, and so its certificate, exists. The two compose.
- The cross-host relay (`exp-multihost-relay`) carries, per segment, the certificate beside the
  messages; nothing else in its wire changes.
