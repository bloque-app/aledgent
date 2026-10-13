<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-005 — implementation notes

Written while implementing `F-005` (issue #5), 2026-09-24, after the owner ruled #38: the core stays
a VOS `saga/agents` Agent (`DEC-4` as ruled, `DEC-16`, `F-025`). Where the plan was silent the most
conservative behaviour was chosen, and each such choice is listed here for review.

**Exit criterion: not met.** The conformance suite runs through the Agent's transition for every
case that reaches the library (`through-agent/native`), but the cases that build their own
deployment, and the kernel and store cases, do not go through the Agent (below); and a
reservation took **2.59 s at p99** (`notes/MEASUREMENTS-F005.md`), which was read as missing
`NFR-1`'s 50 ms by about fifty times. `RISK-2` was raised with the numbers. *Amendment 0007 (2026-09-29):* that budget was a misreading. `NFR-1` bounds a
command's acknowledgement; the decision is part of completion (`NFR-15`: 30 s ceiling, 19 s target,
at p99), which 2.59 s meets. The exit criterion's remaining part is the suite through the Agent.

**The heap bound is lifted** (T-005-11 to T-005-14, part of #5, below): the service store is
witnessed leaves, as the kernel's accounts are, so the Agent keeps roots plus bounded records and
its working state no longer grows with the book. It held about thirty operations for its whole
life; 1 000 reservations, captures and transfers through VOS leave its state the same size
(`notes/MEASUREMENTS-F005.md` §8).

## What landed

| Where | What | Licence |
|---|---|---|
| `crates/ledger-core` (`call`) | The library's commands as data: every mutating method records itself as a `Call` (its arguments) and a `Summary` (accepted, or the refusal's code), so the host can send an input's calls to the Agent and the Agent can re-run them | Apache-2.0 OR MIT |
| `crates/core-binding` | The Agent's half with no VOS in it: state layout and budget (`hot`), envelope, witness store, private vault, call dispatch, the transition; and (feature `std`) the host's driver, recovery and replay, and the native port; (feature `ingress`) `AgentCore`, ingress's `boundary::Core` over the Agent | Apache-2.0 OR MIT |
| `services/ingress` (`remote`) | `boundary::Core`'s four operations over a local socket: the server and `RemoteCore`, the client. ingress uses it when `core_endpoint` is configured | Apache-2.0 OR MIT |
| `agents/core` | The VOS actor: a shell around `core_binding::transition` | AGPL-3.0-or-later |
| `agents/core-host` | `aledgent-core-host`: the Agent's Local host through the runtime ABI; `serve` (the deployment), `agent-server` (the raw port, for conformance), `measure`, `smoke`, `package`, `runtime-pvm` | AGPL-3.0-or-later |
| `crates/conformance` (`agent`) | The suite's Agent targets: `through-agent/native` (default) and `through-agent/vos` | Apache-2.0 OR MIT |
| `crates/ledger-core` (`leaves`, `state::tree`) | The service store's collections as leaves of the service tree: recorded in a whole store, fail-closed in a partial one (T-005-11) | Apache-2.0 OR MIT |
| `crates/leaf-store` | Kind-led service keys, removals, the service tree applied alone, a kind's scan (T-005-12) | Apache-2.0 OR MIT |
| `crates/core-binding` (`service`) | The service witness and its root inside the Agent; the host's service tree (`host::ServiceStore`) (T-005-13) | Apache-2.0 OR MIT |
| `tools/ci` | `check-no-vos.sh` (licensing boundary), `check-guest-deps.sh` (`T-005-06`), with fixtures; `tools/cipherclerk-patched.sh` | Apache-2.0 OR MIT |

### Tasks

