<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-035 — notes

Decisions taken in building the key boundary, and what is deferred. The experiment's defaults and the
owner's questions are in the experiment's decision record (kept with the maintainers).

## Decisions

1. **A backend per key** (superseded in part by decision 11). As first built: lane and anchor keys in AWS
   KMS (`ECC_SECG_P256K1`; the signer sends the 32-byte digest, makes `s` low and finds the recovery id);
   the Ristretto keys and the Ed25519 keys sealed under KMS and unsealed at start. Decision 11 keeps only
   the first, as an opt-in.
2. **The book never needs the root, only five answers** (`ledger_kernel::keys::KeyRequest`). The
   derivations are unchanged, so the canary's book is the same book behind the signer.
3. **A blinding is answered in the clear.** It opens a commitment, and the core holds every opening
   anyway (the opening oracle, `DEC-11`). The root that derives it never leaves the signer.
4. **The Agent receives answers, not the root.** The host's replica runs first over a recording custody;
   the driver passes what it recorded, plus the registrar's key (the Agent opens its book with it on every
   invocation). An Agent asked something its host did not answer panics: the invocation aborts and nothing
   commits. The kernel still verifies every signature it is given.
5. **Clients fail closed.** A custody cannot return an error into the native run. The core host's clients
   retry a broken connection or an unavailable backend for a minute, then stop the process with an `ALERT`
   line. Nothing half-done reaches the Agent; the command stays `Received`; a restart rebuilds.
6. **Authentication: peer credentials and a token, on a Unix socket only.** Mutual TLS is not built: every
   caller of the signer is on the same host or in the same ECS task. The token is compared in constant
   time against SHA-256 hashes; two hashes is how it rotates. Every request is re-checked against the
   configuration as it stands, so a reload ends a revoked caller at once.
7. **On ECS the task role is shared.** Every container of a task gets the same role, so on the adapter's
   task the role's own reach (its Vault role, an opted-in KMS key's policy) is the boundary for the lane
   keys; the signer sidecar adds the narrow API, the grants and the log. The owner ruled the sidecar (`kb-q2`, 2026-10-05).
8. **The core link**: a token and the peer's user id, checked before the request is read; a TCP link is
   loopback only, since the token travels in the clear. Over TLS, ingress could leave the core's host: not
   built.
9. **Development keys are declared, never assumed.** The signer refuses a `file` source without
   `"development": true`; the adapter refuses a file or environment key without
   `"development_keys": true`; the core host refuses any key in its environment, development or not. The
   canary and the zombienet harness declare theirs. The in-process core (ingress without
   `core_endpoint`) still takes its root from the environment, with a warning: it is never the
   production path.
10. **Supersedes** `F-005` NOTES decisions 15 (the root and clerk key in the core host) and 16 (no
    authentication on the local socket), and `F-016` NOTES decision 24 (the `KmsSigner` stub).
