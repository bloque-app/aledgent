<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-038 — Ingress as its own fleet, and the core stream

| | |
|---|---|
| Milestone | M-3 — Production V0 (`DEC-27`, proposed 2026-10-15; its operator guides gate `M-3D`) |
| Track | B |
| Home | `services/ingress` (`stream`, `remote`, `service`, `credentials`, `surface`, `store`, `config`, `main`, `readiness`), `agents/core-host` (`serve`, host side only), `services/ingress/migrations/0007_*`, `0008_*` |
| Depends on | `F-009` (the received log, the forwarder, leadership), `F-035` (the signer, the authenticated core link), `F-028` (credential schemes), `F-036` (the genesis gate), `T-009-26` (the migration runner and its checksums) |
| Unblocks | An operator's ingress fleet (Bloque's on AWS is the first); `F-039`'s operator guides for AWS and for one machine |
| Realises | `DEC-27` (a) to (i), (e′), (e″), (h′); `NFR-1`, `NFR-5`, `NFR-16`, `NFR-17`, `NFR-21`, `REQ-AR-7`, `REQ-CS-2`, `REQ-CS-7`, `REQ-CS-8`, `REQ-AU-3`, `REQ-AU-8`, `REQ-AU-20`, `NFR-7` |

## Goal

Ingress keeps taking commands while any execution node, the core on it, or one ingress instance stops.
On 2026-10-14 a reboot of the single execution node took ingress down with it, because ingress ran on that
node and pushed every command to the core host over a Unix socket: the load balancer refused 36 requests
with 502 and 504. After this feature, ingress runs as a fleet of its own, the core host dials out to it,
and a node reboot, a core restart and an ingress release each refuse nothing.

## Spec traces

- `NFR-1`, `REQ-CS-2`: a command that moves value is acknowledged once it is durable in the received log,
  whether or not any core is connected (`DEC-27` (b)).
- `NFR-5`, `NFR-17`, `NFR-21(b)`: no node, core or ingress change needs an outage or a maintenance period;
  any execution node is replaceable while every command is still acknowledged within `NFR-1`.
- `REQ-AR-7`: the component that receives commands is deployable and restartable independently of the one
  that decides them.
- `REQ-CS-7`, `REQ-CS-8`, `NFR-16`: a command answered with its decision gets it, or
  `ERR-DecisionUnavailable` when no core decides within `NFR-16`; it stays logged and is decided once a
  core takes it. Nothing is decided twice or out of the received log's order.
- `REQ-AU-3`, `REQ-AU-20`: the core still checks every credential and authority at decision; ingress's
  copy of verifiers only lets it acknowledge.
- `REQ-AU-8`: the copy holds public material only; no token, key or certificate secret reaches a log, a URL
  or an error.
- `NFR-7`: the stream listener authenticates both ways before it reads a frame; the readiness listener
  serves no ledger fact.

## Design

### 1. The stream protocol (`ingress::stream`)

Ingress is Apache-2.0 OR MIT and links no VOS, so both ends of the stream live in ingress's crate, as the
Unix-socket link's do (`ingress::remote`); the core host calls the dialing end.

- **Framing.** The existing frame: four bytes of length, big endian, then one JSON message, at most
  `MAX_FRAME` (8 MiB). One codec for both links.
- **Versioning.** The core host's first frame is `Hello { versions, core, partitions, principals, token }`.
  Ingress answers `Welcome { version, instance }` with the highest version both speak, or
  `Refused { reason }` (no common version, a draining instance, a failed authentication) and closes.
  Version 1 is this document's. A later version only adds; an older peer is answered in its own.
- **Messages after the handshake**, each with an `id` the answer repeats, so several are in flight:
  `Call { id, request }` (ingress to core: any request of `ingress::remote::Request`, the reads and the
  sequenced feed below), `Answer { id, response }` (core to ingress), `Ping`/`Pong` (both ways),
  `GoAway { reason }` and `Roster { roster }` (ingress to core, §6); a `Hello` may ask for the roster
  only (`roster_only`), answered by a `Welcome` that carries it, and closed. The request and response types are the socket link's, so the
  host answers both links with one function, and the host's calls into the Agent are untouched
  (`DEC-26(d)`, `DEC-27` (i)).
