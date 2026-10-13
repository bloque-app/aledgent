<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-017 — Anchoring and disclosure

| | |
|---|---|
| Milestone | M-2 — On VOS, end to end |
| Track | B |
| Home | `crates/ledger-core`, `agents/core`, and a verifier crate. `DEC-15` has no row for this feature (see Open points) |
| Depends on | F-001, F-005, F-006, F-010, F-011, F-013, F-015, F-016, F-019 |
| Unblocks | F-018, F-023 |
| Realises | DEC-12 |

## Goal
aledgent commits to its book on Kreivo at a bounded interval and on demand. Each anchor binds a stated
entry sequence to the kernel's composite root. It is cited only after it is final. A holder can ask for a
disclosure in one of four shapes. aledgent checks the claim against its own book and signs it. It labels
it as a signed assertion, adds the deployment's assurance level, and names the checkpoint it rests on. A
recipient checks it with a small offline verifier and a public read of Kreivo. At each anchor, the book's
record of value held on Kreivo is compared with the pallet's positions.

## Spec traces
- `REQ-PR-1`, `REQ-PR-7`, `INV-19` — anchor at a bounded interval and on demand, over every entry up to a
  stated sequence and nothing else.
- `REQ-PR-2`, `REQ-PV-3`, `INV-17` — an anchor reveals no account, identity, amount, counterparty or asset.
- `REQ-PR-3`, `REQ-PR-6`, `REQ-AR-6` — say what an anchor proves and that disclosures are signed assertions.
- `REQ-PR-4`, `REQ-DS-3` — a recipient checks a disclosure against an anchor without aledgent.
- `REQ-PR-5` — the book's view of what is held on the settlement layer is compared continuously.
- `REQ-PR-8`, `REQ-PR-9` — only final anchors are cited; anchoring is off every holder's critical path.
- `REQ-DS-1`–`DS-5`, `REQ-PV-6`, `REQ-AT-1`, `REQ-AR-5` — four assertions, named fields, refuse false
  claims, authorised issue, reveal only the claim, audit every disclosure, carry the assurance level.
- `AC-J1.*`, `AC-J3.*`, `AC-I2.*`, `AC-I3.*`, and `AC-I1.1` for the anchor.

## Scope
**In:** the checkpoint record and its commitment; cadence and on-demand triggers; the anchor instruction in
the outbox; finality tracking; `anchor_checkpoint`, `issue_disclosure`, `verify`; the disclosure envelope;
the offline verifier; the settlement-position comparator.
**Out:** the pallet's `anchor` call (`F-019`); submission mechanics (`F-016`); the content of the assurance
declaration (`F-015`); balances and history at a sequence (`F-011`, `F-006`); the audit store (`F-013`);
daily scheduling and paging of the comparison (`F-018`); disclosures a recipient can check without trusting
aledgent (`F-023`, `DEF-3`).

## Design

**Checkpoint.**
```
Checkpoint { number: u64, entry_sequence: Sequence, kernel_root: [u8; 32], commitment: [u8; 32],
             created_at: Time, status: Pending | Submitted | Final, coordinate: Option<Coordinate> }
commitment = blake2b_256("aledgent/anchor/v2" ‖ construction ‖ instance ‖ number ‖ entry_sequence ‖ kernel_root)
```
`kernel_root` is the composite root over CipherClerk's six sub-trees (CipherClerk's source), in the compact
construction of `DEC-25` (`construction = 2`). The commitment binds the construction's name, and the
disclosure envelope (version 3) carries it (`REQ-PR-10`, amendment 0019, 2026-10-05). Construction 1, the
supported opt-in feature `smt-v1` (owner ruling `cs-q04`), keeps the original form, `"aledgent/anchor/v1"`
without the byte. It sits in core
state (`DEC-4`). A checkpoint is created inside one core invocation, so its root and its last entry
sequence belong to the same committed state (`INV-19`). Checkpoints exist only at invocation boundaries.

**Proposal — what goes on chain.** The pallet's `number` is the anchor counter, and the entry sequence sits
inside the commitment. The public then sees how many anchors there are and their order, but not how many
postings the book holds (`REQ-PV-2`). A disclosure reveals the entry sequence it rests on.
*Alternative:* publish the entry sequence as the on-chain sequence, the literal reading of `DEC-9`. It is
simpler, but it leaks the total posting count.