| Task | State |
|---|---|
| T-005-01 | Done, with changes: the pin is the fork's `de0f12f8`, not `7bae926` (below). The Agent installs and answers queries (`hot`, `entry`) through the runtime ABI, not on a `vosx` space (`T-025-05`). It installs only on a runtime with the program limit raised |
| T-005-02 | Done. Layout in `core_binding::hot`; the budget refuses before VOS does. The default budget was set by the heap (decision 9); with layout 2 it bounds the unarchived ring (decision 30) |
| T-005-03 | Done. One `#[msg(linear)] apply` handler takes the envelope; command groups are features of the dispatch, not handlers (decision 6). The driver principal is checked on `ctx.agent_origin()` |
| T-005-04 | Done: tampered leaf, stale root and a read outside the witness each fail with no commit (`tests/agent.rs`) |
| T-005-05 | Done: openings, the key-ring root and the clerk key arrive as caller blobs and are never written (`REQ-RT-6`) |
| T-005-06 | Done: time comes in the calls; `check-guest-deps.sh` refuses a clock, randomness or network crate or use in the guest, with a CI self-test |
| T-005-07 | Done: delta ring and outbox backpressure, archive acknowledgement piggybacked on the next input |
| T-005-08 | Done: driver, recovery by the Agent's input number, the host's log, `AgentCore`, `serve` with persisted runtime state |
| T-005-09 | Partly: the service cases through the Agent natively (below); a VOS subset run by hand; replay from genesis and crash points tested in `core-binding` |
| T-005-10 | Done, and the budget is missed: `notes/MEASUREMENTS-F005.md` |
| T-005-11 | Done: `ledger_core::leaves`, every collection of the service store; decisions 20 to 24 |
| T-005-12 | Done: `F-006` decisions 3, 5 and 7 amended, 22 and 23 added |
| T-005-13 | Done: `core_binding::service`, layout 2, the host's service tree; decisions 25 to 31 |
| T-005-14 | Done: 1 000 operations through VOS, `notes/MEASUREMENTS-F005.md` §8 |

## Decisions taken during implementation

### Licensing (for the owner)

1. **VOS is AGPL-3.0-or-later; aledgent is Apache-2.0 OR MIT. Everything that links VOS is under
   `agents/`, in its own workspace, declared AGPL-3.0-or-later**, and excluded from the root
   workspace. `services/ingress` does not link VOS: it reaches the core over a local socket
   (`ingress::remote`). `tools/ci/check-no-vos.sh` fails the build if a manifest outside
   `agents/` and `spikes/` names a VOS crate or the root lock resolves one, and checks the
   licence of every manifest under `agents/`. **The combined Agent binary (`aledgent-core-host`,
   and the guest package) is AGPL.** Serving third parties over a network with it triggers
   AGPL §13 (offer the corresponding source of the modified program to its network users). The
   code that binary links from the root workspace (`ledger-core`, `core-binding`, `ingress`,
   `leaf-store`) stays Apache-2.0 OR MIT on its own and is conveyed under the AGPL inside the
   combination. **Decision for the owner:** accept that the core's host is an AGPL program
   (and publish its source to its users), obtain a different licence for VOS, or keep the
   decision native.

### Pins and the path

2. **VOS: aledgent's VOS fork, `agent/compact-acknowledged-results`, `de0f12f8`** (`T-025-01`), in
   `agents/Cargo.toml` and `agents/core/Cargo.toml`. Every Linear and Query invocation carries a
   sequenced identity (`SequencedInvocation`), so the Agent does not refuse its 65th.
   `TODO(F-025)`: move to upstream `saga/agents` once merged. Codeberg is reached only by git.
3. **CipherClerk: `e9a7b08` plus `proposals/cipherclerk/0001`**, made reproducibly by
   `tools/cipherclerk-patched.sh` into `.cipherclerk-patched/` (gitignored), and used through a
   `[patch]` in both `agents/` workspaces. The root workspace is untouched. `TODO(F-025)`: the
   upstream commit once 0001 is merged (`T-025-07`).
