<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0028 — Who owns an account

**Status:** drafted and applied on `feat/account-owner-read` on 2026-10-09, at the owner's request. Applied to
`SPEC.md` **v0.4.21**.
**Target:** `SPEC.md` v0.4.20 as on `main` (amendment 0027 applied). Version v0.4.19 is amendment 0026's
(#215, open); this one takes the spec to **v0.4.21** whichever lands first.
**Date:** 2026-10-09
**Rationale in one line:** a service that adopts an account for a user must check that the user owns it,
and no read returned an account's owner to that service.

---

## The problem

The custody service adopts an aledgent account as a user's default account. Before it does, it must check
that the account's owner is that identity: otherwise a fault in the caller, or a forged request, binds one
user's default to another user's account. The custody service holds a delegated grant over the user's identity (amendment 0025 R4), not
necessarily a grant over the account.

No read answers the owner to it. Every route under an account is a command, except `grants(account)`,
which is the operator's and the owner's (`REQ-RC-7`), and `custody`, which is answered only to whoever
reaches the account (`INV-16`), and a grant over an identity reaches none of its accounts (amendment 0025).

## `REQ-RC-7` — one item added

After the `grants(account)` item:

> - `account(account)` returns the account as the book holds it: its owning identity, kind, custody mode,
>   status and when it was opened. It is answered to a principal that reaches the account (`INV-16`), to
>   the operator, to the identities principal (`REQ-ID-6`), and to a principal holding a live grant over
>   the account's owning identity, with a credential not limited to accounts (`REQ-AU-20`). To anyone else,
>   as for an account that does not exist, it is `ERR-UnknownAccount`. *Amendment 0028.*

## §8.2 — one query

> | `account(account)` | The account's owner, kind, custody, status and opening time. To whoever reaches it, the operator, the identities principal, and a holder of a live grant over its owner; `ERR-UnknownAccount` otherwise (`REQ-RC-7`, amendment 0028) |

## Why these choices

- **Who reads it.** Whoever reaches the account already sees more than this (its balances). The operator
  configures the deployment and opens every system account. The identities principal opens accounts for
  any identity it manages (`REQ-ID-6`). A holder of a grant over the owner may already open accounts for
  that owner (amendment 0025 R4); learning which accounts that owner holds, one at a time and by an
  identifier it already has, is less than it can do. None of these gains reach it did not have in
  substance, so `INV-16` holds: each holds authority over the fact.
- **A grant over the identity, not a new reach.** The read answers the account's own facts and nothing
  else. It does not make a grant over an identity reach the identity's accounts for balances, movements or
  commands: amendment 0025's rule stands.
- **`ERR-UnknownAccount`, not `ERR-NotAuthorised`,** to anyone else: the condition every read over an
  account gives a caller that does not reach it (`INV-16`), so the read does not reveal that the account
  exists. The issuance counterparty is not a ledger account a service adopts, and is answered the same way.
- **`closed` beside `status`.** The custody service's check is "open and owned by this identity"; `closed` saves it from
  knowing the four statuses, and `status` keeps a frozen or closing account visible.

*Trade-off, stated plainly.* The holder of a grant over an identity can now confirm, for an account
identifier it holds, that the identity owns it. Before, it could not tell that account from one that does
not exist. That is the point of the read; an identifier it does not already hold is not discoverable
through it.

## What does not change

- No command, no write, no invariant and no error condition. `grants(account)` and `custody` are
  unchanged.
- The core's evaluation and its stored state are unchanged: the read is answered from the account and the
  grants the book already holds. The core actor (`core.vos`) serves no query and does not call the new
  read: its source is unchanged.

## Code

`ledger-core`: `Ledger::account_view` (`ledger/reads.rs`). Ingress: `GET /v1/accounts/{account}`
(`account`), answering `AccountViewBody`:

```json
{
  "account": "did:aledgent:acct:…",
  "owner": "did:aledgent:id:…",
  "kind": "customer | treasury | suspense",
  "custody": "delegated | owned",
  "status": "active | frozen | closing | closed",
  "closed": false,
  "opened_at": 1791593330280
}
```

`openapi.json` and the conformance catalogue regenerated. Tests:
`amendment_0028_an_accounts_owner_is_read_by_who_may_and_refused_to_anyone_else` (ingress: the owner, the
operator, the identities principal, a holder of a grant over the owner and one over the account read it; a
stranger is refused, and the grant over the owner reaches no other owner's account), and
`amendment_0028_the_custodian_reads_the_owner_of_an_account_opened_for_its_user` (sidecar: the custody
service, with its grant over the user, reads the account the onboarding service opened for the user and
sees the user as owner; it is refused on the issuing service's fee account).
