<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0031 — An upgrade of the settlement layer that changes nothing the adapter signs does not stop it

**Status:** applied to `SPEC.md` **v0.4.25** on the owner's go of 2026-10-14.
**Target:** `SPEC.md` v0.4.24 as on `develop`.
**Date:** 2026-10-14
**Rationale in one line:** a settlement layer upgrade that changes nothing a transaction carries should not
stop settlement. One that does change it must still stop it.

---

## The problem

On 2026-10-14 at 21:01Z, Kreivo enacted a runtime upgrade: release `0.17.1`, `spec_version` 134. It added
calls to the contracts' chain extension and changed nothing a transaction carries. Its transaction
extensions, its `transaction_version` and the crates that check its signatures were those of 133.
In `SPEC.md`'s terms, the settlement layer changed its rules without changing what an instruction must
carry. Every deployment's adapter still stopped serving, and settlement and anchoring stood still for about two and
a half hours, until a release that knew 134 was built and deployed.

The specification did not ask for that. `REQ-SA-2` asks the adapter to fail closed when it is connected to
the wrong network. A runtime upgrade does not change the network. The stop came from the plan: `F-016`
listed "the runtime `spec_version` is on an allow-list" among the adapter's start-up assertions, and
`T-027-07` keyed the transaction format by `spec_version`. A version number says that the runtime
changed. It does not say whether a transaction changed.

The opposite failure is real too, and it is why the allow-list was strict. Between 132 and 133, the order
of the message a PASS device signs was flipped. A signature in the old order would not verify, and nothing
in the metadata's names showed it. Kreivo bumped `transaction_version` for that upgrade, from 12 to 13.

The specification says nothing about either case, so each deployment's behaviour on an upgrade is a plan
decision. It should be a requirement.

## The change

### §5.6 (settlement adapter): a new requirement

**`REQ-SA-9`** When the settlement layer changes its rules, the adapter MUST decide whether it can keep
serving from what an instruction it submits must carry under the new rules. That means the version of the
instruction format the layer declares, the parts every instruction must carry and the types of their data,
and the type of each call the adapter submits, all as the layer itself describes them. A version number of
the layer's rules alone MUST NOT decide it, either way. Before it signs the first instruction under rules it
has not served before, the adapter MUST have the settlement layer itself confirm that an instruction it
built and signed under them is valid, and it refuses to serve until the layer does. An operator MAY name in
configuration the format to use for a set of rules. The layer's confirmation is still required.

### §6, Epic E (`US-E4`): two new criteria

- `AC-E4.3` Given a change to the settlement layer's rules that leaves what an instruction must carry
  unchanged, Then the adapter keeps serving, with no new release or configuration (`REQ-SA-9`).
- `AC-E4.4` Given a change that alters what an instruction must carry, or an instruction the layer does not
  confirm as valid, Then the adapter refuses to serve until it has a format the layer confirms
  (`REQ-SA-9`).

### §15 (traceability)

Epic E's row cites `REQ-SA-1`–`REQ-SA-9`.

## What does not change

- `REQ-SA-2`. A different network (genesis) still stops the adapter, whatever its rules.
- The core's side of `AC-E4.2`. While the adapter refuses to serve, the core refuses outbound operations
  with `ERR-SettlementLayerUnavailable`, and parked instructions resume when it serves again.
- No identifier is withdrawn, renumbered or reused.

## Plan

- `F-016`'s start-up assertions replace "the runtime `spec_version` is on an allow-list" with "the runtime's
  transaction surface is one the adapter has a format for, confirmed by the layer (`REQ-SA-9`)".
- `F-027` gets one task, `T-027-12`. The format is chosen by a fingerprint of the surface instead of
  `spec_version`. A runtime not yet served is confirmed with the runtime's own transaction validation, at
  the best block, before the first batch. The `spec_versions` setting becomes optional: an operator's
  mapping from a version to a known format, never a gate.
- `T-027-07` stays done as written. Its tests carry over: the formats of 132 and 133 still differ, and an
  upgrade between them still switches the format of the next batch.

## Identifier ledger

| Action | Identifiers |
|---|---|
| New | `REQ-SA-9`, `AC-E4.3`, `AC-E4.4`, `T-027-12` |
| Withdrawn | None |
| Changed | None |
