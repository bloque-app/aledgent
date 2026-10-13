<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->
# F-036 — notes and decisions

Each decision says who should confirm it. The ones that change `SPEC.md` are amendment 0022's
questions; the others are the plan's, recorded here for the owner's review.

## 1. The seed applies through the commands, one reference per record

Each record runs through the command it stands for, under a reference derived from genesis's and the
record's position, at the header's time. So a seeded book equals, leaf for leaf and posting for posting,
the book the same records make as commands (`req_rc_4_the_seeded_book_is_the_book_the_same_records_make_as_commands`),
but for what only the seed has: its record, its aliases, and the records of its chunks and of its opening
reference (a load's in the other book). Every seeded identity and account has its own operation record,
attributed to the operator (`REQ-AU-7`).

*Cost.* An identity's command writes about six leaves (the identity, its credential and its set, the
operation record, the client-key entry, its audit). Through the Agent each is witnessed absent and
written; that, not the call, is what a chunk carries. Writing one record per chunk instead would save
about a third (amendment 0022 `a0022-q3`).

## 2. A chunk is all or nothing

A record the command refuses takes back every write of its chunk (the service store keeps the chunk's
undo log), and the book stays at the chunk before. Openings are checked before any is posted and posted
last, so a refusal never leaves a posting behind. A refused chunk is a defect of the seed: it is refused
the same way every time, and the book never seals. The builder's checks and the gate exist so that it
does not happen; if it does, the book is re-founded (it holds nothing else yet).

## 3. The opening gate runs on the operator's side of the Agent

`F-014`'s reconciliation needs the whole set and every source extract at once: for a migration, a set of
openings and the source-map entries of several sources, hundreds of kilobytes. That does not fit an
invocation. The builder runs the gate before it writes the seed, and the core host runs it again over the
chainspec before it builds the genesis; the seed carries its reconciliation so anyone can re-run it.
The Agent checks that every chunk is the committed one, posts exactly the committed rows, and at the seal
that each opening position stands at minus its asset's committed total (amendment 0022 `a0022-q2`).

## 4. The seal's guard is in principal resolution

Every command resolves its caller; the guard there covers every command and every caller (a seeded
identity's credential is the operator's key, ruled 2026-10-08, so the operator could act as any of them
before the seal). It reads the seed record inside the command's recorded span, so the Agent witnesses it
and decides the same. Cost: one more leaf, a slot, in every command's witness for the book's life.
Ingress also answers ticks without running them while unsealed. The signals read through the guard.

## 5. No route on ingress, and the core listens only once genesis is in place