4. **The ABI path, not `vosx`.** `vosx space invoke-local` is unusable (`MEASUREMENTS-F002.md`
   §7, §11.2; `T-025-05`), so `core-host` drives the standard AgentRuntime as the Local host's
   journal driver does, as `spikes/vos-host` did: it keeps the Agent's runtime state, signs the
   Authority receipts with the Agent's authority key, and persists the state after every
   invocation (`serve`: write, sync, rename). A restart restores the state and re-attaches the
   installed actor from the Agent's directory.
5. **Toolchain `nightly-2026-03-20`** for everything under `agents/`: the pinned VOS's canonical
   guest toolchain, which also builds VOS's host crates; `F-006`'s `redb` 4 needs rustc 1.90,
   newer than VOS's own workspace pin. VOS's host crates do not build on macOS; CI builds on Linux.

### The Agent

6. **One handler, command groups as features.** `DEC-4` sketched one `#[msg(linear)]` handler per
   command group. The Agent instead has one, `apply(envelope)`, and the groups (`custody`,
   `settlement`, `anchoring`, `operator`, `opening`) are Cargo features of the dispatch, so a
   build can leave groups out. One handler keeps the envelope, the principal check, the witness
   and the all-or-nothing write buffer in one place.
7. **The host runs the library first; the Agent re-runs its calls.** An input runs on the host's
   replica (ingress's own executor), which records the calls it made (`ledger_core::call`). The
   driver sends the calls, the witness of the leaves they touch and the private inputs; the
   Agent runs the same calls over the witness and must reach the same summaries and roots
   (kernel root, service root, audit head). A difference is an incident: the core stops.
   Read-set discovery is therefore exact: the witness is what the replica's run read.
8. **The driver's principal is fixed at genesis** (trust on first use): the origin of the
   genesis invocation becomes the only principal the Agent takes invocations from
   (`REQ-PN-1`). A deployment creates the Agent and sends genesis in one step.
9. **The service store travelled whole** (superseded by decisions 20 to 33). `F-006` decision 5
   (the library's records as service leaves) was not done, so the Agent kept `ledger-core`'s
   service store as its image, in 60 KiB rows, and decoded it on every input, against `DEC-4`'s
   "roots plus bounded records". The budget `H` was set by the actor's 256 KiB heap: the peak
   heap ran at about six times the state, so the default budget of 26 KiB held about thirty
   operations for the Agent's whole life. The service tree has replaced the image.
10. **A 256 KiB stack.** `vosx`'s linker script gives the actor a 64 KiB stack (the lowest
    read-only address), which the library's deepest command overflows (`Panicked`).
    `agents/core/pvm.ld` starts read-only data at `0x40000`, as the AgentRuntime's own layout
    does; it costs about 192 KiB of padding in the program.
11. **Program size.** `opt-level = "z"`, LTO, `panic = "abort"`, `-Zfmt-debug=none`,
    `-Zlocation-detail=none`, `-Cpanic=immediate-abort`. Still 1 968 385 B against the runtime's
    1 280 KiB, so the core installs only on a runtime with the limit raised
    (`proposals/vos/0003`). Everything measured through VOS here used that change.
12. **Privileged reads cross as their audit record** (`Call::PrivilegedRead`): the host's read
    returns the data, the Agent commits the same audit record. Posting drafts cross as their
    parts and are rebuilt through `PostingDraft::balanced`.
13. **Exactly once by input number** (`INV-18`): the Agent answers `Duplicate` to an input
    number it already committed; the host decides an ambiguous failure by rebuilding from the
    Agent and comparing input numbers. The library's own idempotency (operation references,
    client keys) is relied on above that, as in the in-process core.

### The host

14. **`AgentCore` reuses ingress's in-process executor** (`inprocess::Running`, now public) over
    the host's stores, so the Agent core and the in-process core answer every input the same
    way. The archive (delta-ring entries) is written before the Agent is told it is archived;
    replies are logged before they are returned.
