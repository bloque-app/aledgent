<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-009 — Ingress process

| | |
|---|---|
| Milestone | M-2 — On VOS, end to end |
| Track | B |
| Home | `services/ingress` |
| Depends on | F-003, F-005, F-007, DEP-2, DEP-7, DEP-10 |
| Unblocks | F-016, F-017, F-018 |
| Realises | DEC-5, DEC-6, DEC-7 (notification half) |

## Goal
Every command and query reaches aledgent through one process, `aledgent-ingress`. It writes each command to
a durable log before it answers. A core restart then loses nothing and refuses nothing. A command that moves
value (a transfer, a reservation, a withdrawal) gets a `Received` acknowledgement at once; every other
command, and always an account or security command, gets its decision (`REQ-CS-8`, amendment 0008). The same process
drives deadlines with a ticker, and it delivers notifications from the core's outbox and records each
delivery.

## Spec traces
- `REQ-AR-7` — intake restarts apart from the core and never loses a received command.
- `REQ-CS-1`–`CS-8` — command fields, acknowledgement not outcome, the reservation acknowledged and decided after (amendment 0007), the two ways a command is answered (amendment 0008), refusal shape,
  forbidden commands, non-terminal labels, `Received` survives core outage and is decided within a bound.
- `REQ-OP-10`, `REQ-OP-11`, `REQ-RT-2` — client key and operation reference kept apart; key mandatory,
  derived and returned when absent.
- `REQ-PN-1` — initiating principal recorded on every command.
- `REQ-EV-1`–`EV-6`, `REQ-RT-4` — notifications: content, at-least-once, stable identity, authenticity,
  no call-back, durable delivery record.
- `REQ-RT-5`, `REQ-OP-5`, `INV-15` — deadlines advanced by the ticker (`DEC-6`).
- `REQ-AU-8`, `NFR-7`, `REQ-PV-5` — no credential in logs; default-deny routes; every access attributed.
- `NFR-5` — deploy without an outage of the book. `NFR-1` — the acknowledgement, and `NFR-15` —
  completion, both measured (amendment 0007); `NFR-16` — the decided reply, measured (amendment 0008).
- `US-M2`, `US-M3`, `US-K3`, `US-M5` (`AC-M2.*`, `AC-M3.*`, `AC-K3.*`, `AC-M5.1`); `AC-C1.1` through ingress.

## Scope
**In:** the HTTP surface for `CTR-CMD` and `CTR-QRY`; transport authentication; the received-log;
client-key dedup; minting the operation reference; the forwarder; the reservation path (acknowledged, then decided with priority); the
ticker; the notification delivery worker and its recipient registry; this unit's own metrics.
**Out:** every decision (`F-007`, `F-004`, `F-008`, `F-010`); authority and credential sets (`F-003`,
`F-012`); the outbox and what goes in it (`F-005`); settlement instructions (`F-016`); answering queries
(`F-011`, `F-006`); operator command semantics (`F-013`), which ingress only routes; dashboards and paging
(`F-018`).

## Design

**Shape.** One Rust binary with five parts that share one store: the surface, the received-log, the
forwarder, the ticker and the delivery worker. The surface runs on every replica. The forwarder and the
ticker run on one elected leader only.

**Proposal — store.** PostgreSQL with synchronous replication. Leaders are elected with an advisory lock.
Ingress is the unit that scales with request volume (PLAN §1), so several replicas must share one log and
one uniqueness constraint. *Alternative:* embedded `redb` on a single instance with a replicated volume.
Simpler, but it caps ingress at one process.

**Received-log.** One row per command:
`op_ref` (primary key), `principal`, `on_behalf_of`, `surface`, `client_key`, `key_derived`, `body_digest`,
`body` (canonical bytes), `reserves`, `received_at`, `status` (`Received` | `Forwarded` | `Decided`),
`core_input_seq`, `core_reply`. A unique index on (`principal`, `surface`, `client_key`) enforces the
domain `DEP-10` plans. The row is committed on the primary and the synchronous replica before any reply.

