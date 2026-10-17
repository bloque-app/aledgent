<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0033 — Code history that does not depend on the platform

**Status:** **ruled by the owner on 2026-10-17** ("the briefing is good enough, you have my okay"), on the
briefing this amendment writes down. **Applied** to `SPEC.md` **v0.4.27**, to `PLAN.md` `DEC-26`, and to
`F-036` and `F-037`, in the same pass. The new acceptance criteria are registered as pending
(`crates/conformance/src/cases/f037.rs`), as amendment 0023's were.
**Target:** `SPEC.md` v0.4.26 as on `develop` (amendment 0032 applied).
**Date:** 2026-10-17
**Amends:** amendment 0023's applied text (`REQ-RT-15` to `REQ-RT-18`, `US-K7`, `DEC-26`). Nothing of 0023 is
withdrawn.
**Rationale in one line:** the book should be able to move to another runtime later as one more upgrade,
with no new genesis, and that only works if the code history is shaped for it now, before `F-037` is built.

---

## Why now

`F-037` (code history and upgrades) is the next feature to build. Amendment 0023 specified it on
2026-10-08, around the runtime the book runs on today. A runtime with a different execution model is being
built upstream, and the book may move onto it once it settles.

On 2026-10-16 the two runtimes were compared, to find out what such a move would cost. The comparison was
read-only. Its main finding is that the move does not need a new genesis. Everything that decides the book
is aledgent's own: the ledger library, the kernel, the roots, the hash-chained log of inputs and the
archive. A writer on the new runtime can therefore start as the continuation of the old one, at an input
number, with its log chain unbroken.

That holds only if `F-037` does not tie the code history to the runtime of today. Four places in amendment
0023 do. This amendment removes them, as requirements that name the property and not the product.

## What the comparison found

The facts below are the ones the requirements rest on. Each one says where it was read.

- **The runtime of today** is VOS's `saga/agents` line, at the revision 0023 cited (`c54fc1ff`).
- **The new runtime** is VOS's `vos-core` branch, at commit `b74eeef8` of 2026-10-16. Its design is
  `docs/design/core-model.md`, draft 5. It is under construction, so each fact about it is a snapshot of
  that commit.
- **aledgent** is this repository at `94b2912`.

