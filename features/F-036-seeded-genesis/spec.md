<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->
# F-036 — aledgent's chainspec, its genesis state, and the history commitment

**Status:** draft, with amendment 0022 and the owner's ruling `genesis-chainspec` (2026-10-08). This is
the contract of aledgent's **chainspec** (`SPEC.md`'s *seed*): what `migrate chainspec build` writes, what
genesis commits, how the genesis state is built from it, and what checks it. `SPEC.md` `REQ-RC-6` says what
a seeded genesis must do. Implemented by `crates/ledger-core/src/chainspec.rs` and its modules `merkle` and
`history`; the build by `ledger_core::Ledger::genesis` and `core_binding::host::build_genesis`.

Every encoding is `F-014`'s canonical one (`crates/ledger-core/src/export/canon.rs`): integers
big-endian and fixed width, byte strings and sequences prefixed by a `u32` length, an optional value
behind a presence byte, an enum behind a stable one-byte tag, a map in key order. Every hash is
`blake2b-256(domain ‖ parts)`, CipherClerk's, the one every commitment of the book uses; no host call
is added (`DEC-16`, `m-jam-only`).

## 1. Records and sections

A chainspec is a header and a list of records. Records come in sections, in this order, each record tagged
with its section's code:

| Code | Section | Record | Order within the section | Applied as |
|---:|---|---|---|---|
| 1 | Identities | kind (`person`, `organisation`, `agent` tags 1–3), verifier | the builder's: every owner in legacy identifier order, then the seeded service principals in name order (`T-036-30`) | `open_identity` |
| 2 | Accounts | owner (16-byte identity), kind | legacy identifier order | `open_account`, delegated custody |
| 3 | Aliases | source (16 bytes), reference (bytes), account (16 bytes) | strictly increasing (source, reference) | the chainspec's alias table |
| 4 | Openings | account, asset, amount (minor units, `u64`) | strictly increasing (account, asset) | `Opening` postings |
| 5 | Operator grants | kinds (a non-empty set), expiry, justification | the builder's | `grant_operator` to the operator |
| 6 | Grants | a grant request | the builder's: the operator's over `delegated_accounts`, then the grants over identities, then over accounts (`T-036-30`), then the services' operator grants over `delegated_accounts` and their system-account grants (`T-036-31`) | `grant_authority` |
| 7 | Issuance grants | a grant request, justification | the builder's: the migrated book's issuers, then the services' (`T-036-31`) | `grant_issuance` |
| 8 | Credentials | identity, verifiers (1 to 100, distinct, well formed) | strictly increasing identity | `import_credentials`: each marked imported, unscoped |

**Credentials** (owner ruling `mn7`; `REQ-MG-7`, `REQ-AU-20`). An identity's set is the operator-held
`key` its identity record names, plus its existing passkeys as `webauthn-es256` verifiers, marked
imported. An identity without one keeps the operator key and registers a passkey later (at terms
acceptance). Each deployment imports its own environment's passkeys: the builder excludes every line of another
environment and refuses a line that names none. The operator's grants include `import_credentials` when the section is
not empty, and every import carries the same justification. The records go through the command's own
path, so the built book equals the book the same commands make.

**Service principals and amendment 0025's grants of the seed** (`T-036-30`; rulings `a0025-q3`,
`a0025-q4`, clause R4). A services file names the operator's service principals (the custody service, the issuing service, …), each
with the verifier of the credential it authenticates with, and two families of grants. The builder
opens each service as an `organisation` identity **after** every owner, in name order, so no owner's or
account's identifier moves, and appends to the Grants section, after the operator's grant, issued by
the operator as the identities principal:

- one delegated grant over every seeded owner that owns an account (every seeded account is in
  delegated custody), to the service named for `identity_grants`: `open_account` and the kinds its
  holder delegates over that identity's accounts (`REQ-ID-6`, R4), in identity order;