**Triggers.** On each tick (`F-009`), the core creates a checkpoint if the interval has passed and new
entries exist (`REQ-PR-1`, `AC-J1.1`). The interval is configuration. Its value is `OQ-11`'s. An operator
can also run `anchor_checkpoint` through `CTR-OPS`, with an audit record (`REQ-OS-1`). If no entry is new,
the command returns the latest checkpoint and creates nothing. Creating a checkpoint appends
`AnchorInstruction{number, commitment}` to the outbox in the same commit (`REQ-RT-1`). No holder command
waits on any of this (`REQ-PR-9`, `AC-J1.4`).

**Finality** (`REQ-PR-8`). `F-016` observes `Anchored{number, commitment}` in the inbox at a finalized
block and reports it. The core marks the checkpoint `Final` only if the commitment matches. A mismatch
raises an alarm, and the checkpoint never becomes `Final`. If an anchor is provably not executed, the core
emits the same `(number, commitment)` again. That is safe because an anchor moves no value. One anchor is
in flight at a time, because the pallet takes numbers in strict order. A checkpoint that is not `Final`
cannot be cited: `ERR-AnchorNotFinal` (`AC-J1.3`).

**Disclosure issue** — `issue_disclosure(subject, recipient, assertion, checkpoint?)`:
1. *Authority.* The subject's owner authority, or a delegated authority that grants disclosure
   (`REQ-DS-5`). An operator may issue under a legal obligation, with a justification. Otherwise
   `ERR-NotDisclosable`.
2. *Checkpoint.* The one named, or the latest `Final` one. Not final: `ERR-AnchorNotFinal`.
3. *Assertion* (`REQ-DS-1`), mapped to CipherClerk's four claim shapes (CipherClerk's source):
   - `HoldsAtLeast{account, asset, amount, kind}` → `balance_at_least`;
   - `OperationOccurred{operation_ref}` → `transfer_existed` on the operation's postings;
   - `OperationMoved{operation_ref, asset, amount}` → `transfer_amount_equals`;
   - `FlowWithin{subject, direction, window, bound}` → `aggregate_in_window`.
   Anything else: `ERR-DisclosureUnsupported`.
