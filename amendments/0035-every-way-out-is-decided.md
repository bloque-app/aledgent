<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0035 — Every way out is decided, by either generation

**Status:** **ruled by the owner on 2026-10-20**, in the review of `T-037-15` (#44, rulings F2 and N1),
before the genesis code freezes. Applied to `SPEC.md` **v0.4.28** and to `PLAN.md` `DEC-26` (b).
**Target:** `SPEC.md` v0.4.27 as on `develop`.
**Date:** 2026-10-20
**Number:** 0034 is taken by `T-037-05`'s amendment (#45); this is the next free one.
**Rationale in one line:** the way out exists for the book whose switch cannot happen, and then only the
old generation runs; so the old generation must be able to decide every move or cancellation, refusals
included, or one bad request at the head of the queue halts the writer for good.

---

## The problem

Amendment 0033 (D2, D10) gave a fenced writer a way out: an input that moves the scheduled upgrade to a
later input, or cancels it, authorised under the mode in force. `REQ-RT-20` said one that is not so
authorised "MUST be refused with nothing changed", and that such an input at or after *n* is decided the
same by either generation. It did not say what the old generation does with a way out it refuses at or
after *n*.

The first build of the fence (#44) refused such an input as fenced: nothing written, its number not
taken. The review found the consequence:

- An unauthorised cancel, a move to an input that is not later than its own, or a repeated request at the
  head of the queue could then be decided only by the new generation.
- If the new generation is the one that cannot run, which is why the way out is used, the writer stays
  halted, and the valid cancel queued behind the bad one never arrives.

The review also found an input that holds a way out and another command decided incoherently: under the
new generation the cancel was recorded as accepted while the upgrade took effect.

## The ruling

1. **Every way out is decided.** An input that moves or cancels the scheduled upgrade, and nothing else,
   is decided by code of either generation, the same way, at any input number:
   - authorised under the mode in force, and, for a move, to an input later than its own: it takes
     effect;
   - otherwise it is refused as every refused command is: the refusal is recorded, the input takes its
     number, and the upgrade stays scheduled. It takes effect at the next input that is not a move or a
     cancellation.

   A move or cancellation never makes the upgrade take effect, whether it is taken or refused.
2. **A way out is an input of its own.** An input that holds a move or a cancellation and anything else
   (another command, a second move or cancellation, a delivery) is refused whole as
   `ERR-MalformedCommand`. Nothing of it applies, the move or cancellation included, and its number is not
   taken. Code of either generation refuses it the same way, before the fence. No request makes such an
   input, because one command is one input.
3. **The way-out commands are numbered once, in the genesis code.** In the plan's binding (`PLAN.md`
   `DEC-26` (b)), the audit and export codes of the commands of `F-037` are fixed as follows:
   - `RescheduleUpgrade` 51 and `CancelUpgrade` 52 (`T-037-15`);
   - `SetAuthority` 53, `ProposeUpgrade` 54, `ApproveUpgrade` 55 and `ProposeMigration` 56 (`T-037-05`).

## R1 — `REQ-RT-20`, the way-out point

Replace:

> - **The way out.** A scheduled upgrade MAY be moved to a strictly later input, or cancelled, by an input
>   of its own, authorised under the upgrade-authority mode in force (`REQ-RT-18`). One that is not so
>   authorised MUST be refused with nothing changed. While the old code is fenced, such an input is the
>   only one it takes, and it takes its input number like any other input. Such an input at or after *n*
>   MUST be decided the same whichever of the two generations decides it: the upgrade does not take
>   effect at that input, and the generation in force stays the old one. The execution node then installs
>   the old generation's code again before the next input.

with:

> - **The way out.** A scheduled upgrade MAY be moved to a strictly later input, or cancelled, by an input
>   of its own, authorised under the upgrade-authority mode in force (`REQ-RT-18`). Every such input MUST be
>   decided, by code of either generation and the same by both, at any input number. One that is not so
>   authorised, or that moves the upgrade to an input not later than its own, MUST be refused as any
>   command is: its refusal is recorded, it takes its input number, and the upgrade stays scheduled. While
>   the old code is fenced, such inputs are the only ones it takes. Whether taken or refused, such an input
>   at or after *n* does not make the upgrade take effect, and the generation in force stays the old one;
>   the upgrade takes effect at the next input that neither moves nor cancels it. The execution node then
>   installs the code the code map names for the next input. An input that moves or cancels the upgrade
>   and also holds anything else MUST be refused whole, as malformed (`ERR-MalformedCommand`), with nothing
>   of it applied and its number not taken, by code of either generation alike. *Amendment 0035.*

## R2 — `US-K7`, two acceptance criteria

`AC-K7.17` becomes:

> - `AC-K7.17` Given an upgrade scheduled from input *n*, When an input that moves or cancels it is not
>   authorised under the mode in force, before *n* or at or after it, Then it is refused, the refusal is
>   recorded and takes its input number, the upgrade stays scheduled, and code of either generation decides
>   it the same; and When an input moves or cancels it together with anything else, Then the whole input is
>   refused as malformed and nothing of it applies (`REQ-RT-20`, `REQ-RT-18`).

`AC-K7.18` becomes:

> - `AC-K7.18` Given an upgrade scheduled from input *n* and an input that moves or cancels it, authorised
>   or not, arriving as input *n*, When it is decided by the old code, and When it is decided by the new
>   code because the switch was already made, Then both commit the same outcome: the upgrade does not take
>   effect at *n* and the generation stays the old one; and a replay assigns every input the generation the
>   code map, read from the inputs alone, names (`REQ-RT-20`, `REQ-RT-17`).

## What does not change

The fence itself (`REQ-RT-20`, D8), the mode as the only authority over a writer's code (`REQ-RT-18`), the
continuation's scoped refusal (`REQ-RT-21`, D16, D22), and every other acceptance criterion of `US-K7`.

## Plan

`PLAN.md` `DEC-26` (b), "The way out", cites this amendment and carries point 3. `T-037-15` builds it
(`core_binding::fence`, `ledger_core`'s `reschedule_upgrade` and `cancel_upgrade`). `T-037-04` carries the
routes and, as a blocker, their priority over the forwarder's queue and the first-call rule shown end to
end.

## Clarification after the ruling (2026-10-20, owner ruling w6 (B), not part of the approved text)

Added after the owner approved this amendment. It changes neither ruling 1 to 3 nor R1 and R2. R1 says a
move "to an input not later than its own" is refused. Owner ruling w6 (B) adds that a move is refused too
when it is not strictly later than the input the upgrade is scheduled from, so that no member alone brings
an approved upgrade forward. It is recorded in amendment 0034, in its second clarification, which is where
the question of who may take a way out was left.
