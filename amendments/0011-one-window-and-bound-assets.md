<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0011 — one window, and assets bound after it

**Status:** ruled by the owner on 2026-10-02; applied to `SPEC.md` v0.4.5
**Target:** `SPEC.md` v0.4.4 as on `main` (amendments 0008 to 0010); this one takes it to **v0.4.5**
**Date:** 2026-10-03
**Rationale in one line:** the production cutover is the only window there will ever be. Everything that
follows it, an asset moving from the book alone onto the settlement layer included, happens online, at
zero divergence; notifications carry the latest final anchor so that a recipient can search between two
of them; registering an integrating party, which the client key's domain needs, is now specified; and
`NFR-16`'s number, 1 s at p99, is confirmed.

Nothing here resolves a question beyond what was ruled. Where a ruling leaves a gap, the gap is named
in "Questions raised" and the conservative reading is the one written.

---

## R1 — No maintenance window after the cutover

**The ruling** (2026-10-02): the production cutover is the only window there will ever be, and the
spec says so.

**Applied.** A new non-functional requirement, after `NFR-16`:

> | `NFR-17` | **One window.** The load of a deployment's initial state (`REQ-RC-4`) is the only period in which aledgent MAY refuse or defer holder operations for maintenance. After it closes, no change to the deployment MAY need such a period: not binding an asset to the settlement layer (`REQ-AS-8`), not starting or moving anchoring, not moving the deployment to another place on the settlement layer, not an upgrade (`NFR-5`). Each is made online, with every balance unchanged, and every operation either decided as usual or refused under its own named condition (§10). A change that cannot be made this way is not made. |

`NFR-5` is unchanged; `NFR-17` says what it means after the cutover.

## R2 — Book-only assets, and their binding

**The rulings.**

- The first assets are created in the book, under an issuance authority, and bound to the
  chain later, once the settlement layer's side of each is ready.
- The later, one-time move of a book-only asset onto the settlement layer is planned now.
- The settlement layer's metadata for an asset is its view of the asset (`REQ-AS-2`), but its symbol and
  precision are read from their own fields, never from the name.
- An asset whose layer metadata is missing or does not match can still be registered and used in the
  book; it is bound only once the issuance authority has completed its registration.
- Once bound, later changes to the layer's metadata are ignored. No hold.
- The bind completes only at zero divergence.
- An asset that is never registered is never bound.

**Applied.**

§5.3, the asset record:

Before:

> ```
> Asset {
>     id:         AssetId         // closed registry, case-normalised at the boundary
>     precision:  Count           // minor units per unit; lives here and nowhere else
>     kind:       Fiat | Stable | Native | Other
>     status:     Active | Deprecated
> }
> ```

After:

> ```
> Asset {
>     id:         AssetId         // closed registry, case-normalised at the boundary
>     precision:  Count           // minor units per unit; lives here and nowhere else
>     kind:       Fiat | Stable | Native | Other
>     status:     Active | Deprecated
>     binding:    BookOnly | Binding | Bound(SettlementAsset)   // REQ-AS-7, REQ-AS-8
> }
> ```

`REQ-AS-2`, revised:

Before:

> **`REQ-AS-2`** Adding or changing an asset MUST be a reviewed, recorded change, never a side effect at
> runtime. At start-up, the settlement layer's view of an asset MUST be checked against the registry. A mismatch
> MUST be a start-up failure, not a silent divergence.

After:

> **`REQ-AS-2`** Adding or changing an asset MUST be a reviewed, recorded change, never a side effect at
> runtime. The settlement layer's view of an asset MUST be checked against the registry when the asset is
> bound (`REQ-AS-8`), and a mismatch MUST refuse the binding, never become a silent divergence. After the
> binding, the registry is the only authority on the asset (`REQ-AS-9`).

New, after `REQ-AS-6`:

> **`REQ-AS-7`** **Book-only assets.** An asset is registered book-only: in the registry, with its
> precision, and bound to nothing on the settlement layer. Its value enters and leaves the book only by
> issuance and redemption (`REQ-AS-6`), and moves inside it like any other asset's. An external leg in a
> book-only asset MUST be refused at submission with `ERR-UnsupportedLeg` (`REQ-SA-1`).
>
> **`REQ-AS-8`** **Binding.** A book-only asset MAY be bound to the settlement layer, once, after its
> registration, by a principal holding issuance authority for it (§5.9). Binding is a reviewed, recorded
> change with a justification and an audit record (`REQ-AT-1`), and is visible to every authority holder of
> the asset (`REQ-OS-4`). It names the asset's identity on the settlement layer. It succeeds only if all of
> these hold:
>
> - the settlement layer describes an asset there, live, whose symbol, normalised (`REQ-AS-3`), is the
>   asset's id, and whose precision is the registry's. Both are read from the layer's own fields for them.
>   A name or any other text is never parsed for either. A layer that describes no asset there, or
>   describes it otherwise, refuses the binding with `ERR-UnsupportedLeg`;
> - no other asset is bound, or being bound, to the same identity on the layer (`REQ-AS-3`), and this
>   asset is book-only. Otherwise `ERR-NotAuthorised`;
> - **zero divergence:** what the settlement layer holds for aledgent in the asset equals, to the minor
>   unit, the book's outstanding balance of the asset, which is the negation of what the issuance
>   counterparty holds in it (`INV-5`). The equality MUST be established by the settlement layer, at the
>   point it records the binding, and become final there (`REQ-SL-3`). A binding that does not establish
>   it is refused, and the asset stays book-only.
>
> From the request until the binding is final or refused, the asset is `Binding`. While it is `Binding`,
> issuance and redemption of the asset are refused with `ERR-NotAuthorised`, so that the outstanding balance
> the settlement layer is checking cannot move. A refused binding returns the asset to book-only. Once the
> binding is final, the asset is `Bound`: external legs in it are supported as the adapter declares
> (`REQ-SA-1`); issuance and redemption go on under issuance authority, as for any asset (`REQ-AS-6`);
> and the settlement layer's holding in it is reconciled with the book on `NFR-9`'s third axis from that
> point, the binding's amount its starting position. A binding is never undone.
>
> **`REQ-AS-9`** **After binding.** Once an asset is bound, its record in the registry MUST NOT change
> because the settlement layer's description of the asset changed. Such a change is ignored: it holds
> nothing, refuses nothing, and is not a divergence. The precision stays the registry's (`REQ-AS-1`).

The asset record's `binding` names the three states; the glossary gains:

> | **Book-only asset** | An asset in the registry that is bound to nothing on the settlement layer. Its value enters and leaves by issuance and redemption only (`REQ-AS-7`). |
> | **Binding** | The one-time act that ties a book-only asset to its identity on the settlement layer, at zero divergence (`REQ-AS-8`). |

§7.D, `REQ-OS-2`'s list gains, after "register an asset":

> - bind a book-only asset to the settlement layer, under issuance authority for it (`REQ-AS-8`);

§8.1, after `register_asset`:

> | `bind_asset` | Issuance authority for the asset; a justification; the asset book-only; its identity on the settlement layer named | Asset `Binding`; the settlement layer asked to record the binding against the book's outstanding balance. Once that is final, `Bound` (`REQ-AS-8`). Otherwise `ERR-NotAuthorised`, `ERR-UnsupportedLeg`, `ERR-SettlementLayerUnavailable` or `ERR-JustificationRequired`; a binding the layer refuses returns the asset to book-only |

and `register_asset`'s post-condition:

Before:

> | `register_asset` | Operator authority; reviewed change | Asset registered with exactly one precision (`REQ-AS-1`) |

After:

> | `register_asset` | Operator authority; reviewed change | Asset registered with exactly one precision, book-only (`REQ-AS-1`, `REQ-AS-7`) |

Epic F, a new story:

