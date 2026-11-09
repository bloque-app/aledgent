<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0043 — An asset registered with the wrong precision and never used can be removed

**Status:** **decided by the owner on 2026-11-08**: keep the identifiers, remove the ones never used, register them
again at the right precision. **Applied** to `SPEC.md` **v0.4.31** in the same pass: `REQ-AS-2`, `REQ-AS-10` (new),
the passage on a lagging directory in `REQ-RT-11`, `REQ-OS-2`, the command table (`remove_asset`), `INV-24`, and the
traceability row for Epic F. The build follows it (`T-003-12`).
**Target:** `SPEC.md` v0.4.30 as on `develop` (at `71a19c1`); amendment 0042 is the last on `develop`.
**Date:** 2026-11-08
**Number:** 0042 is the last on `develop`; no open pull request claims 0043.
**Amends:** `REQ-AS-2` (one phrase), `REQ-AS-10` (new), `REQ-RT-11` (one parenthesis), `REQ-OS-2` (one line),
§8's command table (one row), `INV-24` (its wording). No error condition is added: a refusal reuses
`ERR-NotAuthorised`, `ERR-UnknownAsset` and `ERR-JustificationRequired`, and no identifier is renumbered.
**Rationale in one line:** an asset registered with the wrong precision can be neither corrected nor re-registered,
and its identifier stays taken for good; if nothing was ever done with it, the only thing lost by removing it is the
mistake.

---

## The problem

`INV-24` and `REQ-AS-1` say an asset has exactly one precision, recorded in the registry, and `F-003` states it more
strongly: "Precision, tag and kind never change". That is right for an asset in use: every amount in the book is an
integer count of that precision's minor units, and changing the precision would silently change what every one of them
means.

It leaves one mistake with no remedy. Suppose an asset `TSTX` is registered at 8 decimals and should have been 6. The
operator can deprecate it (`ERR-AssetDeprecated` for every new operation) but cannot register `TSTX` again, because the
identifier is taken, and cannot edit the precision. If `TSTX` was never used, nothing in the book depends on the
mistake: no balance, no posting, no reservation. Keeping it forces a different identifier on integrations that are
already named for this one.

## The change

A new operator operation, `remove_asset`, removes an asset from the registry. `SPEC.md` `REQ-AS-10` states it. In
short:

1. It is a reviewed change with a justification and an audit record, under its own operator authority (a new kind
   next to `register_asset` and `deprecate_asset`). It does not edit the precision: the asset is removed, and the
   identifier is registered again by the ordinary `register_asset`.
2. The book decides it from its own state, and refuses it with `ERR-NotAuthorised` unless all of these hold:
   - **book-only:** the asset was never bound, no binding of it was ever asked for (the record of a refused binding
     stays, so it counts), and none is in progress;
   - **never used:** nothing was ever posted in it, in any account. A balance that is zero now but was not always
     zero is used;
   - **no Treasury limit in it:** no Treasury account declares a limit in it;
   - **no unrevoked issuance authority over it.** The operator revokes it first (`revoke_issuance`);
   - **no grant that is not revoked bounds the assets it covers with this one in the list:** such a bound counts
     minor units of the precision being replaced;
   - **the indexes behind the last three checks are known complete** (below).
3. An asset that is not registered is `ERR-UnknownAsset`. A review reference of zeroes names no review
   (`ERR-NotAuthorised`), as for `register_asset`; a missing justification is `ERR-JustificationRequired`.
4. The designation the book gave the asset in its own records (its tag, below) is retired with it and never given to
   another asset. Registering the same identifier again is a new registration: a new tag, the new precision, a registry
   digest that differs from the one before the removal and from one that never held the asset.

### "Never used" is derived, not stored

The core has no "ever used" fact, and none can be added soundly. A new per-asset leaf written when a posting is made
would be absent for everything posted before the code that writes it, so every asset used before the upgrade would
look unused, which is the one error this operation must not make. It would also change the service tree's roots for
books that never remove an asset.