- one delegated grant, with no parent, over each of those owners' accounts, to the service named for
  `account_grants` (R4: the identities principal grants directly over an account of an identity it
  manages), in account order. The operator's own accounts get none (R3).

Each carries the file's bounds: one window and one limit, with an optional asset bound, or one limit
per asset, in which case each account (or identity) takes one grant per distinct limit, bound to its
assets (a window counter is per asset). Without a services file the chainspec is exactly what it was:
the services are opened, and these grants made, after genesis. The record format is unchanged.

The same file may also seed the services' other grants (`T-036-31`), each after the ones above so
nothing above moves: issuance grants (ruling `d-swaps-q2`: one per asset, `issue` and `redeem`, bounded
per window and to its asset, `REQ-AU-10`; the operator's own kinds then include `issuance_authority`),
operator grants over `delegated_accounts` (`PRQ-05`: the custody service, by kind), and account-scoped grants on the
operator's system accounts (`au-20`, `au4-q2`: the migration tooling's `system-grants.jsonl`, delegated, by the
operator as the accounts' owner; a line naming an account the operator does not own is refused). What
stays a command after genesis is the identities principal's role (ruling `t036-origins-role`).

Every amount is an integer of minor units. A figure that needs rounding never reaches a chainspec: the
builder converts through the one decimal boundary (`REQ-AS-4`) and refuses it.

The aliases are the opening set's **source map** (`F-014`): the set is the opening rows and the aliases,
and its digest (`OpeningSet::digest`) is the one its reconciliation names.

**The identifier map** (`ids.jsonl`, format `aledgent/ids/v1`): one line per
legacy identifier, `{"legacy","kind","target"}` with `kind` `identity` (the operator's line; or
`service:<name>`, a seeded service principal), `urn` (an owner) or `ledger_account_id`, sorted by legacy
identifier then kind. Its SHA-256 is the header's
`ids_sha256`, so a consumer checks the file against the book's genesis
(`migrate chainspec verify-ids`). Several lines may share a target: an absorbed account names its
identity's default, and a system owner names the operator, which owns the system accounts. A consumer compares **distinct targets**, not lines, with
the header's counts. `chainspec.json`'s `ids` section names the operator explicitly (`ids.operator`,
`did:aledgent:id:…`) beside `ids.targets` and its `operator_lines`, so a consumer reads the operator
rather than inferring it from the operator's line; `verify-ids --chainspec-json` checks both against the
chainspec and the file. The field is outside the chainspec: its digest is unchanged. With seeded
services, `chainspec.json`'s `services` gives each one's identity and credential (what the deployment
file's token entry names) and the number of grants of each family, `ids.targets.services` the
`service:` lines, and `verify-ids --chainspec-json` checks each against the file.

**The merge map and the exclusion file** (format 3; rulings `au-8` and `au-19`): `merges.jsonl`
(`aledgent/merges/v1`, each empty account absorbed into its default) and `excluded.jsonl`
(`aledgent/excluded/v1`, each legacy identifier left out of the book), canonical like `ids.jsonl`, with
their SHA-256 in the header (`merges_sha256`, `excluded_sha256`).

## 2. References

The record at position `n` of section `s`, in the book founded by genesis operation `g`, acts under

    op(g, s, n) = blake2b-256("aledgent/chainspec/op/v1" ‖ g ‖ s ‖ n as u64)[..16]

and mints what its command would under that reference: identity `n` is `IdentityId::mint(op, 0)` with
its credential at ordinal 1, account `n` is `AccountId::mint(op, 0)`, a grant is `GrantId::mint(op,
0)`. The operator is `IdentityId::mint(g, 0)`, as genesis mints it. Every opening is posted under
`op(g, 4, 0)`, as one load's postings are under one reference, at ordinal = the row's position in the
set; the opening positions are minted from it in asset order. Chunk `i`'s own record is under
`op(g, 0xC0, i)`. Every genesis record carries the header's time.

So a builder knows every identifier before the book exists, and a chainspec applies only to the book
whose genesis has `g`.

## 3. Chunks, the tree, the digest

- **Chunk 0** is the header: version (`u16`, 3), `g`, the time, the registry (each asset's symbol,
  location, precision, kind, deprecated flag: exactly genesis's), each section's count, the opening
  commitment (set digest, reconciliation digest, the set's total per asset), the history commitment
  (§4), the genesis code (§7) and the settlement base (amendment 0022, option (e):
  `{ instance, last_anchor, next_seq }`), the identifier map's SHA-256 (`ids_sha256`), and the merge
  map's and the exclusion file's (`merges_sha256`, `excluded_sha256`; format 3), each optional.
- **Chunks 1…** are runs of records, at most `max_records` records and `max_bytes` bytes each. A run
  of openings ends only where a batch of the deployment's `max_legs` non-zero rows does, counted across
  the whole set, so the genesis postings are linked exactly as one load of the same set links them.
- A chunk's canonical form is its index (`u32`), a tag (0 header, 1 records) and the body. Its **leaf**
  is `blake2b-256("aledgent/chainspec/chunk/v1" ‖ index ‖ chunk)`.
- The leaves, in index order, form a binary Merkle tree: a pair hashes as
  `blake2b-256("aledgent/chainspec/node/v1" ‖ left ‖ right)`; **an odd node is carried up unchanged**, never
  paired with itself; the root of one leaf is that leaf.
- The **chainspec digest** is `blake2b-256("aledgent/chainspec/v1" ‖ version ‖ chunk count ‖ root)`.
- **Genesis** carries `{ digest, chunks }` (`genesis.chainspec` in the deployment file). A chunk is applied
  with its **path**: its siblings from the leaf up, one per level at which it had one. The book folds the
  chunk's leaf through the path at its index among `chunks` leaves and accepts it only if the digest it
  reaches is genesis's.

The chainspec file (`chainspec.bin`) is `ALGSPEC1`, then canonically the header, the records, the records per
chunk, and the reconciliation (present when there are openings). Reading it rebuilds the digest and
checks the header against the records. A chainspec is versioned by the header's `version` (3: version 1
had neither the genesis code nor the settlement base, and was never deployed; version 2 had no merge map
or exclusion file digest, and founded rehearsal books, which a re-genesis replaces).

## 4. The history commitment (owner ruling `seed-history-commitment`)

- **Record.** One line of the history bundle's canonical JSON Lines (keys sorted, no insignificant
  whitespace, UTF-8, without its line feed) of one of two tables: the prior system's transactions (table code
  1) or its settlement records (code 2). A line that is not canonical is refused, not
  re-encoded.
- **Leaf.** `blake2b-256("aledgent/history/leaf/v1" ‖ table code ‖ line)`.
- **Tree.** The leaves sorted by value, ascending; two equal leaves are refused (a record twice is a
  defect of the bundle). Pairs hash as `blake2b-256("aledgent/history/node/v1" ‖ left ‖ right)`; an odd
  node is carried up unchanged. The leaf and node domains differ, so no node passes for a leaf.
- **No history.** Zero leaves have the root `blake2b-256("aledgent/history/empty/v1")` and count 0;
  nothing verifies against it.
- **Commit.** `{ root, leaf_count, bundle_digest, scheme_version: 1 }` in the chainspec's header, where
  `bundle_digest` is the SHA-256 of the bundle's signed manifest, as published. The book stores it in its chainspec record and never reads it.
- **Proof.** `{ scheme_version, index, leaf_count, siblings }`. It verifies when the scheme is 1, the
  count is the commit's, and the record's leaf folds through the siblings at the index to the root.
- **Against a genesis.** A verifier holds the deployment's genesis (`chainspec.digest`, `chainspec.chunks`),
  the chainspec's chunk 0 and its path, the record and its proof: chunk 0 must fold to genesis's digest (`chainspec::
  verify_header`), and the record to the header's root (`chainspec::history::verify`).
  `migrate history verify` does both.

Only the root and the count reach the book's inputs. No history record, so no personal data, is ever
an input that anyone replaying the book reads (`INV-17`, `REQ-RT-3`).

## 5. The genesis state (owner ruling `genesis-chainspec`)

The book's first state is **built**, not reached by executing inputs, as a Substrate chain's genesis
state is computed from its chainspec.

- **The build.** `Ledger::genesis` with `Genesis::spec` (the chainspec itself, never serialised) founds
  the book, then applies every chunk in order through `apply_spec_chunk`, as the operator, natively: the
  same code the core Agent runs. The result is sealed. A chainspec that is not the one `genesis.chainspec`
  commits, or is for another genesis, or has a chunk any record of which is refused, founds nothing.
  The state depends on the chainspec and on the deployment's key custody (account keys and blindings
  derive from the key-ring root), so it is built by the signer's client, never with a key in process.