4. *Truth check* at `checkpoint.entry_sequence`, using `F-011` and `F-006` history. The library computes
   the facts. It never takes them from a caller. CipherClerk's `balance_at_least::check` trusts a
   caller-supplied balance (CipherClerk's source). A false claim is refused with `ERR-DisclosureFalse`
   (`REQ-DS-4`).
5. *Envelope* (`REQ-DS-2`):
   `Disclosure { version, instance, subject, recipient, assertion, checkpoint {number, entry_sequence,
   kernel_root, commitment}, assurance {declaration_digest, summary, kind: SignedAssertion}, issued_at,
   claim, signature }`. `claim` is CipherClerk's wire form with `state_root = kernel_root`. `signature`
   covers the whole envelope. Both use the core's clerk key.
6. *Nothing else revealed* (`REQ-PV-6`). No viewing key is shared and no read capability is granted.
7. *Audit.* Every disclosure writes an audit record through `F-013` (`REQ-AT-1`).

**Clerk key.** It reaches the core through the private-input path `F-005` builds for kernel signatures
(`DEC-3`, `REQ-RT-6`). The signing nonce also comes in as an input, or is derived deterministically, so no
randomness is drawn inside the core (`REQ-RT-7`). If `F-005` moves signing out of the Agent, disclosures
follow it.

**Assurance** (`REQ-AR-5`, `REQ-AR-6`). Every disclosure carries the digest and a summary of the
declaration `F-015` generates. The summary says: "Signed assertion. Evidence of aledgent's claim, not of
its truth" (`REQ-PR-6`).

**Verification.** `verify(disclosure)` is a query without side effects. The offline verifier does the same
checks without aledgent (`REQ-DS-3`, `AC-I2.1`). Its inputs are the disclosure, the deployment's clerk
public key and any Kreivo node or light client. It:
1. checks format and version;
2. checks both signatures;
3. reads `Anchors(instance, number)` at a finalized block;
4. recomputes the commitment and compares it;
5. prints the assertion, the assurance level and the signed-assertion label.
**Proposal — key publication:** the clerk public key sits on the pallet's instance record (`F-019`), so the
anchor and the key come from one public source. *Alternative:* publish it out of band.

**Settlement comparison** (`REQ-PR-5`). `F-010` must post settlement-layer legs against a per-asset
settlement-position account. That account must be separate from issuance under issuance authority
(`REQ-AS-6`). A cut is inbox sequence `k` of the instance. The comparator compares two values at minor-unit
resolution: the book's position balance as of the posting that applied entry `k`, and the pallet position
at the finalized block whose inbox head is `k`. It runs at every anchor and on request. `F-018` runs it
daily and pages. Any value that reached the position outside the pallet's calls shows as a difference.
That is intended.

## Test plan
- **Property** (`INV-19`, `AC-J1.2`). Random books. Take a checkpoint at `S`. Rebuild the kernel from
  postings `≤ S` (`F-006`): the root and commitment match. Change any posting `≤ S`: they differ. Append
  postings `> S`: the checkpoint is unchanged.
- `AC-J1.1`: with ticks, a checkpoint appears within one interval when entries exist, and none without.
- `AC-J1.3`: citing a `Submitted` checkpoint gives `ERR-AnchorNotFinal`. A mismatched commitment never
  becomes `Final`.
- `AC-J1.4`: stop the adapter. Holder commands continue at the same latency. Anchor lag grows.
- `AC-I1.1`, `REQ-PR-2`: the anchor instruction type holds only a number and 32 bytes. `INV-17` build gate.
- `AC-I2.1`, `AC-I3.1`: the offline verifier passes on a local Kreivo with every aledgent process stopped.
- `AC-I2.2`, `AC-I3.2`: the recipient's principal still cannot read the subject's account.
- `AC-I2.3`, `AC-I3.3`: the label and the assurance level are present and printed by the verifier.
- `REQ-DS-4`, `REQ-DS-5`: a false claim of each shape gets `ERR-DisclosureFalse`. An unauthorised one gets
  `ERR-NotDisclosable`. The legal-obligation path writes an audit record.
- `AC-J3.2`: send one minor unit to the position with a plain asset transfer. The comparator reports a
  difference of one. `AC-J3.1`: every anchor has a recorded comparison.

## Exit criterion
`AC-J1.*`, `AC-J3.*`, `AC-I2.*` and `AC-I3.*` pass on a local Kreivo with the pallet and the adapter. The
offline verifier checks a disclosure while every aledgent process is stopped.

## Open points
1. **Proposals:** anchor counter on chain with the entry sequence inside the commitment (must match
   `F-019`); clerk key on the instance record; the verifier crate at `crates/verify`.
2. **`DEC-15`** has no row for `F-017`. The owner should confirm the homes above.
3. **"Nothing else"** (`INV-19`, `REQ-PR-7`). The composite root also commits to account records and to
   the kernel's bookkeeping sets (CipherClerk's source). Plan reading: these are functions of the book up to
   `S` and of account openings. The owner should confirm.
4. **Release scope.** `US-I2` and `US-I3` are beyond V0 (§15). `OQ-23` is open. The disclosure half is not
   a V0 gate. `REQ-PR-1`, `REQ-PR-7` and `REQ-AR-5` bind V0.
5. **"Holds at least"** (`REQ-DS-1`) does not name a balance kind. **Proposal:** the assertion names it.
   Under `DEC-2`, `total` is not one kernel layer, so only `available` maps straight to `balance_at_least`.
6. **Windows.** Kernel timestamps inside a batch are backdated (CipherClerk's source), and `OQ-36` is open.
   **Proposal:** `FlowWithin` windows are ranges of entry sequence, with times shown for information.
7. **Overlap.** `REQ-PR-5` traces to `F-018`, while `AC-J3.*` sits in this feature's exit. The split
   above gives the comparator to this feature and the schedule and paging to `F-018`.
8. **Dependency.** The comparator needs the per-asset settlement-position account from `F-010`.
   `OQ-11` (cadence) stays open. Meanwhile the interval is configuration.
