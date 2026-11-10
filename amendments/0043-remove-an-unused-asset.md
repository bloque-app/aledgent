<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0043 — An asset registered with the wrong precision and never used can be removed

**Status:** **decided by the owner on 2026-11-08**: keep the identifiers, remove the ones never used, register them
again at the right precision. **Redesigned on 2026-11-09** as retire, sweep, remove: the owner's default on the
form (`q09-123-sweep`), reversible. **Applied** to `SPEC.md` **v0.4.31** in the same pass: `REQ-AS-2`, `REQ-AS-10`
(new), the passage on a lagging directory in `REQ-RT-11`, `REQ-OS-2`, the command table (`retire_asset`,
`sweep_asset_uses`, `cancel_asset_retirement`, `remove_asset`), `ERR-BatchTooLarge`'s wording, `INV-24`, and the
traceability row for Epic F. The build follows it (`T-003-12`).
**Target:** `SPEC.md` v0.4.30 as on `develop` (at `71a19c1`); amendment 0042 is the last on `develop`.
**Date:** 2026-11-08, redesigned 2026-11-09
**Number:** 0042 is the last on `develop`; no open pull request claims 0043.
**Amends:** `REQ-AS-2` (one phrase), `REQ-AS-10` (new), `REQ-RT-11` (one parenthesis), `REQ-OS-2` (one line),
§8's command table (four rows), §10's `ERR-BatchTooLarge` (its wording), `INV-24` (its wording). No error
condition is added: a refusal reuses `ERR-NotAuthorised`, `ERR-UnknownAsset`, `ERR-InvalidTransition`,
`ERR-BatchTooLarge`, `ERR-AssetDeprecated`, `ERR-MalformedCommand` and `ERR-JustificationRequired`, and no identifier
is renumbered.
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
already named for this one. The production book has two such assets, registered at 8 decimals instead of 6.

## Why one input cannot decide it

Four facts make an asset unused. Two of them are point reads of what the book already keeps:

1. **Never held.** Value can enter an asset only at an account that may go below zero: the issuance counterparty and
   the opening positions (all `Issuance` accounts), and Treasury accounts down to a declared limit (`REQ-LA-2`). The
   journal keeps, for each (account, asset), the running sums of its postings, and removes them only when an undo
   takes the last posting back, so once a posting is final they are permanent: a position that holds nothing now but
   did once still has them. The check reads them for the issuance counterparty and for each registered asset's
   opening position (a point read by its asset, so the read is bounded by the registry), and reads whether an opening
   position was ever opened for this asset.
2. **Never bound.** The registry says book-only, and the settlement record of a binding, which stays even when the
   layer refused it, is absent.

The other two have no structure that covers them:

3. **No unrevoked grant names it**, as the subject of an issuance grant or among the assets a grant's bounds cover.
   The index of grants by asset or identity covers a grant's subject, not its bounds.
4. **No Treasury account, closed ones included, has a limit in it.** A Treasury limit is set when the account opens,
   at any time, not only at genesis.

The first and third drafts (2acabc0, b09b58e) read both whole, `grants.values()` and `accounts.values()`, and claimed
that the host's witness gives the core Agent that read "like any other". It does, and that is the defect: a whole read
ships every leaf of the kind.
The core Agent runs in a 256 KiB arena, takes at most 184 KiB of caller blobs per invocation and 46 KiB per pack. At
the production book's size, about 175 000 ledger accounts and their grants, the read is about a hundred times too big.
The Agent would refuse the removal, and the book could never remove an asset. A never-used asset that the book cannot
remove is a bug (owner ruling).

An index cannot fix it either. An index kept from some rules version on is incomplete for a book that entered that
version by an upgrade, and the production book is such a book; the second draft kept two such indexes, and so had to
refuse every removal on it. Building them in place is the same read of every account and grant.

## The change

Removal becomes three steps, each an operator input of its own, under the one operator authority of the removal
(`OperatorOpKind::RemoveAsset`, as before). `SPEC.md` `REQ-AS-10` states them.

### 1. Retire: freeze the asset

