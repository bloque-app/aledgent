<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0033 — Code history that does not depend on the platform

**Status:** **ruled by the owner on 2026-10-17** ("the briefing is good enough, you have my okay"), on the
briefing this amendment writes down. The design decisions in §"The architect's decisions" were taken on the
same day under that approval, after six independent reviews of the draft (#35): D1 to D7 after the
first, D8 to D14 after the second, D15 to D18 after the third, D19 to D22 after the fourth, D23 to D25 after
the fifth, D26 after the sixth. **Applied** to `SPEC.md` **v0.4.27**, to `PLAN.md` `DEC-26`, and to
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
| P3 | Two values in the hot row come from the runtime: the principal the writer takes inputs from (`driver`), and the slot the runtime observed (`observed_slot`, a diagnostic). Neither enters a ring entry, the log hash or the anchor commitment today. | `crates/core-binding/src/hot.rs:105-106`, `:139-141`, `:196-222` |
| P4 | The new runtime names a program differently. A package is the ELF the actor's crate builds. Installing it derives the program, and the install record keeps the derived program's hash, the transpiler's version and the ELF's hash. The derived hash is the program's identity, and it changes with the transpiler's version. There is no runtime program per agent: the node runs its own default agent program. | vos-core `docs/design/core-model.md` §6.4; `docs/design/slice-4-jam.md`, "The install record" and "Identity"; `agent/default` |
| P5 | The new runtime upgrades an actor by installing a package again into the same instance. The install writes the instance's record unconditionally: there is no compare-and-swap on the old program. An actor cannot read its own program's hash: the self-identity call answers only the instance and the agent. Migration of state on upgrade is an open question. That a re-install keeps the instance's state rows is plausible but not shown by the cited code: the install writes only its record. It is an assumption until a spike shows it. | vos-core `agent/default/src/refine/admin.rs:25-48`; `agent/wire/src/abi.rs:232-233` (`SelfId`); `docs/design/core-model.md` §18, question 11 |
| P6 | On the new runtime, the authority to install a package is the node operator's admin token. It is not anything the book holds. | vos-core `node/host/src/client/operator.rs:1-11`; `node/host/src/node/open.rs:33` |
| P7 | The new runtime keeps each agent's state as rows in its own store, and compacts each agent's log after about a thousand entries. It keeps no history of inputs, and it has no counterpart of the one-file image of P2. | vos-core `docs/design/core-model.md` §9.3, §9.5; `node/host/src/node/config.rs:79-85` |
| P8 | The runtime of today upgrades through its registry, with a compare-and-swap on the old program hash and the installation's revision. It is a signed registry mutation: the signed bytes include the expected revision and the old program hash. The book's writer does not use it yet. | VOS `c54fc1ff` `vos/src/registry.rs:1043-1100`, as amendment 0023 cites it |
| P9 | Replay of the book is native first today. The host applies each archived input with its own copy of the library, then re-sends it to the writer in the runtime and requires the archived log hash. The re-send needs the runtime and the exact program bytes of each range. A restore rehearsal on a test deployment on 2026-10-15 needed the genesis program bytes copied from the old image for that reason. | `crates/core-binding/src/core.rs:427-524`; `crates/core-binding/src/host.rs:1771-1817`; `PLAN.md` `DEC-27` (i) |
| P10 | The new runtime supplies its own clock to a step (the batch time). The book takes time only from its inputs. | vos-core `agent/wire/src/abi.rs` (`Now`); `SPEC.md` `REQ-RT-7` |

## The six recommendations, and where each one lands

| # | Recommendation of the comparison | Requirement | Plan | Rests on |
|---|---|---|---|---|
| 1 | The code version names its platform, and each artefact lists every identity a platform gives it; the runtime artefact becomes optional | §2 **Platform**, **Code version**; `REQ-RT-19` (first point); `REQ-RT-16` (a change of platform is an upgrade) | `DEC-26` (a) | P4 |
| 2 | One host operation switches the program, and the old code refuses input *n* while an upgrade to *n* is pending | §2 **Code generation**; `REQ-RT-20` | `DEC-26` (b), (i) | P5, P8 |
| 3 | Checkpoints are the writer's rows and the book's leaves, not the runtime's image | §2 **Writer state**; `REQ-RT-19` (third point) | `DEC-26` (g) | P2, P7 |
| 4 | A writer can be founded as the continuation of another at input *n*, with its log head, so that moving is an upgrade | §2 **Continuation**, **Writer state**; `REQ-RT-21` | `DEC-26` (j); `F-036` | P1, P7 |
| 5 | Replay is native first for every range, and each range's host is kept in the code archive as its replayer | `REQ-RT-19` (fourth point) | `DEC-26` (d) | P9 |
| 6 | Nothing specific to a platform enters anything hashed, and the upgrade-authority mode is enforced inside the book | §2 **Writer state** (host-local fields); `REQ-RT-19` (second point); `REQ-RT-20` (last point) | `DEC-26` (a), (b′) | P3, P6, P10 |

## The architect's decisions (2026-10-17)

The independent review of the first draft (#35) found that the fence and the continuation were not sound as
written. The architect took the decisions below on 2026-10-17, under the owner's approval of the briefing
the same day. Each says what the review found and what was decided.

- **D1 — A code generation, not a self-identity.** The draft had each package carry a self-identity derived
  from its build profile. A package cannot know the digest of the manifest that names it, so the writer had
  no way to tell which self-identity an upgrade expects. A self-identity derived from the build profile also
  stays the same when only another artefact changes, so the fence could miss a switch. Decided:
  - the writer's state holds its **code generation**: 0 at genesis, one more after each upgrade;
  - every package is built with its generation as a constant, and the manifest lists it;
  - an upgrade names *H* and *n*, and so, implicitly, the next generation;
  - the fetcher refuses a code version whose generation is not the next one, and the host refuses to switch
    to bytes that do not carry it, after checking those bytes against the manifest's digest;
  - the fence compares the generation compiled into the running code with the scheduled upgrade's;
  - every upgrade raises the generation, even one that changes only the runtime or another artefact, so that
    the fence always sees a missed switch;
  - native replay takes each range's generation from the code map.

  A package cannot claim another generation: its bytes, and so the constant, are the ones the manifest's
  digest names.
- **D2 — A way out of the fence.** The draft's fence refused every input from *n*, and a scheduled upgrade
  could not be cancelled, so a switch that failed for good halted the writer with no way out. Decided:
  - while the fence holds, the old code still takes one kind of input: a move of the scheduled upgrade to a
    strictly later input, or its cancellation, authorised under the mode in force and decided by the old
    code, taking its input number like any other input;
  - the host fetches and checks *H*'s artefacts before the upgrade can commit, and switches only to those
    checked bytes;
  - an alert is raised when a writer has been fenced longer than a bound each deployment sets (five minutes
    unless it says otherwise).
- **D3 — One continuation per input number.** Nothing stopped two continuations from the same *n*, which
  would fork the log. Decided:
  - the founding record is written once, under a key derived from the book's genesis, the writer and *n*,
    with a create that fails if the key exists, under the archive's lock;
  - a second founding at the same *n* is refused, and replay refuses a log with two founding records at one
    *n*;
  - feeding stays with one execution node at a time, through the feed lease and its epoch (`F-038`), so a
    retried founding cannot run beside the first;
  - founding is deterministic: the same committed state gives the same record.
- **D4 — What a continuation carries.** The draft listed the carried state three different ways, none
  complete. Decided: the continuation carries the writer's complete committed state after *n* − 1. That is
  every field of the writer's hot state, every record, the unarchived log, pending deliveries and the state of
  its message streams. Equality is defined over all of it, except the fields the specification lists as
  host-local. One definition, **Writer state** in §2, is referenced from the checkpoint, the continuation and
  the equality check.
- **D5 — Tasks.** `T-037-14` (switch) and `T-037-04` (the upgrade input) come before `T-037-15` (the fence).
  When an upgrade is pending, the host cuts its batch so that a restore point lands exactly at *n* − 1. A
  done-when exercises the replayer. `T-037-14` says that the registry upgrade on the runtime of today is signed
  with the core's package key through the signer.
- **D6 — The replayer is outside the code version.** The draft listed the replayer as an artefact of the
  manifest, so every rebuild of the host would have been a code upgrade. Decided: each range's replayer is
  kept in the code archive in an entry of its own, indexed by the code version. A code version covers only
  what decides a transition.
- **D7 — Wording.**
  - Only archived and transferred snapshots must be in aledgent's format; a node's local cache need not be.
  - "A host" becomes "the execution node that drives the writer".
  - The code map derives from the upgrade inputs; the founding record is evidence that replay checks against
    them.
  - `Continuation` is a second founding envelope beside `StateGenesis`, which stays as it is.
  - A citation off by one line is corrected.

The second review, of the draft with D1 to D7 applied, found that the fence held only while an upgrade was
scheduled, that it could not see a runtime supplied apart from the package, that a move or cancellation of
the upgrade raced the switch, and that the way out reopened a fork at a continuation. The architect took
D8 to D14 on the same day, under the same approval.

- **D8 — The generation rule is always on.** It applied only while an upgrade was scheduled, so a stale
  package installed after the switch took inputs unchallenged. Decided: code refuses every input unless its
  compiled generation equals the writer's generation in force. The one exception is an input numbered at or
  after a scheduled upgrade's *n*, where the generation in force plus one is also accepted. The fence
  identifies code up to its generation only. That the bytes within a generation are the code version's is the
  execution node's staging check, by digest.
- **D9 — The runtime is checked by the execution node.** On the runtime of today the core host supplies the
  runtime program itself, apart from the package the registry upgrades (confirmed in
  `agents/core-host/src/driver.rs:198-204`, `Runtime::create`, against `:573-600`, `attach`). The in-book fence
  cannot see it. Decided: when it starts and at every switch, the execution node checks that every artefact
  it runs for the writer is the one the current range's code version names, by digest, the runtime among
  them. It refuses to drive the writer otherwise. Every upgrade still raises the generation.
- **D10 — Input *n* is decided the same under either generation.** If the switch was made before a queued
  move or cancellation of the upgrade arrived as input *n*, the new code took it, and the outcome depended on
  timing. Decided:
  - a move or cancellation committed as input *n* means that the upgrade does not take effect at *n* and the
    generation stays *g*, whichever package decides it;
  - both carry this rule, from `F-037` on;
  - the execution node then installs *g*'s package again before input *n* + 1;
  - the code map is derived from the upgrade, move and cancellation inputs alone, so native replay gives
    every input its generation.
- **D11 — Each input number is archived once.** A move or cancellation in flight at a feed-lease handover
  could commit at the first writer as input *n* after a continuation took *n*, and nothing refused the second
  entry. Decided:
  - the archive accepts each input number of a writer once, by a conditional create per segment, so two
    entries at one number cannot coexist;
  - the feed checks the archive before it sends anything numbered *n* or later to the first writer, and sends
    nothing once a founding record exists;
  - `REQ-RT-21` and `AC-K7.10` now agree with the way out. The first writer refuses every input from *n*
    except a move or cancellation, and takes none at all once a founding record exists.
- **D12 — The founding record comes last.** A record written before a failed check would have stranded the
  book, and a retry after a crash could not tell its own record from a second founding. Decided:
  - the record is written only after every check passes, staging and state equality included;
  - a retry that writes identical bytes succeeds, as a resume of the same founding;
  - different bytes at the same key are refused.
- **D13 — Where the generation lives.** The constant is a 16-byte record: the tag `ALGGEN01` followed by the
  generation as a little-endian `u64`. It sits in a dedicated section of the package's ELF,
  `.aledgent.generation`, kept by the linker and exported as the symbol `ALEDGENT_CODE_GENERATION`, so the
  optimiser cannot fold it away. One reader reads it, in `check-code`, in the host's staging and in the
  pre-switch check:
  - from that section, in an ELF;
  - in a package format that keeps no sections, as the one occurrence of the tagged record in its read-only
    data. No occurrence, or more than one, is refused.
- **D14 — More acceptance criteria.** Added: a stale package refused after a switch, and an upgrade naming the
  wrong generation refused (`AC-K7.16`); an unauthorised move or cancellation refused (`AC-K7.17`); input *n*
  decided the same by either generation (`AC-K7.18`); a second archive entry at one input number refused
  (`AC-K7.19`). `AC-K7.13` and `AC-K7.14` gain the runtime check, the resume and the failed check.

The third review found four places where these rules met each other badly. A continuation had its own writer
identity, so "each input number once" could miss its entries. A way-out input queued at the founding could
trap the continuation on a platform with no older code. The reinstall after a way-out input was not covered
by the switch's own rules. And the fence's number was not tied to the section's. The architect took D15 to
D18 on the same day, under the same approval.

- **D15 — Entries are keyed by the logical writer, and the boundary is claimed once.**
  - Archive entries are keyed by the **logical writer**, never by the instance that runs it. The logical
    writer is the same identity the founding key uses. A first writer and its continuation therefore share
    one key space per input number.
  - The input number *n* at which an upgrade is scheduled is **claimed** once, by a conditional create of
    one key per (genesis, logical writer, *n*). The founding record is written as that claim. The first
    writer's archiver creates its own claim at the same key before it archives the first writer's entry at
    *n*.
  - Whichever create lands first wins. If the first writer's claim landed first, the founding finds it,
    writes nothing and founds nothing, and the first writer continues. If the founding landed first, the
    first writer's entry at *n* is refused, and so never acknowledged.
  - A replay that finds both a founding record and a first writer's entry at one *n* refuses the log. If these
    rules hold, that cannot happen.
- **D16 — No founding over a queued way-out input; after it, the upgrade is final.**
  - No founding is made while the next input queued for the writer is a way-out input. The founding waits
    until the first writer has decided that input.
  - Once a founding record exists, a way-out input is refused everywhere, with `ERR-InvalidTransition`. The
    feed refuses it before sending it. The continuation's code refuses it too, because its writer state
    records that it was founded as a continuation at *n*. The upgrade is final.
- **D17 — The switch rule is symmetric.**
  - Before each input, the execution node installs the code version that the code map names for that input.
    That is a newer generation after an upgrade, and generation *g* again after a way-out input at *n*.
  - The start check does the same: it installs what the code map names instead of refusing, and refuses only
    when it cannot.
  - The signer's grant covers every such install, the reinstall included.
- **D18 — One number, and the way-out rule checked before it runs.**
  - The fence compares the exact value in `.aledgent.generation`. The exported symbol is that section's
    record, defined once in `ledger-core`, and the guest reads it through the same layout as
    `generation_of`.
  - The `code-history` gate checks, for every staged *H*, that *H* decides each way-out input exactly as
    generation *g* does, on fixture inputs. A deviation is caught in staging and CI, not only by replay.

The fourth review found the design sound, with no fork, and four small gaps. The architect took D19 to D22 on
the same day, under the same approval.

- **D19 — The founding checks the upgrade it founds.** A cancellation or move committed just before the
  founding left the first writer at *n* − 1 with nothing scheduled, and the founding would still have
  written its record. That halts the book: the continuation's code is a generation ahead of a state with
  nothing scheduled, and the first writer is barred from *n*. Decided: the founding's checks before the
  build and after it both require that the first writer's committed state still holds the upgrade being
  founded, `upgrade = Some((H, n))`, unchanged. Otherwise no record is written, and the first writer
  continues (`AC-K7.20`).
- **D20 — Both claims are fixed.** Each side's claim record is a fixed function of the genesis, the logical
  writer, *n*, the side, and the book's committed inputs up to *n* − 1. A create that finds the same side's
  identical record is a resume, and succeeds. One that finds the other side's record has lost.
- **D21 — The record names the continuation's instance.**
  - The continuation's instance is named from the claim key. Where a platform assigns ids itself, the host
    records the assignment at the first build and reuses that instance on every retry, so the record's bytes
    do not change.
  - The feed routes input *n* and later only to the instance the record names.
  - An instance from a failed or abandoned build never receives an input, and the runbook tears it down.
- **D22 — `continued_at` is writer state, and its refusal is a fence.**
  - `continued_at` is a committed writer-state field, not a host-local one, set by the founding. §2 **Writer
    state** now names it.
  - Its refusal applies only to a move or cancellation of the upgrade scheduled from `continued_at`. A move or
    cancellation of a later upgrade of the continuation is taken as usual.
  - The refusal works like the fence's: it changes nothing and does not take the input's number. Input *n* is
    then the next input that is not refused, and the upgrade takes effect at it.
  - The feed's refusal is only a fast path with the same effect. The continuation's is the authority.

The fifth review found no blocker and no fork, and three low findings. The architect took D23 to D25 on the
same day, under the same approval.

- **D23 — The instance is recorded beside the claim, never in local state.** D21's fallback kept an
  assigned instance id in one node's state directory. A new lease holder, which starts with no local state,
  would then build a second instance and write a different record. Decided:
  - the fallback is dropped, and the claim record stays a fixed function of (genesis, logical writer, *n*,
    side, the inputs before *n*);
  - the continuation's instance is recorded in a separate write-once object beside the claim,
    `instance/<…n>`, created by the first build, whose content is the instance id;
  - any retry, by any lease holder, reads that object first and adopts the instance it names;
  - both current platforms let the host name the instance, so its id is derived from the claim key, and the
    object records that derived id. On the runtime of today, the host chooses the Agent's creation nonce,
    from which the Agent id is derived (`agents/core-host/src/driver.rs:213-217`). On the new runtime, the
    install takes the instance as an argument (vos-core `node/host/src/node/api.rs:172-177`).
- **D24 — The archive checks the continuation's instance.** An instance the record does not name could still
  reach the archive first and take the key at *n*. Decided: every segment carries the instance that wrote it.
  The archive accepts a continuation-side entry at a claimed *n* only if two things hold: the claim there is
  a founding record, and the entry's instance is the one `instance/<…n>` records. Any other is refused
  (`AC-K7.19`).
- **D25 — The scoped refusal is the fence's kind of outcome, audited outside the book.**
  - The continuation's refusal shares the fence's new call-format version; there is no second version.
  - A refused command, whether refused by the fence or by this rule, is audited at the execution node, in its
    log and in the audit store. It is never committed to the book.

The sixth review found one low gap that D23 opened. A retry after a failed post-check would adopt the
recorded instance, which holds the failed build. A founded Agent cannot be founded again, and neither runtime
guarantees that a name can be used again after teardown. The architect took D26 on the same day, under the
same approval.

- **D26 — Build attempts are numbered.**
  - The continuation's instance name is derived from (claim key, attempt), and the first build is attempt 0.
  - The write-once object becomes `instance/<…n>/<attempt>`, created by a conditional create.
  - A retry, by any lease holder, reads the latest attempt. If that attempt's instance passes the
    post-checks, the retry adopts it. Otherwise the runbook tears that instance down, and the retry creates
    attempt + 1 under a freshly derived name. No name is ever used twice.
  - The founding record names the attempt and the instance that succeeded. The continuation's claim is
    therefore a fixed function of (genesis, logical writer, *n*, side, the inputs before *n*) and that
    attempt, which is itself recorded write-once. The archive's instance check (D24) uses the attempt the
    record names.
  - On the runtime of today this changes `Runtime::create_with`. Its creation nonce is derived from
    (claim key, attempt) instead of today's one-byte discriminator (`agents/core-host/src/driver.rs:213-217`).

## What does not change

- `REQ-RT-15`, `REQ-RT-17`, `REQ-RT-18`, `REQ-PR-11` and `NFR-22` stand as amendment 0023 applied them.
  `REQ-RT-16` gains one point. `REQ-RT-18`'s rule that a change of mode leaves a scheduled upgrade standing
  is unchanged. Moving or cancelling an upgrade (`REQ-RT-20`) is a separate input, authorised under the mode
  in force.
- The anchor `v3` commitment, its fields and its order (`DEC-26` (a), ruling `a0023-q3`). An anchor is still a
  number and 32 opaque bytes (ruling `a0023-q4`).
- The state commitment's construction. A move of platform is not a migration of construction (`REQ-PR-11`).
- Retrieval through `CodeSource`, the legal hold, the plugins and the Kreivo preimages (rulings `a0023-q5`,
  `a0023-q7`, `a0023-q8`).
- The frozen host spec (ruling `a0023-q6`). This amendment uses the fallback of that ruling for one purpose
  only, stated in (d) below. The fence's refusal, and the inputs that move or cancel an upgrade, are added
  as a new version of the call format; no existing version changes.
- `SPEC.md` still names no runtime, product or network. The requirements speak of a **platform**.
- No error condition is added (`REQ-ER-1`), and no identifier is withdrawn, renumbered or reused.

---

## R1 — glossary (§2): one row changed, four added

The **Code version** row gains two sentences, at its end:

> | **Code version** | The identity of everything that decides a writer's transitions: a digest of each executable artefact, together with the settings it was built with. Two builds of the same source with different settings are two code versions. It names the platform its artefacts are built for, each artefact's identities on that platform, and its code generation (`REQ-RT-19`, `REQ-RT-20`). What only replays a range, and decides nothing, is not part of it. |

Four rows are added after **Code archive**:

> | **Code generation** | The count of a writer's upgrades: 0 at genesis, and one more after each upgrade. A writer holds the generation of its code in force. Each code version names one generation, and the code that decides its transitions carries that generation in its own bytes (`REQ-RT-20`). |
> | **Platform** | The execution environment a writer's code runs on: the runtime that executes it and the software on the execution node that drives it, named with their versions. The capabilities of §4.4 are requirements of a platform. Two platforms that both meet them are interchangeable for the book (`REQ-RT-19`). |
> | **Writer state** | Everything a writer has committed after an input. That is its code version and code generation, the construction of its state commitment, its upgrade-authority mode and any scheduled change of it, and any scheduled upgrade. It also includes its configuration and bounds, every record it holds, its counters and log head, its committed inputs not yet archived, the deliveries and notifications it has not yet recorded as made (`REQ-RT-4`), the input number at which it was founded as a continuation, if it was (`REQ-RT-21`), and, for each other writer, the messages it has enqueued, applied, sent and had acknowledged (`REQ-RT-13`, `REQ-RT-14`). Exactly two kinds of field are **host-local** and are not part of it: the identity under which the platform accepts the writer's inputs from the execution node that drives it, and a diagnostic that the platform reports to the writer about an input (such as the slot it observed). No other field is host-local. |
> | **Continuation** | A writer founded to take over another writer's sequence at an input number, from the other's writer state and log head, so that the book's inputs and their hash chain go on unbroken (`REQ-RT-21`). |

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
>   of state. The host-local fields of a writer (§2, **Writer state**) are such values. Time enters the book
>   only in inputs (`REQ-RT-7`).
> - A checkpoint, and every snapshot that is archived or transferred to restore a writer elsewhere, MUST hold
>   the writer state (§2) and the book's leaves, in a form that aledgent defines and that no platform defines.
>   A copy that an execution node keeps only for itself, to restart faster, need not. A snapshot taken on one
>   platform MUST restore a writer on any platform that a later code version of that writer names.
> - Replaying the book's state (`REQ-RT-3`) MUST NOT need the platform that executed an input. For every range
>   of the code map, the book's transitions MUST be reproducible from the inputs by aledgent's own code,
>   under the range's code generation. Where a replay also checks that the platform executed each input as
>   archived, the code archive MUST keep, for that range, everything the check needs to run the range as it
>   ran (its **replayer**), retrievable by digest and indexed by the range's code version (`REQ-RT-17`). A
>   replayer is not part of the code version: replacing it changes no code version.

> **Trade-off, stated plainly.** Keeping the code history free of the platform costs a little now, for a
> move that may never be made. Checkpoints need a format of aledgent's own instead of the runtime's image,
> and every code version carries a list of identities where one would do today. The gain is that a later
> change of runtime is an upgrade, which the book records and replay crosses, instead of a new genesis and a
> second book.

## R4 — `REQ-RT-20` (new): one switch, fenced by the code it replaces

> **`REQ-RT-20`** **The switch of code is one step, and the old code fences it.** Each writer holds, in its
> writer state, the code generation in force. When an upgrade names code version *H* from input *n*
> (`REQ-RT-16`):
>
> - *H* MUST name the generation one more than the writer's. Every upgrade MUST raise the generation, and so
>   change the bytes of the code that carries it, even an upgrade made to change only another of its
>   artefacts, such as the runtime that executes that code. An upgrade whose code version names another
>   generation, or whose code does not carry *H*'s generation in its bytes, MUST be refused with nothing
>   changed, when its artefacts are checked (`REQ-RT-16`).
> - Before the upgrade is taken, the execution node that drives the writer MUST have fetched *H*'s artefacts
>   and checked them against their digests and *H*'s generation. The switch MUST install only those checked
>   bytes.
> - The code that executes a writer's inputs MUST change from the old code version to *H* in one step, taken
>   after input *n* − 1 has committed and before input *n* is executed. A step that fails leaves the old code
>   in force, and is retried.
> - **The switch is symmetric.** Before each input, the execution node MUST have installed the code version
>   that the code map, as derived from the inputs committed so far, names for that input. That is *H* from
>   *n* after an upgrade, and the old code version again after a move or cancellation at *n*.
> - **The fence.** Code MUST refuse every input unless its own generation equals the writer's generation in
>   force, or, for an input numbered at or after a scheduled upgrade's *n*, equals that generation plus one.
>   The rule holds at every input, whether or not an upgrade is scheduled. A refused input changes nothing and
>   does not take its number, so that the same input runs once the right code is in place. This MUST hold
>   whatever the platform checks or does not check when the code changes. Code of *H*'s generation that takes
>   input *n* makes *H* and its generation the ones in force, as its first effect, unless input *n* moves or
>   cancels the upgrade.
> - **The way out.** A scheduled upgrade MAY be moved to a strictly later input, or cancelled, by an input of
>   its own, authorised under the upgrade-authority mode in force (`REQ-RT-18`). One that is not so authorised
>   MUST be refused with nothing changed. While the old code is fenced, such an input is the only one it
>   takes, and it takes its input number like any other input. Such an input at or after *n* MUST be decided
>   the same whichever of the two generations decides it: the upgrade does not take effect at that input, and
>   the generation in force stays the old one. The execution node then installs the old generation's code
>   again before the next input.
> - The code map MUST be derived from the upgrade inputs and the inputs that move or cancel them, alone
>   (`REQ-RT-17`), so that replay gives every input the generation that decided it.
> - **What the fence cannot see.** When it starts, and at every switch, the execution node that drives a writer
>   MUST check every artefact it runs for the writer, by digest, against the code version the code map names
>   for the next input. Where any differs, it MUST install that code version, and it MUST NOT drive the writer
>   with anything else. If it cannot install it, it MUST refuse to drive the writer. This covers any part of
>   the code that the platform supplies apart from the code that carries the generation, such as the runtime
>   that executes it.
> - How long a writer has been fenced MUST be continuously observable (`NFR-11`), and an alert MUST be raised
>   when it exceeds a bound that the deployment states.
> - The authority that decides an upgrade MUST be the writer's upgrade-authority mode, checked by the writer
>   in its own order (`REQ-RT-18`). Whatever authority a platform has to install or replace programs MUST NOT
>   by itself authorise an upgrade. The execution node that drives the writer MUST install only the code
>   version that the code map names for the next input, so that every install follows from an input the
>   writer committed under that mode.

> **Limits, stated plainly.** Someone who holds a platform's own authority to install programs can still
> install other code than the book names. The book cannot stop that. What it guarantees is that every input
> is executed by the code the code map names, or is refused, and that replay under the code map shows any
> input whose archived outcome other code produced. The operator keeps the platform's install authority as
> it keeps its other keys (`NFR-8`). The fence identifies code up to its generation only: two builds of one
> generation look the same to it. That the bytes running are the code version's own is the execution node's
> check, by digest, before the switch and when it starts. That a new code version decides a move or
> cancellation exactly as the old one does is a property of its code. It is checked on fixture inputs before
> the version is staged, and replay shows any deviation. The fence fails closed: a switch that cannot be
> made stops the writer until it is made, or until the upgrade is moved or cancelled.

## R5 — `REQ-RT-21` (new): a writer continued on another platform

> **`REQ-RT-21`** **A continuation.** A writer MAY be continued by another at input *n*: a **continuation**,
> founded from the first writer's writer state (§2) after input *n* − 1, with its log head.
>
> - The continuation MUST be preceded by an upgrade of the first writer that names the continuation's code
>   version from input *n* (`REQ-RT-16`). The first writer's own code is never switched to the continuation's,
>   so it refuses every input from *n* except a move or cancellation of the upgrade (`REQ-RT-20`), and takes no
>   input at all from *n* once a founding record exists.
> - The continuation MUST be founded only from a first writer whose last committed input is *n* − 1, and
>   whose committed state still schedules, unchanged, the upgrade that names the continuation's code version
>   from *n*. The checks made before the continuation is built and those made after it MUST both find so. Its
>   writer state when it is founded MUST equal the first writer's after input *n* − 1, field by field. Every
>   check of the founding MUST pass before its founding record is written, and every replay checks it again.
>   A founding that fails a check writes no record and founds nothing, and the first writer continues.
> - A founding MUST NOT be made while the next input queued for the writer moves or cancels the upgrade.
>   The founding waits until the first writer has decided that input.
> - The founding record MUST be written before input *n* is sent to either writer. Each attempt to build the
>   continuation MUST be numbered, from 0, and its instance recorded once, in a write-once record of its own
>   beside the founding record. No such record may be kept only in one node's own state. Every retry of the
>   founding, by any execution node, MUST read the latest attempt first. It adopts that attempt's instance if
>   the instance passes the founding's checks. Otherwise it MUST discard that instance and build a new attempt
>   under a new name, never reusing a name. Where the platform lets the instance be named, its name MUST be
>   derived from the founding record's key and the attempt. The founding record MUST name the attempt and the
>   instance that succeeded. From then on, every input from *n* goes to that instance alone. An instance whose
>   founding failed or was abandoned MUST never receive an input. Before it sends an input numbered *n* or
>   later to the first writer, the execution node MUST check that no founding record exists at *n*.
> - Once founded, the upgrade the continuation was founded for is final. The continuation's writer state
>   records the input number it was continued at (§2). An input that moves or cancels the upgrade scheduled
>   from that number MUST be refused with `ERR-InvalidTransition`. The refusal changes nothing and does not
>   take the input's number, like the fence's, so the upgrade takes effect at the next input that is not
>   refused. The continuation's refusal is the authority. The execution node MAY refuse such an input before
>   sending it, with the same effect, as a fast path. A refused input, by this rule or by the fence, is
>   recorded by the execution node outside the book and is never part of it. A move or cancellation of a
>   later upgrade is taken as usual.
> - The input number it was continued at is the one field of the continuation's writer state that the first
>   writer's state does not have. Equality covers every other field.
> - There MUST be at most one continuation of a writer at an input number. Its founding record MUST be written
>   once, under a key derived from the book's genesis, the writer and *n*. A write that finds the same record
>   there, byte for byte, is a resume of the same founding and succeeds. One that finds a different record
>   MUST be refused. A replay MUST refuse a log with two founding records at one *n*. Founding MUST be
>   deterministic: the same writer state gives the same founding record. Only one execution node may feed a
>   writer's inputs at a time, so that a retried founding cannot run beside the first.
> - The archive MUST key a writer's inputs by the **logical writer**, the identity the founding key uses, never
>   by the instance that runs it, so that a first writer and its continuation share one sequence of input
>   numbers. It MUST accept each input number of a logical writer once: an entry at a number it already holds
>   MUST be refused.
> - The input number at which an upgrade is scheduled MUST be claimed once, for one writer. The founding
>   record is the continuation's claim. The first writer's archive entry at that number MUST be preceded by the
>   first writer's claim at the same key. Each side's claim MUST be a fixed function of the book's genesis,
>   the logical writer, the input number, the side, and the book's committed inputs before that number, and,
>   for the continuation's, of the build attempt that succeeded, which is itself recorded once. A
>   claim that finds the same side's identical claim is a resume and succeeds. One that finds the other side's
>   claim has lost and MUST be refused. A founding refused this way founds nothing, and the first writer
>   continues.
> - The archive MUST accept an entry of a continuation at or after a claimed input number only if the claim
>   there is a founding record and the entry comes from the instance recorded beside it.
> - A replay MUST refuse a log that holds both a founding record and a first writer's entry at one input
>   number.
> - The continuation's first input is *n*, and its log entry MUST chain to the log head of input *n* − 1. The
>   book keeps its genesis, its construction and its anchor sequence. A continuation is not a new genesis
>   (`REQ-RC-6`), not a migration of construction (`REQ-PR-11`), and not an export and import (`REQ-RC-3`).
> - The code map is derived from the upgrade inputs (`REQ-RT-17`). The founding record is archived with the
>   inputs, as evidence that replay checks against the upgrade that named it.

*`REQ-RT-19` to `REQ-RT-21` ruled by the owner on 2026-10-17 (amendment 0033).*

## R6 — `US-K7`: twelve acceptance criteria added

> - `AC-K7.9` Given an upgrade naming code version *H* from input *n*, When input *n* reaches a writer whose
>   code is not of *H*'s code generation, Then it is refused, nothing changes, its number is not taken, and
>   once the code is switched the same input runs under *H* (`REQ-RT-20`).
> - `AC-K7.10` Given a writer continued at input *n*, Then the book keeps its genesis and its construction,
>   input *n*'s log entry chains to input *n* − 1's, the first writer takes no input from *n* once the
>   founding record exists, and a replay from genesis crosses the boundary and reaches the archived roots and
>   log at every input (`REQ-RT-21`, `REQ-RT-17`).
> - `AC-K7.11` Given a checkpoint, When a writer is restored from it under a code version that names a
>   different platform than the one it was taken on, and the inputs after it are replayed, Then the writer
>   reaches the checkpoint's roots and every archived root after it (`REQ-RT-19`).
> - `AC-K7.12` Given a writer fenced at input *n*, When an input that moves the upgrade to a later input or
>   cancels it, authorised under the mode in force, reaches it, Then the old code takes it as input *n*; and
>   When the writer stays fenced longer than the deployment's bound, Then an alert is raised (`REQ-RT-20`).
> - `AC-K7.13` Given an upgrade made to change only the runtime that executes a writer's code, Then the new
>   code version still raises the code generation and carries it in its code's bytes, a writer whose switch
>   was missed refuses input *n*, and an execution node whose runtime is not the one the current range's code
>   version names refuses to drive the writer (`REQ-RT-20`).
> - `AC-K7.14` Given a continuation founded at input *n*, When a second founding at the same *n* with a
>   different record is attempted, Then it is refused and nothing is founded; When a founding's writer state
>   differs from the first writer's after *n* − 1 in any field that is not host-local, Then no record is
>   written, nothing is founded, and a corrected founding can still be made; When a founding is retried after
>   a crash and finds its own record, byte for byte, Then it resumes; When the next queued input moves or
>   cancels the upgrade, Then no founding is made until the first writer has decided it; When a founding
>   record exists, Then an input that moves or cancels the upgrade it was founded for is refused with
>   `ERR-InvalidTransition`, its number is not taken, and the next input runs as *n* under *H*, while one
>   that moves or cancels a later upgrade is taken; and When a log holds two founding records at one *n*, Then
>   its replay is refused (`REQ-RT-21`).
> - `AC-K7.15` Given a range whose replayer the code archive holds, When the platform's execution of that
>   range is checked with it, Then every input reaches its archived log hash; and When that replayer is
>   replaced, Then no code version changes (`REQ-RT-19`).
> - `AC-K7.16` Given a writer whose generation in force is *g* + 1 after an upgrade, When code of generation
>   *g* is installed again and an input reaches it, Then the input is refused and nothing changes; and Given
>   an upgrade whose code version names a generation other than the writer's plus one, Then it is refused
>   and nothing changes (`REQ-RT-20`).
> - `AC-K7.17` Given an upgrade scheduled from input *n*, When an input that moves or cancels it is not
>   authorised under the mode in force, Then it is refused, nothing changes, and the upgrade stays scheduled
>   (`REQ-RT-20`, `REQ-RT-18`).
> - `AC-K7.18` Given an upgrade scheduled from input *n* and an authorised input that moves or cancels it,
>   arriving as input *n*, When it is decided by the old code, and When it is decided by the new code because
>   the switch was already made, Then both commit the same outcome: the upgrade does not take effect at *n*
>   and the generation stays the old one; and a replay assigns every input the generation the code map,
>   read from the inputs alone, names (`REQ-RT-20`, `REQ-RT-17`).
> - `AC-K7.19` Given a logical writer whose input *n* is archived, When a second entry numbered *n* reaches
>   the archive, Then it is refused; and Given an upgrade scheduled from *n*, When a move or cancellation
>   committed at the first writer and a continuation's founding both claim *n*, Then whichever claims first
>   stands and the other is refused, whether the first writer's entry reaches the archive before or after the
>   continuation's first segment; When an entry at *n* reaches the archive from a continuation instance that
>   is not the one recorded beside the founding record, or at an *n* whose claim is not a founding record,
>   Then it is refused; and a log holding both is refused by replay (`REQ-RT-21`).
> - `AC-K7.20` Given a founding being made for an upgrade scheduled from *n*, When a move or cancellation of
>   that upgrade commits at the first writer before the founding's checks, before or after the build, have
>   passed, Then no founding record is written, nothing is founded, and the first writer continues
>   (`REQ-RT-21`).

## R7 — §15 traceability

Epic K's row cites `REQ-RT-15`–`REQ-RT-21`.

---

## The plan's binding (`DEC-26`, amended)

Everything below is plan-level and names technology. It changes amendment 0023's (a), (b), (b′), (d) and
(g), and adds (i) and (j). "The runtime of today" is VOS `saga/agents`; "the new runtime" is VOS `vos-core`.

### (a) The manifest names its platform and its generation

- The manifest (`aledgent/code/v1`; not yet built, so its first version takes these fields) gains
  `platform`: the platform's name and revision (for example `vos-saga@c54fc1ff`, later
  `vos-core@<revision>`).
- It gains `generation: u64`, the code generation it runs as (D1).
- Each artefact gains `identities`, a list of `(kind, value)` pairs, in place of the single VOS
  `ProgramId` field:
  - on the runtime of today: the VOS `ProgramId`;
  - on the new runtime: the ELF's hash, the derived program's hash and the transpiler's version (P4).
- The `runtime` role is optional. The new runtime has no runtime program per agent. Its node's default
  agent program is named under `platform`, with its hash as an identity.
- **The generation is compiled in** (D13).
  - Every package is built with a `GENERATION` constant equal to the manifest's `generation`.
  - The constant is a 16-byte record: the tag `ALGGEN01`, then the generation as a little-endian `u64`.
  - It is a `#[used]` static in its own ELF section, `.aledgent.generation`, exported as the symbol
    `ALEDGENT_CODE_GENERATION`, so the optimiser cannot fold it away. The symbol is that section's record
    and nothing else. It is defined once, by a macro in `ledger-core`, and the fence reads that very static
    through the layout `generation_of` uses (D18). No second constant exists for the comparison.
  - One reader, `ledger_core::code::generation_of(bytes)`, returns it. It reads the section from an ELF. From
    a package format that keeps no sections, it reads the single occurrence of the tagged record in the
    read-only data, and refuses a package with none or with more than one.
  - The same reader runs in the build-time check (`check-code`, `T-038-25`), in `Upgrade` staging, and in the
    execution node's checks at start and before a switch. The build fails when the constant differs from the
    manifest.
  - The digest check binds the constant to the code version. Bytes the digest names cannot claim another
    generation. Bytes the digest does not name are never installed (staging, (b)).
- **No replayer in the manifest** (D6). *H* covers only what decides a transition. Each range's replayer is a
  separate code-archive entry, `replayer/<hex H>`, holding the digest of the host image that drives that
  range's platform. The image itself is stored by its digest like any artefact. Rebuilding the host is
  therefore not a code upgrade.
- **Nothing of the platform is hashed** outside the manifest (`REQ-RT-19`, P3, P10):
  - `driver` is the writer's authorised submitter: the runtime's principal today, the core host's session
    key on the new runtime;
  - `observed_slot` stays a diagnostic;
  - these two are the host-local fields of §2 **Writer state**, and no other field is. Both are set when a
    writer is founded, are never carried by a snapshot or a continuation, and never enter a root, a log
    hash, a checkpoint or an anchor;
  - the anchor `v3` commitment is unchanged, and every field of it is aledgent's.
- The hot row gains `generation: u64` beside 0023's `code`, `construction`, `upgrade`, `authority` and
  `authority_next`. Native replay passes each input's generation to the transition. It reads that generation
  from the code map, which is derived from the `Upgrade`, `RescheduleUpgrade` and `CancelUpgrade` inputs
  alone (D10).

### (b) The switch, behind one host operation, and the fence

**Staging.** The host fetches *H*'s manifest and artefacts through `CodeSource`. It checks each digest, that
the manifest's `generation` is the writer's plus one, and that the package's compiled `GENERATION` is that
number. Only then does it submit `Upgrade { code: H, from: n }`. The checked bytes are kept in the host's code
cache. The switch installs those bytes and nothing else.

**The cut at *n* − 1.** While an upgrade is scheduled, the host ends its batch at *n* − 1. A restore point
and a checkpoint therefore land exactly at *n* − 1 (`T-005-15`'s restore points are decision boundaries).

**`switch(H, n)`.** Step 4 of amendment 0023's upgrade becomes one operation of the core host, taken after
input *n* − 1 has committed and before input *n* is sent:

- on the runtime of today: VOS's registry upgrade, a compare-and-swap on the old program hash and the
  installation's revision (P8). It is a signed registry mutation, signed with the core's package key through
  the signer (`F-035`). The host treats "already switched to *H*" as success, not as a stale compare-and-swap;
- on the new runtime: an install of *H*'s package into the same instance (P5). The new runtime offers no
  compare-and-swap, so the host checks the instance's install record before and after. That the instance's
  rows survive a re-install is an assumption (P5), to be shown by the spike before this form is used;
- across platforms: the founding of a continuation on the new platform ((j)).

A host that crashes between the switch and input *n* resumes from the writer's committed state, as before.

**The artefact check** (D9, D17). When the host starts, and at every switch, it checks every artefact it runs
for the writer against the code version the code map names for the next input, by digest. On the runtime of
today, that means the runtime program it hands to `Runtime::create` as well as the package. Where any artefact
differs, the host installs the code map's version, through `switch`, from its staged copy. It refuses to drive
the writer only if it cannot. The in-book fence cannot see the runtime there, because the host supplies it
apart from the package that the registry upgrades.

**The switch is symmetric** (D17). Before each input, the host installs the code version the code map names
for it. That is *H* from *n*, or generation *g* again after a way-out input at *n*. Each install is the same
`switch`, signed through the signer on the runtime of today. The signer's grant covers every install the code
map names, the reinstall included. A crash between a way-out input's commit and the reinstall is resolved at
start, by the same rule.

**The fence** (`REQ-RT-20`, D8). The upgrade handler and this rule are in *H₀*. The rule is checked at every
input, whether or not an upgrade is scheduled.

- Code whose compiled `GENERATION` is the hot row's `generation` *g* takes inputs as usual.
- Code whose `GENERATION` is *g* + 1 takes an input only when `upgrade = Some((H, n))` and the input is
  numbered *n* or later.
- Any other code refuses the input. The refusal is an outcome of its own, added as a new version of the call
  format: nothing is written, `inputs` does not move, and the host reads it as "install the range's code
  first". The continuation's scoped refusal, (j), uses the same outcome and the same version. A refused
  command is audited at the execution node, in its log and the audit store, never in the book (D25).
- The new runtime consumes a failed step, but the book's input number is aledgent's, so nothing of the book
  is consumed.
- Code of generation *g* refuses input *n* and later, except a way-out input. Code of generation *g* + 1
  takes input *n*. Unless input *n* is a way-out input, its first effect is to move `upgrade` into `code` and
  set `generation = g + 1`.

**The way out** (D2, D10).
- `RescheduleUpgrade { from: m }`, with *m* later than its own input number, and `CancelUpgrade` are new inputs
  on `CTR-OPS`, authorised under the mode in force like `Upgrade`. One that is not authorised is refused,
  with nothing changed.
- Code of both generations decides them the same: the upgrade does not take effect at that input, a cancel
  clears `upgrade`, a reschedule moves `from`, and `generation` stays *g*.
- If the host had already switched to *H*, it installs generation *g*'s staged package again before the next
  input, by the symmetric switch.
- `T-037-12`'s gate decides every way-out fixture input under each staged *H* and under generation *g*, and
  requires the same committed state (D18).

**The alarm.** The host exports `aledgent_writer_fenced_seconds`, per writer. An alert fires when it exceeds
`fence_alert_after`, a deployment setting whose default is five minutes (`NFR-11`).

### (b′) The mode is enforced in the book

The upgrade-authority mode stays the only authority over a writer's code (`REQ-RT-18`, `REQ-RT-20`). The host
takes `switch` only for an upgrade the writer has committed. Both platforms' install authorities are host
keys, never authorities over the book:

- on the runtime of today, the core's package key, held by the signer (`F-035`);
- on the new runtime, the node's admin token (P6), held like the core host's other keys (`F-035`, `NFR-8`).

The operator's runbook never installs a package except through `switch`.

### (d) Replay per range: native first, with a replayer per range

- The book side of every range is replayed natively, under the frozen host spec, as ruling `a0023-q6` (a)
  decided. Each range runs under its generation, read from the code map. It needs no platform (P1, P9).
- The writer side re-sends each input to the platform and requires the archived log hash. It runs each
  range under that range's replayer: the host image found at `replayer/<hex H>` in the code archive.
- This is ruling `a0023-q6`'s fallback form, a host build per range, used for the writer-side check only. It
  is kept beside the code version, not in it (D6). The owner's approval of 2026-10-17 covers recording a
  replayer for every range. The book side never uses it.
- An auditor without the old platform can replay a range natively, through the transition over rows in
  memory. They check the platform's execution only where they hold its replayer.

### (g) Checkpoints as rows and leaf chunks

- An archive checkpoint holds the writer state (§2): every hot-row field but `driver` and `observed_slot`,
  the configuration, every record, the unarchived ring entries and the outbox. It holds them as a sorted
  list of key and value, in aledgent's encoding, plus the leaf chunks amendment 0023 (g) already required
  for fast resync.
- A snapshot sent to another node for warp sync has the same format.
- `agent.state` stays the core host's local cache on the runtime of today. It is never archived and never
  sent.
- Checkpoints already written as `agent.state` stay readable by a converter that keeps the runtime of
  today's codec.
- The same archive restores a writer onto either runtime (P2, P7).

### (i) What the upgrade does on each runtime

| Step | Runtime of today | New runtime |
|---|---|---|
| Identity of the code | `ProgramId` of the package and the runtime program | The ELF's hash, the derived program's hash, the transpiler's version; the node's default agent program under `platform` |
| `switch(H, n)` | Registry compare-and-swap, signed with the core's package key through the signer | Install into the same instance, the record checked before and after |
| Safety when the switch is missed | The compare-and-swap, and the fence | The fence alone |
| Who may switch | The writer's mode; the host switches only for a committed upgrade | The same; the admin token is a host key, never an authority |
| Checkpoint | Rows and leaf chunks; `agent.state` a local cache | Rows and leaf chunks |

### (j) The continuation founding (`Continuation`, beside `StateGenesis`)

- `StateGenesis` founds a writer from a chainspec, and stays as it is. A second founding envelope,
  `Continuation`, founds a writer from a predecessor at input *n*. It carries the genesis digest, the
  predecessor's writer id, `at = n`, the log head of input *n* − 1, and the writer state after *n* − 1
  (§2, as (g) encodes it).
- **Order** (D12):
  1. Pre-checks: *H* is staged; the next queued input is not a way-out input (D16); no claim exists at *n*;
     the predecessor's committed `inputs` is *n* − 1, and its committed `upgrade` is `Some((H, n))` (D19);
     and its hot row and log head equal its checkpoint at *n* − 1 ((b)'s cut).
  2. Build the continuation from that checkpoint.
  3. Post-checks: the predecessor's committed `inputs` is still *n* − 1 and its `upgrade` still `Some((H, n))`
     (D19); the continuation's writer state equals the checkpoint's, field by field, and its roots equal the
     host's rebuilt replica.
  4. Write the founding record, as the claim at *n* (D15).
  5. Send input *n* to the continuation.

  A failed check at step 1 or 3 writes no record and founds nothing, so the founding can be corrected and
  made again.
- **Write once** (D3, D12).
  - The founding record is archived under `claim/<hex blake2b-256(genesis ‖ logical writer ‖ n)>`, with a
    conditional create (`If-None-Match: *`) under the archive's Object Lock. The logical writer is the id of
    the writer the book's genesis founded, which every continuation keeps.
  - The record is a deterministic encoding of (genesis, logical writer, *n*, side = continuation), the
    predecessor's state, and the build attempt and instance that passed (D20, D26). A create that finds the
    same bytes at the key is a resume of the same founding, and succeeds. One that finds different bytes is
    refused.
  - **The instance, by attempt** (D23, D26). Each build attempt *a*, from 0, is recorded in
    `instance/<hex blake2b-256(genesis ‖ logical writer ‖ n)>/<a>`: a write-once object, created
    conditionally, whose content is the instance id.
    - A retry, by any lease holder, reads the latest attempt. If that attempt's instance passes the post-checks,
      the retry adopts it. Otherwise the runbook tears the instance down, and the retry creates attempt *a* + 1
      under a freshly derived name.
    - Nothing about an attempt is kept in a node's local state, and no name is used twice, so neither runtime
      needs to allow a name to be reused after teardown.
    - On both runtimes the host names the instance, so the id is derived from (claim key, *a*).
      - On `saga`, this is a change to `Runtime::create_with`. Its creation nonce, from which
        `AgentId::derive` makes the Agent id, is derived from (claim key, *a*) instead of today's one-byte
        `keys.discriminator` (`driver.rs:213-217`).
      - On `vos-core`, it is the install's `instance` argument.
    - A platform that assigns ids itself would build first and then create the attempt's object. A build that
      finds the object already naming another instance abandons its own.
  - The feed routes input *n* and later only to the recorded instance. An instance from a failed or abandoned
    build never receives an input, and the runbook tears it down.
  - Replay refuses a log with two founding records at one *n*.
  - Feeding stays with one core at a time, through `F-038`'s feed lease and its epoch, so a retried founding
    cannot run in parallel.
- **One entry per input number, one claim per boundary** (D11, D15).
  - The archive accepts each input number of a logical writer once. Segments are created conditionally, under
    keys named by the logical writer (never the Agent instance) and their first input number. The archiver
    refuses a segment whose first input is not the last archived input plus one. Because of (b)'s cut at
    *n* − 1, a segment starts at *n* on either side of a boundary.
  - Before the predecessor's archiver writes the segment that holds its entry at a scheduled *n*, it creates
    `claim/<…n>` with its own claim record. That record is a fixed function of (genesis, logical writer, *n*,
    side = predecessor): it holds no lease epoch, time or instance, so a retry by a new lease holder finds
    the same bytes and resumes (D20). If it finds a founding record there, it refuses the segment, and
    the entry is never acknowledged. If the founding finds the predecessor's claim there, it writes nothing,
    founds nothing, and the predecessor continues with its way-out input.
  - Before the feed sends anything numbered *n* or later to the predecessor, it reads `claim/<…n>`, and sends
    nothing once a founding record is there.
  - Once a founding record exists, a `RescheduleUpgrade` or `CancelUpgrade` of the upgrade scheduled from *n*
    is refused with `ERR-InvalidTransition` (D16, D22).
    - The continuation's refusal is the authority. It reads `continued_at = n`, a committed writer-state
      field set by the founding.
    - The feed's refusal is a fast path with the same effect.
    - Either refusal is fence-style: nothing is written, the number is not taken, and the next input runs as
      *n* under *H*.
    - A way-out input for a later upgrade of the continuation is taken as usual.
    - `continued_at` is the one field the equality check leaves out.
  - **The archive checks the instance** (D24, D26). Every segment's header names the instance that wrote it.
    At a claimed *n*, a continuation-side segment is accepted only if `claim/<…n>` holds a founding record and
    the segment's instance is the one recorded for the attempt that record names, `instance/<…n>/<a>`.
    Otherwise it is refused, and nothing in it is acknowledged. The claim that governs a segment is the
    latest claimed *n* at or before its first input.
  - **The refusal's form** (D25). The scoped refusal is the same outcome as the fence's, in the same new
    call-format version. A refused command, from the fence or from this rule, is audited at the execution
    node, in its log and the audit store, and never committed to the book.
  - Replay refuses a log that holds both a founding record and a predecessor entry at one *n*.
- The code map's boundary at *n* comes from the `Upgrade`. The founding record is evidence that replay checks
  against it.
- A move to the new runtime is then:
  1. an `Upgrade` whose code version names the new runtime, from *n*;
  2. the predecessor fenced at *n*;
  3. `Continuation` founded on the new runtime;
  4. input *n* sent there.

  There is no new genesis, no bridge anchor and no change of construction. The next anchor names the new
  code version.
- It also serves on one platform. A continuation onto a fresh Agent of the same runtime is how the gate
  rehearses the move before the new runtime exists.

## Carried to the plan

- `PLAN.md` `DEC-26`: (a), (b), (b′), (d) and (g) amended; (i) and (j) added; the status names this amendment.
- `PLAN.md` `F-036`: `StateGenesis` is unchanged, and `F-037` adds `Continuation` beside it.
- `PLAN.md` `F-037`, and `features/F-037-code-history/plan.md` (new) and `tasks.md`:
  - `T-037-01` to `T-037-03` take the platform field, the identities, the optional runtime role, the
    generation, the host-local fields kept out of every hash, and a commitment built from aledgent's fields
    alone;
  - `T-037-04`, `T-037-05`, `T-037-09`, `T-037-11` and `T-037-12` follow (b), (b′), (d) and (g);
  - four new tasks: `T-037-14` the `switch` operation, `T-037-15` the fence and the way out, `T-037-16`
    checkpoints as rows and leaf chunks, and `T-037-17` the continuation;
  - the table is reordered to build order, with `T-037-14` and `T-037-04` before `T-037-15` (D5).
- `crates/conformance`: `AC-K7.9` to `AC-K7.20` registered as pending.
- `docs/code-history.md`: a section on what keeps the code history independent of the runtime.
- Upstream: nothing is proposed to VOS by this amendment. A compare-and-swap on install, and a way for an actor
  to read its own program's hash, would each make the new runtime's switch safer, but the fence does not
  depend on them.

## Identifier ledger

| Action | Identifiers |
|---|---|
| New | `REQ-RT-19`, `REQ-RT-20`, `REQ-RT-21`; `AC-K7.9` to `AC-K7.20`; glossary rows **Code generation**, **Platform**, **Writer state** and **Continuation**; `T-037-14`, `T-037-15`, `T-037-16`, `T-037-17` |
| Changed | The glossary row **Code version** (two sentences added); `REQ-RT-16` (one point added); §15, Epic K; `DEC-26` (a), (b), (b′), (d), (g), and (i), (j) added; `T-037-01` to `T-037-05`, `T-037-09`, `T-037-11` to `T-037-13` |
| Withdrawn | None |

## Ruling and decisions (2026-10-17)

- **The owner: apply as briefed.** The owner approved the briefing this amendment writes down: the six
  recommendations, as requirements and as the plan's binding above. That includes a replayer recorded for
  every range under ruling `a0023-q6`'s fallback.
- **The architect: D1 to D26**, taken under that approval after the six reviews of #35. They are recorded in
  §"The architect's decisions".
