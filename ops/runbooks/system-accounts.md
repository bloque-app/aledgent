<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: creating a system account

`REQ-RC-7` (amendment 0024) · `REQ-AU-2` · `REQ-ID-7` · `REQ-AT-1` · `REQ-AT-3` · `INV-16`

**Owner:** platform, with the owner of each service that holds a grant on the account.
**Rules that do not bend:** a system account is owned by **the operator's identity** and opened under it;
services act on it only through **account-scoped, bounded grants** on exactly the accounts they operate;
no service holds the operator's token or credential. Everything here goes through the command surface
(`CTR-OPS`, `REQ-AT-3`): nothing writes a store by hand, and nothing moves value outside a command. A
service names a system account by its **role**, never by its identifier: the identifier changes at every
genesis, the role does not.

The accounts that exist at genesis come from the chainspec with their role aliases already; this runbook
is for one created after the book is serving.

**What you need:** the operator's credential, used through the signer for steps 2, 3 and 4 only; the
deployment's ingress URL; the service principal's identity (`did:aledgent:id:…`). Below, `$OP` is the
operator's bearer token, `$ACCOUNT` the new account and `$SERVICE` the service principal.

## 1. Decide, and record the decision

In the change's tracking issue, before anything is opened:

- the **role**: `<service>/<purpose>`, for example `payments/provider-cost/newrail`. Lower case, `/`
  separated, at most 256 bytes, no personal data (`INV-17`);
- the **kind**: `customer` (or `suspense`) unless it must go below zero, then `treasury` with a per-asset
  limit;
- the **assets** it holds;
- **which service principal** acts on it, with which operations (`transfer`, `reserve`, `capture`,
  `release`), and which bounds (per operation, per window, assets, counterparties). A role with no service
  stays under the operator alone and gets no grant.

The issue records the decision, never the identifiers.

## 2. Open the account, as the operator, under the operator's own identity

```sh
curl -sS -X POST "$INGRESS/v1/accounts" -H "authorization: Bearer $OP" \
  -H "client-key: system-payments-provider-cost-newrail" -H 'content-type: application/json' \
  -d '{"owner":"<the operator identity>","kind":"customer","custody":"delegated"}'
```

The operator identity is what `aledgent-ingress founded --config <file>` prints. The reply's
`result.account` is `$ACCOUNT`. The client key makes a retry safe: the same key returns the same account.

## 3. Register its role

```sh
curl -sS -X POST "$INGRESS/v1/ops/accounts/$ACCOUNT/aliases" -H "authorization: Bearer $OP" \
  -H 'content-type: application/json' \
  -d '{"source":"system-role","name":"payments/provider-cost/newrail","justification":"<issue>: <why>"}'
```

`source` is `system-role` for every role: the same source a seeded genesis gives its system
accounts (16 bytes, `system-role` zero-padded; its hex spelling `73797374656d2d726f6c650000000000` is the
same source). An alias names one account **for good**: a role already registered (to this account or
another) is refused `403 ERR-NotAuthorised`, and nothing re-points or removes it. Pick the name in step 1
with that in mind. The command is audited with its justification (`REQ-AT-1`).

## 4. Grant the service what it needs

If the service principal is not tied to its integrating party yet, tie it first
(`POST /v1/ops/integrating-parties/{party}/principals`, `tie_principal`). Then the account-scoped grant,
from the account's owner (the operator) to the service, with the bounds of step 1:

```sh
curl -sS -X POST "$INGRESS/v1/grants" -H "authorization: Bearer $OP" -H 'content-type: application/json' \
  -d '{"kind":"delegated","principal":"'"$SERVICE"'","subject":{"account":"'"$ACCOUNT"'"},
       "ops":["transfer"],"per_window":{"length":86400000,"limit":"<minor units>"},"assets":["<asset>"]}'
```

A grant needs at least one bound (`REQ-AU-2`, `ERR-UnboundedGrant`). This account-scoped grant is the
**only** way a service reaches a system account: an operator grant over every delegated account (such
as a custody service's) never covers an account the operator's identity owns, whatever its custody
(amendment 0025 R3). So a system account is safe in delegated custody, and no broad grant is
ever the answer for one.

## 5. Make the service name the role, not the identifier

One line in the service's configuration names the role (`payments/provider-cost/newrail`). At start the
service resolves it as its own principal:

```sh
curl -sS "$INGRESS/v1/aliases?source=system-role&name=payments/provider-cost/newrail" \
  -H "authorization: Bearer <the service's token>"
# {"account":"did:aledgent:acct:…"}
```

It answers only to a principal that reaches the account: before step 4 the service gets
`404 ERR-UnknownAccount`, as for a role that does not exist. Never paste the account's identifier into a
configuration, a secret store's entry or an environment variable.

## 6. Check

All reads; nothing changes.

- **The grants on the account** are exactly the decision's: one per service of step 1, nothing else.

  ```sh
  curl -sS "$INGRESS/v1/accounts/$ACCOUNT/grants" -H "authorization: Bearer $OP"
  ```

  Each item names the grant, its grantor (the operator), when it was granted, `revoked` if it was, and its
  terms (holder, operations, bounds). Only the operator and the account's owner read this; a grantee gets
  `403`.
- **The role resolves**, as the service (step 5), to `$ACCOUNT`.
- **The account** is empty and owned by the operator: `GET /v1/balances?account=$ACCOUNT` shows zero.
- **One smoke operation** as the service inside its bounds succeeds, and one outside them is refused
  `ERR-BoundExceeded` or `ERR-NotAuthorised`.
- **No other service reaches it**: one operation on it with a token that holds only a broad grant (a custody service's)
  is refused `ERR-UnknownAccount`.
- **The audit trail** (`REQ-AT-1`) holds the open, the alias and the grant, each attributed.

## 7. Retire it later

Revoke the grant (`POST /v1/grants/{grant}/revoke`), let the service stop resolving the role, drain the
account through the command surface, freeze it (`POST /v1/ops/accounts/{account}/freeze`) and close it
(`POST /v1/accounts/{account}/close`: only at zero balance, `REQ-LA-4`). The alias stays, as every alias
does, and keeps naming the closed account: a role is never reused for another account. Choose a new role
for a replacement.

## Archived identifiers

An identifier a migration left behind (the exclusion list the chainspec commits) is answered
`410 ERR-IdentifierArchived` wherever a request names it, for good (`REQ-RC-7`): in a path, a query
parameter, a command's body, or as an alias's name. It cannot become a role. To use such an identity
again, open it afresh with a new identifier.

## Exercised

`services/ingress/tests/system_accounts.rs` runs steps 2 to 6 through the surface;
`crates/ledger-core/src/ledger/tests/aliases.rs` the command's refusals and its audit record; the
conformance case `ERR-IdentifierArchived` (`F-009`) the archived identifiers. A deployment's first rehearsal should run this runbook once end to end for one
role and keep the output.
