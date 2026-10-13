<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-009 — implementation notes

Written while implementing `F-009` (issue #9), 2026-09-23. The plan's proposals were followed as
accepted: PostgreSQL for the received-log, a random 128-bit operation reference, HTTP and JSON
described by OpenAPI 3.1, and HMAC-SHA256 notification signing. Where the plan was silent, or where
it and the merged code disagreed, the more conservative behaviour was chosen (refuse rather than
allow, hold a request open rather than answer early); each such choice is listed here for review.

## The boundary, and issue #38

`F-005` (the core as a VOS Agent) is blocked on the owner's decision in issue #38: whether the
reservation decision runs inside a VOS Agent (`DEC-4` as planned) or in a native process (`DEC-1`;
`notes/MEASUREMENTS-F002.md` §8 measured 737.8 ms p99 inside an Agent, then read against a 50 ms
budget that amendment 0007 has since withdrawn: the decision is part of completion, `NFR-15`).
Ingress is needed under both. So everything ingress needs from the deciding component is one Rust
trait, `ingress::boundary::Core`:

| Method | What it carries |
|---|---|
| `apply(CoreInput) -> CoreReply` | One input — a command, a tick (`DEC-6`) or a delivery record (`DEC-7`) — and its reply: the monotonic **input number** and the outcome (decided with its state, refused with its `ERR-*`, a tick's report, a recorded delivery). Idempotent on the input's key (`INV-18`, `REQ-RT-2`). |
| `outbox(cursor, undelivered, now)` | A page of the notifications the core committed with its transitions, with whether each one's delivery is recorded (`REQ-RT-4`, `REQ-EV-6`). |
| `last_tick()` | The time of the last tick the core applied (`RISK-7`). |
| `query(caller, Query, now)` | A query as a principal; not an input (`REQ-QR-3`). |

A failed call is `Unavailable` (the input certainly did not run) or `Ambiguous` (it may have: a VOS
503/504); the forwarder retries the identical input either way. Every value that
crosses the boundary is serialisable, so an implementation behind a network call carries the same
values as one in process.

On ingress's side of it: the durable received-log, client-key dedup, the `Received` acknowledgement
(transfers and, since amendment 0007, reservations), the forwarder, the ticker and notification delivery.

**One implementation** is provided, `ingress::inprocess::InProcessCore`: `ledger-core` in this
process, over `F-001`'s in-memory store, behind a write-ahead journal of its inputs (memory or a
file). It is what the binary and every test run today. **The VOS Agent implementation is `F-005`'s
and is not built here.** Whatever #38 rules, `F-005` implements this trait; if #38 rules (b), the
in-process implementation is already the native one, and needs the durable service store of
`F-006` in place of the replay below.

## What landed

| Where | What |
|---|---|
| `crates/ledger-core/src/ledger/transfers.rs` | `Ledger::receive_transfer`: the core's half of a `Received` acknowledgement (closes `F-007`'s and `F-008`'s `TODO(F-009)` for intake at `Received`) |
| `services/ingress/src/wire.rs`, `query.rs` | The wire forms of commands, operator commands, replies, notifications, and `F-011`'s queries, replies and cursor (closes `F-011`'s "F-009: the wire form") |
| `services/ingress/src/command.rs`, `operator.rs` | Validation into `ledger-core`'s types, canonical forms, request digests, derived client keys, the intake of each command |
| `services/ingress/src/auth.rs` | Deny-by-default bearer authentication; token digests only; the `DEP-7` switch |
| `services/ingress/src/boundary.rs` | The `Core` trait and what crosses it |
| `services/ingress/src/inprocess.rs` | `InProcessCore`, `MemJournal`, `FileJournal`, fault injection |
| `services/ingress/src/store/` | The received-log, delivery attempts, recipients and leadership: `mem.rs`, and `pg.rs` with `migrations/0001_received_log.sql` |
| `services/ingress/src/service.rs` | One replica: leadership, the forwarder, the ticker, the delivery worker |
| `services/ingress/src/delivery.rs` | Signing, the reference verifier, the HTTP transport, backoff |
| `services/ingress/src/surface.rs`, `openapi.rs`, `openapi.json` | The routes of `CTR-CMD`, `CTR-QRY` and `CTR-OPS`, the access log, and the generated OpenAPI 3.1 document |
| `services/ingress/src/metrics.rs`, `config.rs`, `clock.rs`, `main.rs` | Metrics, configuration, the ingress clock, the `aledgent-ingress` binary |
| `services/ingress/src/testkit.rs` | An in-memory deployment for the tests and the conformance suite |
| `services/ingress/tests/` | `surface.rs`, `forwarder.rs`, `log_scan.rs`, `postgres.rs` (testcontainers) |
| `crates/conformance/src/cases/f009.rs` | Every `F-009` case, run |
| `.github/workflows/ci.yml` | Docker checked on the test runner; the no-float check over `services/` |

## Decisions taken during implementation

### The command path

