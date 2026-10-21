<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0038 — An upgrade is scheduled by time; the writer fixes its input number

**Status:** **awaiting the owner's approval.** Drafted on 2026-10-21 from the owner's ruling w20 (b) in the
Decisions form: "limit by time instead of by input number". Nothing here is applied to `SPEC.md` or `PLAN.md`,
and no code is changed. Three questions for the owner are at the end (§"Questions for the owner"); the design
does not depend on their answers, only some limits do.
**Target:** `SPEC.md` v0.4.28 as on `develop` (at `c3461c0`); `PLAN.md` `DEC-26` (b) and (b′); `F-037`.
**Date:** 2026-10-21
**Number:** 0037 is taken by PR #52 (per-transaction approval at ingress); this is the next free one.
**Amends:** §2 (one glossary row); `REQ-RT-16`; `REQ-RT-20` (its opening, the way-out point and the code-map
point); how `REQ-RT-21` and `US-K7` read "an upgrade from input *n*"; `US-K7` gains five acceptance criteria.
Nothing is withdrawn. The input-number form stays decidable for every input already in an archive.
**Serves:** `REQ-RT-16`, `REQ-RT-17`, `REQ-RT-18`, `REQ-RT-20`, `REQ-RT-21`, `REQ-RT-7`.
**Rationale in one line:** the book's input number moves at no fixed rate, so a limit counted in inputs cannot be
turned into a date; the book already keeps one clock that only moves forward, so an upgrade can name a time on
that clock, and the writer turns it into an input number at the moment the clock passes it.

---

## The problem

Today an upgrade says "code version *H* from input *n*" (`REQ-RT-16`, amendment 0023 (b)). Everything after it
is built on that number: the fence (`REQ-RT-20`), the host's switch before *n*, the batch cut at *n* − 1, the
way outs (move to a later input, cancel), staging, the continuation (`REQ-RT-21`) and replay.

The plan limits how far ahead *n* may be: `MAX_LEAD` = 2¹⁷ inputs after the input that decides it (amendment
0034, its first and third clarifications; owner ruling w9). That text says "about 91 days". It is only true at
the slowest rate the ticker allows, one input a minute. At one input a second it is about 36 hours; at ten a
second, about 3.6 hours. Production has no traffic history yet, so nobody can say which. An operator who wants
the upgrade on a given day cannot pick *n*, and cannot know whether the day is even within the limit.

The owner ruled (w20 (b)) that the limit is a time, and the upgrade is planned by time.

## What the code does today (verified on `develop` at `c3461c0`)

- **Where *n* lives.** The writer's hot row holds `upgrade: Option<([u8; 32], u64)>`, the code version and the
  input number (`core_binding::hot::Hot::upgrade`). The fence reads it (`fence::WriterCode::due`: `seq >= n`), so
  do `code_for`, `switch::target_code` and the host's `exec_code_of`. The library sees it through
  `Ledger::set_scheduled_upgrade`, and schedules a new one through `Ledger::take_upgrade`, which
  `fence::decide` writes into the hot row. All three callers (`transition.rs`, `mapped.rs`, `host.rs`) use that
  one seam.
- **An input carries no time of its own.** `InputEnvelope` (`envelope.rs`) holds the sequence number, the calls,
  the deliveries and blob keys. Time is per call: a command's `Ctx::now` is the time ingress forwarded it
  (`forwarded_at`, else `received_at`: `services/ingress/src/service.rs`, `input_of`); a tick carries the time
  the ticker read; a delivery carries its own time. These times come from whichever ingress instance handled
  the row, and they are **not monotone in input order**: reservations go to the front of the queue, a way out
  overtakes the queue (`T-037-04`), and a row carried from an ambiguous send is fed again later (`F-038` §3,
  amendment 0036).
- **The book has one clock that never goes back.** The operation store keeps a high-water mark of time
  (`lifecycle::store`, the `LIFECYCLE_CLOCK` leaf). Only a tick raises it (`Ledger::tick_chosen`,
  `OpWrite::Clock`). A tick that brings an earlier time is answered `ignored` and changes nothing; a write that
  would lower the mark is refused (`OpRefusal::ClockBackwards`). It is part of the service tree, so the service
  root covers it, and a replay of the archived ticks reproduces it exactly. The Agent replays a recorded tick
  through the same function (`Ledger::tick_due` → `tick_chosen`).
