<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-003 — Identities, accounts, assets, authority

| | |
|---|---|
| Milestone | M-1 — The book, in a process |
| Track | A |
| Home | `crates/ledger-core` (modules `id`, `asset`, `identity`, `account`, `authority`) |
| Depends on | `F-001`. `DEP-7`, `DEP-8`, `DEP-10` (policy, not code) |
| Unblocks | `F-004`, `F-007`, `F-008`, `F-010`, `F-011`, `F-012`, `F-013` |
| Realises | `DEC-1`, `DEC-3` (authority above the kernel) |

## Goal
The book knows who exists, what they own, which assets exist at what precision, and who may do
what. Every command is authorised against an explicit, bounded authority. Evaluation fails closed.
The authority used is recorded, so "the operator acted" is never confused with "the holder acted".
Amounts cross into the book as exact minor units, through one conversion.

## Spec traces
- `REQ-ID-1`, `REQ-ID-2`, `REQ-ID-5`: minted identifiers: closed kinds, validated, no personal data, not tied to a settlement address.
- `REQ-LA-1`–`REQ-LA-6`: account ownership, kinds and their negative-balance policy, no deletion, pooled accounts as grants.
- `REQ-AS-1`–`REQ-AS-6`, `INV-24`, `INV-7`: one registry, normalised ids, precision only there, one decimal boundary, issuance.
- `REQ-AU-1`–`REQ-AU-8`, `REQ-AU-10`: fail-closed authority, bounded grants, immediate revocation, no widening, bounded operator authority, custody, attribution, credential hygiene, issuance authority.
- `REQ-PN-1`: initiator and on-behalf-of recorded.
- `INV-16`: no ledger fact without authority. `INV-22`: no self-widening. `INV-9`: an existing account can be credited.
- `REQ-AG-1`, `REQ-AG-2`: agents have their own credential. Bounds are enforced at the moment of the operation.

## Scope
**In:**
- identifier minting and parsing;
- the asset registry and the single decimal boundary;
- identities, and ledger accounts with their kernel sub-accounts;
- `open_identity`, `open_account`, `close_account`;
- the credential-set data model and principal resolution;
- authority grants: owner, delegated, operator and issuance, with bounds, expiry and revocation;
- the evaluator;
- read reach for `INV-16`;
- `issue` and `redeem`;
- attribution fields on the operation record.

**Out:**
- Adding and removing credentials, custody switching, recovery and successors (`F-012`). This
  feature models the recovery kind but never evaluates it.
- `register_asset` and issuance grants as audited operator commands, and freeze (`F-013`).
- Operation records, states and client keys (`F-007`).
- Transfers (`F-008`). Reservations (`F-004`). Queries (`F-011`).
- Window allowances and their restoration (`US-G4`, beyond V0).

## Design

**1. Identifiers** (`REQ-ID-1`, `REQ-ID-2`, `INV-17`). **Proposal:** `did:aledgent:<kind>:<body>` at
the surfaces. The kinds (`id`, `acct`, `auth`, `cred`, `op`) are a closed enum. The body is 26
characters of Crockford base32 over 16 bytes. Inside the library each kind is its own newtype, with
no conversion between kinds. `parse` checks prefix, kind, part count, alphabet and length before it
reads the body. The body is `blake2b-128("aledgent/id/v1" ‖ kind ‖ minting operation reference ‖
ordinal)`: deterministic on replay (`REQ-RT-3`), unique because operation references are (`F-007`),
and free of caller text. *Alternative:* random bytes supplied as an input by ingress. The account
reference for `REQ-SL-7` is derived from an `AccountId`, never an `IdentityId`. A compile-fail test
pins this (`REQ-ID-5`).

**2. Assets** (`REQ-AS-1`–`AS-5`, `INV-24`, `INV-7`).

```
Asset { id: AssetId, tag: u32 /* kernel ledger tag */, precision: u8, kind, status: Active | Deprecated }
```

- **Ids. Proposal:** an `AssetId` is normalised at every boundary to upper-case ASCII
  `[A-Z0-9]{2,12}` (`REQ-AS-3`). *Alternative:* case-folded Unicode with a stricter allow-list.
- **One registry.** `AssetRegistry` is the only type that holds `precision`. The kernel tag is
  assigned once at registration and never reused.
- **One boundary.** `Minor::from_decimal(&str, &Asset) -> Result<Minor, Refusal>` is the only
  decimal conversion in the workspace. More fractional digits than the precision gives
  `ERR-PrecisionExceeded`. It never rounds or truncates. Every internal API takes `Minor` (a `u64`
  newtype) or `NonZeroMinor`. A zero amount on a movement gives `ERR-ZeroAmount`.
- **Status and start-up.** An unregistered id gives `ERR-UnknownAsset`, a deprecated one
  `ERR-AssetDeprecated`. A registry digest lets `F-016` check the settlement layer's view (`REQ-AS-2`).

**3. Identities.** `Identity { id, kind: Person | Organisation | Agent, status, acts_for }`. Only an
agent names `acts_for`, and it must (`REQ-AG-1`). `open_identity` runs under a grantable operator
authority (`DEP-8`): policy, not a code path.