| # | Finding | Read in |
|---|---|---|
| P1 | The book's decisions do not depend on the runtime. The transition is written against a table of rows, and runs the same inside the runtime and natively. The roots, the log hash chain and the archive are aledgent's own formats. | `crates/core-binding/src/transition.rs:1-13`; `crates/core-binding/src/hot.rs:196-222`; `crates/core-archive/src/record.rs:1-23` |
| P2 | The writer's own state is small: the hot row, the configuration and the unarchived ring entries. It is kept inside the runtime's own encoding of state. An archive checkpoint is that encoding, saved as one file, so a checkpoint can only be restored by the runtime that wrote it. | `crates/core-archive/src/record.rs:17-20`; `agents/core-host/src/serve.rs:148-242` (`ALGCORE1`) |
| P3 | Two values in the hot row come from the runtime: the principal the writer takes inputs from (`driver`), and the slot the runtime observed (`observed_slot`, a diagnostic). Neither enters a ring entry, the log hash or the anchor commitment today. | `crates/core-binding/src/hot.rs:104-105`, `:139-141`, `:196-222` |
| P4 | The new runtime names a program differently. A package is the ELF the actor's crate builds. Installing it derives the program, and the install record keeps the derived program's hash, the transpiler's version and the ELF's hash. The derived hash is the program's identity, and it changes with the transpiler's version. There is no runtime program per agent: the node runs its own default agent program. | vos-core `docs/design/core-model.md` §6.4; `docs/design/slice-4-jam.md`, "The install record" and "Identity"; `agent/default` |
| P5 | The new runtime upgrades an actor by installing a package again into the same instance. The install writes the instance's record unconditionally: there is no compare-and-swap on the old program. An actor cannot read its own program's hash: the self-identity call answers only the instance and the agent. Migration of state on upgrade is an open question. | vos-core `agent/default/src/refine/admin.rs:25-48`; `agent/wire/src/abi.rs:232-233` (`SelfId`); `docs/design/core-model.md` §18, question 11 |
| P6 | On the new runtime, the authority to install a package is the node operator's admin token. It is not anything the book holds. | vos-core `node/host/src/client/operator.rs:1-11`; `node/host/src/node/open.rs:33` |
| P7 | The new runtime keeps each agent's state as rows in its own store, and compacts each agent's log after about a thousand entries. It keeps no history of inputs, and it has no counterpart of the one-file image of P2. | vos-core `docs/design/core-model.md` §9.3, §9.5; `node/host/src/node/config.rs:79-85` |
| P8 | The runtime of today upgrades through its registry, with a compare-and-swap on the old program hash and the installation's revision. The book's writer does not use it yet. | VOS `c54fc1ff` `vos/src/registry.rs:1043-1100`, as amendment 0023 cites it |
| P9 | Replay of the book is native first today. The host applies each archived input with its own copy of the library, then re-sends it to the writer in the runtime and requires the archived log hash. The re-send needs the runtime and the exact program bytes of each range. A restore rehearsal on a test deployment on 2026-10-15 needed the genesis program bytes copied from the old image for that reason. | `crates/core-binding/src/core.rs:427-524`; `crates/core-binding/src/host.rs:1771-1817`; `PLAN.md` `DEC-27` (i) |
| P10 | The new runtime supplies its own clock to a step (the batch time). The book takes time only from its inputs. | vos-core `agent/wire/src/abi.rs` (`Now`); `SPEC.md` `REQ-RT-7` |

## The six recommendations, and where each one lands

| # | Recommendation of the comparison | Requirement | Plan | Rests on |
|---|---|---|---|---|
| 1 | The code version names its platform, and each artefact lists every identity a platform gives it; the runtime artefact becomes optional | §2 **Platform**, **Code version**; `REQ-RT-19` (first point); `REQ-RT-16` (a change of platform is an upgrade) | `DEC-26` (a) | P4 |
| 2 | One host operation switches the program, and the old code refuses input *n* while an upgrade to *n* is pending | `REQ-RT-20` | `DEC-26` (b), (i) | P5, P8 |
| 3 | Checkpoints are the writer's rows and the book's leaves, not the runtime's image | `REQ-RT-19` (third point) | `DEC-26` (g) | P2, P7 |
| 4 | A writer can be founded as the continuation of another at input *n*, with its log head, so that moving is an upgrade | §2 **Continuation**; `REQ-RT-21` | `DEC-26` (j); `F-036` | P1, P7 |
| 5 | Replay is native first for every range, and each range's host is kept in the code archive as its replayer | `REQ-RT-19` (fourth point) | `DEC-26` (d) | P9 |
| 6 | Nothing specific to a platform enters anything hashed, and the upgrade-authority mode is enforced inside the book | `REQ-RT-19` (second point); `REQ-RT-20` (last point) | `DEC-26` (a), (b′) | P3, P6, P10 |

## What does not change

- `REQ-RT-15` to `REQ-RT-18`, `REQ-PR-11` and `NFR-22` stand as amendment 0023 applied them. This amendment
  adds to them and does not reword them, except for one point added to `REQ-RT-16`.
- The anchor `v3` commitment, its fields and its order (`DEC-26` (a), ruling `a0023-q3`). An anchor is still a
  number and 32 opaque bytes (ruling `a0023-q4`).
- The state commitment's construction. A move of platform is not a migration of construction (`REQ-PR-11`).
- Retrieval through `CodeSource`, the legal hold, the plugins and the Kreivo preimages (rulings `a0023-q5`,
  `a0023-q7`, `a0023-q8`).
- The frozen host spec (ruling `a0023-q6`). This amendment uses the fallback of that ruling for one purpose
  only, stated in (d) below.
