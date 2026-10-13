<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-031 — stress fixes in ingress: parked notifications, batched marks, a ticker that waits

Fixes for the stress suite's findings (F-032, in the stress workflow's run summaries) that live in
ingress: F5, F13 and F2. Tasks `T-031-15` to `T-031-17`. To be folded into `NOTES.md`.

## T-031-15 — F5: notifications without a recipient

**Before.** A notification owed to a principal with no recipient was attempted on every pass of the
delivery worker, a failed-attempt row each time, with backoff capped at a minute and no end: after
ten minutes at 10 × the reference hour, 21 426 undelivered and 124 490 failed attempts; after the
four-hour soak, 104 888 undelivered and 7.0 million attempt rows. On the core Agent, 4 096
undelivered notifications made the core refuse every input (`Limits::outbox`).

**Now.**
- A principal with no recipient, or a recipient without a secret (`REQ-EV-4`), **parks** its
  notifications: no attempt is made or recorded. They stay in the core's outbox, counted as
  undelivered (`REQ-EV-6`), and go out as soon as a recipient with a secret is registered.
- A notification to a registered recipient gets at most `delivery_max_attempts` attempts (24 by
  default: about twenty minutes of the existing backoff), then is parked the same way.
- Recipients are read once per pass, not once per notification.
- The Agent's outbox bound goes from 4 096 to 2²⁰ for new books: the notifications are service
  leaves, not Agent state, and the bound stopped the whole book for holders who never registered.
- A counter `aledgent_ingress_delivery_parked_total`.

**Decision (owner question `s-04`, unruled).** This is option C of `s-04` ("park"), the one that
loses nothing: a parked notification is still in the book and still undelivered, and the backlog is
watched as `NFR-11`'s count and age of undelivered notifications. Retries to a recipient that keeps
failing end at the bound: resuming them is an operator action not built here (*deferred*).

## T-031-16 — F13: two commits per input

**Before.** The forwarder marked each row forwarded, then decided, one row at a time: two commits
per input, which bounded the in-process core at the storage's commit latency (20 × the reference
load L on a 2-vCPU Linux runner, about 13 × L on a laptop).

**Now.** `Store::mark_forwarded_many` and `Store::mark_decided_many`: one statement each on
PostgreSQL (`ANY`, and an `UPDATE … FROM unnest`), defaults that loop for other stores. The
forwarder (with lever 1's batches, `T-031-04`) marks a batch's rows at once and records the decided
prefix at once: two commits per batch.

## T-031-17 — F2: idle ticks

**Before.** The ticker logged a tick every interval, and each tick is a full core input: on the
Agent, idle ticks took 28 % (recompiler) to all (interpreter) of the core; under a queue the ticker
starved (ticker age 205 s) and deadlines stopped advancing.

**Now.** `Core::next_due` reports when a tick next has work: the earliest deadline of an operation,
or the time the next checkpoint is due if one could be taken (`Ledger::next_due`). The ticker skips
its tick while nothing is due, and ticks at least every `tick_heartbeat` (60 s by default) so that
the core's clock stays observable. A core that cannot say (`Unavailable`, an older core host) is
ticked every interval as before. Under a queue, lever 1's batches carry the tick with the commands
ahead of it.

**Decision (owner question `s-03`, unruled).** This is option A of `s-03` ("tick only when a
deadline is due"), the one that changes no outcome: every deadline still fires at the first tick
after it, as before; only ticks that would have fired nothing are skipped. A counter
`aledgent_ingress_ticks_skipped_total`.

## Measured

See the PR: stress runs on this branch against the earlier runs' summaries.