**4. Ledger accounts** (`REQ-LA-1`–`LA-5`, `INV-9`). A `LedgerAccount` has one owner, a kind, a
custody mode and a status. Its balances live in kernel sub-accounts, one per asset (`F-001` §2). The
kernel flags follow from the kind (`REQ-LA-2`):

| Kind | Kernel flag | Negative-balance rule |
|---|---|---|
| Customer | `BALANCE_MUST_NOT_BE_NEGATIVE` | kernel (`INV-3`) |
| Treasury | none | service check against a declared limit per asset, in the same apply unit |
| Issuance | none | none. One account for the book, owned by the operator's organisation identity, one sub-account per asset. It is the issuance counterparty |
| Suspense | **Proposal:** `BALANCE_MUST_NOT_BE_NEGATIVE` | kernel. *Alternative:* none, with reconciliation to zero only |

- **Lazy sub-accounts.** A kernel sub-account is created in the same apply step as the first posting
  that credits it. Any account can be credited in any registered asset at once (`INV-9`, `AC-A1.1`,
  `AC-A1.4`).
- **Closing** needs zero in every asset and balance kind, and no open reservation
  (`ERR-AccountNotEmpty`, `REQ-LA-4`). It sets `Closed`, deletes nothing, and later postings get
  `ERR-AccountClosed`. No field names a product, card or handle (`REQ-LA-5`).

**5. Principals and credentials** (`REQ-PN-1`, `REQ-AU-8`). `ledger-core` verifies no request
signatures. Ingress and VOS authenticate first (VOS gives a stable `PrincipalId` per call;
`DEP-7` makes direct holder credentials a policy switch). The binding (`F-005`)
passes `Caller { principal, credential, on_behalf_of }`. `F-003` keeps the credential-set model:

```
Credential { id, identity, marker: Ordinary | RecoveryHeld, scope: Option<NonEmptySet<AccountId>>, status }
```

An unknown, removed or wrong-identity credential gives `ERR-CredentialInvalid`. So does an
initiator that cannot be resolved (`REQ-PN-1`; **Proposal** for the mapping). No secret is held
here, and types that could carry one implement no `Debug`, `Display` or `Serialize` (`REQ-AU-8`).
Changing the set is `F-012`'s work.

**6. Authority grants** (`REQ-AU-2`–`AU-5`, `REQ-AU-10`, `INV-22`).

```
Grant { id, kind: Delegated | Operator | Issuance, principal, subject: Account(AccountId) | Asset(AssetId),
        ops: NonEmptySet<OperationKind>, bounds: Bounds, parent: Option<GrantId>, expires, revoked }
Bounds  // at least one of: per_operation, per_window, assets, counterparties (REQ-AG-2)
```

Owner authority is not a grant. It is derived from ownership and the credential set.

- **Unbounded grants.** `Bounds` has no empty value. `Bounds::new` with nothing set gives
  `ERR-UnboundedGrant` (`REQ-AU-2`, `AC-G1.1`).
- **Grants never change.** There is no update operation. Narrowing creates a child grant with
  `parent` set. Its **effective** scope and bounds are the meet (intersection and minimum) of the
  whole chain, computed at every evaluation. So a child can never act wider than its parent, whatever
  it declares. That makes widening structural, not a check (`REQ-AU-4`, `INV-22` at service, as
  `REQ-AR-6` declares for V0).
- **Revocation** marks one grant. Evaluation walks the chain on every use, so descendants stop at
  once, with no expiry to wait for (`REQ-AU-3`, `AC-G3.1`).
- **Operator grants** name operation kinds, never "all". They are usable only on an account whose
  custody is `Delegated` at the moment of evaluation (`REQ-AU-5`, `REQ-AU-6`).
- **Issuance grants** are over an asset and carry `per_window` and asset bounds (`REQ-AU-10`).

**7. Evaluation** (`REQ-AU-1`, `REQ-AU-7`, `REQ-LA-6`, `REQ-AG-2`).

`authorise(caller, op_kind, account, asset, amount, counterparty, now) -> Result<AuthorityUsed, Refusal>`
runs in a fixed order:

1. Credential → `ERR-CredentialInvalid`.
2. Reach to the account → `ERR-UnknownAccount`. It comes before any status, so a status cannot leak
   an account the caller cannot reach.
3. Owning identity's status → `ERR-IdentityRestricted`. Account status → `ERR-AccountClosed`.
4. Asset → `ERR-UnknownAsset` or `ERR-AssetDeprecated`.
5. Candidates. **Proposal:** prefer owner (credential in scope), then the delegated grant with the
   narrowest effective scope, then operator. None left → `ERR-NotAuthorised`. Only revoked ones
   left → `ERR-AuthorityRevoked`.
6. Bounds → `ERR-BoundExceeded`.

Any internal error counts as "no authority" (fail closed). The result, `AuthorityUsed { kind,
grant, principal, credential, on_behalf_of }`, goes on the operation record (`F-007`) and the audit
record (`F-013`) (`REQ-AU-7`, `AC-G1.2`).