- **Keepalive.** Each side sends `Ping` every `keepalive_ms` (1 000 by default) and drops a peer it has
  heard nothing from for `dead_after_ms` (3 000 by default). A dropped stream fails its pending calls: a
  feed as ambiguous (it may have run), a read as unavailable (reads change nothing).

### 2. Transports

- **Mutual TLS over TCP** (`tls:<host:port>`), with rustls through `tokio-rustls`, both already in the
  workspace's lock: ingress presents a server certificate and requires a client certificate, both under the
  operator's own CA (`stream.tls.ca`); the core host checks ingress's certificate against the same CA and the
  configured `server_name`. A peer without a certificate from that CA gets nothing: the handshake fails
  before any frame. This listener is for cores only, never the client API's.
- **A Unix socket on the same host** (`unix:<path>`), for one machine (`DEC-27` (h′)): the peer's user id
  from the socket's credentials, and a token whose SHA-256 ingress lists, compared in constant time, as the
  socket link does today (`F-035`). The core host checks the other way too: it sends its token only to a
  socket whose peer runs as one of `stream.dial.peer_uids`, so a process that took the socket's path gets
  nothing. Over TLS the certificate is the authentication and no token is sent.
- Private keys and certificates are read from files named in the configuration; a TLS listener without its
  CA, certificate or key does not start.

### 3. The feed lease, with an epoch

The forwarder of `F-009` feeds the core. From this feature it feeds only while it holds the **feed lease**
of the partition it feeds, in ingress's database (`feed_lease`, migration 7): one row per partition with its
holder, an **epoch** that rises each time another holder takes it, its expiry on the database's clock, and
the next **feed sequence**.

- An instance tries the lease only while it can reach a core that writes the partition: in stream mode, a
  live stream whose core declared it; on the socket or in process, always. It renews every third of the
  lease and stops renewing when it loses the stream, so the lease expires.
- A row gets its feed sequence, gapless per partition, the first time the forwarder sends it, in the same
  transaction that marks it forwarded, under the lease's holder and epoch. A forwarder whose epoch is no
  longer the lease's is refused that transaction (fenced) and stops.
- The queue is read in-flight first: undecided rows that already have a feed sequence, in that order, then
  the rest as before (reservations first, then arrival). A row the core may have run is therefore always
  resent with the sequence it was first given, after every row with a lower one.
- Rows the core certainly did not run (answered unavailable, or never sent) give their sequences back, the
  partition's tail, under the lease (`Store::unfeed`), and return to the queue: a reservation logged
  meanwhile still goes before them (`T-009-04`, `T-031-18`), and the next row sent takes the first of those
  sequences again. The core host expects exactly that sequence, since nothing at or past it ran. A row
  answered ambiguous in any round of a send stays in flight even if a later round answers it
  unavailable: sequences are never given back below the highest position ever answered ambiguous.
- Today the book has one writer, the core Agent, so there is one partition, `book`. The sharded profile
  routes each row to its payer's partition; that routing is `T-038-22`'s, and the lease and the sequencer
  are per partition already.
- Leadership (the ticker and the delivery worker, `F-009`) is unchanged, except that an instance that
  cannot reach a core does not take it: delivery pages the outbox through the core.

*Trade-off, stated plainly:* a row the core may have run (an ambiguous answer) keeps its place ahead of
any reservation logged after it, where before the forwarder retried it first anyway; and every lease
renewal is a write to ingress's database, every third of the lease.

### 4. The core host keeps the order (the sequencer)

A sequenced feed is `Feed { partition, epoch, items: [Fed { seq, input, signature }] }`, where `input` is
the core input's JSON exactly as signed. The host's sequencer, in front of the Agent's core:

1. checks every item's signature (§5); one that is missing or wrong refuses the whole feed, and nothing in
   it reaches the Agent;
2. fences: an epoch below the highest it has seen for the partition is refused (`Fenced`), nothing runs;
   a higher one is adopted, and its first sequence becomes the expected one (a new feeder resends from its
   lowest undecided row);