11. **Vault, not KMS** (owner ruling `key-custody-opt-in-kms`, 2026-10-04). `NFR-8` is the requirement; KMS
    is not. Every key's default source is Vault: the signer logs in itself with Vault's AWS IAM auth (it
    signs an STS `GetCallerIdentity` with its instance or task role's credentials, and Vault replays it),
    reads a KV v1 secret (values under `.data`), and holds the keys in memory only. Nothing secret is in
    its environment or configuration; a Vault token in a file is for development only. AWS KMS stays, as
    an opt-in per key, for secp256k1 lane and anchor keys only. The KMS-sealed path and KMS Ed25519 are
    dropped: Vault replaces them, and a second opt-in would cost code, docs and tests for no gain.
    Holders' passkeys (P-256) are never the signer's.
12. **The IAM role is the Vault credential.** Whoever holds the bound role can log in. On the core's node
    every process can reach the instance role through the metadata service, so the node must close it to
    every user but the signer's (an `iptables` owner match) and fetch the other processes' Vault secrets
    before, or under another role. One Vault role per signer (`<core signer role>`,
    `<settle signer role>`), each reading its own secret only. The owner ruled the restriction
    (`kb-q4`, 2026-10-05) over a signer on an instance of its own: uid `65533` alone may reach
    `169.254.169.254` and `fd00:ec2::254` (IMDSv2, hop limit 1), and the boot order is: other
    processes' secrets fetched by a oneshot unit under the node's own service role, then the lock,
    then the signer, then the core host and ingress.
13. **The signer follows a common Vault layout**: a KV v1 mount `secret/<service>`, an AWS auth backend
    at `auth/<service>` with an `iam` role bound to the service's task or instance role, and a
    `<service>-ro` policy. An operator with another layout configures it in `signer.json`.

14. **One voter's key is the root of an Agent's messages** (owner ruling `rq-02`, 2026-10-05, for the MVP,
    until `F-021`). In the sharded profile each Agent's host certifies every commit that sends a message
    to another Agent with that Agent's voter key (R-03, `F-033` `T-033-13`). The key follows decision 11:
    in Vault (`the core signer's secret`, fields `voter-book-<n>` and `voter-principals`), read and
    used only by the signer, never in KMS, never in the core host's process, never granted to the relay.
    A key apart from the Agent's authority key: the authority key signs VOS receipts, the voter key
    signs commit claims (domain `aledgent/stream/commit-signature/v1`).
    - **Alerts on signing anomalies.** Built: every certificate is checked by its receiver, and one the
      pinned voter key did not make, or one for another Agent or runtime, pages security at once
      (`AledgentR03CheckFailed`). The signer logs every Ed25519 signature (`signed`, with caller and key)
      and every refusal (`refused:`). Not built: an alarm on those refusals. The signer exports no metric;
      the alarm is a CloudWatch Logs metric filter, the operator's.
      The signer is on the go-live path, which is frozen, so no exporter was added to it.
    - **What the root means** (trade-off, stated plainly): the signer cannot judge a claim, so whoever
      can make it sign (the core host, while it runs) can certify a message the Agent never committed.
      R-03 cannot see that; reconciliation (`NFR-9`) is the backstop. Three voters (`F-021`) make it a
      quorum's.
    - Not in the canary: its single core Agent sends no message between Agents, and a key field its
      Vault secret does not hold would stop its signer at start (`config.rs`'s canary test lists the
      four keys).

15. **The AWS region is required, never defaulted** (owner ruling `oss-q10`, 2026-10-05, `T-035-10`).
    The STS login region (`vault.auth.aws_iam.region`) used to default to one region, and the KMS
    client fell back to the environment's. Now `Config::check` refuses a configuration without the
    login's region, and one with a KMS key but no `aws_region`, with an error that names the field
    (`REQ-AU-8`: never a default). The templates already set both.

## Measured

- The signer's round trip on a Unix socket is tens of microseconds; a transfer asks two to four questions
  (blindings, signatures). Not yet measured under the stress suite's load (`T-035-07` runs the signer in
  the suite; a run is the measurement).

## Deferred

- **The voter keys from the signer** (`rq-02`). The sharded host takes any `provenance::Voter`; the
  bench signs with test keys. The core host serving the sharded profile, and passing the signer's
  `voter-*` keys as its voters, comes with the sharded serve path.
- **The signer's refusal alarm**: a CloudWatch Logs metric filter on its `refused:` warnings, routed to
  security (infrastructure). A metric exported by the signer itself, paged by a rule, is post-freeze work
  (owner ruling `rq-06`, 2026-10-05; `T-035-11`, `PLAN.md` §4).

- **The shard prototype** (`crates/shards`, `aledgent-core-host shards`) still carries the root in its
  private input. It is not on the go-live path; it moves to answers before the book is sharded.
- **`02-pass.mjs`** registers a PASS device by signing its attestation with a local secret, which then
  goes into Vault. Only a lane opted into KMS would need that signature from the signer.
- **`opsctl backup/restore core`** of the in-process core read the root from the environment; the Agent
  core's are `TODO(F-005)` anyway.
- **Infrastructure**: the signer's Vault mount, IAM-auth roles and policies, the node's metadata-service
  restriction, the signer's user and socket directory on the node, the sidecar in the adapter's task, the
  ECR repository, and KMS keys for opted-in lanes. Not in this repository.
- **The link over TLS**, so that ingress can leave the core's host.
