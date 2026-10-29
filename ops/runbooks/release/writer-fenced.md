<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: the core's writer is fenced

`REQ-RT-20`, `REQ-RT-18`, `NFR-11`, `REQ-AT-3` · `F-037`, `T-037-15` · **Pages:**
`AledgentWriterFenced` (fenced longer than `fence_alert_after`, five minutes) · **Owner:** platform.

**What it means.** At every input the core Agent compares the rules version compiled into the
package it runs with the rules version its state names (the fence). The code installed is not the code
the writer's state names for the next input, so the Agent refuses every input except a way out of a
scheduled upgrade: nothing is written, no input number is taken, and inputs wait `Received`. It
happens when a switch to a scheduled upgrade was not made, or failed, or when a stale package (an
older rules version) or an early one (the next rules version before its input) was installed.
`aledgent_writer_fenced_seconds` says how long; the core's standard error and the audit store's node
trail (`node.log`) hold one `fenced` record per refusal, naming the input, the rules version installed
and the rules version the state names.

## Preconditions

- [ ] Read the latest `aledgent-core: AUDIT: input … refused by the fence` line: the rules version
  installed, the rules version in force, and the scheduled upgrade with the input it applies from.
- [ ] Read which program the writer is in (`T-037-23`): `GET /v1/ops/signals` with the operations
  principal returns `writer_code` with the version in force (`code`), its `generation` and the
  `upgrade` scheduled (the version and the input it applies from), as of the last input that
  committed; the exporter shows the same as `aledgent_writer_generation{code}` and, while an upgrade
  is scheduled, `aledgent_writer_upgrade_scheduled_input{code}` (the label is the first eight bytes
  of the version in hex).
- [ ] Read the core's standard error for what happened to the code, one line each, naming the input
  (best effort: if a send was ambiguous and the input did commit, its line can be missing, and the
  signals above are the exact record):
  `input N is the first to run code …` (a switch took effect), `input N took the way out: …`,
  `input N scheduled an upgrade to code …`, and `the platform now runs code … for input N` (an
  install). Before an upgrade was recorded, `aledgent-core: refused an upgrade to code …` and the
  front door's `an upgrade was refused before it was logged` name the reason (another runtime,
  another package key, or code that could not be fetched or staged).

## Steps

1. **Check whether the switch happened.** If the state names an upgrade that applies (its input is
   the next one) and the installed package is the old rules version, the switch to the code map's
   version was missed: the core host installs it through `switch` at start and before each input
   (`T-037-14`), from its staged copy. Restart the core host so it runs that check; never install a
   package by hand outside `switch` (`REQ-RT-20`: the platform's install authority is not an
   authority over the book).
2. **If the new code cannot run**, take the way out. The door (`T-037-22`) refuses before recording
   a version that names another runtime than the host runs, or whose package has another key,
   state layout, constructor ABI or lane requirements than the package running. It does **not**
   check everything the runtime checks: the claim over the program running (`T-037-19`) is not
   checked yet, and on the VOS this tree pins a stateful actor cannot change program at all, so a
   version that passed the door can still be refused when the switch tries it, and a package that
   is broken or a platform that failed reaches here too. A `RescheduleUpgrade` to a later input or a `CancelUpgrade`, authorised under the upgrade-authority
   mode in force (`CTR-OPS`). The old code decides it even while fenced. Check that it reached the
   writer: a way out queued behind other inputs does not overtake them until `T-037-04`'s route gives
   it priority. After a way out at the upgrade's input, the core host installs the old rules version's
   package again before the next input.
3. **If the installed package is a rules version the state never names** (a stale package after a
   switch, or one installed early), the code map's version was replaced outside `switch`: treat it as
   an incident, find who installed it, and restart the core host so it installs the code map's
   version.
4. Confirm `aledgent_writer_fenced_seconds` is back to 0 and inputs commit again.

**Notes.** (a) Roll the front door out before the core: an older front door re-serialises the
signals and drops `writer_code`, which then reads as "not reported". (b) The door compares a
version with the package the host runs. If a host starts without the bytes of that package (when
the core restarts on a version it must re-stage, `T-037-21`, and the re-staging failed), it refuses
every later `Upgrade` with "the package this host runs is not held" until the host restarts with
them: that is a refusal by the host, not by the version.

Nothing here changes the book's storage: every step is an input through the command surface or a
restart (`REQ-AT-3`).

## Exercised

On every change: `ops/tests/alerts.rs` checks that `AledgentWriterFenced` pages the test route past
five minutes; `core-binding`'s `tests/fence.rs` and `tests/agent_core.rs` exercise the fence, the way
out and the node's audit. The switch and its check at start are `T-037-14`'s.