1. **The operation reference is random**, 128 bits from the OS CSPRNG (`F-009`'s accepted proposal),
   not `F-007`'s `keys::operation_reference` derivation. The core takes the reference it is given
   (`F-007` decision 7 made every command idempotent on it), so both work; the random one carries no
   caller input. The mapping from client key to reference is the received-log's unique index.
2. **The client key's domain is (principal, surface, key)** (`DEP-10`, as planned): commands on
   `CTR-CMD` and operator commands on `CTR-OPS` are two domains. A derived key is
   `H(surface, H(domain ‖ principal ‖ on-behalf-of ‖ surface ‖ canonical body))`, through
   `F-007`'s `keys::derive_client_key`, returned as hex, and a retry that supplies it names the same
   request. The canonical body is re-rendered from the parsed command, so `"010"` and `"10"` are
   one request.
3. **Only transfers are acknowledged at `Received`.** The plan's step 7 acknowledges every command
   that does not reserve. But only a transfer has a core-side `Received → Rejected` path
   (`F-008` decision 11): every other command the core decides and completes at once, and a
   refusal of it records nothing (`F-007` decision 8) — so a refusal after a `Received` reply would
   be reported nowhere, no notification and no operation to query (`REQ-CS-4`, `REQ-EV-1`). Those
   commands (capture, release, issue, redeem, identities, accounts, grants, operator commands) are
   logged first like every command, then answered with the core's decision, in arrival order.
   `REQ-CS-2` allows it: the state may be `Received`, not must. **Since the owner's ruling of
   `OQ-61` (2026-09-29, amendment 0007) reservations (`reserve`, `withdraw`) are acknowledged at
   `Received` too, first in the queue**: the core takes them in with `Ledger::receive_command` and
   records a refusal as `Received → Rejected` (or `Received → Accepted → Rejected` when the kernel
   refuses), with its notification, exactly as for a transfer. A repeat of the same request gets the
   same refusal. `ERR-DecisionUnavailable` now applies to the commands still answered with their
   decision. **Amendment 0008 (ruled 2026-09-30) makes this the spec's rule** (`REQ-CS-8`): a command
   that moves value is acknowledged at `Received`; every other command is answered with its decision,
   and an account or security command always is, never `Received`. Checked on 2026-10-01: ingress and
   both cores already behaved so (`Command::intake`, and the core's `Acknowledged` names only
   transfer, reserve and withdraw). `Command::intake` now names every variant, so a new command must
   be put in one way or the other on purpose. `capture`, `release`, `issue` and `redeem` stay answered
   with their decision while `OQ-63` is open.
4. **A shape fault is `ERR-MalformedCommand`** (amendment 0021, `REQ-ER-3`, ruled on 2026-10-05; it
   replaces the earlier choice of failing closed as `ERR-NotAuthorised`, which §10 then had no
   condition for). A body that is not the command, an unknown or missing field, a wrong type, an
   identifier, hex or enumerated value that does not parse, an empty list that must not be, a header
   that does not parse, a client key outside its bounds, and a body over the 16 KiB limit for anything
   but a transfer all get it, with status 400. A value that parses but names nothing keeps its own
   condition: an account `ERR-UnknownAccount`, an operation `ERR-UnknownOperation`, an asset
   `ERR-UnknownAsset`, an amount `ERR-PrecisionExceeded` (as `F-003` refuses a decimal it cannot
   represent). A transfer over the limit is `ERR-BatchTooLarge` (`REQ-CS-10`). A proof that does not
   decode stays `ERR-CredentialInvalid`, the no-bearer routes stay deny-by-default, and queries keep
   their own conditions.
5. **Amounts on the wire are decimal-integer strings of minor units**, as planned. So ingress never
   needs the registry to convert them; `F-003`'s `Ledger::parse_amount` stays the one decimal
   boundary for any surface that takes decimals.
6. **HTTP statuses.** Every refusal is a 4xx (`REQ-CS-4`): 401 `ERR-CredentialInvalid`; 403 for
   authority and restriction; 404 for an unknown account, asset or operation; 409 for a reused
   client key, a terminal operation, an invalid transition, a duplicate observation; 413
   `ERR-BatchTooLarge`; 422 for the rest. The table is published per condition in the OpenAPI
   document (`x-http-status`) and the contract test checks every one.
7. **A reservation while the core is down holds the request open** (plan open point 2): no reply
   before the durable decision (`REQ-CS-3`), no timely refusal without an amendment. A caller that
   gives up retries with the same client key and gets the decision. Queries while the core is down
   are held open the same way.
8. **A command acknowledged at `Received` that the core refuses before opening an operation** —
   its credential was removed since ingress authenticated it, say — has no operation in the core
   to query. `operation(reference)` then shows the refusal the core returned for that row, as
   `Rejected` with its `ERR-*`, so a caller told `Received` is never left without the outcome
   (`REQ-CS-6`). A row the core has not decided shows `Received`, from the log, even while the core
   is down.

### The received-log

9. **Ticks and delivery records are rows too.** Every core input is a row, so the input numbers the
   rows hold are gapless over everything the core applied (`T-009-03`), and replay order is exact.
   At most one tick waits undecided: ticks do not pile up during an outage, and the first after it
   carries the current time.
10. **Forward time is fixed on the first attempt.** A command's `now` is the time it was first
    forwarded, kept on the row, so every retry is identical. A command forwarded
    before an outage is decided with that time.
11. **Commit before reply, with synchronous commit.** Every PostgreSQL connection runs with
    `synchronous_commit = on`; with `require_sync_standby` at `true` the store refuses to start
    unless the server names a synchronous standby. Tests run a single server with the requirement
    off. **`"rds-multi-az"`** (`T-009-14`, the owner's ruling of 2026-10-02): an RDS Multi-AZ
    instance replicates every write to another zone at the storage level before the commit returns,
    and names no synchronous standby, so `true` would refuse it. The store checks only that the
    server is RDS (it defines `rds.*` parameters) and trusts the setting as declared; whether
    Multi-AZ is on cannot be seen from PostgreSQL. Trade-off, stated plainly: a deployment that
    declares it on an RDS instance without Multi-AZ gets a single-zone instance's durability and no
    warning. `false` suits only a single-zone instance whose operator accepts that loss.
11a. **TLS in production** (`T-009-14`, ruled 2026-10-02). The configuration's `profile` has no
    default. Under `production` the store refuses, before it opens any connection, a connection
    string that does not say `sslmode=require`; the refusal names the mode found, never the string.
    rustls then checks the server's chain and host name, so `require` here is libpq's
    `verify-full`; tokio-postgres parses no `verify-ca` or `verify-full`, so `require` is the one
    value. `development` takes the string as written. The adapter applies the same check
    (`T-016-13`), as does `opsctl restore core`, with ingress's profile.
12. **Leadership in PostgreSQL is a session advisory lock** on a connection of the replica's own, as
    planned; in memory it is a lease its holder renews every third of its length. A leader that
    dies loses the lock with its connection, or the lease when it runs out: one tick interval, with
    the lease set to it. Trade-off, stated plainly: a leader partitioned from the database but still
    running keeps forwarding until its connection drops, so two forwarders can overlap briefly. The
    core's idempotency on the input key keeps that safe (`INV-18`); it can reorder inputs.
13. **Waiting callers on another replica poll.** A caller waiting on a decision is woken at once
    when its own replica leads, and otherwise reads the row every `poll` (20 ms by default).
    PostgreSQL `LISTEN/NOTIFY` would remove the poll; not built.

### The client key's domain (T-009-16, amendment 0011)

The decisions below take the conservative reading where amendment 0011 R4 leaves a gap; the owner
may rule otherwise.

- **The domain.** A command row is unique on (domain, surface, client key). The domain is
  `party:<did>` for a principal tied to an integrating party, and the principal's own `did:` for one
  tied to none (migration `0003_client_key_domain.sql`; every earlier row was a principal's own, and
  is backfilled so). The principal stays a column of its own, and the request digest names it, so
  the same key from another principal of the same party is `ERR-ClientKeyReused` (409), never the
  first principal's reply. The core needs no change for that: it replays by operation reference
  and refuses a request whose initiator differs. `keys::operation_reference` now derives from the
  domain (`KeyDomain`); a principal tied to none derives what it derived before.
- **Where ingress learns a tie.** From its own store first (`party_ties`), then from the core
  (`boundary::Core::party_of`, in-process, Agent and remote cores), keeping what the core names.
  The forwarder also keeps every tie a `register_integrating_party` or `tie_principal` the core
  accepted, before it marks the row decided: so every command that arrives after the operator has
  the tie's reply is logged in the party's domain, also while the core is down. A tie is never
  undone, so a kept tie is never stale.
- **A tie made after the principal used keys.** A key the principal used before it was tied stays in
  its own domain: a repeat of it is answered from that row (a changed request under it is
  `ERR-ClientKeyReused`), and no other principal can reach that row. Keys first used after the tie
  are the party's. So no key is ever answered across domains; the cost is that a principal's
  pre-tie key and another party principal's identical key are two requests.
- **Known limit.** A principal whose tie ingress has not kept, asked about while the core does not
  answer, is logged in its own domain. Only a tie that did not pass through this ingress's forwarder
  can be in that state (one restored with the book by `F-014`), and only until the first command of
  that principal while the core is up. The effect is the one above: its key is answered to it
  alone, never across domains, and a colliding key from another principal of its party in that
  window is a second request rather than a refusal.
- **Cost on the command path.** A principal tied to none costs one read of the core per command
  (a map lookup in process; one local-socket round trip to the Agent host). A tied principal costs
  one store read for the tie and one for its own pre-tie key.

### The in-process core

14. **Durability by replay.** `ledger-core`'s service store is in memory until `F-006`/`F-005` give
    it a durable form. So the in-process core journals every input before applying it and replays
    the journal from genesis on start (`REQ-RT-3`; time comes in the input, `REQ-RT-7`; signing
    randomness is seeded, `F-001`). An input journaled but not applied is applied by the replay; an
    input whose reply was lost is answered from the replayed replies with its first input number.
    Trade-off, stated plainly: a start costs one kernel apply per input ever received. That is fine
    for tests and a trial; it is not a production core. Either `F-005`'s Agent (with its own
    commit) or the native core on `F-006`'s durable store replaces it.
15. **Delivery records live beside the book**, in the in-process core's committed state (the set of
    delivered notifications, rebuilt by the replay), not in `ledger-core`'s operation store. The
    outbox and what goes in it are `F-005`'s to make durable (plan §Scope); `F-005`'s
    implementation of the trait decides where its records go.
