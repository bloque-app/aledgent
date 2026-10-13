<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0002 — reservations as a property, acceptance before completion, custody switching, federation, the settlement layer's shape, and the first assurance level

**Status:** applied to `SPEC.md` v0.3.0 (2026-09-21)
**Target:** `SPEC.md` v0.2.0 → v0.3.0
**Date:** 2026-09-21
**Rationale in one line:** the owner's review of the fit against the intended stack, four parked notes,
and one product request each require a change to what the specification says, and none of them may be
decided in the plan.

Owner rulings this amendment records (2026-09-21): a kernel change is possible and not assumed; the
custody switch is wanted both ways; federation admits both the immediate and the netted realisation;
the settlement layer's requirements may be sharpened toward a pallet; the three operating numbers stay
as targets. Items marked **[recommendation]** were not ruled on and are the author's proposal, to be
struck or kept.

No identifier is renumbered. Withdrawn identifiers are not reused. New identifiers extend their family.

---

## A1 — A reservation is a property, not a layer inside an account

### The problem

§5.4 and `REQ-KER-3` fix three balance layers *inside one account*, with movement between them
expressed as entries. That is one realisation, and it is not the only correct one: a realisation that
holds committed value in a dedicated holding position, with every movement an ordinary balanced posting,
satisfies every story and invariant that reservations exist for. Fixing the mechanism excludes it, and
makes the specification depend on a kernel capability that need not exist.

### The amendment

**`REQ-KER-3` (revised).** The kernel MUST be able to represent, per account and per asset, value that
is **available**, value that is **reserved** to one named operation, and value that is **encumbered**
under a standing rule, such that each is separately readable and their sum is the total. Every movement
among the three MUST be expressed as balanced entries in the book (`INV-1`). Whether the three are
partitions of one account or positions the account draws on is a realisation choice, and either MUST
satisfy `REQ-BA-1`–`REQ-BA-4`.

**§5.4 (revised heading and lead).** "Balance kinds" replaces "Balance layers". The lead reads: for
each (account, asset), three kinds of balance — available, reserved, encumbered — and their sum, total.
The glossary entries for *balance layer*, *available*, *reserved* and *encumbered* are reworded
accordingly; the term **balance kind** is canonical and *layer* is retired from normative text.

**`REQ-BA-6` (new).** `reserved` MAY be a derived number — the sum of the account's open reservations —
provided it is read from the same consistent view as `available` (`REQ-BA-3`, `REQ-QR-2`).

**`INV-3` (unchanged in text).** With the property wording, a realisation that keeps the never-negative
check on the spendable balance at kernel strength remains conformant; `REQ-AR-4` continues to forbid
weakening it.

### What this costs, stated plainly

The specification no longer promises that "reserved" is a stored balance. A surface that shows it may
be summing. That is exactly the position `REQ-BA-3` already requires — one view, one number — and it
costs nothing a holder can observe.

---

## A2 — Acceptance is not completion

### The problem

`REQ-CS-2` requires a command to return synchronously with the operation's state. Two consequences were
raised by the owner. First, a caller can read an acknowledgement as completion, and for an operation
with an external leg completion is only `Settled`, which the synchronous reply can never carry. Second,
the requirement assumes the component that decides is always reachable; if it is being redeployed, the
only honest reply is "accepted, not yet decided", which the current wording forbids.

### The amendment

**State `Received` (new, §5.6).** An operation MAY be durably received before it is `Accepted`. In
`Received` no balance effect exists and no deadline has started. `Received → Accepted` or `Received →
Rejected` MUST follow within a stated bound; a `Received` operation past that bound is an incident, not
a silent state.

**`REQ-CS-2` (revised).** A command MUST return synchronously with the **operation reference** and the
operation's state at that instant, which MAY be `Received`. The reply is an **acknowledgement**; it is
not an outcome. It MUST NOT wait on the settlement layer.

**`REQ-CS-3` (unchanged).** A command that reserves returns only after the reservation is durable. The
revision of `REQ-CS-2` therefore applies to commands that do not reserve, and to the accept-or-refuse
decision itself only where the caller has explicitly asked for asynchronous acceptance. **[recommendation, accepted]**
A reservation decision — reserve or refuse — is always synchronous; `NFR-1`'s bound applies to it and to
nothing else.

**`REQ-CS-6` (new).** Completion of an operation is learned by notification (`CTR-EVT`) or by query
(`operation(reference)`), never inferred from the acknowledgement. A reply whose state is not terminal
MUST be labelled non-terminal (`REQ-OP-7`).

**`REQ-CS-7` (new).** A component that receives commands MUST be deployable and restartable
independently of the component that decides them, and MUST NOT lose a received command when the
deciding component is unavailable (`NFR-5`, `REQ-RT-4`).

**[recommendation, accepted] No blocking completion mode.** A "wait until terminal" reply is not offered: it
would re-couple the caller to the settlement layer's timing and hide `Needs-Review`. Callers that want
synchronous completion compose it from acknowledgement plus notification.

