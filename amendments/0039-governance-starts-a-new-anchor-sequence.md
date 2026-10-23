<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0039 — Governance starts a new anchor sequence on a live contract

**Status:** **draft**, for the owner's review. Nothing here is applied to `SPEC.md` or `PLAN.md`; the code
cites it as `[PENDING AMENDMENT 0039]`. The owner's requirement it serves was stated on 2026-10-22 (below).
**Target:** `SPEC.md` v0.4.28 as on `develop` (at `85432f5`); `PLAN.md` `DEC-17`; `F-026` (`T-026-10`).
**Date:** 2026-10-22
**Number:** 0038 is the last on `develop`; no open PR or remote branch claims 0039.
**Amends:** `REQ-SL-1` (one paragraph); closes `OQ-68` for the contract venue. No command, query, error
condition or invariant of `SPEC.md` changes.
**Rationale in one line:** prod's book was re-founded on 2026-10-22 without a settlement base, while the prod
contract, which holds real funds, only accepts anchor *last + 1* and has no way back; governance must be able
to start a new sequence there, as a relay chain's governance sets a parachain's head, without touching
anything else.

---

## The owner's requirement (2026-10-22)

The prod book `p2` was founded without a settlement base (its chainspec header has no `settlement` field), so
its first checkpoint is number 1. The prod settlement contract stands at anchor 17 and accepts only 18 next.
The contract holds real funds and has no reset. The owner wants a next version of the contract to upgrade
the live instance to, with a **governance-gated** way to reset its anchoring (as Polkadot's
`Paras::set_current_head`), **without touching funds, binds, positions or withdrawal windows**, and usable
later for the Spaces venue (`OQ-68`, amendment 0022, `DEC-17`).

## Verified facts

1. **The live instance can be upgraded, and amendment 0022's "the live one cannot be upgraded" is wrong for
   it.** Prod's instance `Da1uhU5rnKTXtQxZDvkqDsEeS2SUaNBktAxvMa8CTKDjrdk` was instantiated through the
   contracts store on 2026-10-12: app 4, licence 0, publisher and licence holder community 1201's account
   `F3opxRaMq…` (deployment record `handovers/rehearsal-2026-10-11/prod-kreivo/kreivo-mainnet-prod.json`).
   `ContractsStore::upgrade(app_id, license_id)` requires the licence owner and runs
   `Contracts::set_code` as Root on the instance (Kreivo `pallets/contracts-store/src/lib.rs:362-387`);
   `publish_upgrade` by the publisher replaces the app's code (`:259-262`, `:440-462`). At the handover a
   dry run of `upgrade` as the community passed the origin and owner checks (`AppInstanceUpToDate`), and as
   the old admin key failed `BadOrigin`; on 2026-10-22, at spec 134, a dry run as the community passed them
   again, and as `HUf7` failed `NoPermission` (`handovers/2026-10-22-settlement-new-instance-referendum.md`
   §4). The test instance (app 3) was upgraded
   in place on 2026-10-11. Amendment 0022 was written on 2026-10-08, before prod's instance existed.
2. **No code runs at upgrade.** `set_code` swaps the code hash; no constructor and no message run
   (`NOTES.md` decision 26). Any migration is lazy.
3. **The deployed code is the T-026-09 code.** The running code hash is `0xa43bfd98…` (app 4's only
   version); its bundle's metadata lists exactly develop's messages, events and storage fields
   (`registration` … `anchor_migration`), and the source is develop's `contracts/settlement/src`, headers
   aside (diffed by the chain-facts handover above). So the "deployed layout" is today's layout: cells
   `01`, `02`, `04` to `08`; `03` and `09` absent.