16. **Who a notification is owed to**: the operation's initiator; for an operation with none (an
    inbound arrival nobody initiated), the operator's organisation identity. SPEC names no other
    party. `OQ-51` and `OQ-52` are closed (amendment 0009): every kind of principal gets push.
17. **The in-process core ships no audit records** to `F-013`'s audit store: `F-005`'s audit worker
    does (`F-013` NOTES). The records are in the service store and survive a replay.

### Notifications

18. **Signature** `v1=HMAC-SHA256(secret, domain ‖ id ‖ attempt_time ‖ body)`, each part
    length-prefixed, in `aledgent-signature`, with `aledgent-notification-id` and
    `aledgent-attempt-time`. The reference verifier rejects a bad signature, a stale attempt and a
    body that names another notification, and recognises a repeated identity as a duplicate — which
    is also how a recipient absorbs an at-least-once redelivery (`AC-M2.3`, `AC-K3.2`).
19. **Secrets at rest** are sealed with ChaCha20-Poly1305 under the deployment's sealing key, the
    principal as associated data. Recipients are registered with `aledgent-ingress recipient`; the
    secret is read from an environment variable, never from an argument.
20. **Backoff** doubles from 500 ms to a 60 s ceiling per notification and never stops. A recipient
    with no endpoint or no secret is an attempt kept like a failure (`REQ-EV-4`, `REQ-EV-6`).

