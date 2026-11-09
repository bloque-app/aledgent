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
2. The book decides it from its own state, as it stands at the input and with no version gate, and refuses it with
   `ERR-NotAuthorised` unless all four of these direct checks hold. Each refusal is a distinct, named condition in the
   store (`Binding`, `Opening`, `Issuance`, `LiveGrant`, `GrantBound`, `TreasuryLimit`), and the same
   `ERR-NotAuthorised` to the caller:
   - **(2) book-only:** the asset was never bound, no binding of it was ever asked for (the record of a refused binding
     stays, so it counts), and none is in progress;
   - **(1) never held:** the issuance counterparty's position, and every opening position, never held it, and
     nothing was ever posted in it in any account. A balance that is zero now but was not always
     zero is used;
   - **(4) no Treasury limit in it:** no Treasury account declares a limit in it;
   - **(3) no unrevoked authority grant names it,** as the subject of an issuance grant or in the assets a grant's
     bounds cover. Grants keep the asset as an identifier, so a registered-again asset would inherit them. The operator
     revokes them first (`revoke_issuance`, `revoke_authority`);
3. An asset that is not registered is `ERR-UnknownAsset`. A review reference of zeroes names no review
   (`ERR-NotAuthorised`), as for `register_asset`; a missing justification is `ERR-JustificationRequired`.
4. The designation the book gave the asset in its own records (its tag, below) is retired with it and never given to
   another asset. Registering the same identifier again is a new registration: a new tag, the new precision, a registry
   digest that differs from the one before the removal and from one that never held the asset.

### "Never used" is derived from the state, not stored

The core has no "ever used" fact, and none can be added soundly. A new per-asset leaf written when a posting is made
would be absent for everything posted before the code that writes it, so every asset used before the upgrade would
look unused, which is the one error this operation must not make.

The book therefore reads what it already records, at the moment it decides, and keeps nothing new:

1. **The issuance counterparty never held it.** Value can enter an asset only at an account that is allowed to go
   below zero: the issuance counterparty and the opening positions (all `Issuance` accounts), and Treasury accounts down
   to a declared limit (`REQ-LA-2`). The journal already keeps, for each (account, asset), the running sums of its
   postings, and removes them only when an undo takes the last posting back, so once a posting is final they are
   permanent: a position that holds nothing now but did once still has them. The check is a point read of those sums for
   the issuance counterparty and for each opening position, and of the opening position of the asset itself. No history
   is needed.
2. **It was never bound.** The registry binding is book-only, and the settlement record of a binding, which stays even
   when the layer refused it, is absent.
3. **No unrevoked authority grant names it.** Issuance grants over the asset are one point read of the existing index of
   grants by asset or identity. A grant whose bounds list the asset is found by reading the grants: a scan.
4. **No Treasury account has a limit in it.** Read across the accounts, closed ones included: a scan.

The two scans are acceptable for an operator's removal, which is rare. They are whole reads of a collection, which the
host's witness gives the core Agent like any other whole read, so the Agent decides the same as the replica. Nothing is
cached or indexed, so a replay of the same inputs reads the same state at the same point and gives the same answer.
Replacing the two scans with indexes was tried and dropped: an index kept from some rules version on is incomplete for a
book that entered it by an upgrade, and the book would have to refuse every removal.

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
| Grants and Treasury limits | an identifier, and minor-unit amounts | An issuance grant over the asset keeps it until revoked. A Treasury limit keeps it. A bound on another grant that names the asset keeps it until that grant is revoked. |
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

### No rules-version gate

`remove_asset` is a new input, so a log holds none before it, and it writes no leaf of a kind that is new. An input of
any earlier rules version writes exactly the leaves its code wrote, and a replay of any recorded history is unchanged
(`REQ-RT-19`, `REQ-RT-20`). So the operation needs no rules version of its own: a book decides it as soon as it runs a
build that has it. An earlier draft gated it at rules version 4 and kept two indexes from that version on. That was
dropped because a book that entered version 4 by an upgrade had incomplete indexes, so removal failed closed, and a
deployed book is such a book: it would have refused every removal. It also could not be set at 1, because a deployed book
has decided inputs at 1.

## Trade-offs, stated plainly

- **Two checks scan.** The Treasury-limit and grant-bound checks read every account and every grant. In the core Agent
  that is a whole read of those collections, which is heavy for a very large book. It is an operator's rare removal, and
  it is the price of answering correctly for a book of any history.
- **Refusals look alike to the caller.** The store names which check refused (six distinct conditions), but the caller
  sees `ERR-NotAuthorised`, as for every other operator refusal, and the audit record says it was refused.
- **Removal is not undo.** An asset removed is gone from the registry; registering it again is a new asset with a new
  tag. Any integration that cached the old tag, or the old precision, must be refreshed.

## What does not change

- A used asset is never removed, and nothing here edits a precision.
- A bound asset stays bound (`REQ-AS-8`). The binding is never undone.
- No requirement, error or invariant other than those listed is changed, and no identifier is renumbered.