**`REQ-EV-1` (revised).** Add `Received → Accepted` to the transitions notified when the caller could not
otherwise observe them.

### What this costs, stated plainly

Every integrating party must handle a non-terminal acknowledgement. Some will treat it as success anyway;
`REQ-OP-7` and `REQ-CS-6` make that a caller defect rather than an ambiguity in the contract.

---

## A3 — Switching an account between owned and delegated custody, both ways

### The problem

Custody mode is fixed at `open_account` and nothing changes it afterwards. `OQ-13` asked whether it may
change; the owner has ruled that it may, in both directions, in place.

### The amendment

**`REQ-AU-11` (new).** An account MAY change custody mode after opening, in either direction, without
being closed and reopened. The change is an operation: idempotent on a client key, attributed
(`REQ-AU-7`), audited (`REQ-AT-1`), and visible to every authority holder of the account (`INV-20`).

**`REQ-AU-12` (new).** **Owned → delegated** grants the operator a power it did not hold. It MUST be
initiated with the holder's own credential and MUST NOT be initiable by the operator.

**`REQ-AU-13` (new).** **Delegated → owned** removes operator authority. It MUST require that a holder
credential exists and has been proven to work before the change takes effect; operations already
reserved run to a terminal state under the authority that started them (`REQ-OP-9`); any standing
encumbrance placed under operator authority MUST be either lifted or explicitly carried over with the
holder's consent.

**`REQ-AU-14` (new).** Delegated authorities granted by the holder (agents, members) are unaffected by a
custody-mode change.

**§8.1 (new rows).** `delegate_custody` and `reclaim_custody`, with the pre-conditions above.

**`US-A7` (new, Epic A).** As an identity holder, I move my account between self-custody and delegated
custody and back, and the direction that gives the operator power is mine alone to trigger.

**`OQ-13` (revised).** The second half is closed; the unit of owned custody remains open.