### Surfaces

21. **Every route authenticates first**, the OpenAPI document and the metrics included; an unknown
    route or method is refused the same way (`NFR-7`). Service principals use bearer tokens; mutual
    TLS is not built (the plan offers either).
22. **`CTR-OPS` is routed** for every `F-013` operator command but the privileged read, which needs
    the audit sink the core holds and has no owning feature.
23. **The adapter's surface is not built** (`F-010`'s `TODO(F-009)`: `record_observation`,
    `acknowledge`, the head, authenticating the source). It belongs with the adapter it
    authenticates, `F-016`; the boundary's input type has room for it.
24. **Time is milliseconds since the Unix epoch**, the ingress clock's unit (`DEC-6`). The
    deployment's deadlines and decision bound are configured in the same unit. `OQ-36` is
    untouched.

### The latest final anchor (T-009-17, amendment 0011 R3)

25. **What the core records.** `Notification` gains `anchor: Option<u64>`, the number of the latest
    checkpoint whose status was `Final` when `commit_plan` committed the transition, read once per
    commit before its writes apply. It is written with the notification and never rewritten, so a
    redelivery carries the same body (`REQ-EV-3`). The identity stays `H(op, transition key)`: the
    anchor is not part of it. A commit that both finalises a checkpoint and records a transition
    (none does today: finality comes from an `Anchored` record's canonical check, which moves no
    operation) would name the anchor final before it, the conservative reading of "final when the
    transition was recorded".
26. **Two leaves, not a walk.** `AnchorStore::latest_final` reads the last checkpoint and the one
    before it by number, instead of scanning every checkpoint backwards: one checkpoint is in flight
    at a time, so the latest final one is one of those two. Every transition now reads it, and a
    scan would have put every checkpoint the book has ever taken into each core invocation's
    witnessed read set (`F-005`).
27. **The body resolves the rest.** The wire body (`NotificationBody.anchor`, always present,
    `null` when none was final) is `{ number, entry_sequence, coordinate: { instance, seq, digest },
    height }`. Ingress's `notification_body` resolves the number against the anchor store on every
    read of the outbox. A final checkpoint never changes (`Final` absorbs), so every read gives the
    same body. The page carries the whole body rather than the number alone: delivery sends what
    the page holds, and ingress has no anchor store of its own.
28. **The block, not the time.** The owner asked for the anchor's block and time. A final
    checkpoint now keeps the height of the block that wrote its `Anchored` record
    (`Checkpoint::height`, from the observation's `at`, recorded by `AnchorWrite::Final` and part
    of the final record, export format 9). The settlement layer's block time is not observed by
    the adapter today, so no time is carried; the core's own clock at finality would be a
    different fact. Adding it needs the observation to carry the block's timestamp (`F-016`).
