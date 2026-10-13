<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0024 — Aliases after genesis, the grants on an account, and archived identifiers

**Status:** drafted and applied on 2026-10-09, on the owner's instruction for an operator's migration, from
rulings already given: `system-accounts-operator` (2026-10-09: the operator needs a clear, documented path
to create system accounts), `au-18` and `au-20` (system accounts owned by the operator,
services acting through account-scoped grants), and `au-19` / `au4-q1` (zero-balance internal identities
left behind, archived, not loaded). The new condition's name and its status code are the owner's to
confirm at review (question below). Applied to `SPEC.md` **v0.4.17**.
**Target:** `SPEC.md` v0.4.16 as on `main` (amendment 0023 applied).
**Date:** 2026-10-09
**Rationale in one line:** the team needs to name a system account by its role after genesis and to read
who may act on it, and a caller holding an identifier the migration left behind must be told so, not
"not found".

---

## The problem

The recipe a developer follows to create a system account after go-live
(`ops/runbooks/system-accounts.md`) had two steps with no command:

- **Step 3, its role alias.** Services find a system account by role (`issuer/fee`), never by identifier,
  since every re-genesis changes the identifier. The SPEC's aliases exist only in a seed
  (`REQ-RC-6`): nothing names an account after genesis.
- **Step 6, the check.** "Each service holds exactly the grants it should" could only be tested by trying
  operations inside and outside the bounds. No query lists the grants over one account.

And the migration leaves the zero-balance test identities behind (`au-19`): their identifiers are in no
`ids.jsonl` line and no alias. A client that presents one today gets `ERR-UnknownAccount`, which reads as "never
existed" or "typo", when the truth is "it existed and was deliberately archived". The SPEC's error list (§10) is closed
(`REQ-ER-1`), so a condition for it needs this amendment.

## §2 — the glossary

**Alias** (revised): "A name for a ledger account, by a source and a name within it: a prior system's
identifier, loaded with a seed, or a name the operator registers after genesis, such as a role
(`REQ-RC-7`). It names one account for good and carries no personal data (`INV-17`)."

**Archived identifier** (new): "A prior system's identifier that the initial state deliberately left out.
It names no ledger account and never will (`REQ-RC-7`)."

## `REQ-RC-7` (new)

> **`REQ-RC-7`** **Aliases after genesis, and archived identifiers.**
>
> - The operator MAY name a ledger account by an alias after genesis (`register_alias`), with a
>   justification, under its own policy authority: no grant gives it. An alias names one account, for
>   good: one that already exists is never re-pointed or registered again, and is refused with
>   `ERR-NotAuthorised`. It is attributed and audited like any operator command (`REQ-AT-1`), and aledgent
>   keeps only a digest of its source and name (`INV-17`).
> - `alias(source, name)` returns the account an alias names to a principal that can reach that account
>   (`INV-16`). To anyone else, as for an alias that does not exist, it is `ERR-UnknownAccount`.
> - `grants(account)` returns every authority grant whose subject is the account, live and revoked, with
>   who granted it, when, its terms and bounds. It is the operator's, and the account owner's.
> - A deployment MAY name prior-system identifiers that its initial state deliberately left out
>   (**archived identifiers**), as a list fixed with its genesis and checked against a digest. A request
>   that names one (in its path, its parameters or its body) MUST be refused with
>   `ERR-IdentifierArchived`, for good: it is never answered as unknown, never resolves to an account, and
>   no alias is registered for it. Nothing of such a request is taken in.
>
> *Trade-off, stated plainly.* `ERR-IdentifierArchived` tells any authenticated caller that the
> identifier existed and was left behind on purpose. That is the intent: a caller holding an old
> identifier learns to open a new one instead of retrying. The list holds identifiers only, never
> personal data (`INV-17`).

"No grant gives it" follows `tie_principal`: an alias configures the deployment, as a party does. A role
alias is a name the whole deployment resolves, so a service that could register one could shadow
another's.

## §8.1 — one command

> | `register_alias` | Operator authority; a justification; the account held and not closed; an alias not registered before; a name that is not an archived identifier | The alias names the account, for good (`REQ-RC-7`). Audit record. Otherwise `ERR-NotAuthorised`, `ERR-UnknownAccount`, `ERR-AccountClosed`, `ERR-IdentifierArchived` or `ERR-JustificationRequired` |

## §8.2 — two queries

> | `alias(source, name)` | The account the alias names, to a principal that reaches it; `ERR-UnknownAccount` otherwise; `ERR-IdentifierArchived` for an archived identifier (`REQ-RC-7`) |
> | `grants(account)` | Every grant over the account, live and revoked: grantor, holder, terms, bounds, when granted and revoked. The operator's and the owner's (`REQ-RC-7`) |

## §10 — one row added

> | `ERR-IdentifierArchived` | The identifier is one a prior system issued that the book's initial state deliberately left out: it names no ledger account and never will (`REQ-RC-7`) |

The list stays closed (`REQ-ER-1`), now with 41 conditions.

## §15

Epic N's requirements gain `REQ-RC-7`.

## Behaviour change for existing clients

- A request naming an archived identifier was `404 ERR-UnknownAccount` (or a shape refusal); it is now
  **`410 ERR-IdentifierArchived`**, permanently.
- A client that lists the closed set of codes must add `ERR-IdentifierArchived` (410): a refusal, final,
  not retryable, "open a new identity". Until it does, it would classify the code as a protocol failure.
- Nothing else changes: no existing command or query answers differently for an identifier that is not
  archived.

## No invariant is affected

`register_alias` writes one alias leaf through the operator frame every operator command uses (attribution,
audit, idempotency on its request). `alias` and `grants` are reads behind `INV-16`. The archived list never
reaches the book: ingress refuses before anything is logged.

## Carried to the plan

- **`F-036`, built in this PR.** `T-036-13` (the alias resolver, `GET /v1/aliases?source=…&name=…`),
  `T-036-26` (`register_alias`, `POST /v1/ops/accounts/{account}/aliases`: a new operator command through
  the core, `Call::RegisterAlias`, audit code 50), `T-036-27` (`grants(account)`,
  `GET /v1/accounts/{account}/grants`), `T-036-28` (archived identifiers at ingress: the deployment file's
  `archived_identifiers`, `{path, chainspec}`, the migration tooling's `excluded.jsonl`, format `aledgent/excluded/v1`;
  410). The source is 16 bytes, written as 32 hex digits or as a short name zero-padded (`system-role`, as
  the migration's role aliases).
- **The core grows by one operator command.** `register_alias` is a book write, so it is a call the Agent
  replays (`m-jam-only` holds: no host call). The queries are answered from the replica, natively.
- **`T-036-29`, built in this PR after #210.** The list's digest is the one the genesis's chainspec commits
  (header `excluded_sha256`, chainspec format 3): ingress reads the chainspec the deployment file names,
  checks it is the one `genesis.chainspec` commits, and refuses to start on a list of another digest or a
  chainspec that commits none.
- **The runbook.** `ops/runbooks/system-accounts.md` is that recipe, now with every step a command.

## Questions for the owner (as asked)

- `a0024-q1`: The condition is named `ERR-IdentifierArchived` (SPEC names no prior system, so not
  "legacy") and answered with HTTP 410 Gone, permanently. Keep the name and the status? Background: §10 is the closed list of every refusal a caller can see; a new entry
  means every client's error table adds it.
- `a0024-q2`: Only the operator's policy authority registers an alias (as `tie_principal`): no grant can
  give it to a service. Keep that, or make it an operator-surface kind a grant can name, so a platform
  principal other than the operator can register roles?
