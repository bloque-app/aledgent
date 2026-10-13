<!-- Copyright (c) 2026 Pagos Cube LLC -->
<!-- SPDX-License-Identifier: CC-BY-4.0 -->
# Amendment 0022 — A seeded genesis

**Status:** ruled by the owner on 2026-10-08 (`a0022-q1` to `a0022-q6`, "Owner rulings" at the end):
applied as drafted, with the window gated on `verify-genesis` (`a0022-q2`). It is applied to `SPEC.md`
(v0.4.15), because the closed list of errors is checked against
`SPEC.md` in both directions (`REQ-ER-1`, the conformance catalogue): code that returns
`ERR-InitialStateNotSealed` cannot build without the row.
**Target:** `SPEC.md` v0.4.14 as on `main` (amendment 0021 applied). It takes the spec to **v0.4.15**.
**Date:** 2026-10-08, revised the same day for the owner's ruling `genesis-chainspec`.
**Feature:** `F-036` (`features/F-036-seeded-genesis/`).
**Rationale in one line:** the owner's model is that the list of identities, accounts, balances and
companies is part of the genesis data; loading it as single commands after genesis takes
hours, so the book's first state is derived from it (in `PLAN.md`'s terms, from a chainspec), with the
same commands' effects, and checked against its commitment.

---

## The problem

`REQ-RC-4` loads an initial state as opening balances, after genesis, into accounts that must already
exist. Everything else of a prior system (its holders, their accounts and the operator's grants) comes in
as ordinary commands, one at a time. For an operator's existing book that is one command per identity and per account.
Measured through the core Agent, a command costs about 40 ms, so the load takes hours, and its rate
falls as the book grows. The window `NFR-17` allows is the only one there will be.

## The ruling sought

A deployment MAY start its book from a **seed**: an initial state committed by genesis, from which the
book's first state is derived, as the commands its records stand for would have made it, before it takes
any command. The book refuses everything until the seed is **sealed** (its first state in place and
checked), under a condition of its own. The plan calls the seed's file the **chainspec** (owner ruling
`genesis-chainspec`, below); `SPEC.md` names no technology and keeps the word *seed*.

## §2 — two glossary rows added

> | **Seed** | An initial state committed at a book's genesis by a digest, from which the book's first state is derived before it takes any command (`REQ-RC-6`). It is **sealed** once that state is in place and checked against the commitment. |
> | **Alias** | A prior system's identifier for a ledger account, loaded with a seed. It carries no personal data (`INV-17`). |

## §5.12 / §8 — `REQ-RC-6` (new), after `REQ-RC-5`

> **`REQ-RC-6`** **A seeded genesis.** A deployment MAY start its book from a **seed**: an initial state
> committed at genesis, instead of loaded after it one command at a time.
>
> - A seed holds identities, ledger accounts in delegated custody, aliases, opening balances with their
>   reconciliation, and grants. Each record MUST be one that a command could have made, and the book's first
>   state MUST be the one those commands would make: under the operator's authority, attributed to it
>   (`REQ-AU-7`), with the same checks.
> - Genesis MUST commit to the whole seed by one digest. The book's first state is **derived** from the seed,
>   not reached by executing commands: two derivations from the same seed and the same deployment MUST give
>   the same book, and a derivation that any record makes fail founds nothing.
> - The opening balances MUST pass the reconciliation of `REQ-RC-4` before the book is founded, and post as
>   `REQ-RC-4` requires: one `Opening` per (account, asset), against the asset's opening position.
> - Until the first state is in place and checked against the commitment, aledgent MUST refuse every command
>   with `ERR-InitialStateNotSealed`. Where the first state is derived outside the book's own execution, the
>   check MUST include a second derivation inside it, record by record from the seed, that reaches the same
>   state; no command is taken before that passes (`a0022-q2`). This is the window of `NFR-17`. Once sealed,
>   the book behaves as a book founded without a seed.
> - Where a seed is applied in parts, each part MUST be checked against the commitment before anything of it
>   takes effect, MUST take effect at most once, and a part that any record of it makes fail takes no effect
>   at all; an interrupted application resumes at the first part not applied.
> - A seed MUST be applied only to the book whose genesis committed it. No command gives a book that is
>   already live a seed: after genesis, the only initial-state load is that of `REQ-RC-4`.
> - Replaying the book derives its first state from the seed again and applies every input after it: the
>   same book (`REQ-RT-3`).
> - A seed MAY commit to the prior system's history by a digest of it alone. That history is not replayed
>   (`REQ-RC-4`), and no history record becomes an input of the book. Anyone holding a record and its proof
>   of inclusion can check it against the commitment without aledgent.
> - An alias maps a prior system's identifier to a ledger account. It carries no personal data (`INV-17`),
>   and aledgent keeps only a digest of the identifier.
>
> *Trade-off, stated plainly.* A seed moves the trust of the initial state to whoever built and reconciled
> it, as `REQ-RC-4` already does for opening balances: aledgent checks that what it applies is what genesis
> committed, and that each record is one a command could have made. It does not check that the prior
> system's figures are right.

