<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0026 — A holder reads its grants over an identity

**Status:** drafted and applied on `feat/identity-grants-read` on 2026-10-09. Its one question,
`a0026-q1`, was ruled (a) the same day: the identity itself reads every grant over it. Applied to
`SPEC.md` **v0.4.22**, after amendments 0027 (v0.4.20) and 0028 (v0.4.21), which merged first.
**Target:** `SPEC.md` v0.4.21 as on `main` (amendments 0027 and 0028 applied).
**Date:** 2026-10-09
**Rationale in one line:** a grant over an identity is named as the `parent` of every account grant made
under it (amendment 0025 R4), and nothing let its holder learn its identifier.

---

## The problem

Under amendment 0025 R4, the custody service opens a user's further accounts under its delegated grant over
that user's identity, which the onboarding service gave it, and grants the issuing service its per-account
grant naming that grant as `parent`. The custody service must therefore know the grant's identifier. For a
user the onboarding service registers, the identifier is in the onboarding service's reply, not the custody
service's. For a migrated user, the grant is a grant of the seed (`a0025-q3`), made
at genesis, and no service was ever answered with it. The two reads that show grants do not reach it:
`grants(account)` (`REQ-RC-7`) is over an account, and only the operator's and the owner's; and the
answering setting (`REQ-CS-9`) names no grant.

## `REQ-RC-7` — one item added

> - `grants(identity)` returns the authority grants whose subject is the identity, live and revoked, with
>   who granted each, when, its terms, its bounds and its parent. The identity itself, the operator and
>   the identities principal (`REQ-ID-6`) get every one, as an account's owner gets `grants(account)`; any
>   other principal gets the ones it holds. A principal that holds none, and a credential limited to
>   accounts (`REQ-AU-20`), is refused with `ERR-NotAuthorised`. *Amendment 0026.*

## §8.2 — one query

> | `grants(identity)` | The grants over the identity, live and revoked: grantor, holder, terms, bounds, parent, when granted and revoked. Every one to the identity itself, the operator and the identities principal; a holder's own to any other principal; `ERR-NotAuthorised` to a principal that holds none (`REQ-RC-7`, amendment 0026) |

## Why these choices

- **The holder reads its own, not the identity's.** The read exists so a holder can name its grant as a
  parent. It shows nothing the holder was not already given, so it needs no grant of its own and creates no
  new reach (`INV-16`): a grant over an identity reaches none of its accounts (amendment 0025).
- **The identity itself reads every one** (`a0026-q1` (a)), as an account's owner reads every grant over
  its account: who may act for it is the identity's to know.
- **The operator and the identities principal read every one.** The identities principal may grant over
  any identity it manages (`REQ-ID-6`), and the operator issued the seed's grants; both need to check that
  each service holds exactly the grants it should, as `grants(account)` lets the operator do for accounts.
- **`ERR-NotAuthorised`, not an empty list,** for a principal that holds none: the same condition
  `recovery_terms(identity)` gives a caller that is neither the identity nor the operator. An identity's
  identifier is not an account's, so `ERR-UnknownAccount` does not apply, and the closed list (`REQ-ER-1`)
  has no condition for an unknown identity.

*Trade-off, stated plainly.* A holder's read is a point read of the grants it holds. The identity's, the
operator's and the identities principal's read every grant the book holds and keep those over the identity:
no index is kept by identity subject, and adding one would change what the book stores. It is a read, rare,
and nothing else waits on it; if it is ever on a path that does, the index is the fix, as a separate
change.

## Ruling: the identity sees every grant over it (`a0026-q1` (a), 2026-10-09)

**`a0026-q1`: does the identity itself see every grant over it?** Ruled (a), yes: an account's owner reads
every grant over its account (`grants(account)`), and an identity reads every grant over itself the same
way, including the ones it did not grant (the identities principal's, and the seed's). It needs a
credential not limited to accounts, as every other reader of this query does. The draft had said no.

## What does not change

- No command, no write and no invariant. `grants(account)` is unchanged.
- The core's evaluation and its stored state are unchanged: the read is answered from the grants the
  store already holds.

## Code

`ledger-core`: `Ledger::grants_over_identity`, beside `grants_on` (`ledger/operator/aliases.rs`). Ingress:
`GET /v1/identities/{identity}/grants` (`identity_grants`), answering `IdentityGrantsBody`
(`{"identity": did, "items": [GrantViewBody, ...]}`, as `grants(account)`'s items); `openapi.json`
regenerated; conformance catalogue regenerated. Tests:
`a0026_a_holder_reads_its_grant_over_an_identity_and_names_it_as_parent` (core,
`ledger/tests/identities.rs`), `amendment_0026_the_grants_over_an_identity_are_read_by_who_may_and_refused_to_anyone_else`
(ingress, `tests/identity_grants.rs`: the identity, the operator and the identities principal read both
grants over it, neither made by the identity; a holder reads its own; a stranger is refused),
`amendment_0026_the_custodian_reads_its_grant_over_an_identity_and_names_it_as_parent`
(sidecar, through the surface). The core actor (`core.vos`) does not serve queries and does not call
the new read: its program is unchanged in behaviour.
