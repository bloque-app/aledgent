<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Code history, code sources and node roles

**Status:** describes amendment 0023 (`amendments/0023-code-history-and-upgrades.md`), which the owner
ruled on 2026-10-08, as amended by amendment 0033
(`amendments/0033-code-history-independent-of-the-platform.md`), ruled on 2026-10-17. **Nothing here is built yet.** `F-037` builds it, and this page becomes the operator's
reference when it lands.

## The trust model, first

aledgent is **distributed, not decentralised**. One organisation runs every host of a deployment, inside one
trust domain. Hosts are replicated for availability; they do not check each other as adversaries.

- A Shared Agent's voters replicate a writer's state and agree on its order so that losing a machine loses
  nothing. They are not a Byzantine quorum, and a deployment with one voter is sound.
- What a third party can rely on without trusting the operator is **verification**, not decentralisation:
  anchors on the settlement layer show the book was not revised after the fact, and anyone the operator gives
  the archive to (an auditor, a regulator) can replay the whole book, under the exact code that ran it, and
  reach the same roots.
- This page uses Substrate words (collator, warp sync, finality) for mechanics only. They do not mean that
  hosts are independent parties, that anyone can join, or that consensus protects against a dishonest
  operator.

## Code history

Every writer of the book (a shard, the principals Agent, the settlement and anchor Agents, or the single core)
records in its own committed state which **code version** executes its next input. A code version is the
digest of a manifest that lists every artefact that decides a transition (the guest package, the runtime
program, and any natively deciding host library), each with its `blake2b-256` digest, its length and its
**build profile**: target, toolchain, `opt-level`, Cargo features, source revision and `Cargo.lock` digest.
Two builds of the same source with different settings are two code versions.

- **Upgrades are inputs.** An operator submits `Upgrade { code, from }` through the operator surface. It is
  authenticated and audited, applies from the named input number onward, and never re-executes an earlier
  input. It is refused unless every artefact of the new version can be fetched and matches its digest.
- **Who may upgrade** is the **upgrade-authority mode**, fixed in the genesis chainspec and kept in each
  writer's state:
  - one principal;
  - a proposer and a different approver;
  - *k* of *n*, optionally with a delay counted in inputs.

  Changing the mode is an input of its own, authorised under the mode in force at that moment. It cancels
  pending proposals, and the next anchor binds the new mode.
- **The code map** (which inputs ran under which version) is read off the book itself: every archived entry
  names its code version.
- **Replay and restore** fetch each range's code and run each range under it, checking every entry against
  its archived log hash.
- **Checkpoints and anchors** name the code version and the construction of the state commitment, so a
  verifier holding an anchor knows what produced the anchored state and how it is committed. A change of
  construction is itself an input (a migration), and its checkpoint binds both roots.

## Independent of the runtime

The code history is shaped so that the book can later move to another runtime as one more upgrade, with
no new genesis (amendment 0033).

- **A code version names its platform**, and lists each artefact's identities on that platform beside its
  own digest. Nothing the platform supplies (its clock, its slot, its identities, its encoding of state)
  enters a root, a log hash, a checkpoint or an anchor.
- **A code generation counts the upgrades.** Each writer holds the generation of its code: 0 at genesis, one
  more after every upgrade. Every package is built with its generation compiled in, and the manifest's digest
  binds that constant to the code version.
- **The switch of code is one host operation**, taken between two inputs, installing only bytes fetched and
  checked before the upgrade was accepted. The code also fences itself, at every input: code whose
  generation is not the writer's (or, from a scheduled upgrade's input *n*, the next one) refuses the input,
  with nothing written. A missed switch, or a stale package after one, halts the writer; it never runs an
  input under the wrong generation. Within a generation, the bytes are checked by digest before they are
  installed, and the node checks every artefact it runs, the runtime included, when it starts and at every
  switch, installing whatever the code map names for the next input.
- **The fence has a way out.** While it holds, the old code still takes an input that moves the upgrade to a
  later input or cancels it, under the upgrade-authority mode. Either generation decides that input the same
  way, so the outcome never depends on whether the switch was already made. An alert fires when a writer stays fenced longer
  than the deployment's bound (five minutes by default).