**Release scope (owner ruling, overriding the author's recommendation).** `US-A7` lands in V0. Because
the switch presupposes owned custody and its recovery model, `US-A5` moves into V0 with it, `DEF-2` is
narrowed to "owned custody as the *default* of a new account", and `OQ-13` (unit of owned custody) and
`OQ-14` (recovery model) become V0-blocking. The owner has further ruled that the recovery authority MUST
be a different principal from the operator authority; that ruling is recorded here and lands in the spec
with the recovery model itself.

### What this costs, stated plainly

The asymmetry will feel unhelpful in support: a holder who has lost their credential cannot be moved to
delegated custody by the operator "to help them". That is the rule, because the alternative is operator
authority by another name.

---

## A4 — Federation is a deployment topology

### The problem

§1.7 assumes one operator and one settlement layer per deployment but says nothing about several
deployments sharing a settlement layer. That topology changes who the custodian is, and the
specification should admit it as a first-class arrangement rather than leave it to `DEF-1`.

### The amendment

**§1.7, assumption 4 (new).** A deployment MAY be one of many attached to the same settlement layer.
Another deployment is an ordinary settlement counterparty; nothing in this specification distinguishes
it from any other.

**`REQ-SL-6` (new).** Value moving between two deployments MUST settle through the settlement layer as
an external leg of each, immediately per operation or netted over a stated window. A netted arrangement
MUST bound the exposure that accumulates inside the window and MUST reconcile to zero at its close
(`DEF-1`, `OQ-32`).

**§4.3 (new row).** *Who is the custodian?* In a single deployment, the operator, in both custody modes
(§8 of the design overview states why). In a federation, each deployment's operator for its own holders;
the operator of a deployment that issues an asset is that asset's issuer, not the custodian of the value
other deployments hold.

**`OQ-9` (revised, now blocking V0).** How does a counterparty — including another deployment — address
one specific account when sending value in, with no per-account record on the settlement layer?
Resolved by A5's `REQ-SL-7`.

### What this costs, stated plainly

The immediate variant pays the settlement layer's cost on every cross-deployment transfer and is public
in aggregate. The netted variant is cheaper and carries credit exposure between closes. The
specification permits both; the plan chooses per relationship.

---

## A5 — What the settlement layer must offer, sharpened

### The problem

`REQ-SL-1`–`REQ-SL-5` describe a settlement layer loosely enough that three open questions — native
coordinates (`OQ-7`), addressing an account (`OQ-9`), and what stays checkable on chain (`OQ-10`) — could
not close. A settlement layer built for aledgent can answer all three, and the specification may require
it without naming any network.

### The amendment

**`REQ-SL-7` (new).** The settlement layer MUST accept value addressed to a deployment together with an
**opaque account reference** the deployment resolves, and MUST NOT require a record per account.

**`REQ-SL-8` (new).** The settlement layer MUST record every value movement to or from a deployment
under a coordinate that is **native to the settlement layer and stable**, and for movements the
deployment originated MUST carry the deployment's **operation reference**, so that each leg's outcome is
determinable by identity (`REQ-SO-3`, `REQ-IN-1`).

**`REQ-SL-9` (new).** The settlement layer MUST expose the movements and anchors concerning a deployment
as a **sequenced record readable by position**, so that observation completeness (`REQ-SA-7`) is a read
over a contiguous range and does not depend on a subscription.

**`REQ-SL-1` (revised).** Anchors MUST be recorded with a per-deployment **sequence** and ordered by it.

**`REQ-SL-10` (new).** The settlement layer MUST hold, for a deployment, nothing beyond: its identity and
settlement positions, its anchors, and the sequenced record of `REQ-SL-9`. In particular no balance,
account or identity of any holder.

**Closed:** `OQ-7` (native), `OQ-9` (by `REQ-SL-7`), `OQ-10` (by `REQ-SL-10`).

### What this costs, stated plainly

The settlement layer becomes something that has to be *built for* aledgent, not merely connected to.
That is a project of its own with its own review, and the specification now depends on it existing.

---

## A6 — The first assurance level

### The problem

`REQ-AR-5` requires a deployment to declare which invariants it enforces and which it attests, and
`OQ-22` asked what V0's declaration is. The fit analysis answers it.

### The amendment

**`REQ-AR-6` (new).** V0's assurance level MUST declare at least: `INV-1`–`INV-4`, `INV-7`, `INV-12` as
**kernel- or store-enforced**; `INV-15`, `INV-16`, `INV-20`, `INV-22` as **service-enforced**; and every
disclosure as a **signed assertion** (`REQ-PR-6`) until an independently checkable construction ships
(`DEF-3`). A later release MAY strengthen any entry; none may weaken one without an amendment
(`REQ-AR-4`).

**Closed:** `OQ-22`.

---

## A7 — Identifier hygiene

### The problem

Two defects in identifier discipline. `REQ-AR-1`–`REQ-AR-3` never existed: the family began at 4, so
§0's rule that a gap means a withdrawal is false there. And `REQ-PR-0`, the attribution rule in §3, sits
in the *provability* family by accident of prefix.

### The amendment

**`REQ-AR-1`, `REQ-AR-2`, `REQ-AR-3` (new).** The three assertions §4.1 states in prose become
requirements in their own right: (1) aledgent MUST NOT make a synchronous call into a caller to reach an
outcome (restating `REQ-EV-5` at the architecture level); (2) the settlement adapter MUST NOT write the
book nor declare an outcome (`INV-11`); (3) settlement-layer key material MUST NOT reside in the
component that holds the book (`NFR-8`).

**`REQ-PR-0` withdrawn; `REQ-PN-1` (new, same text)** under a new family `PN` (principals). Every
reference is updated.

**§0 (revised).** The rule reads: a gap in a sequence means a withdrawal recorded in an amendment, and
every family starts at 1.

---

## A8 — The operating numbers stay as targets

`NFR-1`, `NFR-2` and `NFR-3` are unchanged. A sentence is added to §11: the three numeric targets bind
from the first release in which they can be measured against the deployed host; until then they are
design targets and a plan MUST NOT relax them silently.

---

## What this amendment does not do

- It does not decide the migration of any existing estate (parked note 3); that is plan input.
- It does not choose between holding accounts and a kernel primitive (A1); that is a plan decision that
  may become an upstream request.
- It pulls owned custody into V0 (A3, owner ruling), which makes `OQ-13` and `OQ-14` V0-blocking.

## Carried to the plan

- A1: the realisation of reservations, and the upstream question to the kernel's authors.
- A2: the ingress / core / adapter process split and how a received command survives a core restart.
- A4: which relationships settle immediately and which net, and the bridge-and-venue prior art as the
  netted realisation.
- A5: the settlement pallet's calls and storage, as the realisation of `REQ-SL-1`, `REQ-SL-7`–`REQ-SL-10`.
- A6: the machine-readable form of the assurance declaration.

## Identifier ledger

| Action | Identifiers |
|---|---|
| Revised | `REQ-KER-3`, `REQ-KER-4`, `REQ-CS-2`, `REQ-CS-3`, `REQ-EV-1`, `REQ-SL-1`, `INV-3` (wording), `DEF-2`, `OQ-13`, `OQ-14`, §5.4, §5.6, §1.7, §4.1, §4.3, §0, §11, §15 |
| New | `REQ-BA-6`, `REQ-CS-6`, `REQ-CS-7`, `REQ-AU-11`–`REQ-AU-14`, `US-A7` (`AC-A7.1`–`AC-A7.5`), `REQ-SL-6`–`REQ-SL-10`, `REQ-AR-1`–`REQ-AR-3`, `REQ-AR-6`, `REQ-AR-7`, `REQ-PN-1`, state `Received` |
| Withdrawn | `REQ-PR-0` (→ `REQ-PN-1`) |
| Closed | `OQ-7`, `OQ-10`, `OQ-22`; `OQ-13` in part |
