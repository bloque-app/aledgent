<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0010 — a holder's existing credentials can be imported

**Status:** approved by the owner with `F-028` (#110) on 2026-10-02; applied to `SPEC.md` v0.4.4.
**Target:** `SPEC.md` v0.4.3 (amendments 0008 and 0009 applied). This one takes it to **v0.4.4**.
**Date:** 2026-10-02
**Follows:** the owner's review of #110 (2026-10-02); amendments 0001 (A5) and 0004.
**Rationale in one line:** a deployment that brings holders across from a prior system should not make
them enrol again. Their credentials' public material can be imported into their sets as an explicit,
audited operator operation.

## The problem

`REQ-AU-20` lets only two things add a credential to an identity's set: an existing member of the set,
or a completed recovery. A deployment whose holders already hold credentials in a prior system has no
way to bring those credentials across. Every holder would have to enrol again, through a member they do
not have yet. Otherwise the deployment works around the requirement: for example, by opening each
identity with all its credentials at once, which `open_identity` never meant to carry.

The owner's ruling: `REQ-AU-20` is a requirement, not an invariant. If no invariant forbids an import, a
requirement of its own, in a migration epic, should cover it.

## No invariant forbids it

The following were read against an operator import of public material into an identity's set:

| Invariant | Why it does not forbid an import |
|---|---|
| `INV-22` | A credential cannot widen its own scope or change its own kind. An import changes no existing credential. The imported one is born with the scope the import states (`REQ-MG-7`). |
| `INV-16` | An import gives no principal reach over a ledger fact. The operator's reach is unchanged (`REQ-MG-8`). |
| `INV-20` | Is kept: the import is shown to every authority holder of the identity's accounts. |
| `INV-17` | Public material is not personal data, and no identifier is derived from it. |
| `INV-8`, `INV-25` | Nothing is copied out of the book or split from it. |

None of the others touches credentials. **No `INV-*` forbids it.**

The requirements an import could break are kept by construction:

- **`REQ-AU-6` and `REQ-AU-9`.** In owned custody, the operator must not be able to move value, and must
  not hold a recovery mechanism alone. An import into an identity with an owned-custody account would
  give the operator exactly that, so `REQ-MG-8` forbids it.
- **`REQ-AU-8`.** An import carries public material only.

## The amendment

### `SPEC.md` front matter

`spec_version: 0.4.3` → `spec_version: 0.4.4`; `date` → `2026-10-02`.

### `REQ-AU-20` (revised)

The sentence on adding and removing becomes:

> Adding or removing a credential MUST require an existing credential of the set, or a completed
> recovery (`REQ-AU-15`, `REQ-AU-16`). A credential MAY also be added by an import (`REQ-MG-7`).

The rest of `REQ-AU-20` is unchanged.

### §5.12 Migration (new)

> **`REQ-MG-7`** **Credential import.** A deployment that brings holders from a prior system MAY import
> their existing credentials into their identities' sets. A holder then keeps proving possession with
> what they already hold.
>
> - An import is an operator operation (`REQ-OS-1`). It has its own authority, a required
>   justification and an audit record (`REQ-AT-1`), and it is attributed to the operator (`REQ-AU-7`).
> - It carries public material only, never a secret (`REQ-AU-8`).
> - Each imported credential becomes a member of the identity's set (`REQ-AU-20`), marked as imported.
>   It has the scope the import states, which MUST name only accounts the identity owns.
> - The deployment MUST declare which credentials it accepts for import. Any other credential MUST be
>   refused with `ERR-CredentialInvalid`. So MUST a credential already in the set.
> - The import MUST be visible to every authority holder of the identity's accounts (`INV-20`,
>   `REQ-OS-4`).

> **`REQ-MG-8`** An import MUST NOT give the operator a power it does not already hold.
>
> - It MUST be refused with `ERR-NotAuthorised` while any account of the identity is in owned custody
>   (`REQ-AU-6`, `REQ-AU-9`).
> - An imported credential proves nothing by being imported. Before it takes an account into owned
>   custody, it MUST be proven to work, like any other credential (`REQ-AU-13`).

> *Trade-off, stated plainly.* An imported credential carries the trust of the import. In the same
> way, the first credential the operator gives an identity at `open_identity` carries the trust of the
> opening. Only the holder's proof that it works (`REQ-AU-13`) shows that the holder holds it, not the
> operator.

The identifiers continue the `REQ-MG` family. `REQ-MG-1` to `REQ-MG-6` were withdrawn by amendment 0001
and are not reused.

### `REQ-OS-2` (one item added)

> - import credentials into an identity's set (`REQ-MG-7`);

### §6 — Epic N, Migration (new)

Epic L stays withdrawn, and its letter is not reused.

> **`US-N1`** — As the **operator**, I bring holders across from a prior system with the credentials they
> already hold, so that they do not enrol again.
>
> - `AC-N1.1` Given a credential's public material from the prior system, of a kind the deployment
>   accepts, When I import it into the holder's identity with a justification, Then it is a member of
>   the identity's set, marked as imported. The import is audited, attributed to me, and shown to the
>   identity's authority holders (`REQ-MG-7`, `REQ-AT-1`, `INV-20`).
> - `AC-N1.2` Given a credential of a kind the deployment does not accept, or one already in the set,
>   When I import it, Then the import is refused with `ERR-CredentialInvalid` and nothing changes
>   (`REQ-MG-7`).
> - `AC-N1.3` Given an identity with an account in owned custody, When I import into its set, Then the
>   import is refused with `ERR-NotAuthorised` (`REQ-MG-8`).
> - `AC-N1.4` Given an imported credential, When the holder reclaims custody with it, Then the reclaim
>   needs the credential's proof that it works, as for any other credential (`REQ-AU-13`, `REQ-MG-8`).

### §8.1 Commands (one row added; one kind extended)

> | `import_credentials` | Operator authority for imports; a justification; the identity active, and none of its accounts in owned custody; each credential's public material of a kind the deployment accepts, not already in the set, scoped within the identity's accounts | Each credential a member of the set, marked imported. Audit record. Shown to every authority holder (`REQ-MG-7`, `REQ-MG-8`). Otherwise `ERR-NotAuthorised`, `ERR-CredentialInvalid` or `ERR-JustificationRequired` |

Operation kinds: **credential change** is `register_credential` / `remove_credential` and
`import_credentials`.

### §15 (rows added)

> | N — Migration | `US-N1` | `REQ-MG-7`, `REQ-MG-8`, `REQ-RC-4` | `INV-20`, `INV-22` |

In the release scope:

> | N — Migration | `US-N1` | — |

## What this costs, stated plainly

- **Amendment 0001 is partly reversed.** Its A5 withdrew the migration epic: "a product specification
  cannot carry another party's migration." This epic carries no one party's migration. It specifies one
  generic capability, which any deployment may use or leave unused, beside `REQ-RC-4`'s opening load.
  But the spec again has a migration section, and the owner should see that this is a reversal.
- **One more operator capability.** It is bounded by `REQ-MG-8`, and it is visible. An operator that
  imports its own key into a delegated identity gains nothing it did not hold already.

## Carried to the plan

- **`F-028`** builds `import_credentials` and `tools/pass-import`. The import replaces the earlier draft,
  in which `open_identity` took several initial credentials.
- **The deployment's declaration** of accepted credentials is configuration (`DEC-19`). For passkeys, it
  is the set of accepted relying parties and origins.