- **Only the book's upgrade-authority mode authorises an upgrade.** A runtime's own authority to install
  programs is a host key, never an authority over the book's code.
- **Checkpoints are the writer's state and the book's leaves**, in aledgent's own format, so one archive
  restores onto either runtime. A node's local cache can be in any format; it is never archived or sent.
- **Replay is native first.** The book's state replays from the inputs with aledgent's own code, without the
  runtime. Each range's host image (its replayer) is kept in the code archive beside the code version, not in
  it, so the runtime's execution can be checked too, and rebuilding the host is not an upgrade.
- **A writer can be continued by another at input *n***, carrying its whole state and log head after input
  *n* − 1. The founding is written once per input number, only after every check has passed, as the claim
  of that number, and the archive takes each input number of a logical writer once. A first writer and its
  continuation therefore can never both hold an input. Once a continuation is founded, the upgrade is final. A
  move of runtime is an upgrade naming the new runtime, followed by a continuation on it.

## Code sources

The code map lives in the book. Only **retrieval** is configurable: where a node fetches the bytes for a
digest.

### Why any source is safe

Every fetched artefact is checked against its digest (length and `blake2b-256`) before it is used. Bytes that
do not match are discarded, counted in `aledgent_code_source_mismatch_total` and logged, and the next source
is tried. This is the model of JAM's `lookup`: the program only ever receives bytes the host has verified by
hash. A source can therefore be untrusted, slow or wrong; at worst it costs availability, never correctness.

### Configuration

An ordered list, tried in order with fallback, after a local content-addressed cache:

```toml
[code]
cache = "/var/lib/aledgent/code"
sources = [
  { kind = "fs",    path = "/mnt/aledgent-code" },
  { kind = "s3",    uri  = "s3://my-aledgent-code/prod?region=eu-west-1" },
  { kind = "https", url  = "https://code.example.org/aledgent/{digest}" },
  { kind = "exec",  command = ["/usr/local/bin/my-code-fetcher", "{digest}", "{len}"], timeout_secs = 60 },
  { kind = "kreivo-preimage", endpoint = "light-client" },
  { kind = "plugin", path = "/usr/lib/aledgent/sources/libmysource.so",
    digest = "<blake2b-256 of the library, hex>", isolation = "helper", config = { } },
]
```

In the core host the table is a file of its own, in JSON, named by `aledgent-core-host serve --code-config
<file>` (the deployment file is shared with the front door, which refuses keys it does not know). It has the
same keys. Today only `fs` is built in; any other `kind` stops the node at start, and without the flag the
node has no source of code and refuses every `Upgrade`:

```json
{ "cache": "/srv/aledgent/code-cache",
  "sources": [ { "kind": "fs", "path": "/srv/aledgent/code-src" } ] }
```

Paths are absolute. The cache must not be a source, inside one, or inside the state directory. One regular
file per digest, named by its 64 lowercase hex digits; a symbolic link, a directory or a special file is never
read. `aledgent_code_source_mismatch_total` has one label, `source`: `cache`, or the kind and position in the
list (`fs:0`, `fs:1`).

### Built-in sources

| Kind | What | Notes |
|---|---|---|
| `fs` | A local or mounted directory, one file per hex digest | Network mounts and offline audit copies alike |
| `s3` | An S3-compatible bucket, read-only | Credentials from the environment or the instance role, never in the URI |
| `https` | Any HTTP(S) server or CDN; `{digest}` is replaced by the hex digest | No credential in the URL; an optional header can be read from a file |
| `kreivo-preimage` | Kreivo's preimage pallet, read at a finalized block | The open-source release's code versions are published there. The storage key is the digest and the length. 4 MiB per preimage; larger artefacts are chunked behind a chunk manifest |

### Custom sources: a configured command

`exec` runs a program of your choice with the hex digest and the length as arguments:

- exit 0 and write the bytes to standard output: found;
- exit 3: this source does not hold it;
- any other exit, or the timeout: a failure; the next source is tried.

The command runs outside the process that holds the book, so it can be sandboxed and needs no shared ABI.

### Custom sources: dynamic-library plugins