3. orders: within an epoch, an item below the expected sequence is a duplicate, answered from the core's
   reply log without running again (`INV-18`); an early arrival is held for up to `hold_ms` (500) for the
   gap to fill, then refused (`Gap { expected }`), nothing run;
4. hands the rest to the core as one batch (`apply_batch`, `T-031-04`), and answers each item.

One mutex per partition serialises feeds from every stream and the socket. A host with `ingress_keys`
refuses every unsequenced `apply` or `apply_batch` (an ingress of an earlier release, or anything unsigned)
as unavailable from the moment it starts, with no window before a first feed and none after a restart.
Production requires the keys once the host dials the new link (§5). Without them (development, or a
production host on the legacy socket link alone, §12 step 0), the host takes unsequenced inputs as a host
of the earlier release does, and refuses them once it has seen a sequenced feed: nothing runs out of order
during a mixed rollout.

### 5. Ingress signs what it forwards

Each item carries ingress's Ed25519 signature over `SHA-256("aledgent/ingress-feed/v1" ‖ partition ‖
epoch ‖ seq ‖ input)`, made through the signer service (`F-035`, key `stream.feed_key`); a key from a file
is accepted only in the development profile. The epoch is signed (feeds are signed afresh on every send),
so a captured item replayed under a higher epoch fails the check instead of fencing the real feeder. The
core host checks it against `stream.ingress_keys` (two during a rotation) before the Agent sees the input.
mTLS says which peer is connected; the signature says the command is the one ingress recorded at that
position. The host checks it on every link it serves. In production, once the new link is configured,
the keys are required on both sides: a core host with `stream.dial` refuses to start without
`ingress_keys`, and an ingress with a `stream` section (on the stream or on `core_endpoint`) refuses to
start without `stream.feed_key`, so both links carry signed feeds during the transition (§12). A
deployment with no `stream` section runs the legacy socket link (§12 step 0): ingress sends no feed and
signs nothing, and the core host, with no keys, takes its unsequenced inputs as before this feature. A host
with `ingress_keys` refuses unsigned or unsequenced input from its start whatever else it is configured
with.

### 6. Reads ride the stream

Every read ingress makes of the book (a credential's verifier, a principal's tie, the queries, the outbox,
the last tick, the next deadline, the genesis gate) is a `Call` up the stream the core opened, answered down
it. No core is dialed for a read. An instance with no stream answers credentials from its copy (§7), ties
from its store (as today), and the rest as unavailable.

**Every instance reads: the roster (`T-038-21`, the owner's option (b), 2026-10-15).** Each region's
cores are deployed beside that region's own set of ingress instances, which hold its partitions, so a
core only ever dials its own region's instances. Then:

- **Advertised addresses.** Each instance advertises an address its region's cores reach it at
  (`stream.advertise`; by default detected from `stream.listen`: the listener's own host, or, on every
  interface, the address this host routes from, which on ECS or Kubernetes is the task's or pod's), in
  ingress's database (`ingress_roster`, migration 8): one row per live instance, renewed every
  `roster_ms` (2 s) for three times that on the database's clock, deleted on drain.
- **Roster, then direct dials.** Every `Welcome` carries the live rows, and ingress sends a `Roster`
  message down every stream when they change. A core host dials the fleet's stable name as before, takes
  the roster from that greeting, then dials **every** instance in it directly, over the same mutual TLS
  (the fleet's certificate names the stable name, whatever address it is dialed at) or the same socket
  credentials, keeping one stream per instance and dropping the stable-name one. It reads the roster again
  from the stable name (`Hello { roster_only }`, answered and closed) on every go-away and while no stream
  is up, and drops the streams of instances that left. An empty roster (nothing advertised) keeps the
  stream through the configured addresses, as before.
- **Commands stay single-fed.** Only the feed lease's holder feeds a partition, as before. The core host
  binds the lease's epoch (signed, §5) to the stream that brought it, and refuses the same epoch on any
  other stream `Fenced`: the other streams only read. When the binding stream closes, its feeder may come
  back on a new one; a higher epoch on another stream is a new holder, as before.
- **Every instance answers reads** (credentials, ties, queries) from the core over its own stream. An
  instance with no stream to a core yet answers `ERR-DecisionUnavailable` (503, `Retry-After`), never a
  request held open (§8). Reclaims are unchanged (§7).
- **Failover.** When the feeding instance goes, its lease lapses and an instance that already has a
  stream takes it: no reconnect on the way. The go-away drain stays (§9), and the draining instance leaves
  the roster before it sends its go-away.

*Why (b) and not a relay between instances (option (a)):* a relay adds ingress-to-ingress trust and a hop
on every read, and keeps every reader depending on whichever instance holds the core's one stream; with
regional fleets beside their cores, the roster adds neither. The operator's only requirement is that a
region's cores can reach that region's ingress instances, which the core stream needs already. On one
machine the roster is the local instances (`DEC-27` (h′)).

*Trade-off, stated plainly:* each core holds a stream to every instance of its region, so a fleet of N
instances means N streams per core and N keepalives; and the roster is one more write per instance every
`roster_ms`.

### 7. The durable verifier copy, and door checks

`verifier_copy` (migration 7) keeps, per credential, the identity, kind, scheme and verifier the book last
answered: an API key's hash, a passkey's encoded key (public key, credential handle, `rpId`, backup flag,
sign count), a `key` credential's public key. Public material only (`REQ-AU-8`).

- Every answer the book gives refreshes the entry; an answer of "no working credential" deletes it, on
  whichever instance reads it, whoever copied the entry (another instance, or this one before a restart).
- The copy is read only when no core answers. A proof is checked against a verifier the book admitted, never
  accepted for its form alone: an unknown API key, or an assertion from a passkey the book never admitted,
  fails as it does today.
- A credential removed while no core answered is acknowledged and then refused at decision: the core checks
  the set and the authority (`REQ-AU-3`, `REQ-AU-20`). No credential joins a set while no core answers.
- **`key` proofs at the door.** A reclaim's `key` signature is checked by ingress against the credential's
  verifier (live or copied) over the reclaim challenge before the command is logged, and a forged or
  mismatched one is refused `ERR-CredentialInvalid` instead of acknowledged; the core still verifies it.
  The challenge binds the account's custody epoch, which is book state: when no core answers that read, the
  reclaim (a command answered with its decision, `REQ-CS-8`) is answered `ERR-DecisionUnavailable` at once.

### 8. `ERR-DecisionUnavailable` instead of a gateway error

A command answered with its decision waits for it on its row. If no instance holds a live feed lease for
its partition (no core connected, or a handover in progress), the wait is bounded by `NFR-16` (1 s) rather
than `decision_wait`, and the reply is `ERR-DecisionUnavailable` (`REQ-CS-7`). The command stays logged and
is decided once a core takes it. Ingress no longer depends on any core to answer, so a load balancer in front
of it never sees ingress down because a core is.

*The owner's ruling (2026-10-15, `T-038-23`):* `ERR-DecisionUnavailable` stays HTTP 503. What `DEC-27` (g)
forbids is a 5xx with nothing behind it (no instance answering, a bare 502 or 504 from a load balancer);
a 503 from ingress carrying the named error is allowed. It carries `Retry-After: 1`
(`surface::DECISION_RETRY_AFTER_SECS`): one second is `NFR-16`'s bound and the order of a planned
hand-over (§9, well under a second), so a client retrying with the same client key usually finds a core
deciding again; after a crash (a few seconds: `dead_after` and the lease) a retry is answered the same
named condition, never another outcome. The OpenAPI document declares the header on the 5XX response.
A query is bounded the same way: an instance whose core does not answer within `NFR-16` (no stream yet,
a core restarting) answers `ERR-DecisionUnavailable` instead of holding the request open.

### 9. Go-away and the drain

On `SIGTERM` an ingress instance: (1) reports not ready, so its load balancer stops sending it requests;
(2) stops starting feeds and lets the one in flight finish; (3) releases its feed leases (expires them);
(4) sends `GoAway` on every stream and refuses new handshakes as draining; (5) keeps answering clients for
`drain_ms`, then exits. A core that receives `GoAway` dials a new stream at once, keeps answering the old one
until it closes, and the instance it lands on takes the lease at its next renewal. Decisions pause for well
under a second; acknowledgements never pause. A crashed instance is noticed by keepalive within
`dead_after_ms`, and its lease expires within the lease.

### 10. Readiness

`readiness_listen` (optional, its own port) serves `GET /ready` (200 once the store answers and the
migrations are applied or verified, 503 while draining or when the store does not answer) and `GET /live`.
It carries no ledger fact and no credential, so it sits outside the client surface's deny-by-default
(`NFR-7`) on a port the load balancer's health check alone reaches. `/metrics` stays authenticated.

### 11. Configuration

One file for both processes (`DeploymentConfig`), as today:

- `stream.listen` (ingress): `tls:<addr>` with `stream.tls` (`ca`, `cert`, `key`), or `unix:<path>` with
  `stream.link` (`token_sha256`, `uids`); `stream.feed_key` (the signer's endpoint and key id) or, in the
  development profile only, `stream.development_feed_key` (a file); `stream.advertise` (the roster
  address, detected by default); `keepalive_ms`, `dead_after_ms`, `drain_ms`, `roster_ms`.
- `stream.dial` (core host): `connect` (addresses), `server_name`, `tls`, `partitions` (default
  `["book"]`), for a Unix socket the token from `ALEDGENT_CORE_STREAM_TOKEN` and `peer_uids` (ingress's user
  ids, required); `stream.ingress_keys` (hex, required in production).
- `readiness_listen` (ingress).

An ingress with `stream.listen` serves the stream instead of dialing `core_endpoint`; one with
`core_endpoint` works as today. The core host serves `core_endpoint` and dials `stream.dial` at once.

Which link ingress forwards on follows from the `stream` section alone: without one, the legacy socket
link (rows sent as `apply_batch`, exactly as before this feature: no feed, no feed lease, no roster, no
signature); with one, the sequenced feed under the feed lease, signed with `stream.feed_key`. Ingress names
the link in a log line at start.

### 12. The transition, both transports at once

0. **Keep the legacy socket link until the core host is rebuilt.** A node never updates its core host in
   place: the host's image carries the genesis package's bytes, so a release of ingress meets the core host
   it already had, which speaks only the socket link of `F-035` and answers a feed `unavailable`. Merging
   this feature therefore changes nothing for a running deployment: with no `stream` section its ingress
   starts on the legacy link (no feed key needed in production), sends `apply_batch` as before, takes no
   feed lease and joins no roster, and its decisions and ticks carry on against the old host
   (`f_038_s12_a_new_ingress_runs_against_a_core_host_of_the_earlier_release`). A deployment leaves step 0
   only once its core host is rebuilt with this feature's code, with the genesis package and runtime bytes
   copied from its pinned image (the Agent does not change, `DEC-27` (i)). That rebuilt host, still with no
   `ingress_keys` and no `stream.dial`, runs against the legacy ingress as the old one did. Then the keys
   go on both sides (`ingress_keys` on the host, `stream.feed_key` on ingress, in one `stream` section),
   which switches ingress to the sequenced feed on the same socket; then the new link (step 1 onwards).
1. Release this feature everywhere: the node's ingress (socket), the core host (socket and stream), with
   `ingress_keys` and the feed key configured on both. The node's ingress now feeds under the lease, signed.
2. Start the fleet's instances in stream mode beside it. Whichever holds the lease feeds; the other cannot
   (the database fences it, the host fences its epoch); duplicates are answered from the reply log.
3. Move clients to the fleet, stop the node's ingress, then remove `core_endpoint` from the host.

## Exit criterion (`PLAN.md` §4, `F-038`)

A node reboot, a core restart and an ingress blue/green release, each under the stress suite's load, refuse
no request and acknowledge every command within `NFR-1`; no command is decided twice or out of order across
a handover; a command answered with its decision gets its decision or `ERR-DecisionUnavailable`, never a
gateway error; a forwarded command without a valid ingress signature never reaches the Agent. The protocol,
the lease, the sequencer and the door checks are proven in this repository's tests; the run under load on
real infrastructure is `T-038-20`'s, and the operator guides are `F-039`'s (`T-038-18`, `T-038-19`).

## Not in this feature

- Routing rows to several partitions and regional cores (`T-038-22`): the sharded profile is not yet what
  production runs.
- Infrastructure (load balancers, certificates, the ECS service): the operator's repository.