- **Window counters** live in hot state, keyed by (grant, window index), with windows derived from
  the time input. They are charged in the same apply unit as the posting.
- **Pooled accounts** (`REQ-LA-6`, `AC-A4.*`): each drawing identity holds its own delegated grant.
  `OQ-5` is not resolved. The model has no notion of integrating party, so it serves either answer.

**8. Read reach** (`INV-16`). `reach(caller, subject)` is the one function `F-011` uses on every
read. A grant reaches only its subject (`REQ-PV-2`). No reach gives `ERR-UnknownAccount`. It is
service-enforced, as `REQ-AR-6` declares; §9 names Store (`F-015` Open points).

**9. Issue and redeem** (`REQ-AS-6`, `AC-F4.*`).

`issue` needs an issuance grant within its bounds. It is one `Issue` posting: debit the issuance
counterparty's sub-account, credit the destination. `redeem` needs the same grant. It debits the
holder's available (flagged, so the kernel refuses an overdraw) and credits the issuance
counterparty. Both are two-entry kernel transfers, so the per-asset zero-sum keeps the trial
balance at zero (`INV-5`).

## Test plan
- **Acceptance criteria, as `F-015` cases at the service layer:**
  - `AC-A1.1`, `AC-A1.4`: a new account is credited at once in an asset it never held.
  - `AC-A1.2`: no cost field exists in any response type.
  - `AC-A1.3`: with `F-007`.
  - `AC-A2.1`; `AC-A2.2`: with `F-008`.
  - `AC-A4.1`, `AC-A4.2`: pooled drawing with and without a grant.
  - `AC-F1.1`, `AC-F1.2`, `AC-F3.1`.
  - `AC-F4.1`–`AC-F4.3`.
  - `AC-G1.1`–`AC-G1.3`, `AC-G2.1`, `AC-G2.2`, `AC-G3.1`; `AC-G3.2`: with `F-004`.
- **Errors** owned by this feature (`F-015` §7): one case each, with shape checks.
- **Property tests:**
  - a random chain of child grants never has a larger effective scope than its root (`INV-22`);
  - revoking any ancestor denies every descendant at once (`REQ-AU-3`);
  - `from_decimal` never rounds, over random strings and precisions (`REQ-AS-4`);
  - `parse(mint(x)) == x`, and malformed ids of every kind are refused before the body is read
    (`REQ-ID-1`).
- **Invariants:** `INV-9`, `INV-16`, `INV-17` (identifier grammar), `INV-20` (attribution present on
  every accepted operation), `INV-22` and `INV-24`, at the layers set in `F-015` §6.

## Exit criterion
From PLAN §4: *`AC-A1.*`, `AC-A2.*`, `AC-A4.*`, `AC-F1.*`, `AC-F3.1`, `AC-F4.*`, `AC-G1.*`–`G3.*`
pass.* Made concrete: those cases are green in the `m1` profile on both `F-001` stores, together with
the cases for this feature's 13 errors and for `INV-9`, `INV-16`, `INV-22` and `INV-24`.

**Build order.** `AC-A1.3` (a repeated client key returns the same account) needs `F-007`, and
`AC-A2.2` (a transfer between two of my accounts) needs `F-008`. Both close with those features, not
here, so that `F-003` depends only on `F-001`.

## Open points
- **Overlap with `F-012`.** PLAN lists "credential sets with account scoping" under both `F-003`
  and `F-012`. The split proposed here: `F-003` owns the model and its use in evaluation, and
  `F-012` owns the commands that change the set (`REQ-AU-20`), custody switching and recovery. The
  owner should confirm.
- **Order of features.** `AC-A1.3` and `AC-A2.2` need `F-007` and `F-008`. `F-003` is done only when
  those land, or the owner moves the two criteria.
- **`INV-16`, `INV-22` layers.** They are service-enforced here, as `REQ-AR-6` asks. §9 names Store
  and Credential. A credential-layer form would be signed attenuation tokens, which is a later
  hardening. See `F-015` Open points.
- **Window restoration.** `REQ-AG-3` serves only `US-G4`, which is beyond V0. Counters are kept, but
  restoration is not built. Issuance's per-window bound is therefore not restored on reversal in V0.
- **Credits to closed accounts.** `INV-9` for an inbound arrival addressed to a closed account is
  `F-010`'s routing question. Here, a closed account refuses postings.
- **Asset range.** `u64` minor units bound the largest holding per asset (`F-001` Open points).
  `register_asset` should refuse a precision for which `u64` cannot hold the expected supply. The
  threshold is for the owner.
- **Proposals to review:** the identifier grammar and its derivation, the asset-id alphabet, the
  Suspense flag, the authority preference order, and the mapping of an unresolvable initiator to
  `ERR-CredentialInvalid`.
- Exposure: `DEP-7` (direct holder credentials), `DEP-8` (who opens identities; `OQ-58`, `OQ-21`
  unresolved) and `DEP-10` (client-key domain, via `F-007`).
