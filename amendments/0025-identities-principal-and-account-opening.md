<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0025 — The identities principal's role, and who opens accounts

**Status:** drafted and applied on 2026-10-09, from rulings already given: `OQ-64` (2026-10-02, closed by amendment 0009), `prq-05-fix` (2026-10-08) and, for clause R2,
`pk-register` (a) (2026-10-09). Its four questions were
ruled the same day, all as recommended (`a0025-q1` to `q4`, below). Applied to `SPEC.md` **v0.4.18**.
**Target:** `SPEC.md` v0.4.17 as on `main` (amendment 0024 applied).
**Date:** 2026-10-09
**Rationale in one line:** the identities principal could be "whoever plays that role", but nothing said
how a principal comes to play it, and no principal but the identities principal and the owner could open
an account, so the services that open accounts for a customer had to hold the operator's token.

---

## The problem

`OQ-64`, as the owner ruled it on 2026-10-02:

> Opening identities is an exclusive authority of the identities principal (the operator, or whichever
> principal plays this role), but opening accounts is not limited to it. An identity can create its own
> extra accounts, and the identities principal can create accounts too.

Amendment 0009 wrote that into `REQ-ID-6`. Two things were left open, and both became blocking on
2026-10-08, when `prq-05-fix` ruled that the issuing service, the custody service and the onboarding service each
hold **their own** principal with operator grants, and that **no service holds the operator token**:

1. **How another principal comes to play the identities principal.** `REQ-ID-6` says the role may be
   played by a principal other than the operator, and that its authority is "split by kind ... like
   every operator authority (`REQ-OS-1`)". No operator capability named it, so only the operator itself
   could open an identity. The onboarding service, which opens every new customer's identity at registration,
   could not do it as its own principal.
2. **A service that opens a customer's further accounts.** Under the interim account model
   (`accounts-default-and-virtual`), the custody service opens a new account under the customer's identity
   whenever a `virtual` product is added. `REQ-ID-6` lets only the identities principal or the identity
   itself open an account. The custody service is neither, and the customers' identities (operator-managed,
   by the owner's ruling that identities open with an operator-held credential) do not act through it. So
   it could open them only with the operator's token.

## `REQ-ID-6` (revised; the text below is the first draft, before the rulings of R1 and R4: `SPEC.md` holds the final one)

> **`REQ-ID-6`** Identities MUST be opened only by the **identities principal**: the operator, or the
> principal that plays that role in a deployment. The role is an operator authority of its own kind,
> `open_identity` (`REQ-OS-1`): only the operator grants and revokes it, with a justification, and the
> operator keeps it while another principal holds it. A ledger account is opened by the identities
> principal, for any identity; by the identity that will own it: an identity MAY open further accounts of
> its own, under its own authority (`US-A2`); or by a principal holding a delegated grant over that
> identity that names `open_account`. Such a grant is made by the identity itself or by the identities
> principal, names `open_account` and nothing else, and opens only customer accounts in delegated custody
> for that identity; no other grant names `open_account` (§5.9). The authority to open identities is split
> by kind, attributed and audited, like every operator authority (`REQ-OS-1`, `REQ-AU-7`, `REQ-AT-1`).
> *Amendment 0025.* [The rest of the requirement is unchanged.]

What changed, and why:

- **The role is a grant.** Granting it is the operator's alone, as every operator-surface grant is
  (`REQ-OS-1`), so "whoever plays this role" is always someone the operator chose, on the trail with a
  justification, and revocable at once (`REQ-AU-3`). The operator keeps the role: a deployment never
  loses the ability to open identities because the principal holding it was revoked.
- **A third way to open an account: a delegated grant over the identity.** It is the identity's own
  authority to open further accounts (`US-A2`), delegated. The identity grants it, or the identities
  principal, which may open accounts for that identity anyway and so grants no more than it holds (§8.1:
  "Grantor holds the authority being granted"). It is narrow by construction: one identity, one kind,
  customer accounts only, delegated custody only. Owned custody stays the holder's own choice, made with
  the recovery mechanisms shown to them (`REQ-AU-9`), never a delegate's.

*Trade-off, stated plainly.* Like every grant, a grant over an identity must state at least one bound
(`REQ-AU-2`). Opening an account moves no value, so a value bound does not limit it, as it does not limit
a grant that names only `close_account` or `disclose` today. What limits it is its shape: one identity,
`open_account` alone, customer accounts in delegated custody. A holder of such a grant can open any number
of empty accounts for that identity until it is revoked.

## §5.9 — `Authority`

> `account:    AccountId | Asset (for issuance authority) | IdentityId (to open its accounts)`

## §8.1 — two rows revised