> **`US-F5`** — As an **issuer**, I move an asset I issued in the book onto the settlement layer, without a
> window and without a difference.
> - `AC-F5.1` Given a book-only asset, Then an external leg in it is refused at submission with
>   `ERR-UnsupportedLeg`, and issuance in it works (`REQ-AS-7`).
> - `AC-F5.2` Given a binding whose settlement-layer asset is missing, not live, or has another symbol or
>   precision in its own fields, Then it is refused with `ERR-UnsupportedLeg`. A name is never read
>   (`REQ-AS-8`).
> - `AC-F5.3` Given a binding where the settlement layer's holding differs from the book's outstanding
>   balance by one minor unit, Then it is refused, and the asset stays book-only with its issuance
>   authority (`REQ-AS-8`).
> - `AC-F5.4` Given a binding at zero divergence, Then while it is pending the asset's issuance and
>   redemption are refused with `ERR-NotAuthorised`; and When it is final, Then the asset is bound, its
>   external legs are supported, and `NFR-9`'s third axis finds no difference (`REQ-AS-8`, `NFR-9`).
> - `AC-F5.5` Given a bound asset, When the settlement layer's description of it changes, Then nothing in
>   the book changes, and nothing is held or refused (`REQ-AS-9`).

§15: Epic F's row gains `US-F5` and `REQ-AS-7`–`REQ-AS-9`; V0's scope gains `US-F5`.

**What does not change.** `INV-24` (one precision, in one registry) and `REQ-AS-1`: a book-only asset's
precision is recorded at registration, a reviewed change; the binding checks the layer against it and
never takes it from the layer. Every error is one §10 already names.

## R3 — Every notification carries the latest final anchor

**The ruling** (2026-10-02): every notification carries the latest final anchor, so that a client
pushed a `Needs-Review` can respond with a process that checks between the two anchors the notifications
provide (or the last anchor and now).

**Applied.** `REQ-EV-1`, one sentence added at its end:

> A notification MUST also carry the latest anchor whose publication had reached finality when the
> transition was recorded (`REQ-PR-8`): its number, the last entry it commits to (`REQ-PR-7`), and its
> coordinate on the settlement layer. Where no anchor was final yet, it says so. What it carries is fixed
> when the transition is recorded, so that a redelivery carries the same body (`REQ-EV-3`).

Epic M, `US-M2` gains:

> - `AC-M2.4` Given a notification, Then it names the latest anchor that was final when its transition
>   was recorded, or says that none was. Every delivery of it names the same one (`REQ-EV-1`, `REQ-EV-3`).

## R4 — `OQ-65` closed: registering an integrating party

**The ruling** (2026-10-02): the operator registers parties; one party per principal.

**Applied.** `REQ-ID-7`, its last sentence replaced:

Before:

> What registering an
> integrating party records, and which commands carry these operations, is `OQ-65`.

After:

> Registering an integrating party is an operator operation (§7.D): it records the party's identifier and
> the principals tied to it. Only the operator ties a principal to a party, when it registers the party or
> later, and a principal is tied to at most one party; tying it to a second is refused with
> `ERR-NotAuthorised`. A principal tied to none, such as the operator's own, is a client-key domain by
> itself (`REQ-OP-10`). Which identity providers the deployment accepts, and on what terms, is the
> deployment's configuration, set by the operator alone.

`REQ-OP-10`: where it names "the integrating party the initiating principal belongs to", it gains "or the
principal itself, where it belongs to none (`REQ-ID-7`)".

§7.D, `REQ-OS-2`'s list gains:

> - register an integrating party, and tie a principal to it (`REQ-ID-7`);

§8.1 gains:

> | `register_integrating_party` | Operator authority; a justification; a party identifier not registered before; principals tied to no party | Party registered with its principals (`REQ-ID-7`). Audit record. Otherwise `ERR-NotAuthorised` or `ERR-JustificationRequired` |
> | `tie_principal` | Operator authority; a justification; the party registered; the principal tied to no party | Principal tied to the party; from then on its client keys are the party's (`REQ-OP-10`). Otherwise `ERR-NotAuthorised` |

§14, `OQ-65`:

> | `OQ-65` | *Closed by amendment 0011:* the operator registers an integrating party with the principals tied to it, and ties principals to it later; a principal belongs to at most one party, and one tied to none is its own client-key domain (`REQ-ID-7`, `REQ-OP-10`). | — |

