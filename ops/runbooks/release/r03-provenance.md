<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: a message between Agents failed its R-03 check (the sharded profile)

`REQ-RT-13`, `INV-18`, `NFR-8`, `NFR-9`, `REQ-AT-3` · R-03 · `F-033` · **Pages:** `AledgentR03CheckFailed`
(at once), `AledgentR03Unverified` (a message unverified for over a minute, for five minutes) · **Owner:**
security; platform assists.

**Status in this release.** No message crosses Agents with **one core Agent**, the single-voter profile
this release deploys. The metrics come with the sharded profile's host. They are deferred in
`ops/src/signals.rs`, and the rules are committed so that they page the day the metrics appear.

## What the check is

Every message between the book's Agents (`credit/<n>`, the principals `feed`, and `trim/<n>`, a peer
telling a sender how far it applied its credits, `rq-05`) is checked by the receiving
Agent, in its own transition, against its sender's certified commit: a hash chain over the stream, and the
sender's voter's signature over the chain's head. In this release each Agent has one voter, whose key is
in Vault and is used only by `aledgent-signer`.

In **`report`** mode, the default, the check runs but does not gate (`enforce` refuses a failing
message): a message that fails is **applied anyway**. So a
page means that a message its sender did not commit has probably already moved value. Treat it as a
security incident, never as noise.

Each failure writes one line to the core host's standard error:

```json
{"event":"r03_check_failed","mode":"report","receiver":"book-1","stream":"credit/0","reason":"forged","applied":12,"verified":10,"applied_anyway":true}
```

| `reason` | What it means |
|---|---|
| `forged` | The messages applied do not chain to the sender's certified head: one was written, changed, reordered or left out after the sender committed it. The relay or its transport. On `trim/<n>`: a trim past what the peer certified, or a peer that applied credits this shard never sent; in `report` the credits were forgotten anyway, and only the relay's copy still delivers them |
| `wrong_committee`, `bad_signature`, `non_canonical`, `insufficient_quorum`, `unknown_sender`, `wrong_agent` | The certificate is not the pinned voter's, for that sender. A forged certificate, or pins that drifted from the deployment |
| `wrong_program` | The sender's commit ran under another runtime program than the one pinned: an unannounced runtime change |
| `not_in_claim` | The certificate carried with the stream does not cover it: a relay fault |

A stream that failed **keeps failing**: once a forged message is applied, the receiver's chain no longer
meets its sender's. Nothing clears it yet: an operator's re-base after an incident is later work (`T-033-16`).

## Preconditions

- [ ] The receiver, the stream and the reason, from the log line.
- [ ] Whether one stream fails or many: one points at a sender or one link; many point at the relay.
- [ ] For `AledgentR03Unverified`: whether certificates stopped (the sender's host cannot reach the signer)
  or the relay stopped carrying them.

## Steps

1. **Contain.** Stop the relay (it runs as its own process and OS user).
   - Nothing crosses Agents while it is stopped. Credits wait in transit: no value is lost, and it is
     neither payer's nor payee's until the credit lands.
   - Revocations wait too. Freeze, through the command surface (`freeze_account`), the accounts any
     exposed credential reaches until the feed runs again (`NFR-20`).
2. **Measure the effect.** Run reconciliation on demand (`opsctl reconcile`).
   - A forged credit shows on the second axis, in transit; a forged `settle` or `evidence` on the third.
     A forged `feed` entry (a grant) shows on no axis: list the grants the receiving shard applied since
     the stream's last verified sequence (the log line's `verified`) and compare them with the principals
     Agent's registry.
   - Any difference: follow [divergence](divergence.md). Correct only through the command surface: a
     reversal by two principals (`REQ-AT-3`, guardrail 7). Never edit a store.
3. **Find the writer.**
   - `forged` or `not_in_claim`: the relay's host. Keep its process, memory and logs as evidence.
   - A certificate reason: the signer's log of every use of the voters' keys (`F-035`), and the pins the
     deployment admitted. A signature the signer never made means the key left Vault or the signer.
   - `wrong_program`: compare the runtime each Agent pins with the release's; follow
     [divergence](divergence.md).
4. **Rotate.** The relay's credential always ([key-rotation](key-rotation.md)). A voter's key if any
   certificate reason is involved: a new key means pinning its committee again in every receiver, which is
   the deployment's step through each Agent's driver, never the relay's.
5. **Restart the relay** only when the writer is found and removed.
6. **Write the incident** under the operator's incident process. What never goes in a message: keys,
   tokens, account identifiers.

## Verification

- [ ] Reconciliation on demand: every axis at zero, or every difference owned and being corrected through
  the command surface.
- [ ] No new `r03_check_failed` line, and `aledgent_r03_unverified_age_ms` back under a minute.
- [ ] The failing stream is recorded in the incident with its last verified sequence: it keeps failing
  until the re-base exists (`T-033-16`).

## Rollback

Nothing here edits a store. Setting the mode to `off` silences the check; it is never a fix.

## Exercised

On every change: `crates/shards/tests/r03_report.rs` (a forged message is reported and applied in
`report` mode) and `ops/tests/alerts.rs` (`req_rt_13_a_failed_r03_check_pages_security_at_once`). The
response itself is not exercised automatically yet; it is exercised by hand in a staging deployment of the
sharded profile.