## Epic N — `US-N2` (new)

> **`US-N2`** — As the **operator**, I start a book from a sealed seed of the prior system's identities,
> accounts, aliases, balances and grants, so that the migration takes minutes and not hours.
> - `AC-N2.1` Given a genesis that commits a seed, When the book is founded, Then its first state is the one
>   the commands its records stand for would have made, and it is sealed (`REQ-RC-6`).
> - `AC-N2.2` Given a seeded book that is not sealed, When any command is submitted, Then it is refused
>   with `ERR-InitialStateNotSealed` and nothing changes (`REQ-RC-6`).
> - `AC-N2.3` Given a part that is not the committed one at its position, or one already applied, When I
>   apply it, Then it takes no effect: a part already applied answers as it did (`REQ-RC-6`, `INV-18`).
> - `AC-N2.4` Given a seeded book, When its first state is derived again from the seed, or a seeding in
>   parts is interrupted and resumed, Then the book is the same (`REQ-RC-6`, `REQ-RT-3`).
> - `AC-N2.5` Given a seed whose opening balances do not reconcile with every required source, When I apply
>   it, Then it is refused before any part takes effect (`REQ-RC-4`, `REQ-RC-6`).

## §10 — one row added

> | `ERR-InitialStateNotSealed` | The initial state committed at the book's genesis is not yet in place and checked, and the book takes no command until it is sealed (`REQ-RC-6`) |

The list stays closed (`REQ-ER-1`), now with 40 conditions. No existing condition fits:
`ERR-DecisionUnavailable` promises that the command is kept and decided later, which a seeding book
does not do, and `ERR-NotAuthorised` would tell an integrating party to page on a configuration fault.

## §15 — the tables

Epic N becomes `US-N1`, `US-N2`; its requirements add `REQ-RC-6`; its invariants add `INV-17` and
`INV-18`. `US-N2` is in V0.

---

## The genesis is a state, built from a chainspec (owner ruling `genesis-chainspec`, 2026-10-08)

The first draft applied the seed inside the core Agent, chunk by chunk, each chunk an input. Measured on
a rehearsal deployment that day, it ran at **17–23 records a second and
falling**, the same as loading command by command.
Batching six records a chunk saved nothing; the cost is per record inside the PVM, about 40 ms. The same
seed applies in process in under three seconds. The owner ruled:

> The genesis **state** is defined by a chainspec, as in Substrate, not reached by executing transactions.