- **The ticker.** The ingress leader logs a tick every `tick_interval`. It skips one only while the last tick is
  younger than `tick_heartbeat` (60 s by default) **and** the core says nothing is due before now
  (`Ledger::next_due`). So the book clock lags real time by at most about a minute while the ticker is alive.
- **The host knows each input before it sends it.** It runs the input natively on its replica, computes the
  writer's code after it (`Driver::decided`), and only then switches and sends. A batch holds inputs of one code
  version: the host flushes when the next input's code differs (`other_code` in `Driver::stage`). An upgrade
  that the replica schedules for the very next input already cuts the batch there.

## The question, and three ways to answer it

If an upgrade is scheduled as (*H*, time *T*), which input is "the input *n* where the new program takes
effect"? The host must install *H* **before** it sends the first input *H* must decide, so whatever defines *n*
must be known to the host before that input is sent, and must be decided the same by every replay.

| | (1) Keep *n*; the operator picks it; limit it in time | (2) *n* is the first input whose own time is at or after *T* | (3) Two steps: the clock makes it due, the next input is *n* |
|---|---|---|---|
| How | The deciding input checks that *n* will be reached within 91 days | The fence admits *g* + 1 for an input stamped at or after *T* | When an input brings the **book clock** to *T* or past it, the writer records *n* = that input + 1 |
| Who knows *n* first | Everyone, from the start | Nobody until that input arrives; the host must read its stamp | The host, as soon as it has run the input that fixes it, before it sends *n* |
| Failure | No fixed rate, so "within 91 days" cannot be checked. The limit would be a guess, which is the problem w20 removes | An input has no single time (one per call, one per delivery). Stamps are not monotone: input 10 stamped after *T* and input 11 stamped before it would run *H*, then *g*. A way out at 10 followed by an earlier-stamped input would flip the switch *H* → *g* → *H*. A row fed again under a later lease can carry a different stamp, so whether it is "after *T*" can change between two sends of one input. Fixing that needs a latch in committed state, which is option (3) | The clock lags real time by up to the ticker's interval (about a second, a minute at worst); a stalled ticker delays the upgrade |
| Cost | Small, and does not meet the ruling | A fence that reads stamps, a latch, a switch that can flap | One record, one check in the tick, new call forms; the fence, the switch, the batch cut, the continuation and replay are unchanged |

**Chosen: (3), with the book clock as the trigger.**

## The design

### Two phases

An upgrade scheduled by time is first **pending**, then **fixed**.

- **Pending.** The upgrade names *H* and a time *T* on the book clock. It has no input number. Every input runs
  under the code in force, and the fence admits only generation *g*, as when nothing is scheduled.
- **Fixing.** When an input brings the book clock to *T* or past it (today that is always a tick), the writer
  fixes *n* as the input after it, or later if the mode's delay requires (below). This happens in that input's
  own transition, decided by generation *g* alone, since the input comes before *n*.
- **Fixed.** From then on the state is exactly today's `upgrade = Some((H, n))`: the fence, `switch(H, n)`, the
  batch cut, the restore point at *n* − 1, `NotRun`, the continuation's checks and replay all work as they do now.

The input that fixes *n* is input *n* − 1 (when no delay holds it back), so the host's batch ends there, and the
checkpoint that tick may cut lands at *n* − 1, which `T-037-14` already asks for.

### Where the pending upgrade lives

In a new record of the service tree, `upgrade-at` (a new leaf kind, next to the upgrade-authority record), holding
*H*, *T* and the earliest input it may be fixed at (`not_before`, below). It exists only while an upgrade is pending.
Absent, it adds no leaf, so a book that never schedules by time keeps every root it has today.

The hot row does not change: no field, no type, no layout. This was chosen over changing the type of `upgrade` to
"either an input or a time". That alternative would change the bytes of a hot row that holds an upgrade. The
first time this matters is the upgrade *into* the first code version that knows the time form, scheduled by the
code before it: at input *n* the new code would have to read a hot row that holds an upgrade in the old encoding.
The record avoids that, and it avoids a change to `Fenced`, the refusal the Agent sends its host, which
carries `upgrade` and which the execution node audits.

### The limits