`retire_asset` names the asset, a reviewed change and a justification. The book refuses it with `ERR-NotAuthorised`
unless the asset is registered and book-only, never asked to bind, has no opening position, and the issuance
counterparty and every opening position never held it (checks 1 and 2, point reads). An unknown asset is
`ERR-UnknownAsset`; one already retiring is `ERR-InvalidTransition`. As an early refusal it also reads the index of
grants over the asset, one point read: an unrevoked issuance grant found there refuses it. The sweep checks every
grant anyway, so the index's completeness is not relied on.

From that input the asset is **retiring**, a status of its own in the registry, and the book refuses everything new
that would name it:

| New use | Refused with |
|---|---|
| Any operation in it (issue, redeem, transfer, reservation, opening load, reversal) | `ERR-AssetDeprecated`, as for a deprecated asset |
| A grant whose subject is it, or whose bounds list it | `ERR-AssetDeprecated` |
| A Treasury limit in it, on an account opened or put again | `ERR-AssetDeprecated` |
| A binding of it | `ERR-AssetDeprecated` |
| Deprecating it | `ERR-AssetDeprecated`: the retirement keeps the status it returns to |

The store refuses each of these writes too (`INV-24` is enforced at the store, `REQ-AR-4`), so no path round the
command checks can name a retiring asset. Revoking a grant is not a new use, and stays allowed while the asset is
retiring: that is how the operator clears a finding.

### 2. Sweep: every account, then every grant, a page at a time

`sweep_asset_uses` names the asset, one **page**, and a justification. The book keeps, beside the retiring asset, the
status it had before and the sweep's **cursor**: where the next page starts.

- **The order.** Every record of the book is a leaf of its service tree, at a key that is the kind of record, then a
  hash of the record's own key. The sweep follows that key order: ledger accounts (kind 4) first, then grants (kind 5),
  which come next in the same order. It is the only order the book can prove complete for a part of a collection:
  the tree can show that a subtree holds exactly the leaves given and nothing else, as it already shows for a kind
  read whole, while the order of the records' own keys is not an order of the tree. A record's key in the tree
  depends only on its content, so the order is the same on the core, a replica, a replay and after an import.
- **The page.** A page is one subtree of that order, named by where it starts and how many leading bits of the key it
  fixes (its level). It must start at the cursor, be aligned to its level, and lie inside one kind. It may hold at most
  **64** records (`MAX_SWEEP_PAGE`), and, past its first record, at most **24 KiB** of their canonical keys and values
  (`MAX_SWEEP_PAGE_BYTES`): a grant's counterparties and assets have no size cap, so a count alone would not keep a page
  inside one pack. A page of one record is never refused for its size; any input that reads the record brings it. The cursor then moves to the end of the subtree; past the end of the accounts it
  enters the grants, and past the end of the grants the sweep is complete.
