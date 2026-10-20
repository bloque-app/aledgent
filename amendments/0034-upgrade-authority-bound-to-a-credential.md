<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0034 — The upgrade authority is held with a credential, not only by an identity

**Status:** **ruled by the owner on 2026-10-19: approved as drafted.** Drafted on 2026-10-19 from the
review of PR #45 (`T-037-05`, finding F1), on the coordinator's ruling that a mode's member is bound to a
credential. PR #45 implements it. It is not yet applied to `SPEC.md`: the code and the plan cite it as
`[PENDING AMENDMENT 0034]` until the pass that applies it.
**Target:** `SPEC.md` v0.4.27 as on `develop`.
**Date:** 2026-10-19
**Amends:** `REQ-RT-18` (amendment 0023, R6) and its "Limits, stated plainly". Nothing is withdrawn.
**Rationale in one line:** a mode that names identities alone can be taken over by whoever can change those
identities' credentials, and the operator can. The mode must name what only the member holds.

---

## The problem

`REQ-RT-18` makes the upgrade-authority mode the only authority over a writer's code, and it lets a book move
away from "one operator principal" to "a proposer and a different approver" or "*k* of *n*". The point of
those modes is that the operator alone can no longer change the code.

As written, the mode names **principals**, and a principal acts with any credential of its set
(`REQ-AU-20`). But the operator can add a credential to an identity's set:

- by import (`REQ-MG-7`), under an operator grant it can give itself;
- through the other paths that add a member to a set on the operator's side.

`REQ-MG-8` already says an import must not give the operator a power it does not hold. It enforces this for
custody only: an import is refused while any account of the identity is in owned custody. It does not reach
the upgrade authority. So the operator can import a credential into each principal the mode names, then
propose and approve an upgrade as those principals. The review of #45 reproduced this. A non-genesis mode
then protects against nothing the operator does.

The same review found a second, weaker path. The operator can restrict or close any identity
(`set_identity_status`), and closed is absorbing. That cannot authorise anything, but it can stop the
principals a mode needs.

## The change

### R1 — `REQ-RT-18`: a member is a principal and one of its credentials

Replace the third bullet of `REQ-RT-18`:

> - A new mode MUST be refused, with nothing changed, unless it can be satisfied: every principal it names
>   exists and can hold the authority, and every threshold can be met.

with:

> - A mode MUST name each principal together with one of that principal's credentials. The principal holds
>   the authority only when it acts with that credential. A credential that joins the principal's set later,
>   by any path, MUST NOT count, nor MUST any other credential of the set. A mode MUST name each principal
>   with one credential only.
> - A new mode MUST be refused, with nothing changed, unless it can be satisfied:
>   - every principal it names exists, can hold the authority, and holds the credential it is named with,
>     in force and not limited to some of its accounts;
>   - every threshold can be met;
>   - its delay, if it has one, can elapse within the book's life.
> - While a change of mode is scheduled and not yet in force, the mode it replaces MUST NOT authorise
>   anything more: no upgrade, no migration, no other change of mode.

The genesis mode is unchanged: "one operator principal" (ruling `a0023-q2`) is the operator's organisation
identity with any of its credentials. It is the operator's by definition, so binding it to a credential would
protect nothing.

### R2 — `REQ-RT-18`'s "Limits, stated plainly"

Replace the paragraph's last two sentences:

> The mode names principals; their credentials and revocations still come from the registry's feed, so a
> revoked approver is refused within `NFR-20`'s bound, not at once. A lost key under "one principal" blocks
> upgrades until that principal's credentials are recovered (`REQ-AU-15`). There is no bypass, by design
> (guardrail 7).

with:

> The mode names each principal with one credential. Whether that credential and its identity are still in
> force comes from the registry's feed, so a revoked or closed member is refused within `NFR-20`'s bound,
> not at once. The operator can restrict or close a named identity. That stops the member from acting: it
> can delay an upgrade, never authorise one.
>
> A member whose credential is lost cannot act again. Recovering the identity (`REQ-AU-15`) gives it a new
> credential, which the mode does not name. Only a change of mode, authorised by the members who remain,
> names the new one. So:
>
> - under "one principal" (the genesis mode excepted), a lost credential, or a closed identity, blocks every
>   upgrade and every change of mode for good;
> - under "a proposer and a different approver", the book is blocked when no proposer remains with a
>   different approver beside it;
> - under "*k* of *n*", the book is blocked when *n* − *k* + 1 members are lost.
>
> There is no bypass, by design (guardrail 7). A deployment that leaves the genesis mode should choose a
> mode with members to spare.

## What the plan does with it (informative, `DEC-26` (b′), `T-037-05`)

- A member is `(principal, digest of its credential's verifier)`. The digest is domain-separated and covers
  the verifier's scheme and its public material, so no secret enters the mode (`REQ-AU-8`). A caller is a
  member when its principal and the verifier of the credential it presented are the member's. Ingress and the
  core's binding authenticate that credential before the library sees the call.
- The delay is at most 2¹⁶ inputs. The core writer takes at least one input a minute (the ticker's
  heartbeat), so the longest delay elapses in about 45 days.
- Until the export carries the mode (`T-037-17`), an export of a book that left its genesis mode, or has a
  change of it scheduled, is refused rather than imported under the genesis mode.

## Trade-off, stated plainly

The first credential of an identity is the one the operator gives it at `open_identity`. It carries the trust
of the opening, as `REQ-MG-8`'s trade-off says. A mode that names that credential is only as independent of
the operator as that opening was. Members should be named with a credential they enrolled and proved
themselves (`REQ-AU-13`). The change of mode that names them is where that is checked, by the members who
authorise it.

## What does not change

- `REQ-RT-15` to `REQ-RT-17` and `REQ-RT-19` to `REQ-RT-21`.
- The genesis mode (`a0023-q2`) and its digest.
- `REQ-MG-8`. It holds for the upgrade authority as a consequence of R1, because an imported credential is
  never a member's.
- `AC-K7.7` and `AC-K7.8`. `AC-K7.8`'s "a principal who does not exist" now also covers a principal named
  with a credential it does not hold.

## Ruling

Approved by the owner as drafted on 2026-10-19, R1 and R2 both, including that under "one principal" a lost
credential blocks the book for good, with no recovery path. The alternative, a recovery path that the
operator's own recovery authority could also use, would reopen the takeover R1 closes.
