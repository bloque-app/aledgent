<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-003 — implementation notes

Written while implementing `F-003` (issue #3), 2026-09-23. The plan's proposals were followed as
accepted: the identifier grammar and its derivation, the asset-id alphabet, the Suspense flag, the
authority preference order, and the mapping of an unresolvable initiator to `ERR-CredentialInvalid`.
Where the plan was silent, the more conservative behaviour was chosen (refuse rather than allow);
each such choice is listed here for review.

## What landed

`crates/ledger-core` gained, on `F-015`'s `refusal` skeleton:

| Module | What |
|---|---|
| `id` | `did:aledgent:<kind>:<body>`, one newtype per kind, minting, validating `parse`, the account reference of `REQ-SL-7` |
| `asset` | `AssetId`, the one `AssetRegistry`, `Minor`, `NonZeroMinor`, `Minor::from_decimal`, the registry digest |
| `identity`, `account` | identities with `acts_for`; ledger accounts, kinds, custody, kernel sub-accounts, the Treasury rule |
| `principal` | `Caller`, the credential-set model, `resolve` |
| `authority` | grants, `Bounds`, the meet of a chain, window charges, `AuthorityUsed` |
| `eval` | the evaluator in the plan's fixed order, issuance authority, read reach |
| `state` | the service store and its write rules |
| `ledger` | `Ledger`: genesis, `open_identity`, `set_identity_status`, `open_account`, `close_account`, `grant_authority`, `revoke_authority`, `authorise`, `issue`, `redeem`, `parse_amount`, `reach_*`, `available`, `trial_balance` |

## Decisions taken during implementation

### Store layer

1. **A service store with its own write rules.** `INV-7`, `INV-17` and `INV-24` name the store.
   The kernel's leaf store holds no registry, identity or grant, so `state::State` is the store of
   everything else, and it enforces its rules on every write batch, all or nothing, whoever writes:
   one registry slot and one precision per asset (`INV-24`); a precision above `MAX_PRECISION` and
   a window counter past its limit or `u64::MAX` refused (`INV-7`); only identifiers that are the
   minting of the operation stored beside them (`INV-17`); records inserted once, `Closed` and
   `Removed` absorbing, one revocation, one issuance counterparty, no unattributed record
   (`INV-20`). In `M-1` it lives in memory on both in-process targets; its durable home is Agent
   state (`F-005`) and the side store (`F-006`).
2. **New conformance port.** `Port::ServiceStore` and `Harness::service_store()` (default `None`)
   give store cases an empty service store, as `F-001`'s store cases get an empty leaf store.
   Forging a record whose identifier is not its minting needs `ledger-core`'s new
   `conformance-ports` feature; `tools/ci/check-conformance-ports.sh` now rejects a production
   crate that enables it as well as `ledger-kernel`'s, with a CI fixture that must be rejected.
3. **The service port has its own book.** `InProcess::service()` bootstraps a book of the target's
   kind and runs genesis on it (`USD`, `EUR`, `BTC`, and `OLD` deprecated). It is not the book the
   kernel and binding ports hand out; no case needs both.

### Identifiers

4. **Body derivation.** The first 16 bytes of `blake2b-256("aledgent/id/v1" ‖ kind ‖ op ‖
   ordinal)`, not a native blake2b-128, the same construction `F-001` uses for its ids.
5. **Canonical form only.** Parsing accepts upper-case Crockford only, and a first character of
   `0`–`7` (the two spare bits zero). A lower-case body is refused, so one identifier has one
   spelling.
6. **Operation references** are `ledger-kernel`'s `OperationRef` under the `op` kind. Until
   `F-007` mints them and owns client keys, a command whose operation reference already has a
   record is refused with `ERR-ClientKeyReused` (`TODO(F-007)`: return the original outcome for an
   identical request).

### Assets and amounts

7. **The registry is fixed at genesis**, a reviewed configuration (`REQ-AS-2`). `register_asset`
   and deprecation as audited commands are `F-013`'s.
8. **`MAX_PRECISION` is 18.** The plan leaves the threshold to the owner (Open points). 18 is the
   largest precision at which `u64` minor units still hold more than one whole unit.
9. **`from_decimal` is strict by digits**: `12.340` is refused for a precision of 2, as the plan
   says. A string that is not a plain decimal (`-1`, `1e2`, `.5`, `1,5`), or whose value does not
   fit in `u64` minor units, is also `ERR-PrecisionExceeded`: SPEC §10's list is closed and has no
   better entry. An asset id that cannot be spelled is `ERR-UnknownAsset`.

### Identities, accounts and the operator

10. **Genesis** creates the operator's organisation identity and first credential, and the
    issuance counterparty (kind `Issuance`, owned by the operator identity). No authority of any
    kind is evaluated over the issuance counterparty: value moves through it only by `issue` and
    `redeem`, and nothing is issued into it.
11. **The operator's policy authority** (`DEP-8`, `OQ-58`, `OQ-21` unresolved). Opening identities,
    changing an identity's status, opening accounts for others, and making operator and issuance
    grants are the operator identity's, with an unscoped credential, recorded as operator
    authority with no grant. Nothing grants it to others yet; `F-013` turns these into audited
    operator commands.
12. **Who opens what.** A holder opens only a Customer account for itself, with an unscoped
    credential. Treasury and Suspense accounts are the operator's. Agents own no account: SPEC §3
    says an agent holds delegated authority only.
13. **`set_identity_status`** was added: `ERR-IdentityRestricted` and `AC-I4.2` need an identity
    that is restricted or closed, and nothing else changes it. The operator cannot change its own.
14. **Treasury limits** are declared per asset at opening. An undeclared asset has a limit of zero.
    Past the limit is `ERR-InsufficientAvailable`.
15. **`close_account`** checks available in every registered asset, deprecated ones included.
    Reserved value comes with `F-004` (`TODO(F-004)`: refuse while a reservation is open);
    encumbered value is zero in V0 (`DEF-11`).

### Principals and credentials

16. **Resolution.** A restricted principal is `ERR-IdentityRestricted`; a closed one is
    `ERR-CredentialInvalid`. An agent always acts on behalf of the identity it acts for; claiming
    another is `ERR-CredentialInvalid`.
17. **Credential scope limits every authority over accounts** — owner, delegated and operator —
    not only owner authority. Issuance authority, opening accounts, and making operator or issuance
    grants need an unscoped credential. The store refuses a scope naming an account the identity
    does not own.

### Grants and evaluation

18. **Operator grants may be over every delegated-custody account** (`Subject::DelegatedAccounts`),
    beside one account; either counts only while the account's custody is delegated. The plan's
    `Subject` had `Account` and `Asset` only.
19. **A chain is at most 8 grants** (`MAX_CHAIN`). A longer or broken chain is no authority.
20. **Amount bounds are minor units of the operation's asset.** Window counters are kept per
    (grant, window, asset), and a use charges the window of **every** link of the chain, so children
    cannot multiply their parent's window between them.
21. **Bounds are checked on the one candidate chosen**, as the plan's order has it; the evaluator
    does not fall back to another candidate whose bounds would allow the operation.
22. **Bound dimensions.** A counterparty bound on a value-moving operation with no counterparty is
    exceeded (fail closed). An operation that moves nothing (`CloseAccount`) is not limited by an
    amount bound.
23. **Reach for evaluation counts revoked and expired grants**, so `ERR-AuthorityRevoked` can be
    told from `ERR-NotAuthorised`. Reach for reads needs live authority. An operator grant does not
    reach an owned-custody account at all (`REQ-AU-6`: no operator authority exists over it), so
    the operator gets `ERR-UnknownAccount` there.
24. **`redeem` needs two authorities**: issuance authority for the asset, and authority over the
    debited account for `Redeem`. SPEC §8.1 lists only the first; `REQ-AU-1` (kind, account and
    bounds) and `REQ-AU-6` (no operator power over owned custody) need the second. The record
    carries both.
25. **`grant_authority`** accepts a child that declares more than its parent: the effective
    authority is the meet, so nothing widens (the plan's "structural, not a check"). The grantee
    must be an active identity. An issuance grant must carry a per-window bound and an asset bound
    on its asset, or it is `ERR-UnboundedGrant` (`REQ-AU-10`).
26. **`revoke_authority`** is open to the owner of the account a grant is over, to the holder of
    any grant it was narrowed from, and to the operator for operator and issuance grants. Revoking
    a revoked grant writes only the record.

### Refusals

27. **Binding faults are `ERR-Internal`** (amendment 0006 item 8, `REQ-ER-2`, ruled 2026-09-29;
    they failed closed as `ERR-NotAuthorised` before), the fault kept for the operator
    (`Ledger::last_fault`) and counted as an `internal_fault` incident (`Ledger::internal_faults`).
28. **Kernel outcomes**: `Insufficient` → `ERR-InsufficientAvailable`; `Closed` →
    `ERR-AccountClosed`; `Duplicate` → `ERR-ClientKeyReused` (unreachable while decision 6 holds).

## A reading for the owner: `AC-A4.2`

`AC-A4.2` says a drawing without authority is refused with `ERR-NotAuthorised`. The plan's order
checks reach before candidates, and `INV-16` forbids revealing an account to a principal that has
no authority over it. So a drawing identity that reaches the pool — it holds a grant, but none
covers this drawing, or its grant has lapsed — gets `ERR-NotAuthorised`; a principal with no
authority over the pool at all gets `ERR-UnknownAccount`. Neither is ever let through. The case
asserts both. If the owner wants `ERR-NotAuthorised` in the second case too, `INV-16` and the
`ERR-UnknownAccount` condition need an amendment.

## Conformance (T-003-09)

- 42 cases in `crates/conformance/src/cases/f003.rs` run: `INV-7`, `INV-17` and `INV-24` at the
  store, `INV-9`, `INV-16`, `INV-20` and `INV-22` at the service, the 13 errors, and 22 acceptance
  criteria. All pass on both in-process stores; in the `m1` profile every `F-003` case is green.
- `INV-22` at the credential layer (§9's target) stays pending on `F-003`, reported and not
  required: signed attenuation is a later hardening (plan Open points).
- **Moved**: `AC-G3.2` to `F-004` (an operation already reserved runs to a terminal state after a
  revocation: it needs a reservation; the plan runs it "with `F-004`"). `INV-16` at the store
  layer to `F-011` (its storage trait filters every read by the authorised scope inside the store,
  `F-011` §2). An `F-003` `INV-16` case at the service layer was added for `reach`.
- `AC-A1.3` and `AC-A2.2` stay pending on `F-007` and `F-008`, as registered.

## Deferred

- `TODO(F-007)`: operation references and client keys; the attribution record folds into the
  operation record.
- `TODO(F-004)`: `close_account` refuses while a reservation is open; `AC-G3.2`.
- `F-011`: the query surface over `reach_account` and `reach_identity`; `available` and
  `trial_balance` are placeholders for its `balances` and `trial_balance`.
- `F-012`: changing the credential set, custody switching, recovery (the recovery authority kind is
  modelled in `AuthorityKind` and never evaluated here).
- `F-013`: `register_asset`, deprecation, freeze, and the audited forms of the operator's policy
  authority.
- `F-005`, `F-006`: the durable home of the service store.