29. **The page budget, measured.** `boundary::tests::an_outbox_page_at_its_longest_fits_the_reply_limit`
    builds a page of `OUTBOX_PAGE` notifications with every field at its longest — the longest
    state, the longest refusal with its description, every number at `u64::MAX` — and checks it
    against the 16 KiB reply limit (`REPLY_LIMIT`). Measured: 887 bytes per notification with the
    anchor, 652 without it, 392 without a refusal. The old page of 50, sized on "well under 256
    bytes" each, was already over the limit for refused notifications (about 32 KiB) before the
    anchor; with it, 50 would be about 44 KiB. `OUTBOX_PAGE` is now 16 (about 14.3 KiB at the
    longest). The delivery worker pages by cursor, so a smaller page costs only more reads.

### The answering setting (T-009-18, amendment 0013 R1)

30. **The answering setting lives in ingress's store, not the book** (amendment 0013, `T-009-18`).
    It decides when ingress replies, not what the core decides, so the core never sees it: a row is
    logged `synchronous` or not, and that flag is all the forwarder and the reply read. It is kept
    with who set it and when (`REQ-AU-7`), logged on change, and read only for transfers and
    reservations. A store that does not answer that read holds the request open, as an append does:
    replying `Received` to a principal that asked for decisions would break its contract. The routes
    sit outside the received-log, like `/v1/auth/*`: setting it to the same value twice has the same
    effect, so it needs no client key.
    Its audit record is that row and the log line; it does not reach the core's audit trail, which
    records operations. **Ruled by the owner on 2026-10-04** (amendment 0018 R3, `REQ-CS-9`): ingress's log is
    enough, since the setting changes no balances, so it stays here.

### Pass-through data (T-009-21, amendment 0020 R1, `prq-08`)

31. **Pass-through data lives in ingress's received-log, not the book.** `REQ-PO-5` says where it
    comes back, not where it is kept, and `T-009-21` planned to carry it to the core with the
    command. It is not carried: ingress takes `reference` off the five commands that submit an
    operation of their own (`transfer`, `reserve`, `withdraw`, `issue`, `redeem`) before the command
    is canonicalised, folds it into the request's digest (`request_digest_with`: unchanged for a
    command without one, so older rows keep their digests), and keeps it in the row
    (`received_log.pass_through`, migration `0005`), which is committed before any reply names it
    (`REQ-AR-7`) and never deleted. Ingress echoes it from there, per reader: in the reply, in
    `GET /v1/operations/{reference}`, in each notification before it signs the body, and in each
    movement of `GET /v1/movements` and `GET /v1/statements`. Why:
    - `REQ-PO-4` holds by construction: what the core never receives, no decision can branch on.
    - `INV-17` holds by construction: it is in no state commitment, no disclosure and no anchor,
      not even as a leaf's hash.
    - The core Agent is unchanged: no new input, no host call, no state, `core.vos` the same size.
    - The outbox page budget (decision 29) is untouched: 1 KiB a notification would not fit.
    Trade-offs, stated plainly. The book's export does not hold it: the core never does. **Ruled
    2026-10-09 (`pq8-export` (a)):** the core backup carries it beside the export, read from this log
    after the checkpoint (`ops::backup::core`, artifact `pass-through.jsonl`, format
    `aledgent/pass-through/v1`: operation, principal, the data as sent), and an import restores it
    onto the restored log (`ops::restore::restore_pass_through`: missing data written back, equal
    data left, other data or another principal's row a failed restore). The cost the owner accepted:
    the received-log is the only copy, so it is backed up and replicated with the book (PostgreSQL
    base backup and WAL, `ops::backup::pg`). Each echo is one read of the store:
    one keyed read per command reply that names another operation, per operation read and per
    notification, and one `op_ref = ANY(…)` read per page of movements. A store that does not
    answer an echo makes the read `ERR-Internal` and ends the delivery pass, rather than answer
    without the data.
    - **Who sees it.** The submitting principal, and a principal tied to the same integrating party
      (`REQ-ID-7`); a principal tied to none is its own party. The payee of a transfer reads its own
      movement without it (`AC-M8.4`). A notification goes to the operation's initiator, so it
      carries it.
    - **Capture and release** act on the reservation's operation (`INV-14`): they submit no
      operation of their own, take no `reference` (unknown field, `ERR-MalformedCommand`), and their
      reply carries the reservation's.
    - **Kept as sent.** The value is the caller's bytes (`serde_json`'s `RawValue`): key order,
      spacing and numbers are never re-read, so it comes back byte for byte. The bound is on those
      bytes, 1 024 at most. Two spellings of one value are two requests under one client key.
      `null` is none. Over the bound, or not one JSON value, the body does not parse:
      `ERR-MalformedCommand` (`REQ-ER-3`, amendment 0021), never logged; its `Debug` shows its length
      only.
    - **A refusal carries none.** A refusal's body is `{error, description}` and nothing else
      (`REQ-CS-4`); a command acknowledged and then refused shows it on the operation and its
      notification.

### Latency from the received-log (T-009-25, owner ruling of 2026-10-13)