- `SPEC.md` still names no runtime, product or network. The requirements speak of a **platform**.
- No error condition is added (`REQ-ER-1`), and no identifier is withdrawn, renumbered or reused.

---

## R1 — glossary (§2): one row changed, two added

The **Code version** row gains one sentence, at its end:

> | **Code version** | The identity of everything that decides a writer's transitions: a digest of each executable artefact, together with the settings it was built with. Two builds of the same source with different settings are two code versions. It names the platform its artefacts are built for, and each artefact's identities on that platform (`REQ-RT-19`). |

Two rows are added after **Code archive**:

> | **Platform** | The execution environment a writer's code runs on: the runtime that executes it and the host that drives it, named with their versions. The capabilities of §4.4 are requirements of a platform. Two platforms that both meet them are interchangeable for the book (`REQ-RT-19`). |
> | **Continuation** | A writer founded to take over another writer's sequence at an input number, from the other's committed state and log head, so that the book's inputs and their hash chain go on unbroken (`REQ-RT-21`). |

## R2 — `REQ-RT-16`: one point added

After "A change of build settings alone is an upgrade":

> - A change of the platform that executes a writer is an upgrade too, and it MAY be made by founding a
>   continuation of the writer on the new platform (`REQ-RT-21`).

## R3 — `REQ-RT-19` (new, after `REQ-RT-18`)

> **`REQ-RT-19`** **The code history does not depend on the platform.** A book MUST be able to change the
> platform its writers run on through upgrades alone, with no new genesis and no export (`REQ-RT-8`). To that
> end:
>
> - A code version MUST name the platform it runs on, and list, for each of its artefacts, every identity
>   under which that platform knows the artefact, beside the artefact's own digest. An artefact that one
>   platform needs and another provides itself MAY be absent from a code version.
> - A value that a platform supplies MUST NOT enter anything that a root, a log hash, a checkpoint or an
>   anchor covers, except through the code version that names the platform. Such values include the
>   platform's clock, its slot or block number, its identities for principals and programs, and its encoding
>   of state. Time enters the book only in inputs (`REQ-RT-7`).
> - A checkpoint, and every snapshot kept to restore a writer, MUST hold the writer's state in a form that
>   aledgent defines and that no platform defines: the writer's own records and the book's leaves. A snapshot
>   taken on one platform MUST restore a writer on any platform that a later code version of that writer
>   names.
> - Replaying the book's state (`REQ-RT-3`) MUST NOT need the platform that executed an input. For every range
>   of the code map, the book's transitions MUST be reproducible from the inputs by aledgent's own code.
>   Where a replay also checks that the platform executed each input as archived, the code archive MUST keep,
>   for that range, everything the check needs to run it as it ran, by digest, like any artefact
>   (`REQ-RT-17`).

> **Trade-off, stated plainly.** Keeping the code history free of the platform costs a little now, for a
> move that may never be made. Checkpoints need a format of aledgent's own instead of the runtime's image,
> and every code version carries a list of identities where one would do today. The gain is that a later
> change of runtime is an upgrade, which the book records and replay crosses, instead of a new genesis and a
> second book.

## R4 — `REQ-RT-20` (new): one switch, fenced by the code it replaces

> **`REQ-RT-20`** **The switch of code is one step, and the old code fences it.** When an upgrade names code
> version *H* from input *n* (`REQ-RT-16`):
>
> - The code that executes a writer's inputs MUST change from the old code version to *H* in one step, taken
>   after input *n* − 1 has committed and before input *n* is executed. A step that fails leaves the old code
>   in force, and is retried.
> - Until that step is taken, the code version in force MUST refuse input *n* and every later input. It
>   changes nothing, and does not take the input's number, so that the same input runs under *H* once the
>   step is taken. This MUST hold whatever the platform checks or does not check when the code changes.
>   Each code version MUST therefore be able to tell from its own build whether it is the version an upgrade
>   names.
> - The authority that decides an upgrade MUST be the writer's upgrade-authority mode, checked by the writer
>   in its own order (`REQ-RT-18`). Whatever authority a platform has to install or replace programs MUST NOT
>   by itself authorise an upgrade. A host MUST take the step only towards the code version an authorised
>   upgrade names, and only at the input it names.