The book instead derives it from records that are permanent and are read by key, never by scanning accounts or
postings (`REQ-RT-1`). Value can enter an asset only at an account that is allowed to go below zero. A Customer or
Suspense account is never negative, so it holds only what it received. The accounts that may go negative are:

- the **issuance counterparty** and the **opening positions** (both `Issuance` accounts). The journal keeps, for each
  (account, asset), the running sums of its postings, and removes them only when an undo takes the last posting back, so
  once a posting is final they are permanent. A posting in the asset at either leaves them, and the opening position of
  the asset is itself a record;
- **Treasury accounts**, which go below zero only down to a limit declared per asset (`REQ-LA-2`). A new index, the
  Treasury accounts that declare a limit in each asset, is derived from the accounts, kept as they are put and never
  exported, like the index of owned custody. An asset it names is refused;
- nothing else.

An asset none of them has touched has had no first posting, so it has had none. The check is point reads
(registry binding, the binding record, the opening position, the journal sums of the issuance counterparty and of each
position, the Treasury index) and two indexes of grants by asset: by subject, and by the assets a bound names.

### Tags

Registration gives an asset a kernel tag, once (`F-003`: "assigned once at registration and never reused"). Removal does
not lower the next tag, so a retired tag is simply a gap, and `INV-24` stays true. What outside the registry stores an
asset's tag or identifier, and what was done about it:

| Where | What it holds | Handling |
|---|---|---|
| The registry leaf | every asset with its tag, and the next tag | Unchanged format. It already carried the next tag, and its decoder asks only for strictly increasing tags, so a gap is the record of a retirement. |
| The registry digest | id, tag, binding, precision, kind and status of every asset | Did not commit to the next tag, so a removed asset and one never registered hashed alike. After a retirement it commits to the next tag, under another domain; a registry that never removed an asset digests as before. Nothing outside tests reads the digest: anchors commit to the kernel root, not to it. |
| An export | the registry, one record per asset with its tag | The import re-registered assets one by one and refused a tag it did not get back, so any gap broke it; and the next tag was not carried, so retiring the highest tag would hand it out again after an import. The import now registers each asset at its recorded tag. A registry that retired a tag adds one last section, `service.retired-tags`, with the next tag. A book that never removed an asset writes no such section: its export is byte for byte what it was, and the format version stays 12. A build without this section refuses an export that has one. |
| Kernel accounts | the asset tag in each account's key | Created lazily with the first posting that touches them. An asset never posted in has none. |
| The posting journal | the asset, by identifier, on each line | An unused asset has no line. |
| Anchors and checkpoints | the kernel root | History. A checkpoint taken before the removal commits to what the kernel held then, which never held the asset. |
| Settlement | a binding record per asset, a support declaration of (kind, asset) pairs, an attestation per location | A binding record, including a refused one, keeps the asset. A support declaration names the identifier only, no tag and no amount, and stays; an external leg in a book-only asset is refused anyway (`REQ-AS-7`). |
| Partitions | the tag, on messages between writers and in the issuance members | See the next section. |
| Grants and Treasury limits | an identifier, and minor-unit amounts | An issuance grant over the asset keeps it until revoked. A Treasury limit keeps it. A bound on another grant that names the asset keeps it until that grant is revoked (an index by asset). |
| The audit trail | the identifier, in the subject of a record | History. The removal is itself a record that names the asset and the review. |

### Partitions

`REQ-RT-11` says an asset's registration only becomes true and stays true, so a partition may check against a directory
copy that lags. A removal makes that false for the one kind of asset it removes. Nothing is lost by it: the asset
was never used, so no partition holds a balance in it and no credit to it can be in transit. But the registry's writer
cannot see a partition's postings (`REQ-RT-13`), and a partition that has not yet applied the revocation of an issuance
authority can still issue.