**Command path.**
1. Authenticate the transport. Every route is deny-by-default (`NFR-7`). Service principals use mutual TLS
   or a bearer token. Holder and agent credentials pass only when the `DEP-7` switch allows them. Ingress
   checks only that the caller is who it claims. The core checks authority and the credential set, so a
   revocation takes effect at once (`REQ-AU-3`). Credentials are stripped before anything is logged
   (`REQ-AU-8`).
2. Validate the shape with the shared `ledger-core` types (`REQ-CS-1`). Amounts go through `F-003`'s single
   conversion (`REQ-AS-4`). A command larger than the core's 16 KiB invocation limit is
   refused. For a multi-leg command that is `ERR-BatchTooLarge`.
3. Take the client key, or derive it as `H(domain ‖ principal ‖ surface ‖ canonical body)`. No time and no
   random value go in (`REQ-OP-11`). A derived key is returned in the reply.
4. Look up the key. Same key and same digest: return the stored reply. Same key and a different digest:
   `ERR-ClientKeyReused` (`REQ-OP-10`).
5. Mint the operation reference with `F-003`'s identifier kind. **Proposal:** 128 random bits from the OS
   CSPRNG. *Alternative:* a keyed MAC of (principal, surface, key). Random keeps the reference free of any
   caller input.
6. Append the row and commit.
7. A command that does not reserve: reply `{operation_reference, state: Received, terminal: false,
   client_key}` (`REQ-CS-2`, `REQ-CS-6`). A command that reserves: go to the reservation path.

**Forwarder.** It is the only writer into the core. It sends every core input: commands, ticks, the
adapter's observations and acknowledgements (`F-016`), and delivery records. It sends one input at a time,
because the core is a single-writer Agent. Each call goes to the VOS HTTP ingress with the
operation reference as the `Idempotency-Key`. A VOS 503 or 504 does not prove that
nothing ran, so the forwarder retries the identical request. The core is idempotent on the
operation reference (`F-007`, `INV-18`). The core returns a monotonic input number with each reply. Ingress
stores it in `core_input_seq`. This gives `F-018` an exact replay order. Reservations go to the front of the
queue. Order inside the core is by input number, not by log order. The decision bound of `REQ-CS-7` is a
configured value. The age of the oldest `Received` row is a metric.

**Reservation path** (`REQ-CS-3`). The command is logged, then forwarded at once. Ingress waits for the
core's durable decision and replies `Reserved`, or `Rejected` with its `ERR-*`. If the core is down, the
request stays open until the core decides. `REQ-CS-3` forbids a reply before the reservation is durable. A
caller that times out retries with the same client key and receives the decision. See Open points.

**Ticker** (`DEC-6`). The leader appends a tick input `{now}` every `tick_interval` and forwards it. `now`
comes from the ingress host clock. The core moves deadlines forward only from ticks (`REQ-RT-7`). The
schedule is the set of deadlines in core state, so it survives any restart (`REQ-RT-5`). Ticks are logged
inputs, so replay is deterministic. The core records the time of the last tick, and `F-018` derives the
ticker's age from it (`RISK-7`). If `DEP-2` brings self-firing timers, the ticker is removed and the core
interface does not change.

**Notification delivery** (`DEC-7`).
- The worker pages through the core's outbox with a query (`kind = notification`, cursor). Pages fit the
  16 KiB reply limit.
- The core sets each notification's identity as `H(op_ref, from_state, to_state)` (`REQ-EV-3`). The body
  is `{id, operation_reference, state, outcome?, error?}` and stands alone (`REQ-EV-1`, `REQ-EV-2`).
- The recipient registry maps a principal to an endpoint and a verification secret. Secrets are encrypted
  at rest and never logged. A recipient with no secret gets no delivery. The notification stays
  undelivered and visible (`REQ-EV-4`).
- **Authenticity** (`REQ-EV-4`; ruled by amendment 0009, `OQ-51`): HMAC-SHA256 under a secret shared
  with each recipient, over `id ‖ attempt_time ‖ body`, sent in a header with `attempt_time`. The
  recipient rejects an old timestamp or a repeated `id`. The Ed25519 alternative is withdrawn: aledgent
  signs no notification. The recipient registers its destination and secret through the API (`T-009-15`, `OQ-66`).