> **Limits, stated plainly.** Someone who holds a platform's own authority to install programs can still
> install other code than the book names. The book cannot stop that. What it guarantees is that every input
> is executed by the code the code map names, or is refused, and that replay under the code map shows any
> input whose archived outcome other code produced. The operator keeps the platform's install authority as
> it keeps its other keys (`NFR-8`).

## R5 — `REQ-RT-21` (new): a writer continued on another platform

> **`REQ-RT-21`** **A continuation.** A writer MAY be continued by another at input *n*: a **continuation**,
> founded from the first writer's committed state after input *n* − 1, its log head among it.
>
> - The continuation MUST be preceded by an upgrade of the first writer that names the continuation's code
>   version from input *n* (`REQ-RT-16`). The first writer's own code is never switched, so it refuses input
>   *n* and every later one for good (`REQ-RT-20`).
> - The continuation's state when it is founded MUST equal the first writer's committed state after input
>   *n* − 1: its roots, its counters, its code and its upgrade-authority mode. It MUST be checked to be so
>   before the continuation takes input *n*, and every replay checks it again.
> - The continuation's first input is *n*, and its log entry MUST chain to the log head of input *n* − 1. The
>   book keeps its genesis, its construction and its anchor sequence. A continuation is not a new genesis
>   (`REQ-RC-6`), not a migration of construction (`REQ-PR-11`), and not an export and import (`REQ-RC-3`).
> - The continuation's founding MUST be archived with the inputs, so that the code map shows the boundary
>   and replay crosses it (`REQ-RT-17`).

*`REQ-RT-19` to `REQ-RT-21` ruled by the owner on 2026-10-17 (amendment 0033).*

## R6 — `US-K7`: three acceptance criteria added

> - `AC-K7.9` Given an upgrade naming code version *H* from input *n*, When input *n* reaches a writer whose
>   code has not yet been switched to *H*, Then it is refused, nothing changes, its number is not taken, and
>   once the code is switched the same input runs under *H* (`REQ-RT-20`).
> - `AC-K7.10` Given a writer continued at input *n*, Then the book keeps its genesis and its construction,
>   input *n*'s log entry chains to input *n* − 1's, the first writer refuses every input from *n*, and a
>   replay from genesis crosses the boundary and reaches the archived roots and log at every input
>   (`REQ-RT-21`, `REQ-RT-17`).
> - `AC-K7.11` Given a checkpoint, When a writer is restored from it under a code version that names a
>   different platform than the one it was taken on, and the inputs after it are replayed, Then the writer
>   reaches the checkpoint's roots and every archived root after it (`REQ-RT-19`).

## R7 — §15 traceability

Epic K's row cites `REQ-RT-15`–`REQ-RT-21`.

---

## The plan's binding (`DEC-26`, amended)

Everything below is plan-level and names technology. It changes amendment 0023's (a), (b), (b′), (d) and
(g), and adds (i) and (j). "The runtime of today" is VOS `saga/agents`; "the new runtime" is VOS `vos-core`.

### (a) The manifest names its platform

- The manifest (`aledgent/code/v1`; not yet built, so its first version takes these fields) gains
  `platform`: the platform's name and revision (for example `vos-saga@c54fc1ff`, later
  `vos-core@<revision>`).
- Each artefact gains `identities`, a list of `(kind, value)` pairs, in place of the single VOS
  `ProgramId` field:
  - on the runtime of today: the VOS `ProgramId`;
  - on the new runtime: the ELF's hash, the derived program's hash and the transpiler's version (P4).
- The `runtime` role is optional. The new runtime has no runtime program per agent. Its node's default
  agent program is named under `platform`, with its hash as an identity.