Ingress never builds or applies a chainspec. The core host builds the genesis state, installs it in its
replica, checks it against the manifest and founds the Agent before it opens its socket, so a chainspec
book is sealed before ingress can reach it (boot order: signer, core, ingress). `ERR-InitialStateNotSealed`
is the condition of the chunk-by-chunk path: a book founded on the commitment without the chainspec
(`verify-genesis`'s scratch Agent, an in-process core).

## 6. Aliases are kept as digests

`alias_key = blake2b-256("aledgent/alias/v1" ‖ source ‖ reference)`. The book can resolve an alias for
whoever presents the legacy identifier (`Ledger::resolve_alias`), and keeps nothing of it. Low-entropy
identifiers are guessable from their digest; the ruling that identifiers carry no personal data
(`INV-17`) is what makes that acceptable.

## 7. Chunk size

The chunks are now internal to the chainspec: the digest's tree, and the unit of `verify-genesis`. Six
records a chunk is what the core Agent's 256 KiB guest heap holds, so the chunk path
`verify-genesis` runs works on the real guest.

## 8. The data source is an input

`migrate seed build --source extract --extract <dir> --environment test|prod` reads the transform's
outputs of one environment. A deployment's real book is seeded from its prior system's own
databases. The synthetic book's counts are synthetic defaults only.

## 9. What the seed does not carry

- History (committed by its root, `spec.md` §4).
- Passkeys: a deployment's existing passkeys are imported after the seal with
  `import_credentials` (`F-028`), which needs the identities to exist; a seed section for them is a later
  step if the count grows.
- Treasury limits and owned custody: every seeded account is delegated, with no limit below zero.

## 10. Ruling `genesis-chainspec` (2026-10-08): the genesis is a state

The first draft applied the seed through the core Agent, a chunk an input; on a rehearsal deployment it ran at 17–23
records a second, as slow as loading command by command. The owner ruled the Substrate model: the genesis
state is defined by the chainspec and built natively, with the same library code; the Agent starts from its
roots in one invocation (`Envelope::StateGenesis`). Amendment 0022 explains why genesis is not re-executed at
boot (`REQ-RT-3`, `DEC-4`) and what `verify-genesis` restores.

Decisions in it, for the owner:

- **The core host rebuilds the genesis at every start** instead of copying `build-spec`'s leaf store into
  its replica: the replica is rebuilt on every start by design (`T-031-14`), and the rebuild is what checks
  the roots. `build-spec`'s store is the review copy and the size measurement (`a0022-q3`).
- **The Agent takes the roots on its founder's word**, checked by the manifest, every host's rebuild and
  `verify-genesis`. *Ruled 2026-10-08 (`a0022-q2`):* the founder's word founds the Agent but does not open
  the book: no traffic until `verify-genesis` passes. Enforced by the core host since `T-036-25`
  (`a0022-q2b`: reads, V1–V4, are allowed meanwhile): ingress refuses every command
  `ERR-InitialStateNotSealed` until `install-verified` has put a passing report for this chainspec and
  these genesis roots in the core host's state directory. The gate is at ingress and the host, not in the
  Agent: a refusal there is never logged, so it never takes an input number or a replay slot, and the
  core program does not change.
- **The state depends on the key custody**: account keys and blindings derive from the key-ring root, so
  `build-spec` runs with the deployment's signer, like `serve`. Two deployments with different roots build
  different kernel roots from one chainspec; the service root is the same.

## 11. Memory (the coordinator's finding of 2026-10-08)

A 10× native build reached 16 GB. What the build held besides the book is now streamed (`spec.md` §6): peak
RSS grows by a base and a slope, measured at 1× and 2×. What remains O(n) is `ledger_core`'s service store, held whole in memory,
as the core host's replica holds it when serving: about 1.3 KB a record. A disk-backed service
store is `T-036-17` (`a0022-q5`; *ruled 2026-10-08:* in memory for now, `T-036-17` required before about
3×; design in §13). `--max-memory` refuses a chainspec above a bound before building.

## 12. Stress on grown state

- **The core Agent's book** (`e2e/stress/run.sh GROWN=1|10`): the throwaway deployment's genesis commits a
  synthetic chainspec built for it, and the core host builds its state; the workload's holders are opened on
  top. This is the path a real deployment takes.
- **The shard Agents** (`shards`/`fleet --chainspec`): a shard Agent keeps its book in its rows (layout 3),
  so its grown state cannot be installed from roots; it is opened and funded through the principals Agent
  before the timed phases. **The partition rule is the router's own**: the account's partition
  (`Partition::of`, a hash of its identifier, `REQ-RT-9`), then the shard holding that partition's range
  (`Directory::shard_of`). VOS's Local Agent image limit bounds each shard's book until external state
  (`DEP-11`): a 10× book over 45 shards exceeds it, which needs `DEP-11`.

## 13. The genesis code (coordinator, 2026-10-08, aligning with PR #201)

The chainspec names its genesis code with amendment 0023's manifest, not a format of this feature's
own: `ledger_core::code` implements that manifest as amendment 0023 (a) describes it, so `F-037` reuses
the type for the hot row's `:code` and the code map. Choices made here, each open to amendment 0023's
owner:

- **The whole manifest is in the header, not only H₀.** A host checks its binaries against the
  chainspec alone, with no code archive yet (amendment 0023 (c) is later). It costs a few hundred bytes
  in chunk 0.
- **`native-decide` is the core host's executable.** The genesis is a transition the host decides
  natively, so the executable that builds it is an artefact of H₀ (amendment 0023 names
  `native-decide` for this case). So any rebuild of the core host is another H₀ for a book not yet
  founded.
- **A restart does not check the executable.** Its one decision, the genesis, is checked against the
  manifest's roots and the Agent's on every start. Requiring the founding executable forever would stop
  a host fix from ever reaching a running book; the package and the runtime are still checked, since
  there is no upgrade yet and the Agent must run H₀. Question `a0022-q6`.
- **Build profiles come from a file** (`--profile`), written by the release pipeline (the x86 bench
  writes one). A binary cannot say its own `opt-level` or toolchain reliably; its digest binds them.
- **The settlement base lands in the same format version** (2), as amendment 0023 proposes: carried and
  committed now; the anchor store and the adapter read it later (`T-036-14` to `T-036-16`).
- **The `opt-level` is `s`**, from `main` (#203): this branch was rebased onto it, so the chainspec book
  runs the opt-`s` core (2,828,930 bytes, under VOS's 4 MiB prepared-program cache and Kreivo's 4 MiB
  preimage limit). The manifest records `s` in the package's build profile; any other `opt-level` is
  another H₀.

## 14. `T-036-30`: the service principals and amendment 0025's grants of the seed (2026-10-10)

**Why.** In a rehearsal these grants were made after genesis by a script, one command each: per-account
grants to the issuing service, and grants over identities to the custody service (made by the onboarding
service as the identities principal). At a real book's volume that is many minutes of commands inside the
migration window. Rulings `a0025-q3` and `a0025-q4` put them in the chainspec.

**The problem.** The grants name the custody service's and the issuing service's principals, which were opened at runtime
(`POST /v1/identities`), so their ids did not exist when the chainspec was built.

**What was done.** The seed opens them too. A services file (`migrate chainspec build --services`)
names each service and the verifier of the credential it authenticates with; the builder seeds each as
an `organisation` identity **after** every migrated identity, in name order. Identity `n` is minted from
`op(g, 1, n)` alone, so appending moves no migrated identity, and no account is added, so no account
moves. Their ids and credentials are known before the book exists, and the grants name them:

- the identities principal is the operator at genesis (it keeps the role, `a0025-q1`), so the operator
  issues both families directly, as R4 says ("how the operator, as the identities principal, issues the
  seed's grants at genesis"): over an identity (delegated, `open_account` and the delegated kinds), and
  over each account of a managed identity (delegated, no parent);
- the records are the existing `Grant` record in the existing Grants section, after the operator's own
  grant (ordinal 0, unchanged), so the format stays at 4 and the core needs no change;
- bounds come from the file, per window and per asset; a window counter is per asset, so differing
  per-asset limits become one grant per distinct limit.

Without the file the chainspec is byte-identical to before: checked against a rehearsal
extract (its digest rebuilt exactly). With a services file the same extract gives the grants the script
made; every `ids.jsonl` line of format 4 is unchanged and one is added per service (`service:<name>`);
`bench` seals it in process.

**What changes in a rehearsal's procedure.** The service tokens map, in `ingress.json`, to the seeded
principals `chainspec.json`'s `services` names (identity and credential), instead of to identities the
script opens; the script then skips opening the custody and issuing services, the per-account grants
and the grants over identities. What remains after genesis: the onboarding service (unless it is seeded
too), its `open_identity` role, the issuing service's issuance grants, the custody service's operator
grant over `delegated_accounts`, and the issuing service's grants on the system accounts (the
migration tooling's `system-grants.jsonl`).

**Left to the owner** (each implemented conservatively, behind the file):

1. *Seed the identities principal's role?* Ruled no (`t036-origins-role`): it stays one command. The onboarding service can be seeded as a service (its identity is then
   fixed), but its `open_identity` role cannot: the seed's operator-grant record gives kinds to the
   operator itself only, so seeding the role needs a record that names a principal (format 5). Today the
   role stays one command after genesis.
2. *Seed the remaining service grants?* Ruled yes: `T-036-31` seeds the issuing service's issuance grants, the custody
   service's operator grant over `delegated_accounts` and the issuing service's system-account grants from
   the same file, after `T-036-30`'s, in format 4. `ids.jsonl` is unchanged.
3. *Kind of the issuing service's per-account grants.* Ruled delegated (`t036-swaps-grant-kind`). The rehearsal script issued them as `operator` grants (from `prq-04`);
   R4 says the identities principal grants **delegated** authority over a managed identity's account, so
   the seed's are `delegated` (the in-process tests show the issuing service transferring under them). A book founded on
   the seed and the rehearsal's book differ in that field, and in the authority kind each of that
   service's operations records.
4. *Bounds for a deployment.* The file carries the operator's figures (`prq-04`); a rehearsal used one
   window of 24 h and one limit for every asset. A real deployment's bounds are the operator's to set.

## 15. `verify-genesis` in a migration window: the engine, and segments (`T-036-32`, `T-036-33`, 2026-10-10)

**The finding.** A rehearsal deployment's `verify-genesis` ran at 26–37 ms a record because it ran on the
PVM's interpreter: the core host's default unless `F025_BACKEND=recompiler`, which nothing in that
deployment sets. A record is 12–21 M of gas inside the PVM; the interpreter runs about
0.5–0.6 G/s, the recompiler about 4 G/s. Every x86 figure the migration window was planned on was
taken on the recompiler. Who confirms: the coordinator, on a rehearsal deployment, with the branch's image.

**`T-036-32`: the engine.** `verify-genesis` picks the recompiler where VOS has one (x86-64 Linux) and the
interpreter elsewhere; `--engine` or else `F025_BACKEND` overrides it; it logs the engine it ran on. Both
engines are VOS's PVM, executing the same program to the same state (`F025_LOCKSTEP`); the gate is the
same check either way. `serve` keeps its default (owner question `vg-q2`).

**`T-036-33`: segments, the design.** `verify-genesis --segments K` cuts the chunks into `K` runs of about
as many records and verifies each in a scratch Agent of its own, in parallel (one child process each,
`verify-genesis … --segment i/K`).

- *Segment 0* is today's run cut short: a scratch Agent founded on the chainspec's commitment alone
  (`Envelope::Genesis`, unsealed), taking chunks `0..e₀` as inputs.
- *Segment i > 0* needs the state at chunk `sᵢ`. Its host applies chunks `0..sᵢ` natively to its replica,
  with the same `apply_spec_chunk` and context as the native genesis (seconds), and founds the scratch Agent
  on that state in one invocation (`StateGenesis`, `Driver::found_at_chunk`,
  `AgentCore::open_segment`); the Agent then takes chunks `sᵢ..eᵢ` as inputs, witnessed against that
  state as every input is.
- *How the start state is trusted without trusting the native build.* It is not trusted. A segment
  reports the state its Agent was founded on and the state it reached, each as the Agent's hot row holds it
  (`Hot::genesis_state`: the kernel's sub-roots, last commit and next posting, the service root, the audit
  chain, the notification count; everything an input's effect on the book depends on beside the
  configuration and limits, which every segment takes from the same deployment). The verifier accepts the
  chain only if segment `i`'s founding state **equals** the state segment `i − 1` reached inside the PVM,
  field for field, the chunks are consecutive from chunk 0, and the last segment seals and ends at the
  manifest's roots (`chained`). So segment `i` starts from a state the PVM itself reached, by induction from
  the commitment, and the chain is the chunk-by-chunk execution from the commitment, one Agent at a time.
  A native build that is wrong anywhere makes some boundary unequal, and the run fails.
- *Why not checkpoints in the manifest.* The brief proposed that `build-spec` write a checkpoint root every
  `N` chunks into the manifest and each segment check its start against one. That is not needed: the
  boundary states are compared between the segments' own Agents, which is the stronger check (the
  manifest's checkpoints would themselves be the native build's word), so neither the manifest nor the
  report changes. What a manifest checkpoint would add is locating a divergence without running the
  segment before it; the segment logs already locate it to one segment.
- *The report.* Unchanged, format `aledgent/verify-genesis/v1`: verified, the chainspec, the roots reached,
  the chunks and records, the wall time. The segments and each boundary's roots are in the log, not in the
  report (owner question `vg-q1`).
- *Cores.* A single run already keeps about two vCPUs busy (the host's native replica and witnesses,
  the PVM and the signer overlap): on the 2-vCPU x86 runner, two segments took 28.1 s against 24.6 s for
  one. So `K ≤ vCPUs / 2`; on a 2-vCPU node such as a rehearsal deployment's, `--segments` gains nothing and the
  engine is the whole gain. On 16 vCPUs, 6,124 records: 21.9 s with one segment, 12.5 s with two, 7.4 s
  with four.
- *Memory.* A segment holds the book up to its last chunk (its replica, about 1.3 KB a record, §11), so `K`
  segments hold about `(K + 1) / 2` times one run's (4.5 times at `K = 8`). They are
  processes, not threads, so each segment's memory is its own and the same command runs a segment on
  another host.
- *Across hosts* (not built). A child is an ordinary command (`verify-genesis … --segment i/K` prints its
  segment as one JSON line), so the segments can run on several hosts; combining their lines needs a
  `--combine` step that runs `chained` over them and writes the report. Built when a host of enough cores
  is not available.
- *What it does not change.* No root, no chainspec, no package, no report format, no Agent code: the
  founding is the existing `StateGenesis`, the inputs the existing chunk inputs.