- **`aledgent-core-host build-spec --config <deployment> --chainspec <file> --out <dir>`** writes the
  leaf store (`<dir>/replica/`: the kernel's book store and the service tree) and `<dir>/genesis.json`:
  the chainspec digest and chunk count, the genesis operation, the record count, the roots (kernel,
  service, audit), the next posting number, the build time and the leaf store's size.
- **The core host** (`serve --chainspec <file> --genesis-manifest <genesis.json>`) builds the same
  state in its replica before the Agent's first input, refuses to start unless its roots are the
  manifest's, and founds the Agent with one `StateGenesis` invocation carrying the chainspec's
  commitment, the lifecycle configuration, the limits and the hot row's values (sub-roots, last commit,
  next posting, service root, audit head, notifications). On every later start it rebuilds the genesis
  from the chainspec (kept at `<state>/chainspec.bin`) and replays the archive, as it always did.
- **Replay and restore** (`REQ-RT-3`): rebuild the genesis state from the chainspec, then apply every
  archived input. With `--archive`, the core host keeps the chainspec at `chainspec/<digest>.bin` in the
  archive; `restore` takes it from `--chainspec`, the state directory or the archive. `opsctl`'s
  re-derivation takes `--chainspec`.
- **`verify-genesis`** (`--config --chainspec --manifest --runtime --actor`): a scratch Agent, in
  memory, founded on the commitment alone (`Envelope::Genesis`, unsealed), takes every chunk as an input
  (`InputBody::SpecChunk`, `Call::ApplySpecChunk`) inside the PVM and must end at the manifest's roots.
  **The book takes no traffic until it passes** (rulings `a0022-q2`, `a0022-q2b`): it starts once
  `build-spec` has written the manifest, beside the core host's boot. With `--out <file>` it writes its
  report (`core_binding::verified::Report`, format `aledgent/verify-genesis/v1`: passed or not, the
  chainspec's digest and chunk count, the roots reached, records, time).
  It runs the PVM on the recompiler where VOS has one (x86-64 Linux), the interpreter elsewhere
  (`--engine` overrides; `T-036-32`). `--segments K` verifies `K` runs of chunks in parallel, each in a
  scratch Agent of its own, segment `i > 0` founded on the natively applied prefix and accepted only if
  that state is the one segment `i − 1` reached inside the PVM (`T-036-33`, `NOTES.md` §15; its use in a
  migration window waits for `vg-q1`).
- **The gate** (`T-036-25`). `serve` on a chainspec book holds a gate over
  `<state>/genesis.verified.json`: until that file names the book's chainspec and the genesis roots this
  host rebuilt, with `verified: true`, ingress refuses every command `ERR-InitialStateNotSealed` (409)
  before logging it (`Core::genesis_open`), and answers every read, so V1–V4 run meanwhile. The operator
  installs the report with `aledgent-core-host install-verified --config <file> --state <dir> --report
  <file> [--genesis-manifest <file>] [--archive <spec>]`, which checks it first and writes nothing for a
  report of another chainspec, other roots or a failed run; the book opens at its next command, with no
  restart, and stays open across restarts. With `--archive` the report is kept beside the chainspec in the
  archive (`chainspec/<digest>.verified.json`) and `restore` lays it out again. `serve
  --unverified-genesis` skips the gate (test and stress books only). Ticks and delivery records are the
  core's own and are not gated; nothing in the Agent changes.

## 6. Memory complexity

`n` records, `c = n / max_records` chunks.

| | Memory | Note |
|---|---|---|
| `migrate chainspec build` | O(n) | The extract and the records; about 0.32 KB a record |
| The chainspec in a process | O(n) | The records, about 0.4 KB a record in memory |
| Chunk bytes and paths during a build | O(c) | The tree's nodes only (64 bytes a chunk); each chunk's bytes and path made when it is applied |
| The service tree's write | O(1) | Commits of 65,536 leaves; never every leaf's encoding, nor one transaction of all |
| The kernel's leaf store | O(1) | On disk, with a bounded cache |
| **The book's service store** | **O(n)** | Held whole in memory by `ledger_core::state::State`, as the core host's replica holds it when serving: about 1.3 KB a record |

Peak RSS of `build-spec` measured for the 1× synthetic book and for 2×; `--max-memory` refuses a chainspec above a bound before anything is built.
The O(n) service store is the replica's existing design, not the build's; making it disk-backed is
`T-036-17`, required before a book of about 3× is served (ruling `a0022-q5`; design in `NOTES.md` §13).

## 7. The genesis code (amendment 0023, PR #201)

The chainspec names the code its genesis is built and run under, as a Substrate genesis carries `:code`.
The header's `code` is `{ manifest, construction }`: amendment 0023's **code manifest**, unchanged (magic
`aledgent/code/v1`; for the core writer, each artefact's role (`package`, `runtime`, `native-decide`),
unkeyed `blake2b-256` digest, length and VOS `ProgramId`, with its build profile: target, toolchain,
`opt-level`, Cargo features, source revision, `Cargo.lock` digest; the kernel's identity, informative;
the constructions it reads and the message formats), and the construction the genesis is built in, one
of the manifest's. The genesis code version **H₀** is `blake2b-256(manifest bytes)`
(`ledger_core::code`). The book keeps H₀ and the construction in its record of the header.

- **`aledgent-core-host code-manifest --actor <vos> --runtime <pvm> --profile <json> --out <file>`**
  writes the manifest: the digests and `ProgramId`s from the files and the running executable (the
  `native-decide` artefact, since it builds the genesis natively), the build profiles from `--profile`,
  which the release pipeline writes. `migrate chainspec build --code <file>` puts it in the header.
- **The check.** Every artefact the manifest names must be the bytes the process runs, and the
  construction must be the build's:

  | Command | Package (`--actor`) and runtime (`--runtime`) | The executable (`native-decide`) |
  |---|---|---|
  | `build-spec` | checked | checked |
  | `serve` and `restore`, founding (no replica yet) | checked | checked |
  | `serve`, restart (a replica) | checked: no upgrade exists yet, so the Agent still runs H₀ | not checked (ruling `a0022-q6`): any host that complies with the meta-protocol (JAM, as VOS implements it) may restart the book, VOS's native `vosx` included once it is the host; its one decision, the genesis, is checked against the manifest's roots and the Agent's |
  | `verify-genesis` | checked | not checked: the genesis is executed inside the PVM |

  Founding pins the host binary the genesis code names; a restart does not. A mismatch refuses before
  anything is built or founded. A chainspec that names no code is refused
  unless `--unnamed-code` is given, which only test and stress books do.
- **Not here:** the hot row's `code`, upgrades, the code archive and the code map (amendment 0023,
  `F-037`). The core package is built at `opt-level` `s` since #203 (`core.vos` 2,828,930 bytes, under
  VOS's 4 MiB prepared-program cache and Kreivo's 4 MiB preimage limit); the manifest records `s` in the
  package's build profile, so a first deployment's H₀ names it.