- Each package carries a **self-identity**: 32 bytes compiled into the package, derived from the canonical
  build profile, and listed in the manifest as `self_id`. Two builds with different settings have two
  self-identities (`AC-K7.6`). It is how a code version tells from its own build whether it is the one an
  upgrade names (`REQ-RT-20`), on a runtime where an actor cannot read its own program's hash (P5).
- New role `replayer`: a host image that can re-send a range's inputs to its platform, by digest ((d)).
- **Nothing of the platform is hashed** outside the manifest (`REQ-RT-19`, P3, P10):
  - `driver` becomes the writer's **authorised submitter**: the runtime's principal today, the core host's
    session key on the new runtime. It is set when a writer is founded, never carried by a continuation or
    a snapshot, and never enters a root, a log hash, a checkpoint or an anchor;
  - `observed_slot` stays a diagnostic under the same rule;
  - the anchor `v3` commitment is unchanged, and every field of it is aledgent's.

### (b) The switch, behind one host operation

Step 4 of amendment 0023's upgrade becomes one operation of the core host, `switch(H, n)`, taken after input
*n* − 1 has committed and before input *n* is sent:

- on the runtime of today: VOS's registry upgrade, a compare-and-swap on the old program hash and the
  installation's revision (P8);
- on the new runtime: an install of *H*'s package into the same instance (P5). The new runtime offers no
  compare-and-swap, so the host checks the instance's install record before and after;
- across platforms: the founding of a continuation on the new platform ((j)).

Before input *n*, the host asks the switched program for its self-identity, and stops if it is not *H*'s.
A host that crashes between the switch and input *n* resumes from the writer's committed state, as before.

**The fence** (`REQ-RT-20`). The upgrade handler is in *H₀*, and so is this rule. A writer whose hot row holds
`upgrade = Some((H, n))` refuses an input numbered *n* or later unless its own self-identity is *H*'s. The
refusal is an outcome of its own: nothing is written, `inputs` stays at *n* − 1, and the host takes it as
"switch first". The new runtime consumes a failed step, but the book's input number is aledgent's, so
nothing of the book is consumed. Input *n* under *H* moves `upgrade` into `code`, as before.

### (b′) The mode is enforced in the book

The upgrade-authority mode stays the only authority over a writer's code (`REQ-RT-18`, `REQ-RT-20`). The host
takes `switch` only for an upgrade the writer has committed. On the new runtime, the node's admin token
(P6) is held like the core host's other keys (`F-035`, `NFR-8`), and the operator's runbook never installs
a package except through `switch`.

### (d) Replay per range: native first, with a replayer per range

- The book side of every range is replayed natively, under the frozen host spec, as ruling `a0023-q6` (a)
  decided. It needs no platform (P1, P9).
- The writer side (re-sending each input to the platform and requiring the archived log hash) runs each range
  under that range's `replayer`: the host image that drove it, kept in the code archive by digest.
- This is ruling `a0023-q6`'s fallback form, a host build per range named in the manifest, used for the
  writer-side check only. The owner's approval of 2026-10-17 covers recording a replayer for every range.
  The book side never uses it.
- An auditor without the old platform can replay a range natively, through the transition over rows in
  memory, and check the platform's execution only where they hold its replayer.

### (g) Checkpoints as rows and leaf chunks

- An archive checkpoint holds the writer's rows (the hot row, the configuration and the unarchived ring
  entries) as a sorted list of key and value, in aledgent's encoding, plus the leaf chunks amendment 0023
  (g) already required for fast resync.
- The platform-bound fields of (a) are not in it.
- `agent.state` stays the core host's local cache on the runtime of today, never an archive format.
- Checkpoints already written as `agent.state` stay readable by a converter that keeps the runtime of
  today's codec.
- The same archive restores a writer onto either runtime (P2, P7).

### (i) What the upgrade does on each runtime