15. **The key-ring root and the clerk key are in the core host's process**, with the replica, as
    they are in the in-process core today. Guardrail 13 (`§4.1`, `NFR-8`) wants key material
    out of the process that holds the book. They reach the Agent only as caller blobs, never
    its state. **For the owner:** a key-custody process that
    builds the private inputs, or accept this until then.
    *Superseded by `F-035` (2026-10-04):* the keys are `aledgent-signer`'s; the core host holds none,
    and the Agent's private input carries the signer's answers, never a key.
16. **No authentication on the local socket.** `ingress::remote` trusts whoever can connect:
    a Unix socket's file permissions, or TCP on loopback. Deployments must not expose it.
    *Superseded by `F-035`:* a token (SHA-256 in `core_link`) and, on a Unix socket, the peer's
    user id; a TCP link must be loopback.
17. **Secrets from the environment only**, none with a default: `ALEDGENT_CORE_ROOT`,
    `ALEDGENT_CLERK_KEY`, `ALEDGENT_CORE_AGENT_KEY`, `ALEDGENT_CORE_PACKAGE_KEY`. `serve` and
    ingress read the same deployment file; ingress uses the socket when `core_endpoint` is set.

### Conformance

18. **Partial targets.** The Agent targets offer the service and build ports; the kernel,
    binding and raw store cases have no Agent in them and are reported not runnable there, and
    a case that builds its own ingress deployment (`F-009` and later) never reaches the harness,
    so a pass there is reported not runnable, never as a pass (`Harness::exercised`). A profile
    still requires those cases of the complete in-process targets. Running the deployment cases
    over `AgentCore` would need the suite's deployment to take a core; `tests/agent_core.rs`
    runs a real deployment over `AgentCore` instead.
19. **`through-agent/vos` is a manual run** (about 2.5 s per change): `aledgent-core-host
    agent-server` with a guest built with `conformance-ports` (the raw lifecycle ports change
    the call encoding, so the production guest refuses them as malformed).

### The service store as witnessed leaves (T-005-11 to T-005-14, part of #5)

Added 2026-09-24, after the owner ruled to lift the heap bound next, in parallel with `F-025`'s
upstream work. The conformance suite (in-process and through the Agent) and the in-process core
judge behaviour: nothing a command decides changed.

20. **Every collection of the service store is leaves** (`ledger_core::leaves`). A map is a
    `Leaves<K, V, KIND>`, a single value a `Slot<T, KIND>` (absent while it holds its default),
    each entry one leaf of the service tree of its collection's kind (`leaves::kind`); a leaf is
    `F-006`'s `ServiceLeaf` with the canonical key and value in its body
    (`delta::library_leaf`). An export's sections and their encoding are unchanged: a leaf
    collection encodes as the `BTreeMap` it wraps, so `F-014`'s export and import round-trip
    byte for byte as before.
21. **Derived indexes the commands read are leaves; those only queries read are not.** The
    reserved total and open count of each account (which replace the nested open-reservation
    map), accounts by owner, credential sets, grants by principal, transition keys, the deadline
    index, running balances and the last posting are leaves. The journal's time index, its
    per-account and per-position posting indexes and checkpoints, and the operation store's
    open-since index are `Derived`: the host keeps them for queries, the Agent has none, and
    reading one there fails closed.
22. **New derived indexes keep the frequent commands to point reads**: grants by subject account
    (a custody change's holders, which scanned every grant), the account a reference names (an
    observation's crediting, which scanned every account), the live instructions (a finalised
    head, which read every instruction ever given; an operation that ends retires its
    instruction in the same write), the instructions signed and awaiting a nonce fact (the same
    head, since `F-016`'s nonce facts, #89), and the anchor store's last checkpoint number. A duplicate
    corroboration is now a point lookup. Each index is derived from the records, changed in the
    same write, rebuilt on import, never exported, and answers what the scan answered, in the
    same order.
23. **Read-set discovery by recording.** A whole store records the keys each outermost recorded
    call reads or writes, and each collection it iterates: `Ledger`'s `enter` and `leave` turn it
    on and off (`ledger_core::call`), so queries, the host's own reads and a privileged read's
    data are never witnessed. The driver takes what was recorded after each input.
24. **Fail closed.** A partial store answers only the keys its witness gave, present or absent. A
    read or a write of any other key, an iteration over a kind not given whole, or a read of a
    derived index sets its flag, and the Agent commits nothing (`UnprovenRead`), as the kernel's
    witness store does.
25. **A kind is one subtree.** A service key is its kind's code, then 15 bytes of hash (`F-006`
    decision 3, amended). A command that reads a kind whole is given it whole, and the Agent
    checks it: no non-empty frontier node inside the kind's subtree, and a probe key keeps the
    subtree on a proven path when the kind is empty. New keys may be written in a whole kind.
