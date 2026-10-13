<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0029 — The latest anchor in place

**Status:** ruled by the owner on 2026-10-11 (#230, after the independent review's question on `REQ-PR-4`),
and applied to `SPEC.md` **v0.4.23** on `feat/settlement-anchor-in-place`.
**Target:** `SPEC.md` v0.4.22 as on `main` (amendments 0026 to 0028 applied).
**Date:** 2026-10-11
**Rationale in one line:** a settlement layer that keeps every commitment in its current state charges for
each one for good. One that keeps only the latest still records every commitment durably, in its finalized
history, and the requirement should say which of the two it asks for.

---

## The problem

`REQ-SL-1` asks the settlement layer to "durably record" each commitment, and to make "the commitment's
presence, and its order by that sequence, publicly checkable". It does not say where the record lives.
Read strictly, it asks for every commitment to stay in the layer's current state, forever. On a layer that
charges a deposit for each item of current state, that cost grows with the number of anchors, which is one
an hour (`REQ-PR-1`), and the deposit never comes back. That makes the cost per anchor unbounded, and the
layer's state grows along with it.

The owner ruled on 2026-10-11 that the layer keeps only the latest commitment in its current state,
overwritten by each new one, and that deployments already running migrate to that. An earlier commitment is
still recorded durably in two places: in the layer's finalized history, at the points where it was the
latest, and in the sequenced record of `REQ-SL-9` until that record is pruned. To check it, a recipient then
needs a reader that keeps that history.

## `REQ-SL-1` — one paragraph and a trade-off added

After its two items:

> The settlement layer MAY hold only the latest commitment in its current state. An earlier commitment is
> then durably recorded in the layer's finalized history, and checking its presence MAY require a reader
> that keeps that history; it MUST NOT require aledgent's cooperation (`REQ-PR-4`). This assumes that such
> readers are available independently of the operator: parties other than the operator keep the layer's
> finalized history. *Amendment 0029.*
>
> > **Trade-off, stated plainly.** A recipient who checks an anchor older than the latest needs a reader of
> > the settlement layer that keeps its finalized history, or a read saved earlier; any reader serves the
> > latest. A reader that cannot reach that history reports the anchor unreadable, never missing. If the
> > only readers that keep the history were the operator's, the check of an older anchor would in practice
> > depend on the operator; the assumption above is what rules that out.

## Why these choices

- **MAY, not MUST.** A layer that keeps every commitment in its current state still satisfies the
  requirement, and now so does one that keeps only the latest. Which one to build is the plan's choice.
- **Finalized history, not current state.** Once the layer has recorded a commitment at a final point,
  that record cannot be revised (`REQ-SL-3`). That the layer's current state has since moved on does not
  change what was recorded. Presence and order are still public: the order is the sequence each commitment
  carries, and the latest commitment is always in current state.
- **Never aledgent's cooperation, and an explicit assumption** (the owner, 2026-10-11). `REQ-PR-4` stands.
  The independent review of #230 pointed out the tension: for an older anchor, the check is only
  independent if someone other than the operator keeps the layer's history. A read saved earlier is only as
  good as whoever made it. The owner rules that independent access to the finalized history can be assumed
  as granted: readers that keep it are run by parties other than the operator. The requirement states that
  assumption, so a deployment on a layer where it fails is visibly outside it. The review's alternative was
  to keep every anchor's record in the sequenced record and have disclosures carry its coordinate. It was
  considered and not taken.
- **Unreadable, never missing.** A reader that has discarded the layer's past cannot tell an anchor that
  was never recorded from one it cannot see. Reporting it missing would assert something the reader does
  not know.

## What does not change

- `REQ-SL-5`, `REQ-SL-9` and `REQ-SL-10`. The sequenced record still records every anchor, and holding
  less in current state adds nothing new to what the layer holds.
- `REQ-PR-1` to `REQ-PR-11`. Anchoring, its finality (`REQ-PR-8`), and what an anchor establishes are
  unchanged. `NFR-22` still has aledgent keep every checkpoint an anchor bound, with the anchor's record.
- No command, no query, no error condition, and no invariant.

## Plan and code

`PLAN.md`'s settlement contract (`DEC-17`, `F-026`) keeps only its latest anchor, overwritten in place, and
migrates deployments of the earlier code, which kept every anchor by number (`T-026-09`;
`features/F-026-settlement-contract/NOTES.md`, decisions 25 and 26). In the contract venue, the verifier
(`crates/verify`) reads the latest anchor at the finalized head. It finds an earlier one at the finalized
blocks where that anchor was the latest, and reports it unreadable when the node it reads has discarded
those blocks' state.
