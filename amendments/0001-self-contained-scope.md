<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0001 — a self-contained specification with a narrowed scope

**Status:** applied to `SPEC.md` v0.2.0
**Target:** `SPEC.md` v0.1.0
**Date:** 2026-09-16
**Rationale in one line:** v0.1.0 described aledgent by anchoring to the systems it replaces, named the
intended technologies and network, and carried the scope of one integrating party — its
products, its migration, its other value networks, its tenancy model — inside a document that claimed
to be the single, self-contained source of truth for *what* aledgent is.

No surviving identifier changes meaning. Identifiers withdrawn here are never reused; gaps in a sequence
mean a withdrawal. Where a concept survived under a new family name, the old identifier is listed as
**renamed** and its number is preserved inside the new family wherever possible.

---

## A1 — §0 is a reader's guide, nothing else

**The problem.** §0 argued the motivation, stated that the document "fixes architecture" against a
previous state, named the intended runtime, kernel and network in an "intended realisation" note, and
recorded the document's ancestry. All of it is either motivation or plan input, and none of it is what a
reader needs to read the document.

**The amendment.** §0 now holds: a table of what each section answers and who should read it; the
normative-language rule; the identifier table and the never-renumber rule; and the definition of "the
settlement layer" as whatever satisfies §4.4. Motivation, ancestry, the intended stack and the
development process moved to informative notes outside the specification.

## A2 — §1 states the problem without prior art

**The problem.** §1.1 described four production incidents and three structural causes in the systems
being replaced. A reader who has never seen those systems learns what went wrong there, not what problem
a ledger of record must solve.

**The amendment.** §1.1 is written hourglass-fashion: the domain (a ledger of record), the field of
application (parties building financial products who need seven properties from one), and the problem
(the available shapes of ledger each give up some of the seven). §1.2 states what aledgent is as a
product; §1.3 keeps the three-kinds-of-fact separation with "chain fact" renamed **settlement fact** and
"platform fact" renamed **customer fact**; §1.4–§1.7 state audience, scope, and standing assumptions.
The "Trade-off, stated plainly" callout on provability is kept, reworded without reference to any
existing user base.

## A3 — one settlement layer, defined by requirements

**The problem.** The specification named a particular network throughout, and modelled several external
networks ("rails") with a rail adapter each, a rail namespace, and rail-specific errors — the shape of one
integrating party's estate, not of the product.

**The amendment.** aledgent attaches to exactly one **settlement layer** (§1.7), defined only by the
requirements it must satisfy. Consequences:

| v0.1.0 | v0.2.0 |
|---|---|
| `REQ-CH-1`…`REQ-CH-5` (the chain) | **renamed** `REQ-SL-1`…`REQ-SL-5` (the settlement layer), text unchanged in substance |
| `REQ-RL-1`…`REQ-RL-8` (rails) | **renamed** `REQ-SA-1`…`REQ-SA-8` (the settlement adapter); `REQ-RL-2` now asserts the *declared* settlement layer rather than a derived rail identity |
| `REQ-RL-9` (a rail is not a namespace) | **withdrawn** — moot with one settlement layer; `INV-25` carries "one book" |
| `REQ-OU-1`…`REQ-OU-4` (outbound) | **renamed** `REQ-SO-1`…`REQ-SO-4` |
| `REQ-RI-1`…`REQ-RI-5`, `REQ-RO-1`…`REQ-RO-4` (rail surfaces, `CTR-RIN`, `CTR-ROU`) | **renamed** `REQ-SI-1`…`REQ-SI-5` and `REQ-SO-5`…`REQ-SO-8` under one boundary contract `CTR-SET` |
| `ERR-UnsupportedByRail`, `ERR-RailUnavailable` | **renamed** `ERR-UnsupportedLeg`, `ERR-SettlementLayerUnavailable` |
| `US-E2` (cross-network transfer), `US-E4` renumbering | `US-E2` **withdrawn**; `US-E3`, `US-E4` keep their numbers |
| `DEF-5` (other value networks) | **withdrawn** — now a non-goal (§13) |
| `OQ-3`, `OQ-8`, `OQ-34`, `OQ-37`, `OQ-50`, `OQ-57`, `OQ-59` | **withdrawn** — rail naming, cursor ownership, other networks, adapter contribution, chain-operator retirement, network upgrade policy are plan or migration questions |

## A4 — the product is its own universe: no tenant, realm, medium

**The problem.** Tenant, realm and medium modelled the integrating party's organisation, environments and
products inside the ledger. They are that party's facts.

**The amendment.** Isolation rests on authority: a principal can reach no fact it holds no authority over
(`INV-16`, **revised** from a tenant statement). Pooled accounts are explicit authority grants
(`REQ-LA-6`, revised). What an account is presented as is not a ledger fact (`REQ-LA-5`, revised).