32. **The latency bounds are read from ingress's own database.** The owner ruled on 2026-10-13
    that the commands already stored in ingress's database are timestamped there, by a change
    within ingress and not in the core, so `NFR-1`, `NFR-15` and `NFR-16` are queryable through
    plain SQL. Migration 6 adds `arrived_at`, `replied_at`, `completed_at` and `submitted_at`.
    - **`arrived_at` was added beside the two asked for.** `received_at` is taken after
      authentication and the party and answering lookups, and both bounds run from arrival
      (finding F4: an API key's lookup is part of what the caller waits for). It rides in the
      `INSERT`.
    - **`replied_at` is best-effort, off the reply's path:** a `try_send` after the reply, a
      batched writer per replica, `synchronous_commit = off`, and `FOR UPDATE SKIP LOCKED`: a row
      the forwarder holds is skipped and its stamp retried with the next batch, so the forwarder
      never queues behind a stamp. The first version locked rows in `op_ref` order on both sides,
      which made the forwarder wait on the stamper; the x86 stress run on PR #235 showed it near
      saturation (−3.8 % decided at 250/s, decided-reply p99 ×4). A repeat is never stamped. A
      lost stamp stays NULL and is counted.
    - **`completed_at` follows amendment 0018.** At the decision for a refusal, a terminal state,
      `Reserved` for a reservation, or a command with no lifecycle, in the decision's own
      `UPDATE`. A withdrawal completes at the first relay of `Observed` (or a terminal state) to its
      requester; the in-process completion series moved with it (it counted a withdrawal at
      `Reserved`).
    - **A withdrawal's clock starts at its submission** (the owner's ruling on PR #235): `withdraw`
      only reserves, and the requester's delay before `submit_outbound` is not aledgent's. When
      an accepted `submit_outbound` is decided, the forwarder stamps the withdrawal's row with that
      command's arrival (`submitted_at`, one primary-key `UPDATE` per withdrawal); the span runs
      from there to `completed_at`, in the queries and in the in-process completion series.

### The migration runner (T-009-26, owner ruling `v0w-ingress-migrations`, 2026-10-13)

33. **The in-house runner is kept and hardened, not replaced by a library** (ruling (a)).
    `services/ingress/src/store/migrate.rs`.
    - **Checksums, with a one-time backfill.** `ingress_schema` gains `checksum` (SHA-256 of the
      file's exact bytes, hex), added by the runner itself before the loop and only when missing.
      A version recorded with NULL (one applied before checksums were kept) is backfilled from the embedded
      file the first time a migrator sees it, logged at `WARN`. From then on, a difference is a
      start-up error naming the version. Trade-off, stated plainly: a file edited before its
      backfill is recorded in its edited form.
    - **The lock timeout is set after the migrator's lock, not before it.** Every transaction takes
      `ingress_schema` `EXCLUSIVE` first, with no bound (logging first when it has to wait), and
      then runs `SET LOCAL lock_timeout`.
      Only another migrator holds that lock, and its own waits are bounded. Bounding this wait
      too would make replicas that start together during a slow migration fail and restart in a
      loop, and that wait never holds back an append. Everything the file then locks is bounded.
    - **Mismatch is checked before anything changes**, by a read-only pass, and then again per
      version under the lock. A version the build does not have (applied by a later build, as
      after a rollback) is reported `applied/unknown` and never refused, so the previous build
      still starts.
    - **Nothing to do takes nothing.** With nothing pending and every checksum recorded, `apply`
      only reads, so `opsctl` and a replica left at `"apply"` start under a role limited to rows.
    - **`"verify"` passes a missing checksum with a warning.** It writes nothing, so it cannot
      backfill. Pending and mismatched versions are refused, as asked. `migrate --apply` records
      the missing checksums.
    - **`migrate` needs no sealing key**: it reads the configuration only for `profile` (TLS) and
      `migrations`, and the URL from `ALEDGENT_INGRESS_DATABASE_URL`, which it never prints.
    - **`lock_timeout_ms: 0` is refused**, because PostgreSQL reads it as no bound. The timeout
      also bounds how long appends queue behind a migration's lock request; 5 s stays the default,
      as ruled.
    - **Creating `ingress_schema` is serialised** by an advisory lock, and a duplicate `CREATE` is
      taken to mean the table is there. A migrator that waited on the lock can miss a table
      created meanwhile, because its catalogue cache is refreshed only on its next relation lock.
      Before this, migrators that started together on an empty database raced and all but one
      failed (PR #240's review, R1).
    - **No outer bound on the migrator's lock wait** (review R6, declined in part): a peer's lock
      waits are bounded, but its statements' running time is not, and a bound would restart
      replicas in a loop behind a legitimately slow migration. The wait is logged instead.

### The sidecar's principals as configuration (T-009-27, owner ruling of 2026-10-13)

34. **The sidecar is a product-neutral test fixture: its service principals are configuration, not
    an enum** (ruling: "re-seed with a test fixture"). Until now `e2e/sidecar` hard-coded three
    roles under one integrating party's service names; that party now passes its own names to the
    image, and the repository carries none. `e2e/sidecar/src/lib.rs` (`parse_principals`,
    `load_principals`), `deploy/docker/SIDECAR.md` §Service principals.
    - **Roles are named after `SPEC.md`, not after a service.** `issuance_authority` (issuance
      grants, `REQ-AS-6`, `REQ-AU-10`), `operator_authority` (an operator grant over every account
      in delegated custody, by kind, `REQ-AU-5`) and `identities_principal` (`open_identity`,
      `REQ-ID-6`): the authority kinds of `REQ-AU-7` and the role of `REQ-ID-6`. Names are free
      (`[a-z][a-z0-9_-]*`, at most 64 bytes, never `operator`, which the recipients use).
    - **Grant parameters are the role's own**: `ops` (a subset of the role's kinds; for operator
      authority, of the operator's own grant), `assets` (issuance only, each in the registry) and
      `per_day_limit` (a canonical decimal from 1 to `u64::MAX`, the core's bound, `REQ-AU-2`). A
      field the role does not take is an error, not ignored, and so is a field given twice in one
      object (`serde_json` would keep the last). At most one identities principal: the role has
      one holder at a time (amendment 0025). One function, `check_principals`, holds the rules,
      and both the parser and every start apply it, so an `Options` built by hand fails as early
      and as clearly as a configuration (PR #244's review, F1 and F4).
    - **JSON, inline or from a file; not TOML.** `serde_json` is already a dependency, and an
      inline value fits a GitHub Actions `services:` entry. Both set is an error; neither is the
      sample; set but blank is an error, never read as unset, so an unresolved CI expression cannot
      open the sample by accident. The previous `ROLE=TOKEN_VAR` spelling is refused with an error
      saying it is retired, never read as an empty set.
    - **A test token is an explicit opt-in, never a fallback** (`REQ-AU-8`; PR #244's review, F2).
      Each principal names exactly one source: `token_env`, or `"test_token": true`, the public
      token `aledgent-sidecar-test-<name>-token`, the same exception as the operator's token and
      confined to this crate and image. A principal that names neither fails the start.
    - **The sample is opened by default** (`issuer`, `custodian`, `onboarding`), each opting in to
      its test token. Trade-off, stated plainly: a bare start now opens three principals where it
      opened none; a job that wants none passes `{"principals": []}`. The library's
      `Options::default()` still opens none.
    - **Identifiers follow the name.** A principal is opened under the client key
      `sidecar-service-<name>`, as before under its role, so a deployment that configures the names
      it used before keeps the same identifiers and the same `/sidecar` shape
      (`service_principals.<name>.principal`, `.grants`), now with `role` beside them.

## Tests and cases

- **Conformance** (`crates/conformance/src/cases/f009.rs`): 7 run cases, all green on both
  in-process targets: `AC-M2.1`, `AC-M2.2`, `AC-M2.3`, `AC-M3.1`, `AC-K3.1`, `AC-K3.2`, and the exit
  criterion registered under `AC-K3.1`,
  `ac_k3_1_core_restart_under_load_loses_no_command_and_no_notification__service`: the core
  restarted ten times (plain stops, crashes before and after the journal write, a crash after the
  apply with the reply lost) and one ingress replica replaced, under three clients of transfers,
  reservations, captures and releases, with a recipient flapping. It asserts every acknowledged
  operation terminal, every owed notification delivered at least once and recorded, the input
  numbers exactly `1..=N`, and each backlog decided within the configured bound after the core
  returned (a stated bound of 30 s for the test deployment). 100 operations, about 370 inputs
  and 215 notifications per run. The load is kept well below the core's throughput in a debug
  build on a shared CI runner: at three times the rate, the first CI run saturated the core and
  the backlog grew round after round, which measures capacity rather than recovery.
- **Deterministic under contention** (issue #48). The cases were timing-flaky on CI; under local
  CPU contention (six copies of the suite at once beside twenty busy loops) 13 of 24 runs failed
  an `F-009` case; after the changes below, 0 of 24 under the same stress, and 0 of 64 under a
  heavier one (eight copies beside 24 busy loops, slowest backlog 269 ms against the 30 s
  bound). The causes, and what replaced each:
  - `testkit::Deployment::all_delivered` read the outbox *before* seeing every row decided, so an
    empty outbox read just before the core decided a row passed while that row's notifications
    were owed (`AC-K3.2` failed with no redelivery seen). It now sees every row decided first.
  - `AC-K3.1` restarted the core as soon as alice had been told of the release, racing the
    delivery worker's first attempt at bob's last notification ("no attempt kept"); `AC-M2.3`
    slept 300 ms for the same worker. Both now wait until the worker has reached the state the
    case asserts on. The assertions after the restart are unchanged.
  - The exit criterion compared the core's input count with the rows' input numbers at two
    instants while the ticker ran ("not 1..=451: 450 rows"). It now reads the count first and
    the rows after, until one reading holds every counted input by exactly one row; a real gap
    or a number held twice never clears and still fails.
  - Its load was a rate, and its rounds were paced by sleeps. On a slow runner a round could pass
    with little or no load in it, and a fault that had not fired within 2 s was dropped, turning
    that round into a plain stop. The clients also ran free for as long as the run lasted, so a
    slower machine piled up more load: ten times the rows at eight times the slowdown. The
    in-memory store's scans cost O(rows) per undelivered notification per delivery pass, so
    the forwarder starved and the last round's backlog missed the 30 s bound. The load is now a
    budget the rounds hand out: eight operations while the core is up and two while it is down,
    100 in all whatever the machine's speed. Each round waits until its operations are
    acknowledged, waits until its injected fault fires, and waits until at least one command
    has been received while the core is down, so every round has a backlog to recover.
  - Every wait on a condition the deployment reaches by itself has a generous deadline
    (`PATIENCE`, 120 s) and ends as soon as the condition holds. The decision bound is not one of
    these: it is the test deployment's stated 30 s, still checked after every restart.

  The runner now prints each failure's detail as it happens and again after the summary, and a
  panicking case reports its message and where it panicked.
- **`services/ingress/tests`**: `NFR-7` on every route; `REQ-AU-8` by a scan of everything logged
  (its own binary, so its subscriber sees the whole run); `AC-A1.3` and `REQ-OP-11` at the surface;
  `REQ-CS-2` and non-terminal labels; a core-side rejection after `Received`; `AC-C1.1` through
  ingress with the core down; reservation priority; `REQ-CS-4` against the OpenAPI document; the
  committed OpenAPI file current; `INV-18` at the boundary; query routing to `F-011`; `CTR-OPS`
  routing; `T-009-03` with every fault; `AC-C4.1` (a deadline passes only after a tick); leader
  failover within one interval; a replica restart; and, on PostgreSQL in a container, the store's
  rules, leadership, and two replicas losing nothing.
- **Latency (amendments 0007, 0008).** Four series, p50 and p99, kept and exported:
  - the **acknowledgement** of a command that moves value, from arrival until it is durably logged
    (`NFR-1`, 50 ms at p99): `aledgent_ingress_acknowledgement_p99_us`. Until amendment 0008 it
    sampled every command, decided ones included;
  - the **decided reply** of every other command, from arrival until the reply that carries its
    decision or `ERR-DecisionUnavailable` (`NFR-16`, 1 s at p99, proposed):
    `aledgent_ingress_decided_reply_p99_us`. No alert yet: `F-018` adds one once the number is
    confirmed. Cases: `ac_c1_1_a_reservation_is_acknowledged_at_received__service` and
    `ac_m5_1__service` (`f009.rs`). Open: decided commands wait behind every queued transfer in the
    forwarder (only reservations jump the queue), and `decision_wait`'s default is `NFR-15`'s 30 s;
    both are for the owner once `NFR-16`'s number is confirmed;
  - **completion**, from receipt until the decision that completes the command — a refusal, a
    terminal state, or `Reserved` for a reservation (`NFR-15`: 30 s ceiling, 19 s target):
    `aledgent_completion_p99_ms`, in the metrics and in the signals `F-018` pages on. A command with
    an external leg completes later, at `Observed` relayed to its requester (amendment 0018); the
    delivery worker matches that notification to the row and samples it there (decision 32,
    `T-009-25`);
  - the reservation path's reply: `aledgent_ingress_reservation_p99_us`, part of completion, no
    budget of its own. Printed by `ac_c1_1_reserve_returns_after_the_durable_decision`: in a debug
    build, in process, about 1 ms on an idle core.
- **`ERR-DecisionUnavailable`** (amendment 0006 item 8, `REQ-CS-7`). A command answered with its
  decision (`REQ-CS-8`; a reserving command, before amendment 0007) whose decision is not durable within `Config::decision_wait` (default 30 s, `NFR-15`'s ceiling; a plan
  choice for the owner to tune) is answered `ERR-DecisionUnavailable`, 503. The row stays logged
  and is decided when the core returns; a retry under the same client key gets that decision, with
  no second reservation. Case: `err_decision_unavailable__service` (`f009.rs`). `ERR-Internal` is
  500. The OpenAPI document publishes a `5XX` refusal response beside `4XX` on every route.

## Deferred

- `F-005`: the VOS Agent implementation of `boundary::Core`, blocked on issue #38.
- `F-016`: the adapter's authenticated surface (`record_observation`, `acknowledge`, the head).
- `F-018`: dashboards and pages on this unit's metrics (oldest undecided row, ticker age,
  undelivered notifications and their age, reservation p99).
- The privileged read on `CTR-OPS`; mutual TLS; `LISTEN/NOTIFY` for waiting
  callers on non-leading replicas; TLS to PostgreSQL (connections are plain; a deployment puts the
  database on a private network or a local socket).
- The chaos test fails over the core and an ingress replica; failing over PostgreSQL itself is not
  exercised in CI.