- **Who picks the level.** The operator, in the input, so that the page is data the input carries and every node
  decides it the same. Only a page inside the accounts and the grants claims its subtree in the witness; any other is
  refused before anything is read. The probe at a claimed subtree's first key is proven with the leaf that stands
  there, if one does, so a page the ledger refuses before reading it (not at the cursor, not retiring, not authorised)
  is an ordinary refusal on the core and the replica alike. The host's reply to each step suggests the next page's level, the largest that fits, from the
  whole state a replica holds; the suggestion is never part of the decision (the summary compared with the core Agent
  is the outcome's code only), and the core Agent, which has no whole state, suggests none.
- **What a page checks.** Each ledger account in it: no Treasury limit in the asset, closed accounts included. Each
  grant in it: if it is not revoked, its subject is not the asset and its bounds do not list it. Expired grants count
  as unrevoked.

Refusals, each audited, each changing nothing:

| Condition | Error | Named in the store |
|---|---|---|
| The asset is not retiring | `ERR-InvalidTransition` | `NotRetiring` |
| The page does not start at the cursor, or the sweep is complete | `ERR-InvalidTransition` | `NotAtCursor` |
| The level is out of range, the start not aligned to it, or the page outside the two kinds | `ERR-MalformedCommand` | `MalformedPage` |
| The page holds more than 64 records, or more than one and over 24 KiB | `ERR-BatchTooLarge` | `PageTooLarge` |
| A Treasury account in it has a limit in the asset | `ERR-NotAuthorised` | `TreasuryLimit` (the account in the audit record) |
| An unrevoked grant in it is over the asset | `ERR-NotAuthorised` | `LiveGrant` (the grant in the audit record) |
| An unrevoked grant in it bounds the asset | `ERR-NotAuthorised` | `GrantBound` (the grant in the audit record) |

**A finding leaves the asset retiring, and the cursor where it was.** That is decided by how refusals work here: a
refusal commits nothing, so it cannot end anything. It is also the better outcome:

- the pages already swept stay clean, because nothing new can name a retiring asset; so after the operator revokes
  the grant, the same page sent again goes on from there, and a finding late in the sweep does not cost the pages
  before it;
- unfreezing on a finding would reopen the asset to new uses while the operator is still deciding, and would throw
  away the pages swept;
- a finding that cannot be cleared (a Treasury limit: limits do not change, and a closed account keeps its own) has
  a way out: `cancel_asset_retirement` names the asset and a justification, and returns the asset to the status it had
  before the retirement. Cancelling an asset that is not retiring is `ERR-InvalidTransition`.

### 3. Remove

`remove_asset` keeps its form (the asset, a reviewed change, a justification). It is refused with `ERR-NotAuthorised`
unless the asset is retiring and its sweep is complete (`SweepIncomplete`), and the point checks of step 1 still hold
(they cannot fail while the asset is retiring, and are read again anyway). An unknown asset is `ERR-UnknownAsset`. It
removes the asset from the registry, retires its tag, and removes the retirement record. What b09b58e built for the
removal itself is kept: the running sums for "never held", the tag gap, the registry digest that commits to the next
tag after a retirement, and the export and import of `service.retired-tags`.

### Determinism

The same inputs give the same answer on the core, a replica and a replay. Each step reads only the book's records at
its input: the registry, the retirement record, the running sums and openings, and, for a page, exactly the leaves of
its subtree, proven complete. **The freeze is what makes paging sound:** no account or grant that names the asset can
appear between one page and the next, so the conjunction of clean pages is a clean book. A record that changes while
the asset is retiring keeps its place in the order (its key does not change), and can only stop naming the asset
(a revocation), never start.

### The rules version

The core Agent of an older build cannot decode the new `Call` variant, nor `OperatorOpKind` 20, while a new replica
would decide them (`REQ-RT-16`, `REQ-RT-17`). So the four inputs, and a `grant_operator` that names kind 20, are taken
from rules version **2** (`ASSET_RETIREMENT`) and from no earlier one. Before it the replica answers
`ERR-MalformedCommand` and records nothing, so no input carrying them is ever sent to an older Agent. Version 2 is the
first this release introduces: the production and test books run rules version 1, and this release's core package is
built as 2. The gate reads the rules version and nothing else: no index, no flag that a book which entered the version
by an upgrade would lack. The code-history fixture records inputs under versions 2 and 3, and none of them is one of
these, so every recorded input decides as before. The freeze adds reads, not writes, to existing inputs, and a store
holds no retiring asset before version 2, so no earlier input decides differently.

If another upgrade takes rules version 2 before this code ships, the constant moves to the version of the first core
package that carries it.

### Tags, exports and what else stores the asset

Registration gives an asset a kernel tag, once (`F-003`: "assigned once at registration and never reused"). Removal does
not lower the next tag, so a retired tag is simply a gap, and `INV-24` stays true.

| Where | What it holds | Handling |
|---|---|---|
| The registry leaf | every asset with its tag, status and the next tag | A retiring asset's status is a new code, written only while it is retiring, so a registry with none encodes and digests as before. A gap in the tags is the record of a retirement. |
| The registry digest | id, tag, binding, precision, kind and status of every asset | After a retirement it also commits to the next tag, under another domain; a registry that never removed an asset digests as before. |
| The retirement record | per retiring asset: the status before, the cursor | A leaf of its own kind (23), absent for an asset that is not retiring, so a sweep page changes it and not the registry. |
| An export | the registry, one record per asset with its tag and status | A registry that retired a tag adds the section `service.retired-tags`, with the next tag. A book with an asset retiring adds `service.asset-retirements`, after it, with each record. A book with neither writes neither: its export is byte for byte what it was, and the format version stays 12. An import refuses a retiring asset without its record, or a record without its asset retiring. The cursor is a key of the service tree, which depends only on content, so it means the same after the import. A build without these sections refuses an export that has them. |
| Kernel accounts | the asset tag in each account's key | Created lazily with the first posting. An asset never posted in has none. |
| The posting journal | the asset, by identifier, on each line | An unused asset has no line. |
| Anchors and checkpoints | the kernel root | History. A checkpoint taken before the removal commits to what the kernel held then, which never held the asset. |
| Settlement | a binding record per asset, a support declaration of (kind, asset) pairs, an attestation per location | A binding record, including a refused one, keeps the asset. A support declaration names the identifier only and stays; an external leg in a book-only asset is refused anyway (`REQ-AS-7`). |
| Grants and Treasury limits | an identifier, and minor-unit amounts | Found by the sweep; a revoked grant keeps the identifier harmlessly. |
| The audit trail | the identifier, in the subject of a record | History. Each step is a record that names the asset. |

### Partitions

`REQ-RT-11` says an asset's registration only becomes true and stays true, so a partition may check against a directory
copy that lags. A removal makes that false for the one kind of asset it removes. Nothing is lost by it: the asset was
never used, so no partition holds a balance in it and no credit to it can be in transit. But the registry's writer
cannot see a partition's postings (`REQ-RT-13`), and a partition that has not yet applied the retirement can still
take a new use.

`REQ-AS-10` therefore states the rule for a partitioned book: the sweep and the removal are decided at consistent cuts
(`REQ-RT-14`) at which every partition has applied the retirement and the revocation of every issuance authority over
the asset, and reports no posting in it; and a partition that sees the removal refuses anything naming the retired
designation. This is the rule for `Binding` (`REQ-AS-8`), turned the other way. The build here decides it in the single
writer that holds the whole state; the cuts for partitions are for the plan of the partitioned book (`F-031`), and are
not built by this amendment. The layout-3 core (the book in the Agent's committed maps) is not production, and it has
no proof of a subtree: it refuses a sweep page as an unproven read.

## Trade-offs, stated plainly

- **Removal takes several operator inputs**: one to retire, one per page of the sweep, one to remove. A page holds at
  most 64 records, and pages that fit leave about half that on average, so for the production book, about 175 000
  accounts and its grants, the sweep is some thousands of inputs. Each is bounded whatever the book's size; together
  they are proportional to it. It is the price of answering correctly for a book of any size and any history, without
  an index that a book which began without it could never complete.
- **While it lasts, the asset accepts nothing new.** For an asset that was never used that costs nothing; the
  operator can cancel at any time.
- **A finding stops the sweep until it is cleared**, and a Treasury limit cannot be cleared: the operator cancels, and
  the asset stays.
- **The order is the tree's, not the records' own.** The cursor is a 16-byte tree key, and an operator's tooling picks
  each page's level, retrying one level deeper on `ERR-BatchTooLarge`. The host suggests the level, so in practice a
  script follows the suggestions.
- **Refusals look alike to the caller.** The store names which check refused, and the audit record names the account
  or grant found, but the caller sees `ERR-NotAuthorised`, as for every other operator refusal.
- **Removal is not undo.** An asset removed is gone from the registry; registering it again is a new asset with a new
  tag. Any integration that cached the old tag, or the old precision, must be refreshed.

## What does not change

- A used asset is never removed, and nothing here edits a precision.
- A bound asset stays bound (`REQ-AS-8`). The binding is never undone.
- No requirement, error or invariant other than those listed is changed, and no identifier is renumbered.