| v0.1.0 | v0.2.0 |
|---|---|
| `REQ-ID-3` (one tenant per identity), `REQ-ID-4` (legacy alias resolution, tenant-scoped) | **withdrawn** |
| `REQ-ID-5` | unchanged in substance (identifier never derives an external name); §5.1 lists `REQ-ID-1`, `REQ-ID-2`, `REQ-ID-5` |
| `ERR-TenantMismatch`, `ERR-LegacyIdentifierUnresolvable` | **withdrawn** |
| `US-A3` (several media on one account) | **withdrawn** |
| `OQ-20`, `OQ-43`, `OQ-44`, `OQ-45` | **withdrawn**; `OQ-5` (pooled accounts across integrating parties) **revised** and kept |
| `register_tenant` (§8.1) | removed |

## A5 — integration, not compatibility; no migration epic

**The problem.** Epic L (migration of an existing estate), Epic M's `US-M1` (existing callers keep
working), `CTR-CMP` and `REQ-CO-*`, `REQ-MG-*`, `NFR-12`, `NFR-13`, `DEF-9` and a dozen open questions
specified one integrating party's cutover. A product specification cannot carry another party's
migration.

**The amendment.** What survives is generic: an export/re-import capability (`REQ-RC-3`, from
`REQ-MG-5`) and an initial-state load with zero-divergence reconciliation (`REQ-RC-4`, from `REQ-MG-1`
and `REQ-MG-3`), plus `load_opening_balances` in §8.1 and `AC-K5.2`.

| v0.1.0 | v0.2.0 |
|---|---|
| `US-L1`…`US-L5`, `US-M1`, `US-M4` (platform holds no balance) | **withdrawn**; `US-M4`'s substance survives as the new `US-M4` — *renamed in place*: "I never need to hold a balance of my own" (same number, same intent, generic party) |
| `REQ-CO-1`…`REQ-CO-5`, `REQ-MG-1`…`REQ-MG-6` | **withdrawn** (see `REQ-RC-3`, `REQ-RC-4`) |
| `CTR-CMP` | **withdrawn** |
| `NFR-12` (migration window), `NFR-13` (platform constraints measured at composition time) | **withdrawn**; the measured constraints are plan input |
| `DEF-9` (alias layer retirement), `DEF-6` (compliance limits and card rules), `DEF-10` retained | `DEF-6` **withdrawn** — a non-goal; `DEF-11` **added** (encumbrance, `US-C5`) |
| `OQ-1`, `OQ-2`, `OQ-4`, `OQ-15`, `OQ-16`, `OQ-18`, `OQ-19`, `OQ-26`…`OQ-29`, `OQ-38`…`OQ-40`, `OQ-42`, `OQ-46`…`OQ-49`, `OQ-53`…`OQ-55` | **withdrawn** — migration, compliance-rule, kernel-gap and runtime questions belong to the plan |
| `OQ-60` | **added** — whether the initial-state load is a standing capability |

## A6 — issuance is a first-class, bounded authority

**The problem.** With one settlement layer, value from any other source (a bank, a card network, another
chain) must enter the book somehow. v0.1.0 had `issue` and `redeem` under operator authority only.

**The amendment.** `REQ-AS-6` and `REQ-AU-10` define **issuance authority** over an asset, grantable by
the operator, bounded and attributable; `US-F4` covers the issuer's story; `REQ-AU-7` records issuance as
a fourth authority kind.

## A7 — every "today…" justification removed from normative text

**The problem.** Around forty requirements carried a sentence of the form "today X happens, which is why".
Each was true and each made the document depend on knowledge of a prior system.

**The amendment.** Requirements keep their reason in one clause where it is intrinsic; the evidence moved
to informative notes. No requirement changed meaning as a result.

## What this costs, stated plainly

The specification no longer says how the party that wrote it will get from where it is to here. That is
the right place for that not to be — but it means the plan must carry a migration design the spec no
longer constrains, and the exit criterion for that migration (zero divergence, sustained) now lives in
`REQ-RC-4` rather than in a story of its own. The document also no longer names the network it will run
on; anyone reading it must accept that "the settlement layer" is defined by §4.4 and nothing else.

## Carried to the plan

- The intended bindings: runtime, kernel and settlement layer.
- An operator's migration: extraction of its existing book, opening balances via `REQ-RC-4`, adopted
  accounts, compatibility with existing callers and their known breaking behaviours, cross-network legs
  a client relies on, credentials to rotate.
- Every withdrawn open question above, as a plan-level decision.
- Measured platform constraints (former `NFR-13`).