> | `open_account` | Identity active; kind and custody stated. Opened by the identities principal, by the identity itself, or under a delegated grant over the identity naming `open_account`, which opens a customer account in delegated custody only (`REQ-ID-6`). Owned custody only for an identity whose credential set is not empty (`REQ-ID-8`) | *(unchanged)* |
> | `grant_authority` | Grantor holds the authority being granted, or owns the account. At least one bound stated. A grant over an identity is delegated, names `open_account` alone, and is made by the identity or the identities principal (`REQ-ID-6`) | *(unchanged)* |

## `REQ-OS-2` — one item added

> - grant and revoke the identities principal's role (`REQ-ID-6`);

## What does not change

- **Opening an identity carries no justification of its own**, whether the operator or the holder of the
  role opens it: it is attributed to the role's grant (`REQ-AU-7`) and recorded, and the grant itself was
  made with a justification. This is how the operator has opened identities since amendment 0009
  (question 2).
- `REQ-ID-7`: registering an integrating party and deciding which identity providers the deployment
  accepts stay the operator's alone. No grant gives them, and the identities principal's role does not.
- `REQ-ID-8`: an identity with an empty credential set is still opened only under the operator's
  authority to open identities, which is this role. `T-028-10` to `T-028-12` stay unbuilt for
  this operator (the owner's ruling that identities open with an operator-held credential).
- `REQ-AU-5`, `REQ-AU-6`: the role is not over any account. A grant over an identity reaches none of its
  accounts: it moves no value and reads nothing.

## Clause R2 — `register_credential` by the identities principal (ruling `pk-register`)

**Ruled by the owner on 2026-10-09, `pk-register` (a):** the identities principal may authorise
`register_credential` on identities it manages, in place of the operator-held key; the user's browser
still runs the WebAuthn ceremony on an ingress-issued `register` challenge.

**The problem.** A customer's passkey is registered when they accept the terms (`tos-passkey-
registration`, `tos-aledgent-only`). The customer's identity is managed by the operator: its credential is
operator-held (the owner's ruling that identities open with an operator-held credential), so the customer has nothing to present to
`register_credential` themselves, and `REQ-AU-20` lets only "an existing credential of the set" add one.
The onboarding service, which runs that step, would need the operator's token, which `prq-05-fix` forbids.

**`REQ-AU-20` (one sentence added).**

> The identities principal MAY also add a credential to the set of an identity it manages, acting for
> it: an identity that is not the operator, not an agent, and holds no account in owned custody. It
> authorises the addition only: a credential that proves possession is proven by its own ceremony, and a
> holder-held recovery credential is never added this way. The addition is attributed to the identities
> principal's authority and to the identity it acted for (`REQ-AU-7`).

**§8.1, `register_credential`:** "also by the identities principal acting for an identity it manages
(`REQ-AU-20`, amendment 0025 R2)".

*Trade-off, stated plainly.* This is the power the operator-held key already has over these identities,
given to the principal that plays the identities principal: no more. A passkey still proves possession:
the user's browser runs the WebAuthn ceremony on a `register` challenge ingress issued, and ingress
verifies it before the command is logged; the onboarding service only authorises the addition, it never
holds the key. What it does let it do is add a credential of a kind that proves nothing (a `key` verifier, an API
key) to a managed identity's set. "Manages" stops at owned custody: an identity holding any account in
owned custody is its holder's, and the operator has no power there (`REQ-AU-6`, as `REQ-MG-8` says of an
import).

**Code.** `Ledger::register_credential` takes the managed set when a principal that is not an agent
names another identity in `on-behalf-of` (`managed_set` in `ledger/custody.rs`); everything else about
the command is unchanged. Tests: `req_au_20_the_identities_principal_registers_a_credential_for_a_
managed_identity` (core), `amendment_0025_r2_onboarding_registers_a_users_passkey_and_the_user_signs_in`
(sidecar, the ceremony end to end).

## Clause R3 — broad grants never reach the operator's own accounts (ruling `au4-q2`)

`au4-q2`: services get account-scoped grants on the operator-owned system accounts they operate, "not
broad operator grants". System accounts are opened in delegated custody (`ops/runbooks/system-
accounts.md`), so an operator grant over every delegated account (the custody service's, PRQ-05) reached them too.

**`REQ-AU-5` (one sentence added).**

> A grant over every account in delegated custody MUST NOT cover an account the operator's own identity
> owns: such an account is reached only by a grant that names it.

Chosen over a custody of its own for system accounts: it needs no new custody mode, no change to how
the genesis's system accounts are loaded, and it holds for every account the operator owns, whoever
opens one later. The operator itself is unaffected: it owns them.

**Code.** `eval::covers_in`, used by authorisation, reach and the authority holders (`INV-20`). Tests:
`au4_q2_a_broad_grant_never_reaches_a_system_account_and_an_account_scoped_one_does` (core); the sidecar's
`prq_05_each_service_acts_within_its_grants_and_is_refused_outside_them`.

## Rulings this applies, and what it leaves to the owner

Applied: `OQ-64` (2026-10-02), `prq-05-fix` (2026-10-08), `accounts-default-and-virtual` (the custody service opens a
`virtual`'s account under the user's identity).

The four questions this amendment raised were ruled on 2026-10-09, all as recommended:

- **`a0025-q1`: one holder.** The role is granted alone (never with other kinds), and at most one
  principal besides the operator holds it: granting it to another revokes it from the holder in the same
  operation, and the store refuses a second live holder.
- **`a0025-q2`: a justification, optional, as pass-through.** No new field. `open_identity` and
  `open_account` accept the pass-through `reference` of `REQ-PO-5`, and the identities principal puts its
  own justification there: the onboarding service sends the registration's identifier. It is echoed in the reply and in
  `operation(reference)`, as on any operation.
- **`a0025-q3`: the custody service's grants for migrated identities are in the chainspec.** The seed carries, as
  grants of the seed (`REQ-RC-6`), one grant over every loaded identity that holds accounts in delegated
  custody, to the principal that opens its further accounts, issued by the operator as the identities
  principal at genesis. The change to the operator's migration tooling is `T-036-30`.
- **`a0025-q4`: the opener grants the issuing service.** Whoever opens an account grants the issuing service
  its per-account grant: the onboarding service at registration, as the identities principal; the custody
  service for a new `virtual`, under its grant over the identity. Migrated accounts get the issuing
  service's grants in the chainspec, as in q3 (`T-036-30`).

## Clause R4 — the opener delegates over the accounts it opens, never wider (`a0025-q4`)

A grant over an identity names `open_account`, and MAY also name the kinds its holder delegates over that
identity's accounts in delegated custody. It reaches no account by itself. Its holder grants delegated
authority over such an account **only as a narrowing of it**: the account grant names the identity grant
as its `parent`, and the effective authority is the meet of the chain, as for every grant: the kinds both
name, the smaller bound, every window of both (§5.9, `REQ-AU-4`). So the custody service can give the issuing service exactly what the onboarding service gave it over that
identity, or less, and nothing broader; and revoking the custody service's grant stops every grant made
under it (`REQ-AU-3`). A grant over an identity is never narrowed into another grant over
an identity: the holder cannot pass `open_account` on.

The identities principal grants delegated authority over an account in delegated custody of an identity
it manages (as in R2) directly, with no parent: the same power the operator-held key has over that
identity. That is how the onboarding service grants the issuing service over the default account it opens, and how the operator, as the
identities principal, issues the seed's grants at genesis.

*Trade-off, stated plainly.* A grant over an identity's per-window bound is one counter for every account
grant made under it: all of the issuing service's grants over one customer's accounts share the window the onboarding service
gave the custody service for that customer. That is the point (nothing broader), and the operator sets that
bound once per customer, not per account.

## Code

`T-036-30` (2026-10-10), the seed's grants of `a0025-q3` and `a0025-q4`: the operator's migration tooling
seeds the service principals after every migrated identity, so their identifiers are fixed at genesis and
no migrated identifier moves, and appends to the seed's Grants section, issued by the operator as the
identities principal, one delegated grant over every seeded identity that owns an account (to the custody
service: `open_account` and the kinds it delegates) and one delegated grant with no parent over each of
those accounts (to the issuing service), with the bounds the file gives. No change to the core or to the
seed's format (`features/F-036-seeded-genesis/spec.md` §1, `NOTES.md` §14). Tests: `t_036_30_*` in the
migration tooling and in `ledger-core`'s `ledger::tests::seeding`.

`ledger-core`: `OperatorOpKind::OpenIdentity` (code 19), `OperationKind::OpenAccount` (tag 13),
`Subject::Identity` (tag 4); `open_identity`, `open_account`, `grant_authority` and `revoke_authority` as
above; the store refuses `open_account` in any grant not over an identity, and a grant over an identity
naming anything else. Ingress: `open_identity` among the operator-surface kinds, `open_account` among the
operation kinds, `{"identity": did}` as a grant subject. Tests: `crates/ledger-core/src/ledger/tests/
identities.rs`; `e2e/sidecar/tests/sidecar.rs` (`prq_05_*`) through the surface. No host call is added
(`m-jam-only`): the change is evaluation logic in the core.