26. **The Agent folds the service root itself** (`core_binding::service`): the frontier is
    `F-006`'s multiproof, but the root is computed in the binding so that the whole-kind check
    sees where each frontier node stands, and every frontier node must be used. The root after
    is the same proof over every leaf given or created, as it stands. `Roots::service` is now
    this root, not the digest of an export's sections (an export's manifest keeps that digest).
27. **Removals.** An entry of a derived index that stops holding is removed (a deadline met, a
    pending custody change closed, a binding cleared); the leaf store empties the leaf, so the
    root is the one the tree would have without it (`F-006` decision 7, amended). The library's
    records are never removed (`REQ-LA-4`). Delta records carry removals (`RECORD_VERSION` 2).
28. **The host keeps the Agent's service tree in a leaf store of its own** (`host::ServiceStore`,
    the service tree alone), applied once per committed input with `BookStore::apply_service`,
    without delta records: an input may make several kernel commits or none, and the kernel's
    records are numbered by kernel commit. The Agent's delta ring and the host's archive are the
    committed transitions, and `host::replay` rebuilds the replica and the tree from them,
    checking every entry's service root (`INV-6`). Trade-off, stated plainly: the service tree
    has no archive of its own; it is exactly as durable as the host's archive of inputs, as the
    replica already was.
29. **Deliveries are leaves** (`leaves::kind::DELIVERED`, the binding's own), not Agent rows. The
    outbox's backpressure counts with two counters of the hot row (`notified`, `delivered`)
    instead of the outbox's size, which the Agent no longer holds. The rule is the one before.
30. **Layout 2.** The hot row loses the image's length and chunks and gains `notified`; the image
    and the delivered rows are gone. The Agent's state is the hot row, the configuration and the
    unarchived ring: about 600 B with the ring archived, whatever the book holds. The default
    budget stays 26 KiB, now a bound on the unarchived ring.
31. **Recording without `unsafe`.** Reads record through `&self`. Where the target has atomics
    (the host) the flags are atomic and the recording sits behind a spin lock (`spin` 0.9,
    already in the root lock), so a whole store stays `Sync` for concurrent query readers, who
    never record and never take the lock. The guest has no atomics and one thread, and uses
    cells.
32. **What is still read whole, and what it grows with.** A tick reads the deadline index (open
    operations whose deadline has not yet been marked passed); a finalised head reads the live
    instructions and those awaiting a nonce (withdrawals in flight); the proof that an expired withdrawal was not included
    counts final positions over its window by reading every final position (it grows with the
    finalised observations: `TODO(F-005)`, an ordered kind for final positions); a rescan reads
    every ingested observation; settlement gaps and incidents, and anchor incidents and
    comparisons, are single leaves holding lists (they grow with those events); completing an
    opening load reads every load. None is on the reservation, capture or transfer path:
    `tests/heap.rs` asserts that no kind is read whole there.
33. **Cost.** The guest program grows by 179 KB (2 147 300 B with every group, measured; 2 168 660
    B once rebased onto #89's settlement additions; still under the raised limit,
    `proposals/vos/0003`), which costs about 116 M more invoke gas for the runtime to reload on
    every invocation. A reservation's actor gas rises from about 173 M to about
    297 M: most of it is the service proof's hashing (a fold over about twenty leaves, twice,
    through 128 levels). That cost does not grow with the book but for the proofs' depth, which
    grows with the logarithm of their leaves (1.3 % over 1 000 operations); the image it replaced
    grew with the book itself (`notes/MEASUREMENTS-F005.md` §8).