- After a 2xx reply, the worker sends `record_delivery(id)` through the forwarder. The core marks it
  delivered in committed state (`REQ-RT-4`, `REQ-EV-6`). Failed attempts are kept in the ingress store.
  Retries back off exponentially and never stop. Nothing is dropped.
- The worker never blocks the core, and the core never waits for a recipient (`REQ-EV-5`).

**Queries.** Ingress authenticates a query and routes it to `F-011`'s query entry points. Queries are not
core inputs and do not enter the received-log. The access log keeps the request id and the principal only
(`REQ-PV-5`).

**Proposal — wire format.** HTTP/1.1 with JSON, described by OpenAPI 3.1 generated from the shared Rust
types. Amounts are decimal-integer strings in minor units. A refusal is a 4xx with `{"error": "ERR-…"}`,
never a 200 (`REQ-CS-4`, `REQ-ER-1`). *Alternative:* gRPC. JSON is easier for agents and integrators to
read.

## Test plan
- **Unit.** Key derivation is stable across retries and holds no time (`REQ-OP-11`). Repeat with the same
  body returns the same reply. A different body gets `ERR-ClientKeyReused` (`AC-A1.3` at the surface).
- **Contract.** Every `ERR-*` reply is checked against the OpenAPI description. A mismatch fails the build
  (`REQ-CS-4`, `REQ-ER-1`). Every non-terminal reply carries `terminal: false` (`REQ-OP-7`).
- **Integration**, ingress and the core on a Local Agent: `AC-C1.1` (reserve returns after the durable
  decision); `AC-M2.1`–`AC-M2.3`; `AC-M3.1` with every recipient down; `AC-K3.1` and `AC-K3.2` across a
  worker crash between delivery and record; `AC-C4.1` with `F-007` (a deadline passes only after a tick,
  `INV-15`); `INV-18` (a forwarded input sent twice changes nothing).
- **Chaos.** Steady mixed load. Kill and restart the core, then an ingress replica, then fail over
  PostgreSQL. Count acknowledged operation references and delivered notification ids.
- **Latency.** Record p99 of the acknowledgement (`NFR-1`), of the decided reply (`NFR-16`) and of
  completion (`NFR-15`); the reservation path is part of completion (amendment 0007). Measurements for
  `M-2`, not gates.

## Exit criterion
`AC-M2.*`, `AC-M3.*` and `AC-K3.*` pass. The core is restarted ten times under steady mixed load. Every
acknowledged operation reference reaches a decision. Every owed notification is delivered at least once and
recorded. Every `Received` row is decided within the `REQ-CS-7` bound after the core returns.

## Open points
1. **Proposals:** PostgreSQL store; random operation reference; HTTP and JSON with OpenAPI; HMAC
   notification signing.
2. **Reservation while the core is down.** Holding the request open meets `REQ-CS-3`. §10 had no
   error for "the deciding component is unavailable". *Ruled by amendment 0006 item 8:*
   `ERR-DecisionUnavailable`, safe to retry with the same client key; past a stated wait the
   request is answered with it, and the command stays logged.
3. `OQ-51` and `OQ-52` are closed by amendment 0009: relay to the caller's destination with a shared
   secret, and a push contract for every kind of principal (`REQ-EV-4`, `REQ-EV-7`). Ingress pushes to
   endpoints the operator registers until `T-009-15`; `operation(reference)` stays for recovery.
4. `DEP-10`: the client-key domain is (principal, surface) and must be revisited before `M-2`.
   `DEP-7`: holder credentials are a switch. `DEP-2`: the ticker stays until the host has timers.
5. Interface asks of `F-005` and `F-007`: accept an ingress-minted operation reference, return an input
   number, and expose the outbox and last tick time as queries.
6. The 16 KiB invocation limit caps command size and multi-leg size (`OQ-24`).
7. Ticks use the ingress clock. `OQ-36` (which clock is authoritative) stays open. `OQ-35` (which
   non-terminal states callers see) is `F-007`'s. Ingress shows the state the core returns.
8. `RISK-7`: a stalled ticker stalls deadlines. Leader failover and the ticker-age page in `F-018`
   mitigate it.