**The chainspec** is the seed format, renamed: canonical, versioned, one digest over the whole (the root of
its chunks' tree), the opening gate, aliases, grants and the history root (`features/F-036-seeded-genesis/
spec.md`). The chunks stay as an internal detail: they give the digest its tree, and they are the unit of
the second execution below.

**The genesis state** is built natively, deterministically, by the same library code the Agent runs
(`Ledger::genesis` with the chainspec: every chunk through `apply_spec_chunk`), with the deployment's key
custody. `aledgent-core-host build-spec` writes it as a leaf store and a manifest of its roots, for review
before the window. The core host rebuilds it in its replica before the Agent's first input, refuses to
start unless its roots are the manifest's, and founds the Agent on them in **one invocation** (a new
envelope, `StateGenesis`, after every existing variant; no host call is added, `m-jam-only`).

**Why genesis is not re-executed at boot.**

- **Substrate.** A chain's genesis block is not produced by executing extrinsics: its state is computed
  from the chainspec by the node, natively, and committed by its state root. Every node computes the same
  state from the same chainspec; that, and not an execution trace, is what makes it the chain's genesis.
- **`REQ-RT-3` (deterministic, replayable).** Replay is: derive the genesis state from the chainspec
  again, then apply every input after genesis. Both are deterministic, and both are checked: every core
  host start rebuilds genesis and compares its roots with the manifest's and the Agent's, and every input
  after it is replayed and checked as today. The chainspec is kept beside the archive, by its digest, so a
  restore rebuilds genesis from it.
- **`DEC-4` (the leaf store derives from committed inputs).** The genesis leaf store derives from one
  committed input, the chainspec, sealed by its digest in genesis. Nothing in it is taken from anywhere
  else.
- **What the Agent takes on trust, and what checks it.** The Agent stores the roots its founder hands it,
  as it already takes its driver: only the Agent's owner can found it. Three things check them: the
  manifest, reviewed before the window; every host's rebuild; and `verify-genesis`, which **must pass
  before the book takes any traffic** (ruling `a0022-q2`). The founder's word founds the Agent; it does
  not open the book.

**`verify-genesis`, the second execution, on the window's critical path (ruling `a0022-q2`).** It founds
a scratch Agent on the commitment alone and feeds it every chunk as an input, inside the PVM, and passes
when the Agent's roots equal the manifest's. It restores the property the first draft had: that the
genesis state is also what the Agent's own code reaches, chunk by chunk, under the PVM's metering. **No
traffic reaches a chainspec-founded book until it passes**: no caller is switched to the book and no
command is admitted, smoke tests included; the operator's read-only queries (V1–V4) may run meanwhile. It
needs only the chainspec, the reviewed manifest and the signer configuration, so it starts as soon as
`build-spec` has written the manifest, on a machine of its own, in parallel with the core host's boot and
V1–V4; the window's next step waits for its pass. A failure is a no-go. With the core package at
`opt-level` `s` the 1× synthetic book verifies in about **half an hour** on a 2-vCPU x86 runner, so it adds
about 5 minutes to the window beside the boot and V1–V4; segments on several hosts would bring it under 10 minutes (not built). This is why the chunk-by-chunk path, its refusal
before the seal and its resumption stay in the library: they are what `verify-genesis` runs.

**The seal.** A built genesis is born sealed in the Agent. The core host listens only once the genesis is
installed and its roots verified, so a chainspec book never answers a command before it. Under
`a0022-q2` the book is not open until `verify-genesis` has passed as well, and since `T-036-25` the core
host enforces it: until a passing `verify-genesis` report for its chainspec and genesis roots is installed
(`aledgent-core-host install-verified`), ingress answers every command `ERR-InitialStateNotSealed` before
logging it, and answers every read, so the operator's V1–V4 run meanwhile (`a0022-q2b`). The gate lives
at ingress and the core host: the Agent and its program are unchanged. `ERR-InitialStateNotSealed` also stays the condition for a book
founded unsealed (the chunk-by-chunk path, an in-process core given the commitment without the
chainspec).

## Why history is committed and not replayed (owner ruling `seed-history-commitment`, 2026-10-08)

The seed's header carries a Merkle root over one leaf per legacy transaction record of the frozen history
bundle (the prior system's transactions and its settlement records), the leaf count, the
bundle's digest and the scheme's version. The book stores that commitment and never reads it.

- **The legacy record is not a book aledgent can adopt.** It holds the defects a prior system's record
  has: settlement records that disagree with its transactions until they are closed by dispositions,
  pending balances and holds, assets that are archived and not migrated. Replaying it
  would make aledgent's book inherit them, or require aledgent to decide which legacy entries were right:
  a decision nobody reviewed. `REQ-RC-4` already forbids replay.
- **Its rows can carry personal data.** The prior system's metadata and settlement records name people and
  addresses. Every input of the book is replayed by anyone who restores or audits it (`REQ-RT-3`), and
  nothing with personal data may become one (`INV-17`). Only a 32-byte root and a count reach the book.
- **It stays provable.** A record from the archive, with its proof, is checked against the root genesis
  committed (through the seed digest), by anyone, without aledgent. The pre-cutover archive stays where
  it is, readable through ingress (ruling `prq-18b`).

The construction is in `features/F-036-seeded-genesis/spec.md` §4: one leaf per canonical JSON Lines
row, tagged with its table, `blake2b-256` with domains of its own, leaves sorted by hash, an odd node
carried up unchanged, and an explicit root for no history.

---

## Re-genesis and anchoring on the settlement layer (owner's request, 2026-10-08)

A seeded book is a new book. A deployment's book may be re-founded at least twice (a rehearsal book, then
the real book seeded from the prior system's data). Each new book must be able to
anchor on Kreivo. This section evaluates how, and proposes one mechanism. It changes nothing in `SPEC.md`
(`INV-19`, `REQ-PR-*` and `NFR-17` are unchanged); the mechanism is a plan concern.

### The facts

- The contract has no notion of a book or a genesis, and no reset, terminate or code upgrade: its messages
  are `new`, `execute`, `execute_with_limits`, `apply`, `deposit`, `credit`, `set_keys`,
  `set_withdrawal_bound`, `prune_inbox` and the reads (`contracts/settlement/src/lib.rs:211-486`).
- An anchor is accepted only as `last_anchor + 1`, else `AnchorOutOfOrder`; a stored number with another
  commitment is `AnchorConflict`, and the same commitment again is a no-op (`lib.rs:798-815`). Anchors
  are never pruned (`lib.rs:73-74`).
- A book's first checkpoint is number 1 (`crates/ledger-core/src/ledger/anchoring.rs:81`), and its anchor
  store refuses any other first number (`crates/ledger-core/src/anchor/mod.rs:600`).
- `Bind` happens once per asset, ever (`lib.rs:824-845`; the `bound` map, `lib.rs:80-83`).
- Positions, withdrawal windows and the inbox are the contract's, per asset and per sequence, not per book
  (`lib.rs:75-83`).
- The adapter refuses an anchor that is not `last_anchor + 1` (`services/settle/src/adapter.rs:1153`), and
  reads the inbox from a cursor kept per instance in its own database (`adapter.rs:2180-2241`).
- `set_keys` and every other admin message are the admin's (`lib.rs:363`), and a deployment contract's admin
  may be a Kreivo community: every admin call is then a proposal and a vote, which takes days and which the
  owner signs.
- The contract cannot change its own code; Kreivo's `Contracts::set_code` needs Kreivo's root origin, not
  the community's.

### The options

| | (a) Book epochs | (b) Reset or prune | (c) A new instance per book | (d) Upgrade with migration | (e) **The new book starts where the contract stands** |
|---|---|---|---|---|---|
| What changes | The contract: anchors keyed by (epoch, number), an epoch opened by an admin call carrying the new genesis commitment; binds and positions scoped per epoch | The contract: an admin message that archives the anchors' digest and zeroes the head | Nothing in the code: publish (already published), license and instantiate again | The contract, through Kreivo root's `set_code`, plus a storage migration | The book: a seeded genesis records the contract's head (last anchor, next inbox sequence) in the seed's header; its first checkpoint is that anchor plus one, and its adapter starts reading the inbox there. The contract does not change |
| Who authorises, how often | One community vote per new epoch, or once for an allowance (below); and before that, a new instance, since the live one cannot be upgraded | One community vote per reset | One community vote per book (instantiation through the contracts store) | A Kreivo root referendum per upgrade; then (a)'s votes | Nobody beyond the operator, who chooses the base when building the seed: no vote, ever |
| Old anchors | Stay, under their epoch | **Erased or overwritten**: a later book's anchor *n* lands where the old one was; only a digest remains | Stay, in the old instance | Stay, if the migration keeps them | Stay, untouched, below the base |
| Inbox entries | Tagged by epoch; a new epoch's adapter starts at its first entry | The head is zeroed, so sequences repeat: an observer can no longer tell entries apart by position | A new inbox | As (a) | Kept; the new book's adapter starts at the recorded next sequence and never ingests an old entry |
| Positions and withdrawal windows | Scoped per epoch, or carried over by an explicit migration | Undefined: zeroing the head does not move value | A new, empty position; value in the old one has to be withdrawn to the new one, within bounds | As (a) | The contract's; the precondition is that every position is zero. Windows run on as they are |
| Binds | Per epoch | Must be reset too, which `REQ-AS-8` forbids for a live book | New | As (a) | Once per asset per contract: the precondition is that no asset is bound |
| The adapter and `settle` | A new `epoch` in its configuration; anchors carry it | Its database must be reset with the contract | A new contract address in `settle.json`, a new settle database | As (a) | A new settle database whose cursor starts at the base; `POST /v1/ops/settlement` unchanged (same instance) |
| Security of the reset | An admin (or allowed key) that opens an epoch cannot rewrite anchors, but it can move the live book's verifiers to a new epoch: anchors must name their book so a verifier is not misled | **An admin that can erase anchors breaks non-revision (`REQ-PR-3`, `INV-19`)** | Nothing new | Root can do anything; nothing new beyond (a) | No new power anywhere; old anchors cannot be touched by anyone |

### Is "reset only while the current epoch has no binds or positions" enough?

For (a) and (b), no, on its own:

- **A book-only book has no binds and no positions and is live.** Stage A of the migration is exactly
  that (`DEC-20`): a deployment runs book-only, with anchors, until the bind. A rule of "no
  binds, no positions" would let whoever may reset do so on a live book in stage A, and its anchoring would
  stop (every later anchor out of order) or, under (b), its record would be erased.
- It also needs **no movement in flight** (no withdrawal or deposit the inbox has not settled), and, under
  (a), anchors that **name their book** (the genesis or seed digest in the commitment, so a verifier
  checks a chain of anchors as one book's).
- With those, (a) is safe against erasure but not against an availability attack on a live book in stage
  A; only the community vote (or keeping the power off the keys entirely) prevents that.

Under (e) the question does not arise: there is no reset. The preconditions bind the operator who
re-founds a book on a contract, not a power anyone holds over it: **no asset bound, every position zero,
no movement in flight, and the previous book no longer submitting** (its adapter stopped, its keys
removed from the settle signer's grants).

### Recommendation: (e), with (c) for any book that has bound value

- **A first deployment's re-geneses** (the rehearsal, then the real book from the prior system's data) are
  book-only until the bind. Each new book records the contract's head in its seed and continues the
  contract's anchor sequence: no vote, no contract change, and the earlier book's anchors stay on Kreivo
  as they are. A rehearsal must not bind or deposit on the deployment's contract; if it does, the real book
  takes a new instance, (c).
- **Each deployment** takes its own instance (`DEC-17`: one instance per deployment).
- **A book that has bound value is never re-founded on its contract.** A deployment that must start over
  after its bind takes a new instance, (c), and moves its value through bounded withdrawals, with the vote
  that implies.
- (a) and (d) are worth having in the contract's next version, for deployments that will re-found books
  more often than a vote allows. They need a new instance first (the live one cannot be upgraded), so
  they cost one vote either way. Not now.
- **Nothing is bound on a deployment's contract before its final re-genesis** (ruling `a0022-q4`): every
  rehearsal and stress book before it stays book-only on that contract, or the contract instance is lost
  to (e) for nothing. And `T-036-14` to `T-036-16` land before any rehearsal book anchors on a contract
  an earlier book anchored on.
- **Resetting a venue's anchoring is desired for the Spaces venue** (`DEC-17`; `OQ-68`), as Polkadot's
  `Paras::set_current_head` sets a parachain's head by governance: a later venue would let a re-founded
  book start its anchors afresh, without (e)'s base and without erasing the earlier book's.

#### What (e) needs, specified (code to land in `F-036`'s second step, `T-036-14` to `T-036-16`)

1. **The seed's header** gains `settlement: Option<SettlementBase { instance: u32, last_anchor: u64,
   next_seq: u64 }>`, read from the contract's `head()` at the block the operator builds the seed at, and
   committed with the rest of the header. Seed format version 2. *Landed with the genesis code (below):
   carried and committed; nothing reads it yet.*
2. **The anchor store** takes a base at the header chunk: a seeded book's first checkpoint is
   `last_anchor + 1`, and `AnchorWrite::Create` accepts that first number instead of 1
   (`anchor/mod.rs:600`). Every later rule is unchanged. The checkpoint's commitment (`aledgent/anchor/v2`)
   already binds the instance and the number; a construction `aledgent/anchor/v3` that also binds the
   book's genesis (seed digest) is recommended, under amendment 0019's versioned commitments, so that a
   verifier reading the contract sees which book an anchor belongs to.
3. **The adapter** checks, when the operator configures settlement (`POST /v1/ops/settlement`), that the
   contract stands where the seed says (`head.last_anchor == base.last_anchor`, no bind, every position
   of the registry's assets zero), refuses to start otherwise, and starts its inbox cursor at
   `base.next_seq` in a new settle database.
4. **The runbook** for seeding a re-genesis book stops the previous book's
   adapter and removes its grants on the signer before the new book's adapter starts.

*Trade-off, stated plainly.* Under (e) the contract cannot tell two books apart; only the anchors'
numbers and (with `v3`) their commitments do. A verifier must know the base of the book it checks. The
alternative that makes the contract know, (a), costs a new instance and a vote now, and gives an admin
(or an allowed key) a power over the live book's anchoring that (e) never creates.

### Imported passkeys and the identifier map

- **Passkeys** (owner ruling `mn7`, 2026-10-03; with `tos-passkey-registration` and `tos-aledgent-only`,
  2026-10-08). Each deployment imports its own environment's passkeys, the other environment's excluded by the
  extract's environment tag. A
  chainspec section *Credentials* gives an identity its existing passkeys beside the operator-held key, as
  `webauthn-es256` credentials marked imported (`REQ-MG-7`, `REQ-AU-20`), applied through
  `import_credentials`.
- **The identifier map.** `ids.jsonl`, which an integrating party imports at cutover, has one canonical form, and its
  SHA-256 is in the chainspec's header, so the genesis commits it and a consumer verifies the file, not
  only its counts.

### The genesis code (coordination with amendment 0023, PR #201)

Amendment 0023 (draft) proposes that "the chainspec's header names the genesis code version *H₀* and the
construction", in this format's version 2. It lands here, with amendment 0023's manifest unchanged
(`ledger_core::code`: magic `aledgent/code/v1`, each artefact's role, unkeyed `blake2b-256` digest,
length, `ProgramId` and build profile, the kernel's identity, the constructions and message formats;
*H* = `blake2b-256(manifest bytes)`). The header carries the whole manifest and the construction; the
book keeps H₀ and the construction in its record of the header.

The core host refuses to build or found the genesis unless its package (`--actor`), runtime
(`--runtime`) and own executable (`native-decide`: it builds the genesis natively) are the manifest's,
and the construction is its build's. **A restart checks the package and runtime only** (ruling
`a0022-q6`): any host that complies with the meta-protocol (JAM, as VOS implements it) may restart a
book, including VOS's native `vosx` binary once it is the host, since the genesis it rebuilds must still
reach the manifest's roots. Founding still pins the host binary the genesis code names. Only a test or stress book
may name no code (`--unnamed-code`). The core package is built at `opt-level` `s` (#203, 2,828,930 bytes, under VOS's 4 MiB prepared-program
cache and Kreivo's 4 MiB preimage limit); the manifest records `s`, and another `opt-level` is another H₀.
`features/F-036-seeded-genesis/spec.md` §7.

---

## Carried to the plan

- `PLAN.md` §4 `F-036`: the chainspec genesis. `aledgent-core-host build-spec` builds the state natively;
  `serve --chainspec <file> --genesis-manifest <file>` rebuilds and installs it and founds the Agent with
  `StateGenesis`; `restore` rebuilds it from the chainspec kept beside the archive; `verify-genesis` is the
  second execution, and the book takes no traffic until it passes (`a0022-q2`).
- The opening gate of `REQ-RC-4` runs in the builder and again in the core host before anything is built,
  over the chainspec's opening set and its reconciliation, which the chainspec carries.
- Memory: the native genesis holds the book's service store whole, as the replica does when serving
  (about 1.3 KB a record); everything else is streamed. In memory for
  now; the disk-backed store, `T-036-17`, is required before a book of about 3× is served (`a0022-q5`).
- The window waits for `verify-genesis` (`a0022-q2`, `a0022-q2b`). The gate is the core host's (`T-036-25`, built): every command is
  refused `ERR-InitialStateNotSealed` until `install-verified` has installed a passing report for this
  chainspec and these genesis roots; reads are answered meanwhile.

## Owner rulings (2026-10-08)

| Question | Ruling | Where |
|---|---|---|
| `a0022-q1` | Apply as drafted | `SPEC.md` v0.4.15 on this branch |
| `a0022-q2` | **Gate the window on `verify-genesis`**: no traffic on a chainspec-founded book until it passes | `REQ-RC-6`; "The genesis is a state" above; `T-036-25` (built: the core host's gate) |
| `a0022-q2b` | While `verify-genesis` runs, the operator's read-only checks V1–V4 may run; no commands or smoke tests until it passes | `T-036-25`: commands refused `ERR-InitialStateNotSealed`, reads answered |
| `a0022-q3` | Rebuild on every start, as built | — |
| `a0022-q4` | Agreed: continue the anchor sequence while nothing is bound; a new instance once value is bound. Nothing is bound on a deployment's contract before its final re-genesis; `T-036-14` to `T-036-16` before a rehearsal book anchors on a contract an earlier book anchored on; resetting a Space's anchoring is an open requirement | "Recommendation" above; `OQ-68`, `DEC-17` |
| `a0022-q5` | The store stays in memory for now; `T-036-17` (a disk-backed replica service store) is required before about 3× | `features/F-036-seeded-genesis/tasks.md` |
| `a0022-q6` | A restart checks package and runtime only: any meta-protocol-compliant host (JAM/VOS) may restart a book, including the native `vosx` once it is the host. Founding pins the host binary the genesis code names | "The genesis code" above; F-036 `spec.md` §7 |

## Questions for the owner (as asked)

- `a0022-q1`: Apply the amendment as drafted (`REQ-RC-6`, `US-N2`, `ERR-InitialStateNotSealed`)? It is
  applied on this branch only, for the closed list's two-way check.
- `a0022-q2`: The core Agent takes the genesis roots on its founder's word, checked by the manifest, every
  host's rebuild and `verify-genesis` (which may finish after the window). Accept that, or require
  `verify-genesis` to pass before the window opens (hours on the interpreter at a large book's size)?
- `a0022-q3`: The core host rebuilds the genesis state at every start (about 40–50 s at 1× here, native) and
  checks it against the manifest, instead of copying `build-spec`'s leaf store into its replica: the replica
  is rebuilt on every start by design (`T-031-14`), and a copied store would have to be recomputed to be
  checked anyway. Accept, or install the built store and check its roots only?
- `a0022-q4`: Re-genesis on a deployment's contract by (e), the new book starting at the contract's head, with
  the preconditions above; a new instance (c) for any book that has bound value?
- `a0022-q5`: The replica's service store is held in memory (about 1.3 KB a record), at
  build and when serving. Make it disk-backed (`T-036-17`) before a 10× book is served, or size the core
  host for it?
- `a0022-q6`: The genesis code's `native-decide` artefact is the core host's executable, so H₀ binds the
  exact host binary that builds the genesis, and `build-spec` and a founding refuse any other. On a
  restart only the package and runtime are checked; the executable may differ, since the genesis it
  rebuilds must still reach the manifest's roots. Accept, or require the founding executable on every
  restart until amendment 0023's upgrades exist (a host fix would then need a re-genesis)?
