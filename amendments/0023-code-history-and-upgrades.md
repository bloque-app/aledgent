<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0023 — code history and upgrades

**Status:** **ruled by the owner on 2026-10-08** (`code-history-amendment`, answers `a0023-q1` to
`a0023-q9`, recorded in §"Rulings"): R1–R5 as drafted, plus R6 (the upgrade-authority mode, `a0023-q2`).
**Applied** to `SPEC.md` **v0.4.16**, after amendment 0022 (v0.4.15, PR #199), following amendment 0013's
practice:
- R1–R6 are in `SPEC.md`, and `PLAN.md` carries `DEC-26` and `F-037` (`features/F-037-code-history/tasks.md`).
- `US-K7`'s acceptance criteria are registered as `Case::pending` (`crates/conformance/src/cases/f037.rs`),
  as `f028.rs` does for `AC-A9.*`.
- It does not wait for `F-037` to be implemented.

The text below is the text applied. It adds no error condition (`REQ-ER-1`).
**Target:** `SPEC.md` v0.4.15 (amendments 0021 and 0022 applied); it takes the spec to **v0.4.16**.
**Date:** 2026-10-08.
**Plan:** adds `DEC-26` and feature `F-037` (§"Carried to the plan"). Touches `DEC-4`, `DEC-22`,
`DEC-23`, `DEC-25`, `F-005`, `F-017`, `F-021`, `F-036`.
**Rationale in one line:** a book that a third party can replay (`REQ-RT-3`) must say which code ran each
input, keep that code for as long as the book exists, and change it only by an input of its own; today it
does none of the three.

---

## What the code does today (verified on `main` at `a3cb28b`)

1. **No code identity is recorded in the book.** The core Agent's hot row holds the layout, the six
   sub-roots, the counters, the log head and the bounds, and nothing about code
   (`crates/core-binding/src/hot.rs:100-140`). A ring entry holds the input number, the calls, the
   deliveries, the roots and the previous log hash (`hot.rs:180-195`). The archive's restore point, segment
   header and checkpoint header (`crates/core-archive/src/record.rs:94-170`) and the restore plan
   (`crates/core-archive/src/fetch.rs:266-280`) name the book, digests and roots, never a program. Neither
   does a checkpoint (`crates/ledger-core/src/anchor/mod.rs:130-155`) or a shard's anchor leaf
   (`crates/shards/src/anchor.rs:26-50`).
   - The one pin is VOS's, not the book's: the installed actor's `ProgramId` in the Agent's runtime state
     (`agents/core-host/src/driver.rs:524-528`), which `attach` checks on a restart and refuses on a
     mismatch (`driver.rs:592-599`). It lives in `agent.state`, a runtime snapshot; an Agent founded afresh
     (`agents/core-host/src/serve.rs:327-329`) installs whatever `--actor` it is given.
   - The runtime program (`--runtime`) is not compared with the saved state: `Runtime::create` builds a
     descriptor from the bytes given (`driver.rs:198-258`) and `restore` swaps the saved state in
     (`driver.rs:306-311`). Whether VOS refuses the mismatch later was not verified.
   - The kernel (CipherClerk) has no identity of its own at run time. It is linked into the guest package
     and into the host binary, and is named only by `Cargo.lock`.
2. **Restore replays every archived input under one build.** `roll_forward_agent`
   (`crates/core-binding/src/core.rs:406-502`) founds or restores the Agent, then re-sends every archived
   entry through `Driver::roll_forward` (`crates/core-binding/src/host.rs:1536-1580`), which applies the
   archived calls natively with the host binary's own kernel and checks each entry's log hash. `restore`
   takes one `--runtime` and one `--actor` for the whole history (`serve.rs:569-591`). The check is sound:
   a history the current build does not reproduce fails with "does not reproduce its archived entry"
   instead of diverging silently. But it means that **any upgrade that changes a transition makes every
   earlier history unrestorable** with the new binaries, and the old binaries are kept nowhere.
3. **Checkpoints and anchors bind the construction only through the build.** The commitment is
   `H("aledgent/anchor/v2" ‖ construction ‖ instance ‖ number ‖ entry_sequence ‖ kernel_root)`
   (`anchor/mod.rs:9`), with `construction()` a compile-time constant (`anchor/mod.rs:62-68`). A
   `Checkpoint` carries no construction field, and `Checkpoint::consistent` recomputes with the current
   build's constant (`anchor/mod.rs:179-187`), so after a change of construction every earlier checkpoint
   would read as inconsistent. The disclosure envelope does carry the construction
   (`crates/verify/src/envelope.rs:193-195`), and the verifier knows both (`crates/verify/src/lib.rs:75-84`),
   which is what `REQ-PR-10` asks for. Neither a checkpoint nor an anchor names the code.
4. **The construction is fixed by a Cargo feature.** `smt-v1` selects `compact_smt::Version::ACTIVE`
   (`crates/compact-smt/src/lib.rs:49-53`), propagated through every crate's manifest
   (`crates/core-binding/Cargo.toml:32`, `agents/core/Cargo.toml:18`, `agents/core-host/Cargo.toml:18`).
   `DEC-25` adds that "a deployment never mixes the two and never changes construction after its first
   anchor". The settlement contract stores each anchor's 32 bytes opaquely
   (`contracts/settlement/src/lib.rs:798-815`): it accepts any construction and cannot tell one from another.
5. **There is no upgrade path.**
   - `hot.rs:33` says a change of layout is "an upgrade through `F-014`", which is export and re-import
     (`REQ-RT-8`, `REQ-RC-3`), still unproven on a large book (`features/F-014-export-import-opening/plan.md:153`).
   - The halt after `FAILED_ATTEMPTS` tells the operator to restart "with a build whose Agent applies it"
     (`core.rs:504-517`): a code change that nothing records, and that `attach` would refuse anyway.
   - VOS has a registry upgrade with a compare-and-swap on the old program hash and a revision
     (VOS `c54fc1ff`, `vos/src/registry.rs:230-245`, `:1043-1100`). The core host's Local Agent does not use
     it.
   - The settlement contract cannot change its own code (amendment 0022's facts, `lib.rs:211-486`).
6. **No gate replays an older history.** The restore drill (`.github/workflows/ci.yml:100-120`) restores a
   history the same build has just made.
7. **Retention is 400 days, not forever.** Archive objects carry Object Lock in compliance mode for 400
   days (`crates/core-archive/src/archive.rs:108-111`). No lifecycle rule deletes them, but nothing keeps
   them either. There is no code archive.
8. **A checkpoint does not shorten the native replay.** An archive checkpoint is the Agent's runtime state
   (`record.rs:14-17`). The replica's leaf store is rebuilt from the archive's whole prefix
   (`Driver::recover_in`, called at `core.rs:441-451`), so a "fast" restore still applies every entry since
   genesis natively.
9. **The sharded host has no off-node archive** (`agents/core-host/src/sharded.rs`), and there are no
   per-shard checkpoints.
10. **The chainspec names no code.** The seed header in PR #199 (`crates/ledger-core/src/chainspec.rs`,
    `SpecHeader`) holds the version, the genesis operation, the time, assets, counts, the opening and the
    history commitment. Substrate's chainspec carries the genesis runtime as `:code`; aledgent's does not.
11. **The build profile changes the code.** The core program `core.vos` is 4,545,236–4,545,244 B at
    opt-level 3 (PR #200), above VOS's 4 MiB prepared-program cache
    (`MAX_CACHED_INNER_PROGRAM_BYTES`, `pvm/runtime/src/inner.rs:29` at `c54fc1ff`): the host re-prepares it
    on every invocation, about 83 % of an invocation's time. At opt-level `s` it is 2,880,661 B and costs
    24 % more actor gas. Same source, different bytes, different gas: a different program.

## What does not change

- `REQ-RT-3`'s promise, `REQ-RT-8` (an upgrade preserves the book), `NFR-17` (no window after the first),
  `NFR-21` (ephemeral nodes), `REQ-PR-1` to `REQ-PR-10`, `INV-17`, `INV-19`, `INV-25`.
- An anchor still reveals nothing (`REQ-PR-2`). A code digest is a version, not book content.
- No requirement names a hash function, a storage product or a runtime.

---

## R1 — glossary (§2), three rows added

> | **Code version** | The identity of everything that decides a writer's transitions: a digest of each executable artefact, together with the settings it was built with. Two builds of the same source with different settings are two code versions. |
> | **Code map** | For each of the book's writers, the ranges of its inputs and the code version that executed each range, derived from the book's own inputs (`REQ-RT-16`). |
> | **Code archive** | Where every code version that has executed an input is kept, by its digest (`REQ-RT-17`). |

## R2 — `REQ-RT-15`, `REQ-RT-16`, `REQ-RT-17` (new, §4.4, after `REQ-RT-14`)

> **`REQ-RT-15`** **The active code is book state.** Each of the book's writers (`REQ-RT-13`) MUST hold, in
> its committed state, the code version that executes its next input, and the construction of its state
> commitment (`REQ-PR-10`). Every checkpoint MUST carry both, and its anchor MUST bind both, so that a
> recipient of an anchor knows which code produced the anchored state as well as how it is committed.

> **`REQ-RT-16`** **An upgrade is an input.** The code version of a writer MUST change only through an input
> in that writer's own sequence: an **upgrade**, which names the new code version by its digest and the
> input number from which it applies.
>
> - It is an operator intervention (`REQ-AT-3`) with its own authority (`REQ-OS-1`), attributed, audited and
>   visible as `REQ-OS-4` requires. It is authorised under the upgrade-authority mode in force when it is
>   decided (`REQ-RT-18`, added by R6 under ruling `a0023-q2`).
> - It applies forward only. No input before the named number is executed again, re-interpreted or
>   re-decided; every outcome already given stands.
> - It MUST be refused, with nothing changed, unless the new code version's artefacts are retrievable and
>   match their digests at the time it is taken.
> - A change of build settings alone is an upgrade (R1).
> - It is made online (`NFR-17`), and it preserves the book in place (`REQ-RT-8`), without an export.
> - Where an upgrade changes what the writers exchange (`REQ-RT-13`), each receiver MUST accept the new form
>   before any sender emits it.

> **`REQ-RT-17`** **Code history.** Every code version that has executed an input of the book MUST be kept,
> retrievable by its digest, for as long as the book exists (`NFR-22`). The code map MUST be derivable from
> the book's inputs alone. Replay (`REQ-RT-3`) and every restore MUST execute each input under the code
> version the code map names for it. Code MAY be retrieved from any source, trusted or not: its bytes MUST
> be checked against the digest before they are used, and bytes that do not match MUST NOT be used.

`REQ-RT-3` gains one sentence: "The inputs include the book's genesis and its upgrades; replay executes each
input under the code that executed it (`REQ-RT-17`)."

## R3 — `REQ-PR-11` (new, §5.10, after `REQ-PR-10`)

> **`REQ-PR-11`** **A change of construction is a migration.** The construction of the state commitment MAY
> change only through an input of its own, made by a code version that knows both constructions. The
> migration MUST be deterministic: the same state in the old construction always gives the same state in the
> new. It changes no leaf, no balance and no identifier. The checkpoint at the migration's input MUST carry
> the root in both constructions, and its anchor MUST bind both, so that the earlier anchors and the later
> ones are linked by one anchored fact. Every checkpoint before it names the old construction, every one
> after it the new.

`DEC-25`'s "never changes construction after its first anchor" becomes "changes construction only by a
migration (`REQ-PR-11`)".

## R4 — `NFR-22` (new, §11): what is kept forever

> **`NFR-22`** **Retention of what replay needs.** For as long as the book exists, aledgent MUST keep: its
> genesis (and any seed it committed, `REQ-RC-6`); every input of every writer, in order, with its reply;
> every code version in the code map (`REQ-RT-17`); every checkpoint an anchor bound, with the anchor's
> record (`REQ-PR-8`); and the audit trail (`NFR-6`). Everything else MAY be pruned once it is re-derivable
> from these: snapshots of a writer's state other than the latest one an anchor verified, indexes, the
> projections a node keeps to answer reads, proofs and witnesses. No proof is needed to bring a node up to
> date: state is re-derived from inputs, or taken from a snapshot and checked against an anchor.

## R5 — Epic K: `US-K7` (new)

> **`US-K7`** — As the **operator**, I upgrade aledgent's code online, and anyone I give the book's archive
> can still replay all of it, under the code that ran each part.
> - `AC-K7.1` Given an upgrade naming code version *H* from input *n*, When it is taken, Then every input
>   before *n* keeps its outcome, every input from *n* runs under *H*, and the next checkpoint and its
>   anchor name *H* (`REQ-RT-15`, `REQ-RT-16`).
> - `AC-K7.2` Given an upgrade naming a version whose artefacts no source returns, or returns with another
>   digest, Then it is refused and the code does not change (`REQ-RT-16`, `REQ-RT-17`).
> - `AC-K7.3` Given an archived history that spans upgrades, When it is replayed from genesis under its code
>   map, Then every writer reaches, at every input, the roots and the log it archived (`REQ-RT-3`,
>   `REQ-RT-17`).
> - `AC-K7.4` Given a migration of construction at input *n*, Then checkpoints before *n* name the old
>   construction and after it the new, the checkpoint at *n* carries both roots, and a proof against any
>   anchor of either side checks (`REQ-PR-10`, `REQ-PR-11`).
> - `AC-K7.5` Given a new node, When it is brought up to date from the latest anchor-verified snapshot and
>   the inputs since, or from genesis and every input, Then it reaches the same roots (`NFR-21` (b),
>   `REQ-RT-17`).
> - `AC-K7.6` Given two builds of the same source with different build settings, Then they are two code
>   versions with two digests, and moving between them is an upgrade (R1, `REQ-RT-16`).

`REQ-OS-2` gains: "upgrade the code of a writer, and migrate the construction of the state commitment
(`REQ-RT-16`, `REQ-PR-11`)". §15: Epic K becomes `US-K1`–`US-K7`, adds `REQ-RT-15`–`REQ-RT-17`, `NFR-22`;
Epic J adds `REQ-PR-11`.

> **Trade-off, stated plainly.** An upgrade is the most powerful thing the operator can do: new code can
> decide differently from the old, including about value. This amendment does not prevent that; it makes it
> recorded, attributable and replayable, so that a third party holding the archive can see exactly which
> code decided which input and check it. Guardrail 7 (no administrative path that moves value) still holds
> for every input, because every input is decided by code the book names, but the review of that code is
> the operator's, until the deployment is federated or the code is published and reproducibly built.

## R6 — `REQ-RT-18`: the upgrade-authority mode (new, after `REQ-RT-17`; ruling `a0023-q2`)

The owner ruled for one operator principal today. The rule that decides who may upgrade must also be fixed
at genesis and changeable over time, with each change validated by the rule in force at that moment. **This
is possible, and the owner's reading is right.** The mode is part of each writer's committed state. A change
of mode is an upgrade-class input, authorised under the mode it replaces. Because a writer orders its inputs
totally and authorisation is a deterministic function of the state and the input, replay reproduces every
decision about authority exactly as it was made (`REQ-RT-3`).

> **`REQ-RT-18`** **The upgrade-authority mode.** Each writer MUST hold, in its committed state, the **mode**
> that decides who may authorise an upgrade (`REQ-RT-16`), a migration of construction (`REQ-PR-11`) and a
> change of mode.
>
> - The book's genesis MUST fix the initial mode (in the seed, where there is one, `REQ-RC-6`).
> - The mode MUST change only through an input of its own in the writer's sequence. That input names the
>   new mode and the input number from which it applies, and it is authorised under the mode in force when
>   it is decided. It applies forward only, like an upgrade.
> - A new mode MUST be refused, with nothing changed, unless it can be satisfied: every principal it names
>   exists and can hold the authority, and every threshold can be met.
> - A mode MUST be one that the writer's current code version can enforce. A kind of mode that the current
>   code does not know needs an upgrade first.
> - A change of mode cancels every upgrade or migration that has been proposed but not yet authorised. An
>   upgrade already authorised and scheduled stands: it was authorised under the mode then in force.
> - Every checkpoint MUST carry the mode in force, and its anchor MUST bind it (`REQ-RT-15`).

Additional acceptance criteria for `US-K7`:

> - `AC-K7.7` Given a book whose genesis fixes the mode "one principal", When that principal changes the
>   mode to "a proposer and a different approver", Then every later upgrade needs both, and the change is in
>   the audit trail and in the next anchor (`REQ-RT-18`).
> - `AC-K7.8` Given a mode change that is not authorised under the mode in force, or that names a principal
>   who does not exist, Then it is refused and the mode does not change (`REQ-RT-18`).

**Limits, stated plainly.**
- A mode is only as strong as the code that enforces it. An upgrade authorised under the current mode can
  install code that interprets modes differently from then on. The book records it, and replay shows it,
  but nothing stops it. This is the same trade-off as R5's.
- The mode is held per writer, not in the registry. Authority over a writer's code is then checked in that
  writer's own order, with no feed lag (`NFR-20`). The cost is that a deployment-wide change of mode is one
  input per writer, and the writers may differ until each has applied it.
- The mode names principals; their credentials and revocations still come from the registry's feed. A
  revoked approver is refused within `NFR-20`'s bound, not at once.
- A lost key under "one principal" blocks upgrades until that principal's credentials are recovered
  (`REQ-AU-15`). There is no bypass, by design (guardrail 7).

`REQ-OS-2` also gains "change the upgrade-authority mode (`REQ-RT-18`)". Epic K adds `REQ-RT-18`.

---

## The plan's binding (proposed `DEC-26`)

Everything below is plan-level and names technology. Where the owner ruled on 2026-10-08, the text says so
and names the ruling. The rest is proposed, to be written into `PLAN.md` as `DEC-26`.

### (a) `:code` in hot state

Like Substrate's `:code` storage key, each writer's committed state carries its code version:

- **Artefact digest:** `blake2b-256(bytes)`, unkeyed, over the artefact's exact bytes. It is the hash JAM's
  `lookup` uses for preimages and Substrate's preimage pallet uses for its keys, so a Kreivo preimage or a
  JAM preimage of the same bytes has the same key (see "Retrieval"). It is not VOS's blob address
  (`vos_blob_hash`, keyed `vos/blob`, `host.rs:2019`) or its `ProgramId`; the manifest records those too.
- **The manifest** (the code version) is a canonical encoding that starts with the magic `aledgent/code/v1`
  and lists, for one writer kind (core, shard, principals, settlement, anchor):
  - each artefact that decides a transition: `role` (`package`, `runtime`, and `native-decide` wherever a
    host decides natively, as `F-034`'s experiment does), `digest`, `len`, VOS's `ProgramId`;
  - each artefact's **build profile**: target, toolchain, `opt-level`, Cargo features (`heap-4m`, `smt-v1`,
    the runtime's JAM profile), source revision and the digest of `Cargo.lock`;
  - the kernel's identity: CipherClerk's revision and crate checksum. Informative: the kernel is compiled
    into the package and the host, and the package's digest is what binds it;
  - the constructions it can read and the message formats it accepts and emits (`REQ-RT-13`).
  The code version *H* is `blake2b-256(manifest bytes)`. It is fetched like any artefact.
- **The hot row** (`hot.rs:100`) gains:
  - `code: [u8; 32]`;
  - `construction: u8`;
  - `upgrade: Option<(H, n)>`;
  - `authority: [u8; 32]`, the digest of the mode in force (R6, below);
  - `authority_next: Option<(digest, n)>`.

  The mode itself is a record in the service tree, and the hot row holds only its digest. All of this is
  about 150 bytes, still bounded (`DEC-4`). Each ring entry gains `code`, the version that
  executed it (32 bytes on about 250, +13 %), so that every archived entry names its code and the code map is
  read off the archive without decoding the inputs. The log hash covers it.
- **Checkpoints** (`anchor/mod.rs:130`) gain `construction` and `code` as fields, and `consistent` uses the
  checkpoint's own construction, not the build's. **One `v3` for both amendments** (ruling `a0023-q3`): the
  commitment binds the book's genesis digest (amendment 0022) and the code version, construction and
  authority mode (this one):
  `H("aledgent/anchor/v3" ‖ construction ‖ genesis ‖ code ‖ authority ‖ instance ‖ number ‖ entry_sequence ‖ kernel_root)`.
  The anchored value is still 32 bytes (ruling `a0023-q4`, (e)). The shard leaf becomes
  `aledgent/anchor/shard/v3`, with each shard's `code`, `construction` and `authority`
  (`crates/shards/src/anchor.rs:5-8`).
- **Genesis:** the chainspec's header names the genesis code version *H₀*, the construction and the initial
  upgrade-authority mode (R6), as Substrate's genesis carries `:code`. *H₀* is built with the core at
  opt-level `s` (ruling `a0023-q9`, handled separately from this amendment). At opt-level `s`, `core.vos` is
  2.88 MB, so it fits VOS's 4 MiB prepared-program cache and one Kreivo preimage. The core host refuses to found a book whose `--actor` and `--runtime`
  are not *H₀*'s. This is a field in the seed format PR #199 is changing to version 2 (the settlement base):
  proposed to land in the same version, by the owner of #199 (read-only coordination here).

### (b) The upgrade, as a logged operator input

1. The operator builds and publishes *H*'s artefacts and manifest to the code archive (c), then submits
   `Upgrade { code: H, from: n }` to the writer through `CTR-OPS`, with its justification.
2. Ingress and the author host check that the configured sources return *H*'s manifest and every artefact,
   each matching its digest; otherwise the input is refused before it reaches the writer.
3. The writer, under its current code, commits `upgrade = Some((H, n))`, with `n` greater than this input's
   own number (at most one pending; a later `Upgrade` before `n` replaces it; `n` may be the next input).
4. Before sending input `n`, the host switches the Agent's program: VOS's registry upgrade, a compare-and-swap
   on the old program hash and the installation's revision (`registry.rs:1043-1100`), or, for the runtime
   program, the management operation under the runtime producer's key (`PLAN.md` `DEC-23`'s cost). The host
   first checks the package's bytes against the manifest.
5. Input `n` runs under *H*. Its first act is to move `upgrade` into `code`. Its ring entry names *H*.
6. Every input before `n` was decided under the old code and is never sent again. A host that crashes
   between steps 4 and 5 resumes from the Agent's committed state, which says whether `n` committed.

Without a way to accept an upgrade in the genesis code, there is no first upgrade: the handler must be in
*H₀*. A first deployment can carry it in its genesis code (amendment 0022, PR #199).

### (b′) The upgrade-authority mode (R6, ruling `a0023-q2`)

- **Modes**, a closed enum in a canonical encoding. Its digest is `authority` in the hot row.
  - `Single { principal }`: one operator principal holding the upgrade authority. This is the genesis mode
    the owner ruled for today.
  - `ProposeApprove { proposers, approvers }`: one principal proposes and a different one approves, each
    holding its own authority, as reversals require (`REQ-OS-3`).
  - `Threshold { members, k }`: *k* distinct members out of *n*.
  - Each of these may carry an optional `delay`: the least number of inputs between authorisation and the
    input number an upgrade applies from. The delay is counted in inputs, because time comes in only as an
    input (`REQ-RT-7`).

  A kind that the current code version does not know needs an upgrade first. The manifest lists the kinds
  each code version enforces.
- **Inputs:**
  - `ProposeUpgrade`, `ApproveUpgrade` and `ProposeMigration`, or a single `Upgrade` under `Single`.
  - `SetAuthority { mode, from }`, which is authorised under the mode in force when it is decided. Under
    `Single`, that principal alone. Under `ProposeApprove`, a proposer and then an approver.
  - All of them are operator interventions on `CTR-OPS`, audited and with a justification.
- **Validation** of a new mode is deterministic and done by the writer:
  - every principal it names exists in the registry version the writer has applied;
  - every set is non-empty;
  - *k* is at most *n*;
  - the mode's kind is in the current manifest.
- **Ordering:**
  - `SetAuthority` cancels every pending proposal (R6).
  - An upgrade already scheduled (`upgrade = Some(..)`) stands.
  - A `SetAuthority` and an `Upgrade` never share an input.
- **Deployment-wide:** one `SetAuthority` per writer, like an upgrade. The operator's tooling submits them
  together, and the anchor shows each writer's mode.

### (c) The code archive and the code map

- **One operator's setting:** an S3 bucket with Object Lock, one object per digest (`code/<hex digest>`),
  written once by the release pipeline and read by every node.
- **"Kept forever" is an S3 legal hold** (ruling `a0023-q5`, `NFR-22`). A legal hold has no expiry.
  - It is on every code object, every input and reply segment, every anchored checkpoint and the chainspec.
  - It sits on top of today's 400-day compliance lock (`archive.rs:108-111`), which stays as a floor nobody
    can shorten.
- *Stated plainly:* unlike compliance mode, a legal hold can be removed by anyone holding
  `s3:PutObjectLegalHold`. That permission belongs to a break-glass role only, and every use of it is
  alerted on. No workload role has it: not the release pipeline, and not any node.

- **The code map** is read off the book: each writer's ring entries name their code (a), and its upgrades
  name their ranges. The archive also keeps `code-map/<writer>` as a convenience copy, which a restore checks
  against the entries and never trusts alone.

### Retrieval: a pluggable `CodeSource` (owner addition, 2026-10-08)

Only retrieval is pluggable. The code map stays in the book, and what is fetched is always checked.

```rust
/// One place code can come from. Untrusted: whatever it returns is checked by the caller.
pub trait CodeSource: Send + Sync {
    /// A name for logs and metrics (no credential).
    fn name(&self) -> &str;
    /// The bytes whose `blake2b-256` is `digest`, `len` long; `Ok(None)` if this source does not hold them.
    fn fetch(&self, digest: &[u8; 32], len: u64) -> Result<Option<Vec<u8>>, SourceError>;
}
```

- **The check is the fetcher's, not the source's.** For each source in order, the node fetches; bytes whose
  length or `blake2b-256` differ are discarded, counted (`aledgent_code_source_mismatch_total`), logged and
  never used, and the next source is tried. The first match is written to a local content-addressed cache,
  which is always tried first. This is JAM's `lookup` model: the program receives bytes the host has
  verified by their hash, so the fetcher can be anything. If no source returns the bytes, the node does not
  run the writer past the range that needs them, and says so: a liveness fault of that node, never a
  correctness one.
- **Built-in sources:**
  - `s3://bucket/prefix?region=…`: the archive's own S3 client (`crates/core-archive/src/s3.rs`), read-only.
    One operator's setting.
  - `fs:/path`: a local or mounted directory (an EFS or NFS mount, a USB copy for an offline audit).
  - `https://host/path/{digest}`: any HTTP(S) server or CDN. No credential in the URL (`REQ-AU-8`); an
    optional header is read from a file.
- **Custom sources, as a configured command.** `exec` runs a program with the hex digest and length as
  arguments; it writes the bytes to standard output and exits 0, exits 3 for "not here", anything else for a
  failure; with a timeout and a size cap. A command runs outside the process that holds the book, can be
  sandboxed, and needs no ABI.
- **Custom sources, as dynamic-library plugins** (ruling `a0023-q7`: plugins are allowed as well as
  commands). Specified in "Plugins" below.
- **Verification is mandatory, whatever the source.** Built-in, command and plugin sources all go through
  the same fetcher, which checks each artefact's length and `blake2b-256`. No source, configuration or plugin
  call can mark bytes as verified or skip the check. The trait's caller does the check, and a plugin never
  sees the cache.
- **Evaluated: IPFS.** Works as `exec` (`ipfs cat <cid>`) or as an `https` gateway. A CID is not our digest
  (a multihash over a UnixFS DAG), so the source needs an index from digest to CID: kept as a hint in the
  manifest (`locators`) or beside it, untrusted, since the bytes are checked anyway. Availability depends on
  someone pinning it. Good as an open-source operator's extra source; never the archive of record.
- **Evaluated: Kreivo preimages.** The preimage pallet keys by `blake2_256` of the raw bytes and the length
  (`PreimageFor((hash, len))`), the same digest as ours, so no index is needed, and reading it needs only a
  finalized storage read (a light client is enough). Against it: the pallet's `MAX_SIZE` is 4 MiB
  (`pallet-preimage` `src/lib.rs:100`), so today's opt-level 3 `core.vos` (4.45–4.55 MB) does not fit as one
  preimage, while opt `s` (2.88 MB) does; a deposit is held per byte (Kreivo
  `runtime/kreivo/src/config/utilities.rs:99-117`); a preimage nobody requested can be unnoted by its
  depositor, so it is not kept for ever on its own; and it makes the code public, which suits the open-source
  release and not a private build. Useful as a public, verifiable mirror; not the archive of record.
  **Ruled `a0023-q8`: yes, for the open-source release.**
  - The release pipeline notes each published code version's manifest and artefacts as preimages on Kreivo.
  - The open-source distribution ships a built-in `kreivo-preimage` source: a finalized storage read of
    `Preimage::PreimageFor((digest, len))` through a light client. The code map's `len` is what makes the
    storage key computable.
  - With the core at opt-level `s` (`a0023-q9`), the core fits one preimage. An artefact over 4 MiB (the
    runtime program, if it ever grows past that) is split into chunks named by a chunk manifest, and each
    chunk is checked like any artefact.
  - An operator's private builds are not published this way unless the operator chooses to.
- **Configuration**, in the node's configuration file, an ordered list with fallback:

```toml
[code]
cache = "/var/lib/aledgent/code"          # content-addressed; always tried first
sources = [
  { kind = "fs",    path = "/mnt/aledgent-code" },
  { kind = "s3",    uri  = "s3://example-aledgent-code/prod?region=<region>" },
  { kind = "https", url  = "https://code.example.org/aledgent/{digest}" },
  { kind = "exec",  command = ["/usr/local/bin/aledgent-code-ipfs", "{digest}", "{len}"], timeout_secs = 60 },
  { kind = "plugin", path = "/usr/lib/aledgent/sources/libmysource.so",
    digest = "<blake2b-256 of the library, hex>", isolation = "helper", config = { endpoint = "…" } },
]
```

### Plugins (ruling `a0023-q7`)

- **ABI: C, versioned.** A plugin is a shared library (`.so`, `.dylib`) that exports `extern "C"` symbols
  only. Rust's own ABI is not stable, so it is never used across this boundary.
  - `aledgent_code_source_abi() -> u32` returns the ABI version, which is `1` for now.
  - `aledgent_code_source_open(config: *const u8, config_len: usize, err: *mut Buf) -> *mut Handle` takes
    the entry's `config` as JSON.
  - `aledgent_code_source_fetch(h: *mut Handle, digest: *const [u8; 32], len: u64, out: *mut Buf) -> i32`
    returns `0` for found, `3` for not held here, and any other value for a failure, with a message in `out`.
  - `aledgent_code_source_free(buf: *mut Buf)` frees a buffer the plugin allocated. The plugin owns every
    buffer it returns, and the host copies the bytes out before freeing.
  - `aledgent_code_source_close(h: *mut Handle)`.

  A header (`aledgent_code_source.h`) and a Rust crate (`aledgent-code-source-plugin`) provide these. The
  crate has a macro that exports any `CodeSource` implementation behind them.
- **Versioning:**
  - The node refuses a plugin whose ABI version it does not support.
  - A later ABI adds symbols with a version suffix (`…_fetch_v2`) and never changes an existing one.
  - A plugin may export several versions, and the node uses the highest one both sides know.
- **Loading:**
  - Plugins are loaded at start, from the configured `path`.
  - The node refuses a library whose `blake2b-256` is not the configured `digest`: the plugin's own bytes are
    pinned like code. It also refuses a library that is writable by anyone but its owner.
  - A plugin that fails to load stops the node at start, because a misconfiguration should be caught early.
    A plugin that fails to fetch at run time is skipped like any other source.
- **Isolation:**
  - The default is `helper`: the plugin is loaded by a separate process, `aledgent-code-fetch`, which the node
    speaks to by the `exec` protocol. Code that is not the book's then never runs in the process that holds
    the book, and a hung or crashed plugin is killed and restarted.
  - `in-process` is opt-in. It saves a process, but a plugin's crash is then the node's crash, and a
    timeout cannot stop a stuck call.
- **The plugin is never trusted for correctness.** Whatever it returns goes through the mandatory digest
  check above. A plugin can make a node unavailable. It cannot make a node run wrong code.

### (d) Restore and replay choose the code per range

`roll_forward_agent` (`core.rs:406-502`) splits the archive at the code map's boundaries. It founds the Agent
under *H₀* (or restores a snapshot under the snapshot's own `code`), and for each range: fetches the range's
version through the sources, switches the Agent's program exactly as the live upgrade did (b, step 4), and
rolls the range forward with today's per-entry check. The restore plan (`fetch.rs:266`) records the map it
used.

The native side matters too: `Driver::roll_forward` applies archived calls with the host's own kernel
(`host.rs:1536-1580`). Either the host applies every older call format exactly as the code that wrote it did,
a compatibility rule the gate in (f) proves on every change; or the manifest names a native host build per
range and the restore runs each range with it. **Ruled `a0023-q6`: the first, with the second as the
fallback.**

The owner accepts that this **freezes the host spec**:
- The call format (`ledger_core::call::Call`) and its native application are versioned.
- A change only ever adds a new call version. An existing one is never changed in place.
- Every host applies every earlier version exactly as the code that wrote it did. The `code-history` gate,
  (f), proves this on every change.

A change that cannot keep this rule names a native host build in the manifest for its range, and restores
run that range with that build. This is the fallback, and it needs the owner's approval each time.

### (e) A construction migration (v1 → v2 style)

- An input `MigrateConstruction { to }`, under a code version whose manifest reads both constructions.
- The leaves, the key space and the leaf hashes do not change (`DEC-25` keeps CipherClerk's), so the
  migration recomputes inner nodes only, which is deterministic. The host computes the new roots natively from
  the leaf store; the Agent's transition takes them on the same terms as amendment 0022's `StateGenesis`: on
  the host's word, checked by every host that replays it and by a second execution off the critical path
  (`verify-genesis`'s pattern).
- The checkpoint at the migration's input carries both roots: one **bridge anchor** whose commitment binds
  `(old construction, old root, new construction, new root)` at one entry sequence. Every earlier anchor stays
  checkable in its construction (`REQ-PR-10`; the verifier already knows both).
- **The settlement contract** today accepts any 32 bytes and records nothing about the construction, which is
  acceptance per anchor by default. Recording it publicly (a construction byte beside each anchor, refused if
  lower than the last) needs a new contract version, and the live contract cannot be upgraded: a new instance
  and a community vote (amendment 0022's facts). **Ruled `a0023-q4`: (a), the construction stays inside the
  32 bytes.**
  - The contract is unchanged.
  - Verifiers read the construction from the checkpoint and the disclosure envelope, and the `v3` commitment
    binds it.
  - The bridge anchor is an ordinary anchor: a number and 32 bytes, whose commitment binds both roots.
  - Why this matters for `pallet-spaces`: an anchor that is only a number and an opaque 32-byte value moves
    to any venue unchanged. Neither the contract's storage nor a future pallet's needs a field for
    constructions, codes or modes. See "Toward `pallet-spaces`".

### (f) The CI gate

A job, `code-history`, on every change that touches a guest, the runtime, the kernel or the call format:

- replay an archived history from genesis under its code map, with sources `fs:` (a fixture archive kept with
  its code) and the cache, and require the archived log hash and roots at every input, and the archived
  commitment at every checkpoint;
- the fixture is a small book recorded by every release and kept in the code archive's test prefix, so each
  release adds a range; on each release, also replay a rehearsal deployment's archive read-only;
- for a change that alters a code version, extend the fixture across a new boundary (old code up to *n*, an
  upgrade, new code after) and replay it whole.
Not on a laptop: it runs on the CI and bench runners.

### (g) Fast and full resync

- **Fast (warp) resync:** the latest **anchor-verified snapshot** of a writer (its state at a checkpoint whose
  roots, code and construction match a final anchor, `REQ-PR-8`), then the inputs since, under the code map's
  tail. Today's checkpoint is only the Agent's runtime state (item 8 above), so a snapshot must also carry the
  leaf store, as content-addressed chunks: the node rebuilds the trees from them and checks the roots against
  the checkpoint and the anchor. No proof is transferred.
- **Full resync:** the chainspec, then every input under the code map. It is what an auditor runs, and what
  the gate in (f) runs.
- Both must reach the same roots (`AC-K7.5`).

### (h) Retention

| Kept for as long as the book exists | Pruned once re-derivable |
|---|---|
| The chainspec and the genesis manifest | Snapshots other than each writer's latest anchor-verified one |
| Every writer's inputs, in order, with their replies (archive segments) | Replicas, leaf stores, indexes, read projections |
| The code archive: every manifest and artefact in the code map | Witnesses and proofs (re-derived from state and inputs when a disclosure needs one) |
| Every anchored checkpoint and its anchor's record | Node-local logs, caches and the code cache |
| The audit trail (`NFR-6`) | |

---

## Node roles, in Substrate terms (owner additions 2 and 3, 2026-10-08)

**Distributed, not decentralised.** aledgent is one organisation's distributed system, inside a single trust
domain: the operator runs every host. The Substrate words below describe mechanics (who authors, how a node catches
up, what finality means), not a trust model. There is no adversarial consensus: a Shared Agent's voters
provide replication and agreement for availability, not Byzantine fault tolerance, and one voter is sound.
What anchors, replay and the code history buy is that **third parties can verify** the book without trusting
the operator: the book is verifiable, not decentralised.

| Substrate term | aledgent mechanic | What it is not |
|---|---|---|
| Collator | A host that authors a writer's inputs (a shard, the principals Agent) | A party competing for blocks |
| Exactly one author per parachain slot | Exactly one author per shard at a time | Leader election among untrusted peers |
| Validators | A Shared Agent's voters | A Byzantine quorum; one voter is sound |
| Relay-chain finality | An anchor final on Kreivo (`REQ-PR-8`) | Finality of the book's own decisions, which are final at commit |
| XCM message validation | R-03: each message between Agents checked against its sender's commit | A trust boundary between organisations |
| `:code`, runtime upgrade | `code` in hot state, `Upgrade` input (a), (b) | Governance by token holders |
| Warp sync | Fast resync from the latest anchor-verified snapshot (g) | A sync that trusts the serving peer |
| Pruned node | A host that keeps only its writers' latest snapshot and the inputs since | — |
| Archive node | The archive itself (S3, (c)), not a host | A host with the full history |
| Bootnode | Node 0's rendezvous (`agents/core-host/src/fleet.rs:6-10`) | A trusted entry point |

**Trust comes from** the Kreivo anchors (relay-chain finality), the archive (the chainspec, every shard's
inputs, the code history), R-03's message checks, and the Shared Agent's voters. **Not** from any host's full
history: no host has one. Hosts are trusted for liveness, never for correctness.

### Single core

One host runs the core Agent: it is the only collator and its own bootnode, and it streams every decision to
the archive (`F-005`, `serve.rs:18-24`). A replacement host is a fresh node that warp-syncs (g).

### Multi-host production

- **Each host** is a warp-synced, pruned collator **for its assigned shards**. It syncs only those shards'
  latest anchor-verified snapshots and the inputs since; it authors those shards; exactly one host authors a
  shard at a time. It keeps no other shard's state and no full history.
- **Node 0** runs the principals Agent, the router (ingress, `DEC-5`) and the coordinator (the anchor's cut
  assembly and the fleet's rendezvous). It too is a pruned collator, for the principals Agent, and it is the
  bootnode.
- **Assignment** of shards to hosts is configuration, held with the directory; moving a shard is: the old
  author stops, the new one warp-syncs that shard and resumes from the stream heads (the relay resends from
  the trim point and the runtime applies nothing twice, `DEC-23`).

### Per-shard warp sync

For shard *s* on a host joining or taking it over:

1. Read the latest **final** anchor from Kreivo (a finalized storage read; a light client is enough).
2. From the archive, read that anchor's cut (its leaves, `crates/shards/src/anchor.rs`) and *s*'s snapshot at
   the cut's sequence for *s*.
3. Check: the snapshot's roots, `code`, `construction` and stream-heads digest equal *s*'s leaf, and the leaf's
   proof reaches the anchor's commitment (`anchor.rs`, `Proof`). Rebuild *s*'s trees from the snapshot's
   leaf chunks and check the root.
4. Read *s*'s code map from the archived entries after the snapshot; fetch each version through the sources,
   checking each digest.
5. Install the snapshot under its `code` and replay *s*'s inputs since, each range under its version, each
   entry checked against its archived log hash.
6. Resume the streams from the heads (credits, the feed, evidence) and, if assigned, start authoring.

This needs, per shard: an off-node archive of its inputs (item 9: none today), snapshots with leaf chunks
(item 8), a code map (a) and a shard leaf that names code and construction (a).

A deployment-wide upgrade is one `Upgrade` per writer, each at its own input number. Where it changes a
message format, the receivers (every shard for credits and the feed, the anchor and settlement Agents)
upgrade first to accept both forms, and the senders after (`REQ-RT-16`'s last point), as XCM versions are
negotiated.

---

## Toward `pallet-spaces`

`DEC-17` moves settlement from the ink! contract to the Spaces pallet later, as a change of venue inside one
deployment. Here is what of this amendment carries over.

| Choice | Carries over? | Why |
|---|---|---|
| Construction inside the 32 bytes (`a0023-q4`) | **Yes, unchanged** | The anchor is a number and an opaque 32-byte value. The `v3` commitment (genesis, code, construction, mode) binds everything inside it, so the pallet needs no new field. Each checkpoint records its venue (`Checkpoint::venue`, `anchor/mod.rs`), so a book can anchor in the contract and then in the pallet, and a bridge anchor works in either |
| Unkeyed `blake2b-256` code digests | **Yes** | It is Substrate's hash. A pallet can compare a code version with a preimage or a runtime-upgrade hash without converting |
| Kreivo preimages as a published source (`a0023-q8`) | **Yes, and stronger** | The preimage pallet is on the same chain. Inside the runtime, `pallet-spaces` could *request* its deployment's code versions through the preimage traits (`QueryPreimage`, `StorePreimage`), as the scheduler and referenda do. A requested preimage is kept while it is requested, so a depositor could no longer unnote it, and the code history would be held on-chain for as long as the Space holds it. In Kreivo, `ManagerOrigin` is root (`utilities.rs:105-117`), so this only works from inside a pallet, not from a contract. It is a design item for `pallet-spaces`, not a promise |
| Upgrade-authority mode (`a0023-q2`) | **Stays in the book** | It governs aledgent's code, not the venue. A Space's own administration (a community, a vote) is a different authority and should not be conflated with it. A later mode kind could require a Space-level approval, as an upgrade of the code that enforces modes |
| Legal hold (`a0023-q5`), plugins (`a0023-q7`), the frozen host spec (`a0023-q6`) | Not affected | They are off-chain |
| Book base on the contract (amendment 0022, option (e)) | **Better in the pallet** | The contract cannot tell two books apart. A pallet can key anchors by book (the `v3` genesis digest), which is 0022's option (a) without a new contract instance. That is out of this amendment's scope |

## Carried to the plan

- `PLAN.md` §2, a new `DEC-26` "Code history and upgrades":
  - (a)–(h) and (b′) above, with the rulings below;
  - `DEC-25`'s "never changes construction" sentence amended (R3);
  - `DEC-4`'s hot state gains `code`, `construction`, `upgrade`, `authority` and `authority_next`;
  - `DEC-17` notes "Toward `pallet-spaces`".
- `PLAN.md` §4, a new `F-037` "Code history":
  - the hot-row and ring-entry fields and checkpoint `v3`;
  - the `Upgrade`, `MigrateConstruction` and `SetAuthority` inputs, and the modes;
  - `CodeSource` with the built-ins (`fs`, `s3`, `https`, and `kreivo-preimage` in the open-source
    distribution), `exec` and plugins (C ABI, `aledgent-code-fetch`);
  - per-range restore under the frozen call format;
  - snapshots with leaf chunks, and the per-shard archive;
  - the `code-history` gate;
  - the legal hold and the break-glass role.

  It comes after `F-036` (the chainspec names *H₀* and the initial mode) and before `F-021` (warp sync is how
  a fresh replica is admitted).
- `F-036`: the chainspec header names *H₀*, the construction and the initial upgrade-authority mode (seed
  format version 2, with the settlement base). This is proposed to PR #199's owner; it is not changed here.
- The core at opt-level `s` before the next genesis (`a0023-q9`): handled separately. *H₀*'s manifest records
  it.
- `SPEC.md`: R1–R6 are written in once #199 merges, at the version after it. `US-K7`'s cases are registered
  as pending with the `F-037` tasks (amendment 0013's practice).
- Open-source documentation: `docs/code-history.md`.

## Rulings (the owner, 2026-10-08)

- `a0023-q1` — **Apply R1–R5 as drafted.** Ruled. Applied to `SPEC.md` after #199 merges, with pending cases (Status).
- `a0023-q2` — **One operator principal today. The mode is fixed at genesis and can be switched over time,
  each switch validated under the mode in force.** Ruled. Specified as R6 (`REQ-RT-18`) and (b′). Confirmed
  possible: the mode is book state, and changing it is an upgrade-class input authorised under the current
  mode. Its limits are stated in R6.
- `a0023-q3` — **One `v3` for both amendments.** Ruled. It binds the genesis digest, the code version, the
  construction and the authority mode ((a)).
- `a0023-q4` — **(a): the construction stays inside the 32 bytes.** Ruled. The contract is unchanged; the
  `pallet-spaces` relevance is in (e) and "Toward `pallet-spaces`".
- `a0023-q5` — **Legal hold.** Ruled ((c)), with the break-glass role.
- `a0023-q6` — **(a), with (b) as fallback; the host spec is frozen.** Ruled ((d)).
- `a0023-q7` — **Plugins allowed as well as commands.** Ruled ("Plugins": C ABI, versioning, loading,
  isolation). Hash verification stays mandatory for every source.
- `a0023-q8` — **Kreivo preimages for the open-source release: yes.** Ruled ("Retrieval"; "Toward
  `pallet-spaces`").
- `a0023-q9` — **Build the core at opt-level `s` now, before the next genesis.** Ruled, and handled outside
  this amendment. *H₀* is an opt-`s` build ((a)). On a live book, a change of profile is an upgrade (R1,
  `AC-K7.6`).