A plugin is a shared library with a **C ABI** (Rust's own ABI is not stable). It exports:

- `aledgent_code_source_abi() -> u32`: the ABI version, currently `1`;
- `aledgent_code_source_open(config, config_len, err) -> handle`: `config` is the entry's `config`, as JSON;
- `aledgent_code_source_fetch(handle, digest, len, out) -> i32`: returns `0` for found, `3` for not held
  here, and any other value for a failure;
- `aledgent_code_source_free(buf)`: frees a buffer the plugin returned;
- `aledgent_code_source_close(handle)`.

The `aledgent-code-source-plugin` crate wraps any `CodeSource` implementation in these symbols.

- **Versioning.** A later ABI adds suffixed symbols (`…_fetch_v2`) and never changes existing ones. A node
  refuses a plugin whose ABI version it does not know.
- **Loading.** The node refuses a library whose `blake2b-256` differs from the configured `digest`, or that is
  writable by anyone but its owner. A plugin that fails to load stops the node at start.
- **Isolation.** By default (`isolation = "helper"`) a separate `aledgent-code-fetch` process loads the plugin,
  so foreign code never runs inside the process that holds the book, and a stuck plugin can be killed.
  `in-process` is an explicit opt-in.

Whatever the source (built-in, command or plugin), the digest check is mandatory, and no API skips it.

Two examples the amendment evaluates:

- **IPFS.** A CID is not the digest aledgent uses, so the command needs an index from digest to CID (the
  manifest may carry CIDs as untrusted hints). Availability depends on pinning. A good extra source; keep an
  archive of record elsewhere.
- **Kreivo preimages** (built in, above). The preimage pallet keys by `blake2_256` of the raw bytes and
  their length, the same digest, so no index is needed. Limits:
  - 4 MiB per preimage. The core guest is built at opt-level `s` to fit;
  - a deposit per byte;
  - a preimage can be unnoted by whoever noted it unless something requests it;
  - the code becomes public.

  A public, verifiable mirror for open-source releases; keep an archive of record as well.

### Retention

Kept for as long as the book exists (on object storage, under a legal hold, which has no expiry): the genesis chainspec, every writer's inputs and replies, every code
version in the code map, every anchored checkpoint, and the audit trail. Everything else (other snapshots,
replicas, indexes, proofs and witnesses, caches) may be pruned once it can be re-derived. No proof is needed to
bring a node up to date.

## Node roles

| Substrate term | aledgent | Not |
|---|---|---|
| Collator | A host authoring a writer's inputs | A party competing to produce blocks |
| One collator per slot | Exactly one author per shard at a time | Leader election among strangers |
| Validators | A Shared Agent's voters | A Byzantine quorum |
| Relay-chain finality | An anchor final on the settlement layer | The book's own finality, which is at commit |
| XCM validation | Each message between Agents checked against its sender's commit | A boundary between organisations |
| Warp sync | Resync from the latest anchor-verified snapshot plus the inputs since | Trusting the serving peer |
| Archive node | The archive (object storage), not a host | A host holding the full history |
| Bootnode | Node 0's rendezvous | A trusted entry point |

- **Single core.** One host authors the only writer, is its own bootnode, and streams every decision to the
  archive.
- **Multi-host.** Each host is a pruned collator for its assigned shards: it warp-syncs only those shards,
  authors them, and keeps no other shard's state or any full history. **Node 0** runs the principals Agent,
  the router and the coordinator, is a pruned collator for the principals Agent, and is the bootnode.
- **Hosts are trusted for liveness, never for correctness.** Correctness rests on the anchors, the archive
  (chainspec, inputs, code history), the message checks between Agents, and the voters' replication.

### Bringing a shard up to date (per-shard warp sync)

1. Read the latest final anchor from the settlement layer.
2. Fetch that anchor's cut and the shard's snapshot at it from the archive.
3. Check the snapshot's roots, code version, construction and stream heads against the shard's leaf, and the
   leaf's proof against the anchor; rebuild the shard's trees from the snapshot and check its root.
4. Fetch every code version the shard's entries since then name, through the code sources.
5. Replay the shard's inputs since the snapshot, each range under its code, each entry checked.
6. Resume the streams from their heads and, if assigned, start authoring.

A **full resync** starts from the chainspec and replays every input under the code map instead. Both reach the
same roots.
