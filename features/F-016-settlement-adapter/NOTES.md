<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-016 — implementation notes

Written while implementing `F-016` (issue #16), 2026-09-23. The plan's proposals were followed as
accepted — a KMS-held secp256k1 signer (as a stub, below), the coordinate `(instance, inbox seq,
hash of the entry)`, a status declaration with a heartbeat, PostgreSQL for the adapter's store —
with two exceptions stated below: chain access goes through subxt rather than Sube (decision 1),
and `NotIncluded` observations are not sent (decision 10). Where the plan was silent, the more
conservative behaviour was chosen (refuse rather than allow, keep reserved rather than release);
each such choice is listed here for review.

## F-005 and issue #38

The plan lists `F-005` (the core as a VOS Agent) as a dependency. `F-005` is blocked on the owner's
decision in issue #38 (VOS Agent or native core). The adapter does not need it: it reaches the core
**only through ingress** (`F-009`) and reads the core's outbox of instructions with a query, so
everything it touches is `F-009`'s `boundary::Core` trait. It was built and tested against the
in-process core (`ingress::inprocess::InProcessCore`), exactly as `F-009` was; nothing here is
VOS-specific. Whatever #38 rules, `F-005` implements the same trait and the adapter is unchanged.

## What landed

| Where | What |
|---|---|
| `crates/ledger-core/src/ledger/settle_adapter.rs` | The adapter's surface on the core: `Ledger::adapter` (its three capabilities), `adapter_instructions` (its one read), `require_available` (the status gate) |
| `crates/ledger-core/src/principal.rs` | `resolve` refuses the adapter's principal on every other command and query; `resolve_adapter`, `is_adapter` |
| `crates/ledger-core/src/settlement/{mod,store}.rs` | `SourceId::of_identity`, `AdapterStatus`, `MAX_ADAPTER_LEASE`, `instruction_bytes` / `instruction_digest`; the store's status write |
| `crates/ledger-core/src/ledger/settlement.rs` | The availability check in `withdraw` and `submit_outbound`; the operator cannot be named the adapter |
| `services/ingress/src/settlement.rs` (and small edits to `wire`, `command`, `surface`, `boundary`, `inprocess`, `openapi`) | `submit_outbound` on `CTR-CMD`, `configure_settlement` on `CTR-OPS`, the adapter's routes, and their wire forms; `openapi.json` regenerated |
| `services/settle/src/adapter.rs` | The step: lease, status, intake, track, observe, submit, re-scan (T-016-04 to T-016-09) |
| `services/settle/src/chain/` | `Chain`; `fake::FakeChain`; `rpc::RpcChain` over subxt: its smoldot light client (feature `light-client`, the default) or, opted into, a node's RPC (feature `rpc`, which `light-client` includes) |
| `services/settle/src/signer.rs` | `Signer`; `DevSigner`; `KmsSigner` stub (feature `kms`) |
| `services/settle/src/store/` | `Store`; `mem::MemStore`; `pg::PgStore` with `migrations/0001_settle.sql` |
| `services/settle/src/link.rs` | The link to ingress: `HttpTransport` (feature `http`), `RouterTransport` (in process) |
| `services/settle/src/{config,main,testkit}.rs` | Configuration; the `aledgent-settle` binary; the in-process deployment for tests and the suite |
| `services/settle/tests/` | `flow.rs`, `restart.rs`, `intake.rs`, `key_hygiene.rs`, `postgres.rs`, `live_node.rs` (ignored) |
| `crates/conformance/src/cases/f016.rs` | Every `F-016` case, run |

This closes `F-009`'s deferred item and `F-010`'s `TODO(F-009)`/`TODO(F-016)` for the adapter's
authenticated surface (`record_observation`, `acknowledge`, the head, authenticating the source),
the real adapter, its signer, its chain access and cursor, and the digest of a pallet inbox entry.

## Decisions taken during implementation

### The core side

1. **Chain access is subxt's dynamic API, with its smoldot light client, not Sube.** The plan's
   accepted proposal was "Sube and smoldot directly". Sube on crates.io is 0.5.0 (valibre-org,
   metadata v14 only, async-std); the current Sube is unpublished, a git dependency on Virto's
   repository. subxt 0.44's dynamic API meets the requirement the proposal served — encoding
   driven by the live metadata only — and its light client *is* smoldot (`subxt-lightclient`). An
   asset value that does not fit the runtime's asset type is an error, never a fallback. Trade-off,
   stated plainly: `RpcChain` uses `PolkadotConfig`, whose transaction extensions are the standard
   ones. Kreivo's own (`PassAuthenticate`, `SkipCheckIfFeeless` wrappers) need a `Config` that
   names them before the adapter can sign for the deployed runtime; that is not built, and cannot
   be verified here (see "The end-to-end run"). As first merged, the binary connected over a
   node's RPC and the light client was an optional feature it never used; since #74 the light
   client is the default build and the default connection, and RPC is an explicit opt-in
   (decision 29).
2. **The adapter's principal is an identity of the book**, and `SourceId::of_identity` is its 16
   bytes. The operator names the adapter by identity in `configure_settlement`, so the
   authoritative source (`REQ-IN-6`) and the only principal the adapter's surface accepts are one
   thing, and no caller can speak as another's source. `resolve` refuses that identity everywhere
   else — every command and every query, including `F-013`'s operator frame — so the adapter holds
   no authority over any ledger account and cannot cause an entry (`REQ-SA-8`) by construction,
   not by the absence of a grant. `configure_settlement` refuses to name the operator itself.
3. **The three capabilities, as routes.** `declare_adapter` is its status and support;
   `ack_instruction` its acknowledgement; `record_observation` covers observations, canonical
   checks, the finalised head, gaps and re-scans — every fact about the layer. Every one is a
   command in ingress's received-log (`Intake::Decide`), so every core input from the adapter is in
   the log, and the adapter's client keys make a retry name the same row.
4. **The status gate.** A declaration is recorded with the core's time and holds for the lease the
   adapter asks for, capped at five minutes. **Once the adapter has declared**, `withdraw` and
   `submit_outbound` are refused with `ERR-SettlementLayerUnavailable` while it refuses or once its
   declaration is stale. Before its first declaration nothing is gated here (`F-010`'s simulated
   driver never declares), but nothing is supported either, so nothing can be withdrawn. The plan
   names `submit_outbound`; `withdraw` is gated too, because `US-E4` says the caller learns this
   "before I have committed anything", and `withdraw` reserves. The gate is checked **before** the
   support check: an adapter that refuses to serve is reported as unavailable rather than as not
   supporting the asset.
5. **`configure_settlement` on `CTR-OPS` takes no justification.** `F-010`'s function takes none and
   is not one of `F-013`'s framed operator commands; its client-key domain is the operator
   surface's. `submit_outbound` is on `CTR-CMD` at `/v1/withdrawals/{reference}/submit`.
6. **The capability manifest** is the adapter's declaration of `(kind, asset)` pairs plus the
   configuration's instance. The genesis is the adapter's to assert (below); the core does not hold
   it. The operator's `declare_adapter_support` (`F-013`) and the adapter's declaration write the
   one declaration, each replacing the last, as `F-013` built it.

### The adapter

7. **Start-up assertions run on every heartbeat**, not only at start: genesis, `spec_version` on the
   allow-list, the pallet in the metadata, the instance registered with every lane's key, and each
   asset's decimals equal to the registry's precision. A failure at any time declares `refusing`
   (with one alert) and stops everything else — nothing is read from, or signed for, a network that
   is not the declared one. A runtime upgrade to an unlisted `spec_version` therefore stops signing
   until the allow-list is updated.
8. **The registry's precision is the adapter's configuration**, copied from the reviewed registry
   change (`REQ-AS-2`). The adapter's principal has no read of the registry (decision 2), so it
   cannot fetch it; a mismatch between the configuration and the registry is an operator error the
   core does not detect.
9. **The coordinate's digest is blake2b-256 of the stored SCALE bytes of the pallet's
   `InboxEntry`** (which include the block number), with no domain: anyone who reads the storage can
   recompute it. The chain's operation reference is the book's 16-byte reference followed by 16
   zero bytes; a record with any other reference is reported without one (the core finds it
   unassociated).
10. **No `NotIncluded` observations.** `F-010` proves non-inclusion from the inbox (its decision 17),
    so the core needs no such report. The adapter keeps its own proofs in its store — dispatch
    failed in a finalized block, era ended with the nonce unspent, nonce spent by another
    extrinsic, never signed (a tombstone) — so it can state them from storage (`REQ-SA-5`). Unknown
    produces no report at all.
11. **Expiry is a height** (`F-010` decision 16), so the plan's "era whose end falls at or before
    the expiry at the fastest known block time" becomes exact: the longest power-of-two era, at most
    `min(max_era, BlockHashCount)` blocks, built at the best head, whose last valid block is at or
    before the expiry. If none fits, the instruction is tombstoned and never signed.
12. **One extrinsic per operation reference, ever.** Once signed, those bytes are the operation's
    only extrinsic: never re-signed after its era ends or its dispatch fails; the core releases it
    once the finalised head proves non-inclusion. The store refuses a second (primary key in
    PostgreSQL). After a crash between storing and broadcasting, the same bytes are broadcast.
13. **Nonces.** One extrinsic in flight per lane (the accepted proposal). The nonce is the larger of
    the lane's next nonce and the finalized account nonce; it stays reserved while its extrinsic is
    uncertain; it is released when the finalized nonce passes it, or reused when the era ended with
    it unspent. A nonce spent by an extrinsic the adapter did not sign raises an alert (two owners
    of one lane) and proves the adapter's own cannot land.
14. **Observation payload.** The stored bytes of the entry; for `WithdrawRefused`, the chain's error
    text (as `F-010`'s driver does), so the core keeps the layer's own words (`REQ-SA-3`).
15. **Provisional reports** are made only for the adapter's own extrinsics seen above finality (the
    plan). A provisional coordinate is checked only once the finalized head covers its position, so
    `still_matches` is decided at finality and never by a best-chain view that may flip. A
    provisional report and the final one of the same coordinate are two requests; the second gets
    "already applied".
16. **The cursor** advances past a position only once the core took it in (any verdict, or
    `ERR-EvidenceInsufficient`, which `F-010` ingests too) and the verdict is stored. Any other
    refusal stops the read with an alert. A position below the head that is neither present nor
    pruned is never skipped: alert, and the read stops there. A run of pruned positions is one gap,
    stored, alerted, reported, and only then passed (`AC-D2.2`). The head is reported every cycle
    with the finalized inbox length, even when the cursor is behind: the core waits for the range.
17. **Liquidity** is the position at the finalized head less the amounts of the adapter's signed,
    undecided extrinsics; fees are the lane's native balance against the configured floor. One
    alert per `(instance, asset)` and one per lane, cleared when nothing waits on it. The check runs
    only before signing: an extrinsic already out is never parked (`AC-E3.3`).
18. **Multi-leg (`AC-E3.3`).** Every outbound operation in the book has one leg (`F-010`), so
    "partially executed" is realised as "already signed": it is never parked; if the layer then
    refuses it, the refusal is reported and the core's deadline takes it to `Needs-Review`.
19. **Parking and the deadline** (plan open point 2): a parked instruction whose expiry can no
    longer be met is tombstoned, never signed, and released on proof (`REQ-OP-9`); the deadline is
    after the expiry by `F-010`'s check at submission, so the release comes first.
20. **The lease's epoch increments on every take**, the same holder's included: a restarted process
    fences its own predecessor. Storing an extrinsic and moving a lane check the epoch in the same
    transaction.
21. **Declarations use a client key unique to the process and the declaration**, so a new
    declaration always reaches the core; every other report's key is derived from what it reports.
22. **The PostgreSQL store** keeps rows as JSON documents under the columns its rules need, with
    `synchronous_commit = on`; a trigger keeps a stored extrinsic's bytes immutable.
23. **Broadcast errors from a node are warnings.** Broadcast is repeated until the finalized head
    decides; "already imported" or "stale" is not the adapter's failure.
24. **Keys.** `DevSigner` reads a hex secret from a file or an environment variable, never formats
    or logs it, and zeroises it on drop. It is development only: `NFR-8` puts production keys
    outside the environment, which is the KMS signer's job. `KmsSigner` (feature `kms`)
    knows its handle and public key and **refuses every signature** until a KMS client is chosen
    (`TODO(F-016)` in `signer.rs`); it never falls back to a local key. No real key or secret is in
    the repository: the test lanes are the constants `[0x5e; 32]` and `[0x6f; 32]`.
    *Superseded by `F-035` (2026-10-04):* the stub is gone; the adapter signs through the signer
    service (`ServiceSigner`), whose lane keys are AWS KMS keys, and a file or environment key needs
    `"development_keys": true`.
25. **smoldot and ed25519-zebra.** smoldot 0.19.4 uses ed25519-zebra's batch verifier, which 4.2
    moved behind its `alloc` feature. The `light-client` feature enables it through an otherwise
    unused optional dependency, so the default build (which includes `light-client`, decision 29)
    and `cargo clippy --all-features` both build.

### Export and import (`F-014`, `REQ-RC-3`)

26. **The adapter's status declaration is exported.** It is observable: once the adapter has
    declared, a refusing or stale declaration refuses `withdraw` and `submit_outbound` with
    `ERR-SettlementLayerUnavailable`, while "never declared" gates nothing (decision 4). An import
    that dropped it would turn a stale or refusing adapter into one that never declared, and
    accept withdrawals the original refuses. So the settlement section carries it as a thirteenth
    record, and `F-014`'s `FORMAT_VERSION` is 2: a version-1 export has twelve records and is
    refused by version, not misread. The declaration's times are the core's input times, so the
    imported book's gate answers exactly as the original's at every time.
27. **What else the adapter changed, and how it crosses.** The adapter's identity and credential
    are ordinary identity records; `configure_settlement`'s choice of it is the configuration's
    `authoritative` source, already exported, from which `is_adapter` is derived, so the imported
    book refuses the same principal the original does. The adapter's acknowledgements,
    observations, checks, heads, gaps and re-scan incidents are writes to the settlement store,
    already exported field by field; its declarations of support are the support record. None of
    its inputs writes an operation record or a client-key entry of its own. Its client keys and
    replies live in ingress's received-log, which is not the book and is not exported.
28. **The comparison now asks the adapter's surface** (`export::compare`): the outbox of
    instructions as the adapter's principal, the status record, and the gate at `now`, at the
    declaration, and on either side of the moment it goes stale. `F-014`'s random book names an
    identity as its adapter, declares through `Ledger::adapter`, and its driver speaks as that
    identity, so every round-trip test covers them. A dedicated test
    (`req_rc_3_the_settlement_adapter_its_status_and_its_records_cross_an_export`) round-trips a
    stale, a refusing and a serving declaration and withdraws on both books. Checked by mutation:
    importing without the status is refused by the root check, and exporting without it is caught
    by the comparison in four of `F-014`'s tests.

### The connection: a light client by default (#74, follow-up to #16)

29. **The adapter connects through subxt's embedded smoldot light client unless its configuration
    says otherwise.** The owner prefers a light client always: the adapter then checks what it
    reads against the chain's own consensus rather than trusting one node's answers, which serves
    `REQ-SA-2` and the finality the cursor reads at (`REQ-SL-3`) better than a single node's word.
    `light-client` is the default feature (it includes `rpc`), and the binary requires it.
    `ProcessConfig` names the connection: `"connection": "light-client"` (the default, may be left
    out) with `chain_spec` and, for a parachain such as Kreivo, `relay_chain_spec`, both paths to
    JSON chain specs with their boot nodes; or `"connection": "rpc"` with `endpoint`, the explicit
    fallback. **It never falls back by itself.** `ProcessConfig::chain_source` resolves exactly the
    connection named, and refuses a field of the other one instead of ignoring it: an `endpoint`
    without `"connection": "rpc"` — which is what a configuration written before this change looks
    like — is an error at start-up that says how to opt in, not a silent RPC connection; a chain
    spec beside `"rpc"` is an error too. A light client that cannot sync leaves the adapter not
    serving, as any unavailable settlement layer does. The binary warns when it runs over RPC, and
    does not log the endpoint. `RpcChain::light` takes the chain spec and an optional relay chain
    spec: with one, the chain is added to smoldot as that relay chain's parachain; without, as a
    chain of its own. `tests/live_node.rs` follows the same rule (`ALEDGENT_SETTLE_CONNECTION`,
    `light-client` by default) and refuses a chain spec any of whose boot nodes is not on this
    machine, as it refuses a non-local RPC endpoint. Trade-off, stated plainly: the light-client
    path is compiled and its configuration tested, but it has not connected to a running chain
    here, for the reasons "The end-to-end run" gives; nor has the RPC path.

### The signing key's nonce as a release precondition (owner ruling, 2026-09-24; #88)

**The ruling.** Before the core releases a reservation because an outbound transaction provably
never executed, a second, independent fact must agree with the inbox proof: the signing key's
account nonce at the finalised head has not moved past the transaction's nonce. The adapter never
re-signs, and stores each signed transaction before it submits it (decision 12), so each nonce of
a lane belongs to one of the deployment's transactions, and a consumed nonce means that
transaction was included. The core's side is `F-010`'s decisions 29 to 35.

**Rejected: a position-balance check.** The owner rejected checking the instance's position
balance instead: a deposit landing in the same gap can mask a withdrawal, so a balance cannot be
attributed to one operation. The liquidity check before signing (decision 17) is unchanged and is
not a release condition.

30. **Every head carries the signing keys' nonces.** The head report (`HeadBody::nonces`) carries
    the account nonce of each signing key the adapter uses (every lane, and the anchor key),
    read at the same finalized block as the head. Its client key adds a digest of the nonces to
    `settle/head/<height>/<next_seq>`.
31. **Recorded before broadcast, in this order: store, record, broadcast.** After an extrinsic is
    stored (decision 12), the adapter records its key, nonce and era's last valid block with the
    core (`POST /v1/settlement/signings`, client key `settle/sign/<operation>`), and broadcasts
    only once the core holds it. So an instruction the core holds as unsigned was never
    broadcast. After a crash between storing and recording, the rebroadcast records first. If the
    core refuses the record, nothing is broadcast: one alert (`sign:<operation>`), and the stored
    bytes wait unsent until their era ends. A tombstone (decision 19) is recorded with the core
    too, after it is stored, so the core may release it on the inbox alone.
32. **A freed nonce is reused only after the core has seen it freed.** Track decides an
    extrinsic dead (its era ended, its nonce unspent) at the same finalized block the step then
    reports as the head, before anything new is signed; and **nothing new is signed in a step
    whose head the core did not take in** (one alert, `head`). So the head that shows the dead
    transaction's nonce unspent at or past its last valid block reaches the core before the lane
    gives that nonce to another transaction, and the core fixes the fact then (`F-010`
    decision 30). An era must also leave a block to be included in (its last valid block after
    the best head), or the instruction is tombstoned: a head at or past the last valid block is
    then always one the core has not yet seen.
33. **What the adapter's own proofs now mean** (decision 10). `EraEnded` agrees with the core's
    unconsumed nonce, and the core releases on both proofs. `DispatchFailed` and `NonceSpent`
    consume the nonce with no inbox record: the core cannot tell either from an execution its
    reader missed, so both go to `Needs-Review` with `IncludedWithoutRecord`, never to a release.
    `NeverSigned` is released on the inbox alone once the tombstone is recorded. The adapter
    still keeps every proof in its store and never reports an outcome (`INV-11`).
34. **A reader fault on the fake chain** (`FakeChain::misread_reference_of`): an inbox entry
    read back with an operation reference not of the deployment's shape, its bytes and
    coordinate intact. The core ingests it as unassociated, the range completes, and only the
    nonce stands between that and a release.

Tests: `inv_11_an_included_transaction_whose_inbox_entry_the_reader_misses_goes_to_review` and
`req_sa_4_heads_carry_the_signing_keys_nonces_and_signings_precede_broadcast` in `flow.rs`;
`req_sa_4_each_disposition_is_kept_apart` now expects the failed dispatch in `Needs-Review`,
reserved, with the incident. The restart matrix and the chaos run pass unchanged with the extra
round trip before every broadcast.

### Amendment 0006 item 3 (ruled 2026-09-29)

A proved rejection releases at once: a `WithdrawRefused` record the adapter reports, final, with
its transaction's nonce consumed on the head the adapter reports, is released by the core with no
review (`F-010`). Nothing in the adapter changed for it: it already reports the refusal with the
contract's words and every head with its keys' nonces. Two tests changed their expectation from
"still reserved" to "released": `t_027_02_req_sa_4_a_refused_call_inside_a_batch_is_recorded_and_the_rest_apply`
(`contract.rs`) and the refusal leg of the dispositions test (`flow.rs`). `AC-E1.4` also runs here by
the adapter's own route to "provably not executed" — never signed within its expiry, or signed and
its era ended unincluded — released on proven non-inclusion.

### TLS to its store in production (T-016-13, ruled 2026-10-02)

`ProcessConfig::profile` (`production` or `development`, no default). In production the adapter's
store refuses a connection string without `sslmode=require` before any connection, with ingress's
`check_tls`, and the connection checks the server's chain and host name as ingress's does. The
zombienet suite's harness runs `development`: its PostgreSQL has no TLS.

## The end-to-end run (T-016-10, and F-019's zombienet run)

**Not run. A local node for the pallet's runtime is not buildable here within the limits given.**
Exactly what is missing:

- **Binaries.** Kreivo on zombienet (`pallets/kreivo/zombienet/aledgent-settlement.toml`) needs
  the Polkadot SDK stable2606 `polkadot`, `polkadot-execute-worker`, `polkadot-prepare-worker` and
  `polkadot-parachain`, and the Kusama relay chain-spec generator (from the fellowship runtimes).
  The machine has `polkadot` 1.5.0 only, no `polkadot-parachain`, no `polkadot-omni-node`.
  Downloading release binaries needs a permission this session did not have; building them from
  the SDK source needs well over 15 GB of target directory, with about 12–18 GB of disk free
  during this session and other features building in parallel.
- **The runtime.** A build of the patched Kreivo runtime and its chain-spec generator
  (`pallets/kreivo/kreivo-branch.sh apply` then a release build): a full Polkadot SDK runtime
  build, also over the disk budget. `F-019`'s earlier build directory is gone.
- **`pallets/kitchensink` cannot stand in:** it is a benchmarking runtime with no consensus, "not
  meant to run a chain".
- **A `Config` for Kreivo's transaction extensions** (decision 1), which cannot be written or
  checked without the runtime's metadata at hand.

What exists instead: `services/settle/tests/live_node.rs`, ignored by default, runs ingress, the
in-process core and the adapter over `RpcChain` against a **local** chain — through the light
client by default, with the raw chain specs zombienet writes, or over a local RPC endpoint when
asked (decision 29), and refuses anything not on this machine. It covers a withdrawal from
reservation to `Settled`, and a wrong declared genesis refused.
`services/settle/README.md` gives the command and the admin set-up (register the instance, set the
lane's key, set a withdrawal bound, fund the position). Every acceptance criterion of the exit
criterion runs against the fake layer, in `settle`'s tests and in the conformance suite.

## Tests and cases

- **Conformance** (`crates/conformance/src/cases/f016.rs`): all 11 registered cases run, on both
  in-process targets: `ERR-UnsupportedLeg`, `ERR-SettlementLayerUnavailable`, `AC-D2.1`, `AC-D2.2`,
  `AC-E1.4`, `AC-E1.5`, `AC-E3.1`, `AC-E3.2`, `AC-E3.3`, `AC-E4.1`, `AC-E4.2`.
- **`services/settle/tests`**: `flow.rs` (26: the happy path; deposits; the lease; the adapter's
  principal refused elsewhere; each start-up assertion; a runtime upgrade; a stale declaration;
  `AC-E4.1`; durable-before-acknowledged; every submission crash point; the nonce rule; the cursor
  under crashes; a pruned range; `INV-12`; a reorg orphaning a provisional record; every
  disposition; unknown; the tombstone; twenty parked withdrawals resuming; fee parking; `AC-E3.3`;
  the re-scan; `INV-11`), `restart.rs` (the restart matrix over all eight crash points, stating
  everything from storage alone; a seeded chaos run of 5 × 120 steps with reorgs, crashes and lost
  submissions, asserting one extrinsic per operation reference and the book agreeing with the
  chain), `intake.rs` (3, against a scripted ingress: `INV-13`, a digest that does not match, a
  second digest), `key_hygiene.rs` (`NFR-8`: logs, errors, store and environment scanned),
  `postgres.rs` (2, in Docker), `live_node.rs` (1 ignored, 2 run: a local endpoint, a chain spec
  with local boot nodes only).
- **Unit**: the era arithmetic (`REQ-SL-4`, an era never outlives the expiry), the signer, the RPC
  helpers; the connection (4, in `config.rs`: the light client by default, the README's example,
  an endpoint without `"rpc"` refused, RPC only when named and alone); `ledger-core`'s adapter surface (3); ingress's wire round trips (2).

## Deferred

- `TODO(F-016)`: a KMS client for `KmsSigner`; a subxt `Config` naming Kreivo's transaction
  extensions; the end-to-end run on a local Kreivo (above); measuring `block_time` on the deployed
  chain for `F-010`'s configuration.
- `TODO(F-018)`: paging on the adapter's alerts (`refusing`, `position:*`, `fees:*`, `gap:*`,
  `incident:*`, `lane:*`, `digest:*`), dashboards of its reports and lanes; the coretime crank
  (plan open point 6).
- `TODO(F-017)`: anchor instructions through the same intake (`{number, commitment}`); `Anchored`
  inbox records are read and reported today, and `F-010` ignores them.
- `TODO(F-011)`: incidents and gaps on the query surface.
- Several lanes for throughput exist (one extrinsic in flight each); pipelining within a lane is
  not built (the plan: only after measurement). Filling a gap from an archive (`REQ-IN-5`, plan
  open point 8) is not built: a gap is recorded and raised.