### The off-node archive and the restore (T-005-15 to T-005-18, part of #5)

The MVP release blocker (#166): the core Agent's backup and restore were
`TODO(F-005)`. The owner ruled the checkpoint archive an S3 sink first (amendment 0018, `DEC-22` (b),
`DEP-11`). At one voter this is the host's own archive; VOS's quorum-certified checkpoints join it with
`F-021`. The trade-offs are in the experiment's decision record (kept with the maintainers);
the ones that stay are recorded here.

34. **The archive is the host's two logs, byte for byte, cut at decision boundaries.** `archive.bin`
    already holds every committed input, hash-chained, with its roots; `replies.jsonl` every reply.
    `core_archive::ArchivingLog` wraps the host's log and marks a cut after each decision's replies
    are durable (two `stat` calls and a lock), and a thread ships the bytes since the last cut as a
    segment. A segment names its predecessor's digest and its offsets in both files, so a contiguous
    run is the logs exactly. A backlog over 16 MiB goes up in parts, entries first, and only the last
    part is a restore point: no restore ever holds a reply whose decision it lacks.
35. **The leaf store is not shipped.** It is a projection of `archive.bin` (`DEC-4`), rebuilt and
    checked root by root on every start of the core host already. A restore re-derives it the same
    way. Shipping it would add a copy that nothing verifies, and buy only restore time.
36. **Checkpoints are `agent.state`, read at a cut.** At a cut, `agent.state` is exactly the Agent
    after that decision: the port saves it after every invocation, and nothing invokes between the
    flush and the replies. A checkpoint every 50,000 inputs or 15 minutes, so that a restore re-sends
    at most that many inputs through VOS.
37. **A restore re-sends, never trusts.** `roll_forward_agent` starts the Agent from the checkpoint,
    or founds it afresh. The checkpoint must stand on the archive's chain: its log head is the
    archived entry's at its input number. It then stages and sends every archived input after it
    exactly as a live input is sent. The input staged must be the archived entry (its log hash), and
    the Agent's reply is checked as a live reply is. The Agent must end at the archive's input
    number, log head and roots. Before that, `opsctl` re-derives the whole book natively from genesis
    and runs the drill's checks, so a restore that would not reach the archive's roots, or that the
    settlement layer's anchors contradict, is refused before anything is laid out.
38. **Never on the critical path; the recovery point is bounded, not zero.** Nothing the core answers
    waits for the sink (gap/5's rule). Losing the node loses nothing acknowledged while ingress's
    received-log survives: `opsctl restore core-tail` re-decides the window after the archive's point,
    each reply checked against the recorded one. Losing both loses the archive's lag at the failure,
    about 1 s while the sink is healthy, paged at 5 min. A synchronous mode, each decision waiting for
    the PUT, would close that at the cost of an S3 round trip per decision. **Ruled by the owner on
    2026-10-05 (`ab-q02`): the bound is accepted and the archive stays asynchronous** (1 s interval,
    paged at 5 min).
39. **One history** (`INV-25`). On start the archiver reads the last segment back and checks the local
    logs hold exactly its bytes. A node with another history (two hosts of one core, or a volume from
    an older snapshot) never writes, and pages (`AledgentCoreArchiveConflict`). The sink is write-once
    (`If-None-Match: *` on S3, a hard link on a directory) and refuses it anyway.
40. **S3 by hand, not the SDK.** SigV4 over `ureq` (already in the workspace for `verify`'s RPC), with
    AWS's documented vector as its test, rather than the AWS SDK's dependency tree in the AGPL
    workspace. Credentials from the environment or the instance role (IMDSv2). A writer refuses to
    start without `kms-key-id` and Object Lock; every PUT carries the lock, the KMS key, its SHA-256
    checksum and MD5. CI exercises it against MinIO with Object Lock and a KMS key.
41. **Two fixes the archive needed.** `FileLog::open` now cuts off a torn last record: before, the
    next append followed it, and a reader stopping at the torn record never saw the rest. And
    `AgentCore::open` refuses to found an Agent that holds nothing while the log holds inputs. That is
    a lost `agent.state`, or a restore laid out and not finished, and founding afresh would serve an
    empty book beside the old replies.
42. **Object Lock, ruled** (the owner, 2026-10-05, `ab-q01`): compliance mode, 400 days, with no
    lifecycle rule that deletes current versions. It is the core host's default
    (`core_archive::archive::DEFAULT_RETAIN_DAYS`); a spec overrides it only with `lock-mode` and
    `retain-days` together. The bucket, its KMS key (S3 encryption only) and the writer's and
    restorer's IAM policies are the operator's to provision (the release documentation lists them).
    The experiment `2026-10-04-agent-backup` is closed: adopted as merged (#171), its defaults folded
    into decisions 34 to 42 and the release docs, and its two questions ruled.

## Tests

- `crates/core-binding/tests/agent.rs`: the Agent natively: principal, witness tampering, budget,
  rings, backpressure, exactly-once, recovery and replay from genesis, crash points.
- `crates/core-binding/tests/heap.rs`: 1 200 reservations, captures and transfers natively: the
  Agent's state, an invocation's peak heap and its witness do not grow, and no kind is read
  whole on that path.
- `crates/core-binding/tests/agent.rs` (T-005-13): a tampered or stale service witness, a service
  leaf withheld behind a proof that still gives the root, and a kind claimed whole with a leaf
  withheld, each refused with no commit; replay rebuilds the service tree.
- `crates/core-binding/src/service.rs`: the service root is CipherClerk's tree root; whole kinds;
  a concealed leaf.
- `crates/ledger-core/src/leaves.rs`, `state/tree.rs`, `delta.rs`: recording, fail-closed reads,
  one kind per collection, removals in the record.
- `crates/leaf-store/src/tests/service.rs`: removals on both paths, a kind's scan.
- `crates/core-binding/tests/agent_core.rs`: an ingress deployment over `AgentCore`; failures before
  and after the Agent ran.
- `services/ingress/tests/remote.rs`: the socket protocol.
- `crates/conformance`: `through-agent/native` in the default run.
- `crates/core-binding/tests/roll_forward.rs` (T-005-16): a fresh Agent rolled forward from genesis
  and from a checkpoint; a tampered entry, a gap, a checkpoint ahead or foreign, refused; an empty
  Agent over a logged archive never founded; a torn last record cut off.
- `crates/core-archive` (T-005-15): records, the write-once directory store, SigV4 against AWS's
  vector; `tests/stream.rs` end to end over a deployment (outage, restart, parts, another history);
  `tests/s3.rs` against MinIO with Object Lock and KMS (CI job `archive-s3`).
- `ops/tests/agent_drill.rs` (T-005-17, CI job `restore-drill`): back up, destroy, restore, replay the
  tail; zero divergence; refusals on the anchors and the roots. `ops/tests/alerts.rs`: the archive's
  alerts page.
- Through VOS, by hand: `aledgent-core-host smoke` (commands, and another principal refused),
  `measure`, `serve` twice over one state directory (found, then restore and recover), and the
  `F-003` and `F-004` cases through `agent-server`: 52 pass, 4 not runnable (kernel and store
  cases), none fail (`notes/MEASUREMENTS-F005.md` §5).

## TODOs

- `TODO(F-025)`: VOS pin to upstream; CipherClerk pin to upstream and delete the script;
  `T-025-03`, `T-025-06`, `proposals/vos/0003`.
- `TODO(F-005)`: the reads still made whole (decision 32), an ordered kind for final positions.
- `TODO(F-005)`: the deployment cases through `AgentCore`; `through-agent/vos` in CI once an
  invocation is fast enough.
- `TODO(F-005)`: key custody out of the core host (decision 15).