| Step | Runtime of today | New runtime |
|---|---|---|
| Identity of the code | `ProgramId` of the package and the runtime program | The ELF's hash, the derived program's hash, the transpiler's version; the node's default agent program under `platform` |
| `switch(H, n)` | Registry compare-and-swap | Install into the same instance, the record checked before and after |
| Safety when the switch is missed | The compare-and-swap, and the fence | The fence alone |
| Who may switch | The writer's mode; the host switches only for a committed upgrade | The same; the admin token is the host's key, never an authority |
| Checkpoint | Rows and leaf chunks; `agent.state` a local cache | Rows and leaf chunks |

### (j) The continuation founding (`F-036`'s `StateGenesis`, generalised)

- `StateGenesis` founds a writer from a chainspec. A second envelope, `Continuation`, founds it from a
  predecessor at input *n*. It carries the genesis digest, the predecessor's writer, `at = n`, the log head
  of input *n* − 1, and the hot state of input *n* − 1 (roots, counters, `code`, `construction`,
  `authority`, `upgrade`).
- The host builds it from the predecessor's checkpoint at *n* − 1 ((g)). It checks the predecessor's committed
  hot row and log head against the checkpoint before founding, and the continuation's roots against its own
  rebuilt replica after.
- The founding is archived as a record of its own, beside the segments, and the code map's boundary at *n*
  points to it.
- A move to the new runtime is then: an `Upgrade` naming a code version whose `platform` is the new runtime,
  from *n*; the predecessor fenced at *n*; `Continuation` on the new runtime; input *n* sent there. No new
  genesis, no bridge anchor, no change of construction. The next anchor names the new code version.
- It also serves on one platform: a continuation onto a fresh Agent of the same runtime is how the gate
  rehearses the move before the new runtime exists.

## Carried to the plan

- `PLAN.md` `DEC-26`: (a), (b), (b′), (d) and (g) amended; (i) and (j) added; the status names this amendment.
- `PLAN.md` `F-036`: `StateGenesis` is generalised by `F-037`'s continuation; nothing of `F-036` changes.
- `PLAN.md` `F-037`, and `features/F-037-code-history/plan.md` (new) and `tasks.md`:
  - `T-037-01` to `T-037-03` take the platform field, the identities, the optional runtime role, the
    self-identity, the platform-bound fields kept out of every hash, and a commitment built from aledgent's
    fields alone;
  - `T-037-04`, `T-037-05`, `T-037-09`, `T-037-11` and `T-037-12` follow (b), (b′), (d) and (g);
  - four new tasks: `T-037-14` the `switch` operation, `T-037-15` the fence, `T-037-16` checkpoints as rows
    and leaf chunks, `T-037-17` the continuation;
  - the table is reordered to build order.
- `crates/conformance`: `AC-K7.9` to `AC-K7.11` registered as pending.
- `docs/code-history.md`: a section on what keeps the code history independent of the runtime.
- Upstream: nothing is proposed to VOS by this amendment. A compare-and-swap on install and a way for an actor
  to read its own program's hash would each make the new runtime's switch safer, but the fence does not
  depend on them.

## Identifier ledger

| Action | Identifiers |
|---|---|
| New | `REQ-RT-19`, `REQ-RT-20`, `REQ-RT-21`; `AC-K7.9`, `AC-K7.10`, `AC-K7.11`; glossary rows **Platform** and **Continuation**; `T-037-14`, `T-037-15`, `T-037-16`, `T-037-17` |
| Changed | The glossary row **Code version** (one sentence added); `REQ-RT-16` (one point added); §15, Epic K; `DEC-26` (a), (b), (b′), (d), (g), and (i), (j) added; `T-037-01` to `T-037-05`, `T-037-09`, `T-037-11`, `T-037-12` |
| Withdrawn | None |

## Ruling (the owner, 2026-10-17)

- **Apply as briefed.** The owner approved the briefing this amendment writes down: the six recommendations,
  as requirements and as the plan's binding above, including a replayer recorded for every range under ruling
  `a0023-q6`'s fallback.