## R5 — `NFR-16` confirmed: 1 s at p99

**The ruling** (2026-10-02): 1 s is right. The owner added that the interpreter is to be
considered gone: the recompiler is accepted upstream, or in progress there, whichever lands first.

**Applied.** `NFR-16`'s closing note:

Before:

> *The number is proposed and waits for the owner's confirmation (amendment 0008, R3).*

After:

> *Confirmed by the owner on 2026-10-02 (amendment 0011, R5).*

The bound and its measurement are unchanged. `ERR-DecisionUnavailable`'s wait at ingress stays 30 s:
`NFR-16` binds commands answered with their decision, and a command that needs the
settlement layer's acknowledgement is not one of them.

## Questions raised

Each is put to the owner. The text above is written on the conservative default named here, until the
owner rules.

| Raised | Default written | Where |
|---|---|---|
| May a bound asset still be issued or redeemed in the book (value entering other than through the layer)? | **Yes, as for any asset** (`REQ-AS-6`); only while the binding is pending are they refused, so that the amount being checked cannot move. The rulings bind the equality at the binding, not after it; and conversions, which happen outside aledgent as a redemption plus an issuance (the owner, 2026-10-03), would otherwise stop for every bound asset | `REQ-AS-8` |
| What of a principal tied to no integrating party? | It is its own client-key domain, as the operator's own principals are today | `REQ-ID-7`, `REQ-OP-10` |
| Does the zero-divergence test count opening balances (`REQ-RC-4`) as outstanding? | **Yes.** Outstanding is every unit the book holds in the asset outside the issuance counterparty, however it entered | `REQ-AS-8` |

## What this costs, stated plainly

- **No second window, ever.** Anything that cannot be done online is not done. A future change that
  needs one, a re-genesis for example, has to be designed as an online migration or not made.
- **After the binding, in-book issuance is not backed on the layer by construction.** An issuer that
  goes on issuing a bound asset in the book creates value the contract does not hold until it deposits
  the same amount; `NFR-9`'s third axis compares deposits, withdrawals and the binding's amount with
  the contract's position, so it does not see that gap. Whether issuance of a bound asset should stop is
  the owner's question `k-01`.
- **The layer's metadata stops mattering after the binding.** A change by the layer's own governance, a
  new symbol or new decimals, does not reach the book. Decimals that change on the layer would make every
  later deposit and withdrawal read a different amount than the book means; the registry is
  authoritative, so that is the layer's divergence, found on `NFR-9`'s third axis, never the book's.
- **An asset whose layer metadata is missing stays book-only forever** unless its metadata is set on the
  layer.
- **A notification's anchor can be an hour old.** It names the latest anchor final when the transition
  was recorded, which can trail the transition by up to an anchoring interval (`REQ-PR-1`) plus finality.

## Versions

`spec_version: 0.4.4` → `0.4.5`; `date` → `2026-10-03`.

## Identifier ledger

| Action | Identifiers |
|---|---|
| New | `NFR-17`, `REQ-AS-7`, `REQ-AS-8`, `REQ-AS-9`, `US-F5`, `AC-F5.1`–`AC-F5.5`, `AC-M2.4`; commands `bind_asset`, `register_integrating_party`, `tie_principal`; glossary *book-only asset*, *binding* |
| Revised | `REQ-AS-2`, `REQ-EV-1`, `REQ-ID-7`, `REQ-OP-10`, `REQ-OS-2`; `NFR-16` (confirmed); §5.3's asset record; §8.1 `register_asset`; §15 (F row, release scope) |
| Closed | `OQ-65` |
| Unchanged | Every invariant and every error |

## Carried to the plan

- `DEC-21` (new): the single-stage rollout. It replaces `DEC-14` and the staged canary.
- `F-029` (reworked): book-only registration, binding by the issuance authority, symbol and precision from
  their own fields, no holds after binding.
- `F-017`/`F-009`: the latest final anchor in the notification (`AC-M2.4`).
- `F-009`: `T-009-16` is unblocked: the client key's domain is the principal's integrating party, or the
  principal.
- `F-015`: the catalogue at v0.4.5.