4. **Prod's state** (chain read 2026-10-22, same handover §1): head `{ last_anchor: 17, next_seq: 17,
   pruned_to: 0 }` (not 14, as the settle database read earlier that day said: anchors 15 to 17 were
   written around 00:30–02:30 UTC); 17 inbox entries, all `Anchored`; every position 0; nothing bound; no withdrawal bound;
   the account holds 185,371.97 COPM and 1,873.944283 DUSD by plain transfer.
5. **An anchor is accepted only as `last_anchor + 1`**; the latest again with its commitment is a no-op
   replay at its original position (`contracts/settlement/src/lib.rs`, `try_anchor`, before this change
   `:938-958`). There is no reset, and `Head` cannot change encoding: it is stored at `02000000` and passed
   between frames (`types.rs`, `Head`).
6. **The admin is a single key and governance does not reach it.** The registration's admin is
   `HUf7Nvfh…`, a personal key (rank-1 member of community 1201), not the community's account or a proxy of
   it. The contract has no message that changes the admin (`lib.rs`, messages). `HUf7`'s own signed origin
   maps to community 2, not 1201 (same handover §5).
7. **A referendum of community 1201 can call the contract as the community's account.**
   `Communities::dispatch_as_account(call)` takes the community's origin (`MemberMgmtOrigin` =
   `EnsureCommunity` or `EnsureCommunityAccount`, Kreivo `runtime/kreivo/src/config/communities/mod.rs:47`)
   and dispatches `call` as `Signed(community_account)` (frame-contrib at Kreivo's pin `0b57691`,
   `pallets/communities/src/lib.rs:601-607`, `functions.rs:217-226`). So a passed referendum enacting
   `dispatch_as_account(Contracts::call(..))` reaches a contract message with the community's account as
   the caller. **Dry-run on prod** (Kreivo mainnet, spec 134, at the finalized head on 2026-10-22,
   `DryRunApi`, nothing signed): as `Communities { 1201, None }`, `Contracts.call` directly fails
   `BadOrigin`; through `dispatch_as_account` the contract is `Called` with caller `Signed(F3opxRaMq…)`
   (community 1201's account), and an admin-only message is refused by the contract (`ContractReverted`),
   so it ran.
8. **Community 1201's track** decides one referendum at a time; prepare 600, confirm 14,400, minimum
   enactment 600 relay-chain blocks: about 26 hours each with the one voting member's aye (same handover §5).
9. **The adapter needs no change to drive the contract after a reset.** It reads `last_anchor` from the
   head and submits `last_anchor + 1` (`services/settle/src/adapter.rs:1401-1430`), and reads the inbox
   from a cursor kept per instance, starting at 0 in a fresh database (`services/settle/src/store/pg.rs:570-584`,
   table `settle_cursors`, `migrations/0001_settle.sql:67-70`). The reset adds no inbox entry kind, so the
   adapter's SCALE mirror of the inbox is unchanged.
10. **A book's first checkpoint is 1.** The anchor store refuses any other first number
    (`crates/ledger-core/src/anchor/tests.rs:46-49`; `anchor/mod.rs:619`). A reset to base 0 lets `p2`
    anchor as founded, with no change to the core (`T-036-14` stays as it is).
11. **The verifier assumes anchor numbers never fall.** It reads the latest at the finalized head and
    finds an earlier anchor by a binary search for the first finalized block holding its number or a later
    one (`crates/verify/src/rpc.rs:185-276`). After a reset that predicate is no longer monotonic, and the
    verifier can return **the other book's commitment** for a number both books used, or call an anchor not
    yet final: asked for `p1`'s anchor *n* while `p2`'s latest is *n*, it returns `p2`'s commitment; while
    `p2`'s latest is below *n*, it answers "not final"; once `p2` is past *n*, the search may land in either
    book. A commitment binds its own book's state, so the result is a false mismatch or a false "not
    anchored", never a false match (bar a hash collision).

**Assumptions** (not verified here):

- *A1.* Kreivo spec 134 (live) keeps `ContractsStore::upgrade` and `publish_upgrade` as read at spec 133.
  The origin checks of `upgrade` and the community's route to a contract call were dry-run at spec 134
  (facts 1 and 7); a `publish_upgrade` of new code and a `set_code` that actually swaps it were not.
- *A2.* The p1 adapter is stopped for good and nothing it signed is still in flight when the reset is enacted.
- *A3.* The WASM built by CI from this branch is what would be published; its code hash is not computed here.

## `REQ-SL-1` — one paragraph added (proposed)

After amendment 0029's paragraph:

> The settlement layer MAY let the deployment's governance start a new sequence for a re-founded book: the
> next commitment is then numbered from a base governance chose, and the new sequence names the book it
> belongs to. Starting one MUST NOT revise or remove any recorded commitment, move value, or change any
> position, binding or bound; every commitment of an earlier sequence stays checkable as `REQ-SL-1` and
> `REQ-PR-4` require, and its sequence MUST be identifiable by anyone reading the layer. *Amendment 0039.*

`OQ-68` is closed for the contract venue by this paragraph; the Spaces venue inherits it.

## The design

### Who may start a sequence: the deployment's governance, a role of its own

- A new cell, `governance` (`ManualKey<0x0000_000B>`, an `AccountId`), absent until named.
- **`set_governance(account)`**, selector `0x0000_0400`. While none is set, **the admin** names it; once
  set, **only governance** replaces it, and the admin can no longer touch it (`NotGovernance`). Emits
  `GovernanceSet { governance }`.
- On prod, governance is **community 1201's account**, which only a passed referendum can make sign
  (fact 7). The admin names it once, right after the upgrade; from then on a reset needs a referendum.
- The admin keeps every power it had (keys, bounds, pruning), so the fast responses stay fast: emptying the
  keys on a compromise is still one signed call, not a 26-hour vote.

### What a reset does: `set_anchor_head(expected_last, base, book)`

Selector `0x0000_0401`. Governance only (`NotGovernance` for anyone else, the admin included, and for
everyone while none is named).

- **Compare and set.** Refused `HeadMoved` unless the head's `last_anchor` equals `expected_last`. A
  referendum takes about 26 hours; an anchor that lands meanwhile makes the reset fail and change nothing,
  instead of dropping a sequence nobody looked at.
- **Base.** Refused `InvalidAnchorBase` if `base + 1` overflows. Any other base is allowed: 0 for a book
  that starts at 1 (prod's `p2`), a higher one for a book seeded with a settlement base (`T-036-14`).
- **Legacy records first.** Refused `MigrationPending` while a deployment of the code before 2026-10-11 still
  holds anchor records by number (a migration running, or not yet started). The admin's `set_keys` finishes
  it (decision 26). Prod has none (fact 3).
- **It writes:**
  - the head's `last_anchor`, to `base` (`next_seq` and `pruned_to` stay);
  - `latest_anchor` cleared: the previous sequence's latest moves into the new record;
  - a new cell `anchor_epoch` (`ManualKey<0x0000_000A>`), overwritten by each later reset:
    `AnchorEpoch { epoch: u32, base, from_seq: InboxSeq, at: BlockNumber, book: [u8; 32], previous:
    Option<LatestAnchor> }`. `epoch` is 1 for the first reset and one more for each later one.
    `from_seq` is `next_seq` at the reset: the new sequence's first inbox position. `book` is an opaque
    32-byte name of the re-founded book, its chainspec (seed) digest. `previous` is the old latest anchor,
    kept in current state; a sequence reset again before its first anchor (a wrong `book`, say) carries
    its own `previous` forward, so the last anchor of the book before is never dropped;
  - if a prune had already passed the old latest's inbox entry (kept while it was the latest), that entry
    is removed, since nothing else would reach it and `previous` keeps the anchor. Otherwise it stays,
    prunable like any superseded anchor's.
- **It emits** `AnchorHeadSet { epoch (topic), base, from_seq, book, previous }`. A verifier tells two
  books apart on one instance by this event or by `anchor_epoch()` (`0x0000_0308`); `governance()`
  (`0x0000_0309`) reads the role.
- **It never touches:** the contract's holdings, any position, withdrawal window or binding, the
  registration (admin, keys, attestation key), governance, the inbox's numbering, or any inbox entry other
  than the one named above. Nothing moves; no transfer is attempted. The tests compare these cells byte for
  byte.

### After a reset

- The next anchor is `base + 1`, stored as before. **The base itself names no anchor of the new sequence**:
  an anchor numbered `base` is refused `AnchorOutOfOrder`, never treated as a replay of the old sequence's
  latest. Before this change that path assumed a record and would have trapped the batch.
- **The legacy migration never starts in a later sequence.** It used to start on any head with
  `last_anchor > 0` and no `latest_anchor`, which is what a reset to a base above 0 looks like; it would
  then have trapped every key and admin message. The start now also requires that no `anchor_epoch` exists,
  and the legacy fallback in `load_latest_anchor` is limited to the first sequence.

### Storage and the lazy migration

- New cells only: `0a000000` (`anchor_epoch`) and `0b000000` (`governance`). Every existing cell keeps its
  key and encoding; `Head` is unchanged.
- **Absence is the deployed state.** No `anchor_epoch` means the instance's first sequence (base 0, from
  inbox position 0); no `governance` means none named. So the upgrade needs **no migration**: the first
  message after `set_code` (on prod, the admin's `set_governance`) writes the governance cell and nothing
  else. A test writes the deployed layout by raw key and raw SCALE (17 anchors, a position, a bound) and
  checks that every existing cell is byte-identical after that message.
- The T-026-09 migration of pre-2026-10-11 deployments is kept unchanged for the first sequence.

### Properties (each with what it serves)

No new `INV-*` identifier is proposed; these are the contract's properties for a reset.

- **S1, no value moves** (`REQ-AT-3`, `NFR-8`): holdings, positions, bounds, bindings unchanged.
- **S2, coordinates are written once** (`REQ-SL-2`, `REQ-SL-9`): `next_seq` never decreases and no entry is
  rewritten, so `(instance, seq)` names one entry for good; a settled withdrawal's entry cannot be replaced
  or re-executed by a reset.
- **S3, non-revision** (`REQ-PR-3`, `INV-19`): no recorded commitment is changed; the last anchor before a
  reset stays in current state, across further resets until a new sequence anchors; earlier ones stay in
  their entries until pruned, their events and finalized history.
- **S4, no cross-sequence replay**: an anchor of an earlier sequence never answers `Applied` at its old
  position after a reset.
- **S5, the sequence is identifiable** (`REQ-SL-1` as amended): every entry at or after `from_seq` belongs
  to the sequence `anchor_epoch` describes; its anchors are numbered above its base.
- **S6, only governance resets; only the admin names governance, once** (`NFR-8`).
- **S7, stale resets fail closed**: `expected_last` must match.
- **S8, grants untouched** (`REQ-SL-10`): registration and governance unchanged by a reset; nothing keyed by
  a holder's reference (`REQ-SL-5`): `book` is a value, never a key.

The debug invariant check (`check_invariants`) adds S5 and that no legacy migration runs in a later sequence.

## Interaction with the adapter, the core and the verifier

- **Anchoring.** No change: after a reset to 0, `p2`'s adapter sees `last_anchor` 0 and submits 1 (fact 9).
- **The inbox cursor (required before `p2`'s adapter starts).** A fresh settle database reads from position
  0, so `p2` would ingest `p1`'s 17 `Anchored` entries as incidents. The cursor must start at the reset's
  `from_seq` (17 on prod): one row, `INSERT INTO settle_cursors (instance, next_seq) VALUES (<instance>,
  <from_seq>)`, before the adapter's first start, in the runbook. `T-036-15` would make the adapter read
  `from_seq` itself; it is not built here.
- **Seeded-genesis base (`T-036-14` to `T-036-16`).** With a reset, a re-founded book may start at 1
  (base 0), so `T-036-14`'s base is no longer needed for that case; a book that does carry a base gets it by
  a reset to that base. `T-036-16` (`aledgent/anchor/v3`, the commitment binds the book's genesis) stays
  useful: it lets a verifier check an anchor's book from the commitment alone.
- **The verifier (`T-026-11`, a precondition of the reset's referendum).** Its search assumes numbers never
  fall (fact 11): after a reset it can answer with the other book's commitment, or "not final". It must read
  `anchor_epoch` (and, for a sequence before the last, the `AnchorHeadSet` events) and search for a book's
  anchor only within that book's sequence: blocks before the reset (`at`) for an earlier one, from it for
  the current one. It is not built here; the reset's referendum waits for it, so that no verifier of `p1`
  or `p2` is answered wrongly.

## Failure modes

| Case | What happens |
|---|---|
| An anchor lands between proposal and enactment | `HeadMoved`; nothing changes; a new referendum with the new `expected_last` |
| Referendum 2's gas or deposit limit too low at enactment | `Contracts.call` fails, nothing changes; a day is lost (step 4 sizes them from a dry run) |
| A verifier reads the instance before `T-026-11` | it can answer with the other book's commitment or "not final" (fact 11): never a false match. Hence the precondition |
| The previous book's adapter anchors after the reset | its next number (`last + 1` of the old sequence) is a gap: refused `AnchorOutOfOrder`, recorded. With a base equal to the old latest, it would land as the new sequence's first: the runbook stops the old adapter first (A2) |
| Governance never named | nobody can reset; the instance runs on as before |
| Admin names itself governance before the community | the reset becomes a single-key power, as before this change: the checklist names the community's account as the first message after the upgrade, and `governance()` is read back |
| Unmigrated pre-2026-10-11 deployment | `MigrationPending`; the admin's `set_keys` finishes the migration first |
| `p2`'s adapter started without its cursor seeded | it reads `p1`'s entries and raises incidents in `p2` (anchoring stops until an operator acts); nothing moves |
| Rollback to the previous code after a reset to a base above 0 and before the next anchor | the old code would start a legacy migration that finds no record and trap every key and admin message. Roll back only before a reset, or after the new sequence's first anchor (or after a reset to base 0) |

## Rejected alternatives

- **The admin resets.** One signed call, but the admin is one personal key (fact 6): that is not the
  governance gate the owner asked for, nor what `OQ-68` describes ("by the venue's own governance"), and
  amendment 0022's analysis found that only a community vote guards a live book's anchoring. Moving the whole admin to the community instead would put the incident responses (empty the
  keys) behind a 26-hour vote.
- **Zero the inbox with the head** (0022's option (b)). Positions would repeat; an observation's coordinate
  `(instance, seq)` would name two entries, and a settled withdrawal's evidence could be replaced. S2 forbids it.
- **An inbox entry for the reset.** It would give the inbox's readers the event in order, but it adds an
  entry kind that the adapter's SCALE mirror cannot decode today: a reset would stop the adapter until it is
  upgraded. Administrative acts (keys, bounds) are events, not entries; the reset follows them, and
  `from_seq` gives the position.
- **Anchors keyed by (epoch, number)** (0022's option (a)). Brings back a storage item per anchor, which
  amendment 0029 removed, for nothing a single sequence record does not give.
- **A new instance** (0022's option (c)) or **continuing the sequence** (option (e)). Both remain possible
  without code; the owner chose the upgrade. Option (e) would need `p2` re-founded with a base.

## How the upgrade would be done (described, not executed)

1. CI builds the contract (`cargo contract build --release`, the `Contracts` workflow); reviewers compare the
   code hash with the bundle to publish.
2. **Referendum 1** of community 1201 (about 26 hours): `Utility.batch_all([ContractsStore.publish_upgrade(4,
   code), ContractsStore.upgrade(4, 0)])`, so that the code voted on is the code installed. The test instance
   (app 3) should take the same code first, as a rehearsal.
3. **At once after enactment**, the admin (`HUf7`) signs `Contracts.call(Da1u…, set_governance(<community
   1201's account>))`, and `governance()` is read back.
4. **Referendum 2** (about 26 hours), once `T-026-11` is released: `Communities.dispatch_as_account(
   Contracts.call(Da1u…, set_anchor_head(17, 0, <p2's chainspec digest>)))`. `Contracts.call` fixes its
   `gas_limit` and `storage_deposit_limit` when it is proposed, about a day before it runs: dry-run the
   exact enacted call as `Communities { 1201, None }` at the finalized head, set the gas limit from the dry
   run with a margin and the deposit limit to the net charge (the new 109-byte record, less the cleared
   `latest_anchor`) with a margin, and check that the community's account (0.173 KSM on 2026-10-22)
   covers it. An anchor landing meanwhile makes it fail `HeadMoved` and change nothing.
5. Seed `p2`'s settle cursor at `from_seq` (17), then start `p2`'s adapter; its first anchor is 1.

Nothing in flight: the p1 adapter is stopped (A2). The withdrawal-to-deployment check compares code
hashes, so any other deployment that withdraws to this one is upgraded in the same window (decision 26, F5);
prod has none.

**Rollback.** Before enactment: vote nay or let it lapse. After step 2: `publish_upgrade(4, <a43bfd98's
code>)` and `upgrade(4, 0)` by referendum; the old code ignores the two new cells, subject to the last row of
"Failure modes".

## Questions for the owner

- **q1. Which digest names `p2` in the reset's `book`: its chainspec digest, or its genesis roots?** The
  reset stores 32 opaque bytes that a verifier uses to tell `p1`'s anchors from `p2`'s. The chainspec digest
  is what the seed and the window records already publish; the genesis roots are what the core's checkpoints
  are built on. Either works; verifiers must be told which one, once. The draft uses the chainspec digest.

## Plan and code

`F-026` `T-026-10` (this change, `contracts/settlement`); `T-026-11` (later: the verifier reads sequences).
`PLAN.md` §4's `OQ-68` paragraph is to read, once this is ruled: the V0 contract's governance can start a new
anchor sequence (`[PENDING AMENDMENT 0039]`).