- **Longest lead:** *T* at most **91 days** after the book clock at the input that decides it, in integer
  milliseconds (`MAX_LEAD_TIME` = 7 862 400 000 ms; `INV-7`, no floating point). It holds for an upgrade, a
  proposal of one, its approval, and a move. (Question Q2.)
- **Shortest lead:** *T* later than the book clock at that input. No other minimum: the next tick fixes it.
- **A book with no clock yet** (no tick ever decided): every time-form input is refused with
  `ERR-InvalidTransition`. In practice a book has a clock within a minute of founding.
- **The mode's delay.** Today it is counted in inputs (`Mode::delay`, `check_from`, at most `MAX_DELAY` = 2¹⁶).
  This draft keeps it so (question Q1) and keeps it exact: the authorising input records
  `not_before` = its own number + delay + 1, and *n* is fixed as the later of (the fixing input + 1) and
  `not_before`. With no delay, `not_before` is its own number + 1 and never binds.

### Every way out, in both phases

One scheduled upgrade at a time, of either form. "Refused" below means refused as every refused way out is
(amendment 0035): recorded, the input takes its number, the upgrade stays as it was.

| State | Cancel | Move by input number (today's form) | Move by time to *T′* (new form) |
|---|---|---|---|
| Nothing scheduled | Taken, changes nothing (today) | Today's rule; changes nothing | Taken if *T′* is within the limits; changes nothing |
| Pending at *T* | Taken: the record is removed | Refused (`ERR-InvalidTransition`): there is no input to move | Taken if *T′* > *T*, *T′* > the book clock and within the longest lead: the record holds *T′*, `not_before` kept |
| Fixed at *n* | Today: the hot row's upgrade is cleared | Today's rule, unchanged (w6, w9) | Taken if *T′* > the book clock and within the longest lead: the hot row's upgrade is cleared and the record holds *H*, *T′* and `not_before` = *n* + 1 |

- **Owner ruling w6 in time.** A move never brings the upgrade earlier. Pending: *T′* > *T*. Fixed: `not_before`
  = *n* + 1, so the new *n′* is strictly after *n*, whether *n* was fixed from a time or scheduled by number.
- **Owner ruling w9 in time.** A move goes at most the longest lead after the book clock at its own input.
- **Fixed by time means fenced next.** When no delay holds it back, *n* is the input right after the fixing one,
  so a way out that arrives after fixing is decided at *n* or later, inside the fence window, by either
  generation the same (amendment 0035). That is what the fence window is for.
- **Both generations decide the new forms the same.** That requires both to know them: true for every upgrade
  from a code version that knows the time form. The `code-history` gate's way-out fixtures (`T-037-12`, D18) must
  include the new move.

### Upgrades in both phases

| State | `Upgrade` by input number (today's form) | `UpgradeAt` by time (new form) |
|---|---|---|
| Nothing scheduled | Today | Writes the record |
| Pending | Replaces it: the record is removed and the hot row's upgrade is set (today's rule otherwise) | Replaces it |
| Fixed | Replaces it (today's rule, unchanged) | Refused (`ERR-InvalidTransition`): cancel or move first |

A change of mode (`SetAuthority`) cancels pending *proposals*, not an upgrade already authorised; a pending
upgrade by time stands, as a scheduled one does today (`REQ-RT-18`). Fixing *n* authorises nothing: it is the
consequence of a schedule the mode in force authorised, so it happens even while a change of mode is scheduled.

### The input-number form stays, and its decisions do not change

The call format is frozen (ruling `a0023-q6`): native replay applies every archived call with the current host's
library, so a call's decision must never change in place. So:

- `Call::Upgrade`, `Call::RescheduleUpgrade`, `Input::ProposeUpgrade` and `Action::Upgrade` keep their fields,
  their tags and their decisions in every state an archive can hold.
- The time form is added: `Call::UpgradeAt { ctx, code, from_time, justification }`,
  `Call::RescheduleUpgradeAt { ctx, from_time, justification }`, `Input::ProposeUpgradeAt { code, from_time }`
  and `Action::UpgradeAt { code, from_time }`, appended after `Call::Upgrade` and before the conformance ports, so
  no existing tag moves. Their audit and export codes are 58, 59 and 60 (`upgrade_at`,
  `reschedule_upgrade_at`, `propose_upgrade_at`), the next free ones after amendment 0035's numbering and
  `Upgrade`'s 57.
- An old-form input behaves differently only in a state that only new-form inputs can create (a pending
  record), so every archived input replays as it was decided.
- The door (ingress, `CTR-OPS`) offers the time form for a writer whose code in force knows it, and the number
  form only for a writer whose code does not (question Q3).

### The ticker

`Ledger::next_due` also returns a pending upgrade's *T*. The ticker then logs a tick as soon as its clock reaches
*T*, instead of waiting up to the 60 s heartbeat. The upgrade is fixed within about one `tick_interval` of *T*,
and within the heartbeat if `next_due` cannot be read.

## Why it holds

- **Determinism.** *n* is a function of the record, the book clock after the input, and the input's number. All
  three are committed state or the input itself, and the replica and the Agent compute them with one function
  (`tick_chosen`). The fixing input comes before *n*, so only generation *g* ever decides it.
- **No input decided by the wrong program.** *n* is committed, in the hot row, by input *n* − 1 (or earlier with a
  delay) before input *n* is sent. The host computes the same *n* on its replica before it sends anything after
  the fixing input, and its batch cut and switch follow, as they do today for an upgrade scheduled for the next
  input. If the host still sends *n* to the old code, the fence refuses it (`NotRun`, amendment 0036).
- **The way out always works.** While pending there is no fence: a cancel or move is an ordinary input. Once
  fixed, it is today's fenced way out, decided by either generation. A refused move leaves the state as it was.
- **Replay and restore.** The record is in the service tree, so a checkpoint carries it (`T-037-16` holds every
  record) and the roots cover it. Replay reruns the ticks, so the clock, and *n*, come out the same. Every ring
  entry already names its code, so the code map read off the archive is unchanged.
- **A crash.** Pending: nothing to do at start except keep *H* staged (below). Between the fixing input's commit
  and the switch: the hot row holds (*H*, *n*), so the start check installs *H* (`T-037-14`), as today. Between
  the switch and *n*'s commit: unchanged.
- **The ticker.** It decides *when*, never *what*. A stalled ticker (`RISK-7`) delays the fixing; nothing runs
  under the wrong code.
- **Clocks that disagree.** The book clock only moves forward, so ticks out of order, ticks below the mark and two
  ticks with one time cannot move *n* back or fix it twice. A command's own time never touches it.

## Trade-offs, stated plainly

- **The operator's clock decides the day.** The book clock is the ingress leader's clock (`DEC-6`; `OQ-36`, which
  clock is authoritative, is still open). A leader whose clock runs ahead raises the book clock for good, and
  fixes every pending upgrade early in real time, as it already fires every deadline early. Today the operator
  can bring a numbered upgrade forward in real time by generating inputs. Neither form defends against the
  operator's own infrastructure; the mode's authorisation does, and is unchanged.
- **The delay stays in inputs** in this draft, so it still shrinks in real time when traffic grows. Q1 asks
  whether to move it to time as well.
- **Changes of mode stay in inputs** (`SetAuthority { from }`, `MAX_LEAD` 2¹⁷ inputs), with the same unknown
  rate. Q1 asks about them too. The "about 91 days" in amendment 0034's first clarification should read "about 91
  days at the ticker's floor of one input a minute, and less at any real traffic".
- **Up to 91 days of waiting means the core host will restart in between.** A restart today loses the staged
  version (gap G2 in the rehearsal runbook); PR #55 (`T-037-21`) re-stages it at start. Scheduling by time makes
  that a hard prerequisite: the time form must not be offered by the door before it is merged.
- **Two forms are carried for good.** The library keeps the number form for replay, whatever the door offers.
  That is code that already exists and is already tested.
- **The upgrade lands within about a second of *T*, not at *T*.** Within the heartbeat if `next_due` fails.

---

## R1 — §2, one glossary row

> | **Book clock** | The latest time the book's inputs have brought in to advance its scheduled work (`REQ-RT-5`, `REQ-RT-7`). It never goes back: an input that brings an earlier time leaves it where it is, and two inputs may bring the same time. |

## R2 — `REQ-RT-16`

Replace the requirement's first paragraph:

> **`REQ-RT-16`** **An upgrade is an input.** The code version of a writer MUST change only through an input
> in that writer's own sequence: an **upgrade**, which names the new code version by its digest and the
> input number from which it applies.

with:

> **`REQ-RT-16`** **An upgrade is an input.** The code version of a writer MUST change only through an input
> in that writer's own sequence: an **upgrade**, which names the new code version by its digest and the time,
> on the book clock, from which it applies.
>
> - The time MUST be later than the book clock when the upgrade is decided, and no further ahead of it than a
>   longest lead that the deployment states. An upgrade that breaks either, or that is decided before the book
>   clock holds any time, MUST be refused with nothing changed.
> - The writer MUST fix the input from which the upgrade applies, *n*, as the input after the first input that
>   brings the book clock to that time or past it, and no earlier than the upgrade-authority mode's delay allows
>   (`REQ-RT-18`). Until *n* is fixed, every input runs under the code in force. Once fixed, *n* is part of the
>   writer's committed state. *Amendment 0038.*

and, in the second point, "No input before the named number" becomes "No input before *n*".

## R3 — `REQ-RT-20`

- Its opening, "When an upgrade names code version *H* from input *n* (`REQ-RT-16`):", becomes "When an upgrade
  names code version *H* and its input *n* has been fixed (`REQ-RT-16`):".
- In the way-out point, replace "A scheduled upgrade MAY be moved to a strictly later input, or cancelled," with
  "A scheduled upgrade MAY be moved to a strictly later time, or cancelled," and replace "or that moves the
  upgrade to an input not later than its own," with "or that moves the upgrade to a time not later than the one
  it was scheduled from or than the book clock at its own input, or further ahead than the longest lead
  (`REQ-RT-16`),". Add at the end of the point: "A move of an upgrade whose *n* is fixed unfixes it; its new *n*
  is fixed as `REQ-RT-16` says, and never at or before the old one. *Amendment 0038.*"
- In the code-map point, "derived from the upgrade inputs and the inputs that move or cancel them, alone" becomes
  "derived from the upgrade inputs, the inputs that move or cancel them, and the inputs that advance the book
  clock, alone".

## R4 — how `REQ-RT-21` and `US-K7` read

In `REQ-RT-20`, `REQ-RT-21` and the acceptance criteria of `US-K7`, "an upgrade (scheduled) from input *n*", "an
upgrade naming *H* from input *n*" and "names the continuation's code version from input *n*" read "an upgrade
whose input *n* the writer has fixed (`REQ-RT-16`)". A continuation is founded only once *n* is fixed. No other
word of them changes.

## R5 — `US-K7`, five acceptance criteria

Added after `AC-K7.22` (amendment 0036). `AC-K7.23` is skipped: amendment 0036's draft used it for a criterion the
owner kept in the plan, and reusing the number would make that history ambiguous.

> - `AC-K7.24` Given an upgrade to *H* scheduled from time *T*, When inputs arrive while the book clock is before
>   *T*, Then each runs under the code in force and no input number is fixed; and When an input brings the book
>   clock to *T* or past it, Then that input is decided by the code in force, the next input is *n*, and input
>   *n* runs under *H* (`REQ-RT-16`, `REQ-RT-20`).
> - `AC-K7.25` Given an upgrade scheduled from time *T*, When inputs bring times out of order, the same time
>   twice, or a time before the book clock, Then the book clock does not go back and *n* is fixed once, at the
>   input after the first one that brings the book clock to *T* or past it (`REQ-RT-16`, `REQ-RT-7`).
> - `AC-K7.26` Given an upgrade or a move whose time is not later than the book clock, or further ahead of it
>   than the longest lead, or a book whose clock holds no time yet, Then it is refused, the refusal is recorded,
>   and what was scheduled stays as it was (`REQ-RT-16`, `REQ-RT-20`).
> - `AC-K7.27` Given an upgrade scheduled from time *T*, When a move to a time not later than *T* is decided,
>   Then it is refused; and Given an upgrade whose *n* is fixed, When an authorised move to a later time is
>   decided at *n*, by the old code or by the new, Then both commit the same outcome: *n* is unfixed, the
>   generation stays the old one, and the new *n* is fixed after the old one (`REQ-RT-20`).
> - `AC-K7.28` Given an archive that holds upgrades scheduled by time, When it is replayed from genesis, Then
>   every input runs under the code version its archived entry names, and each *n* is fixed at the same input as
>   it was (`REQ-RT-17`, `REQ-RT-3`).

## Criteria that stay in the plan

What follows names plan facts (the hot row, the call format, the ticker), so it is `T-037-22`'s own criterion, not
`SPEC.md`'s:

> **`T-037-22` criteria.** The hot row's type, layout and bytes do not change, and
> `t037_15_the_fence_changes_nothing_of_a_book_that_never_upgrades` passes with its pin unchanged; `MODE_DIGEST`
> is unchanged; every number-form call is decided as before in every state an archive can hold; a fixing tick
> ends the host's batch, and a restore point lands at *n* − 1; `next_due` includes a pending *T*; with a delay of
> *d* inputs, *n* is not fixed before the authorising input + *d* + 1; the door refuses the time form for a writer
> whose code does not know it.

## The plan (informative)

### What changes in merged code

| Where | What |
|---|---|
| `crates/ledger-core/src/call.rs` | `Call::UpgradeAt`, `Call::RescheduleUpgradeAt`, appended after `Call::Upgrade`; method names |
| `crates/ledger-core/src/state.rs`, `audit/mod.rs`, `export/records.rs` | `Command::UpgradeAt` (58), `RescheduleUpgradeAt` (59), `ProposeUpgradeAt` (60) |
| `crates/ledger-core/src/leaves.rs`, the store | The `upgrade-at` leaf kind and its slot; absent while empty |
| `crates/ledger-core/src/ledger/operator/upgrade.rs` | `upgrade_at`, `reschedule_upgrade_at`; `cancel_upgrade` also removes a pending record; the tables above |
| `crates/ledger-core/src/ledger/lifecycle.rs` | `tick_chosen`: after the clock is raised, fix a pending upgrade (`self.upgrade = Some((H, n))`), through the existing `take_upgrade` seam |
| `crates/ledger-core/src/code/authority.rs`, `ledger/upgrade_authority.rs` | `Input::ProposeUpgradeAt`, `Action::UpgradeAt`; `check_from_time` (the time limits) beside `check_from`; approval writes the record with `not_before` |
| `crates/ledger-core/src/code.rs` | `WayOut::RescheduleAt { from_time }`: `apply` clears a fixed upgrade (the library writes the record) |
| `crates/ledger-core/src/ledger/anchoring.rs` | `next_due` includes a pending *T* |
| `crates/core-binding/src/fence.rs` | `way_out_of` recognises `RescheduleUpgradeAt`. `admit`, `decide`, `WriterCode` unchanged |
| `services/ingress/src/operator.rs`, `surface.rs`, `openapi.json`, `wire.rs` | The time form on the `CTR-OPS` routes, behind the writer's code knowing it |
| `ops/runbooks/release/` | Scheduling by date; reading the pending record |

Not changed: `hot.rs` (the `Hot` struct, `LAYOUT`, `LOG_DOMAIN`), `envelope.rs` (`Fenced`, `InputEnvelope`),
`switch.rs`, `host.rs`'s batch cut, the continuation's checks (`T-037-17`), checkpoint `v3`.

### Migration

- **No upgrade exists in any book.** Production runs `v0.2.0` (`bloque-ledgent` `deploy/prod.json`), which has
  none of the upgrade calls: #44 to #48 are not in that tag. Test's ingress runs `v0.2.0-dev+c3461c0`, but no node
  sets a code source (`Core::set_code_source` is called by tests only), so every `Upgrade` is refused before it
  is logged; there is no ingress route for `SetAuthority`, `ProposeUpgrade` or `ApproveUpgrade`; and test's core
  runs `--unnamed-code` (runbook gap G6). A way out (move or cancel) with nothing scheduled can have been logged on
  test through the routes #47 added. Test's archive was not read for this draft.
- **Every archive stays replayable whatever it holds**, because the number-form calls keep their tags and their
  decisions, the new calls are appended, and the hot row and ring entry do not change. That is the guarantee;
  the evidence above only says nothing needs it yet.
- **Pins.** The hot-row pin of `t037_15_…` and `MODE_DIGEST` stay unchanged (no hot-row field; `Mode` untouched).
  A new pin covers a book that schedules, fixes and takes an upgrade by time.

### Order against the rehearsal on test

The rehearsal (`handovers/2026-10-21-f037-rehearsal-runbook.md`) can run first, on input numbers. Everything after
*n* is fixed is unchanged here, and that is what the rehearsal tests: staging, the switch, the fence, the way outs,
the revert and restarts. Running it first is **safer**: this change adds a check to the tick, the commonest input,
and rehearsing proven machinery before adding to it keeps two sources of failure apart. It is **cheaper** only if the
time form lands before the code version that founds the final book is cut: then one founding carries it. If it
lands after, the first upgrade of that book is scheduled by number, and that upgrade can be the one that brings the
time form. After this lands, re-run only the rehearsal's scheduling phases (E, F, G) with times, and watch one
fixing tick.

## Proposed task rows (not applied to `features/F-037-code-history/tasks.md`)

| ID | Task | Traces | Done when |
|---|---|---|---|
| T-037-22 | Upgrades scheduled by time (amendment 0038): the `upgrade-at` record; `UpgradeAt`, `RescheduleUpgradeAt`, `ProposeUpgradeAt` (codes 58 to 60) beside the number forms, which keep their decisions; the fixing in the tick (*n* = the later of the fixing input + 1 and `not_before`); the longest lead of 91 days and the shortest (later than the book clock); a book with no clock refuses; the way-out and upgrade tables; `next_due` includes *T*; the door's time form, offered only for a writer whose code knows it and only once `T-037-21` is merged | `REQ-RT-16`, `REQ-RT-20`, `REQ-RT-17`, `REQ-RT-18`, `REQ-RT-7` | `AC-K7.24` to `AC-K7.28` pass, and the plan criteria above; the `code-history` fixtures (`T-037-12`, D18) include a move by time decided by both generations |
| T-037-23 | *Only if the owner answers Q1 "yes":* the mode's delay and changes of mode by time: `Mode` gains a delay in milliseconds under a new mode domain (`aledgent/upgrade-authority/v2`), `SetAuthority` gains a time form, `MAX_DELAY` and `MAX_LEAD` for modes become 45 and 91 days | `REQ-RT-18` | `AC-K7.7` and `AC-K7.8` pass under the time form; a mode with a delay gives the same refusals at any input rate |

**Size, honestly.** `T-037-22`: about 5 days of a developer's work (4 to 6): 2 in the library (calls, record, tick,
tables, authority), 1 in the binding and its tests across the replica, the Agent and the mapped layout, half a day
in ingress, 1 for the acceptance tests and the new pin, half a day for the runbook. Review rounds on this repository
have taken one to two more days per PR. `T-037-23`: about 1.5 days more.

## Questions for the owner

Each is explained from zero; the draft takes the default if there is no answer.

- **Q1. Should the "waiting period" and changes of who may upgrade also be measured in time?** A mode can say "an
  upgrade must wait at least *d* inputs after it is approved", so that the other members have time to react. A
  change of mode (who may approve upgrades) is also scheduled "from input *n*", at most 131,072 inputs ahead. Both
  have the same problem w20 fixed for upgrades: in real time, the wait shrinks as traffic grows. *Recommended:* yes,
  as `T-037-23` (about 1.5 days), while no book has a mode record. *Default:* no; both stay in inputs, and this
  amendment keeps the delay exact in inputs.
- **Q2. How far ahead may an upgrade be planned, and how soon?** *Recommended:* at most 91 days ahead, and any time
  later than the book's clock (the next minute is allowed). *Default:* the same.
- **Q3. If the code that founds the final production book is cut before this lands, is it acceptable that its
  first upgrade is planned by input number?** That code would know only the number form, so the first upgrade must
  use it, planned close to the day, as today. The upgrade that brings the time form can be that first one.
  *Recommended:* yes; do not hold the founding for this. *Default:* yes.

## Not chosen

- **(1)**: a limit in time cannot be checked against a number that moves at no fixed rate.
- **(2)**: it needs a stamp an input does not have, and stamps that are not monotone make the switch flap; fixing it
  needs a latch, which is (3).
- **Changing the hot row's `upgrade` to "an input or a time"**: it changes the hot row's bytes, and the code that
  first knows the time form would have to read an upgrade written in the old encoding at its own *n*.
- **Raising `MAX_LEAD` in inputs** (w20 (a)): not ruled.