`REQ-AS-10` therefore states the rule for a partitioned book: the removal is decided at a consistent cut (`REQ-RT-14`)
at which every partition has applied the revocation of every issuance authority over the asset and reports no posting
in it, and a partition that sees the removal refuses anything naming the retired designation. This is the rule for
`Binding` (`REQ-AS-8`), turned the other way. The build here decides the removal in the single writer that holds the
whole state; the cut for partitions is for the plan of the partitioned book (`F-031`) to build, and is not built by this
amendment.

### Rules version

`remove_asset` is a new input, so a log holds none before it. The two indexes (the Treasury accounts that declare a
limit in each asset, and the grants whose bounds name each asset) are new state, written when an account or a grant is
put, and an input of an earlier rules version must write exactly the leaves its code wrote (the rules version rule,
amendment 0033; `REQ-RT-19`, `REQ-RT-20`).

**The number is 4** (`ASSET_REMOVAL`). It is the next after the highest rules version the released code names, found
in the code and its history, not assumed:

- the core package is compiled as rules version 1 in every release (`GENERATION` in the core package at `v0.4.0`,
  `v0.4.1` and `develop`), and `CHAINED_SETTLEMENT`, `SETTLEMENT_EPOCHS` and `ASSET_CANDIDATES` are all 1;
- a deployed book has decided inputs at 1, so version 1 cannot take the indexes without changing what replaying its
  history writes;
- the code-history fixture of the `code-history` gate (`T-037-12`), present in `v0.4.0` and replayed by every build,
  records a book's history through versions 0, 1, 2 and 3. A first attempt at version 2 failed the gate: a grant put
  under version 2 writes an index leaf and the replay diverges at its input. The fixture is append-only, and the gate
  exists so that a pull request cannot pass by re-recording what it breaks.

So 4 is the first version no recorded history is decided under. This change does not raise the core package's
`GENERATION`: that is the release's step, and a book reaches version 4 one upgrade at a time, each upgrade a package
one rules version above the last. The feature turns on only through an upgrade to a build at 4.

Before version 4, `remove_asset` is `ERR-MalformedCommand` and writes nothing, and an account or grant put writes no
index leaf and reads none.

### Entering version 4: no backfill, so removal fails closed

Building the two indexes for the accounts and grants a book already holds is a scan of every account and every grant.
The writer may not make one (`REQ-RT-1`), the guest's arena bounds an input, and the core has no ordered, bounded read of
those collections to do it a chunk at a time, as the move of settlement incidents does for a small list. A backfill
as part of entering the version is therefore not feasible in this design, and the amendment does not pretend it is.

It fails closed instead. A leaf records whether the indexes are known to hold every account and grant of the book. It is
absent in a book that entered version 4 by an upgrade, and `remove_asset` then refuses every removal with
`ERR-NotAuthorised` whatever the asset, so no asset used only through an account or grant the index missed is ever
removed. An import builds both indexes from every record and sets the leaf, so a book restored from an export can remove.
A backfill in bounded batches, which sets the leaf when it finishes, is the way to enable removal on a book in place, and
is not part of this amendment.

## Trade-offs, stated plainly

- **In a book that entered version 4 by an upgrade, removal is unavailable until its indexes are built.** See above. The
  alternative, accepting a removal on the operator's word that nothing in the book names the asset, would let the core
  remove a used asset, which `REQ-AS-10` forbids.
- **A book founded at version 4 does not set the leaf either, yet.** The genesis path does not know the rules version
  it runs under, so only an import marks a book complete. This is conservative, not unsound.
- **Removal is not undo.** An asset removed is gone from the registry; registering it again is a new asset with a new
  tag. Any integration that cached the old tag, or the old precision, must be refreshed.

## What does not change

- A used asset is never removed, and nothing here edits a precision.
- A bound asset stays bound (`REQ-AS-8`). The binding is never undone.
- No requirement, error or invariant other than those listed is changed, and no identifier is renumbered.
