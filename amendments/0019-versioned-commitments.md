<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0019 — the book's commitments name their construction

**Status:** ruled by the owner on 2026-10-05: "ship v2 from genesis (yes), and offer it to CipherClerk as
patch 0006 (yes)". Applied: `SPEC.md` v0.4.13 (on `main` with amendment 0021 as v0.4.14), `PLAN.md` v0.8.6 (`DEC-25`, accepted).
Owner ruling `cs-q04` (2026-10-05, `PLAN.md` v0.8.7): v1 stays a supported, documented opt-in beside v2;
nothing is removed when CipherClerk merges 0006.
**Target:** `SPEC.md` v0.4.12 (amendments 0001 to 0016, 0018 and 0020 applied; 0017 is F-034's draft).
**Date:** 2026-10-04 (drafted, experiment `D-COMPACT-SMT`); 2026-10-05 (ruled and applied).
**Plan:** `DEC-25` (accepted), PR #168 (the construction), `notes/MEASUREMENTS-F031.md` §11.
**Rationale in one line:** the book's state commitment changes construction before go-live, and a
recipient holding an anchor must always know which construction it commits in.

---

## The problem

The spec says what an anchor establishes (`REQ-PR-3`), that a recipient can check a disclosure against
one (`REQ-PR-4`), and that a checkpoint commits to exactly a stated sequence (`REQ-PR-7`). It does not say
how the state commitment is built, and it should not: that is a plan choice.

But a construction can change. The plan's commitment hashed 128 levels a path; the compact construction
measured by `D-COMPACT-SMT` hashes about `log₂ n + 1`, and halves the core's work per reservation. If a
construction changes after anchors exist, an anchor whose construction cannot be told is a value nobody can
check a later proof against. The spec had no requirement that prevented that.

## What does not change

- `REQ-PR-1` to `REQ-PR-9`. An anchor still commits to book state, still proves non-revision and not
  solvency (`REQ-PR-3`), and a disclosure is still a signed assertion (`REQ-PR-6`).
- `INV-17`, `REQ-PR-2`: the anchored value is still a commitment that reveals nothing; the construction's
  name is a version, not book content.
- No requirement names a hash, a tree or a depth.

---

## R1 — `REQ-PR-10`: a commitment names its construction (new, §5.10, after `REQ-PR-9`)

> **`REQ-PR-10`** Every checkpoint MUST name the construction of the state commitment it carries, and its
> anchor MUST bind that name, so that a recipient checking a disclosure against an anchor knows which
> construction a proof against the anchored state uses. A change of construction MUST NOT make an earlier
> anchor uncheckable: aledgent MUST keep the means to check a proof against every construction it has
> anchored in.

Traces: `REQ-PR-3`, `REQ-PR-4`, `REQ-PR-7`, `AC-I3.1`. Epic J's trace row reads `REQ-PR-1`–`REQ-PR-10`.

## R2 — `AC-J1.5` (new, Epic J)

> - `AC-J1.5` Given a checkpoint a disclosure rests on, Then it names the construction of its state
>   commitment, and its anchor binds that name: the same checkpoint with another construction named does not
>   match the anchor (`REQ-PR-10`).

---

## `DEC-25` (accepted, `PLAN.md` §2)

The book's trees are compact sparse Merkle trees (v2 roots), from genesis: CipherClerk's key space and leaf
hashes, the Diem/Aptos Jellyfish hashing above them, a kernel root with a version byte, a checkpoint that
names its construction and an anchor commitment (`aledgent/anchor/v2`) that binds it. CipherClerk's
full-depth tree (v1) stays a supported, documented opt-in, the feature `smt-v1` (owner ruling `cs-q04`): a
deployment may choose it at genesis, and its checkpoints then name construction 1. v1 is not weaker, only
costlier: more actor gas a reservation, smaller witnesses. This is why `REQ-PR-10` matters beyond a
change of construction: two deployments may anchor in different ones. Measured under the
JAM-compatible default: 137.9 M to 72.0 M of actor gas a reservation, 27.1 to 40.3 reservations a second
at batch 128 under 8 G (+49 %), witnesses 27 % larger. The full decision, its alternatives and its cost are
in `PLAN.md`.
