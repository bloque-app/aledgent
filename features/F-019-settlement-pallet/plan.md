<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-019 — `pallet-aledgent-settlement`

| | |
|---|---|
| Milestone | M-2 — On VOS, end to end |
| Track | C |
| Home | `frame-contrib/pallets/aledgent-settlement`, in the frame-contrib repository. This issue tracks it |
| Depends on | nothing. The coordinate format is agreed with the evidence and adapter features; no code is shared |
| Unblocks | F-016, F-017, F-022 |
| Realises | DEC-9 |

## Goal
Kreivo gets a small pallet that an aledgent deployment uses as its settlement layer. For each deployment it
holds the identity, the keys and the positions, the anchors in order, and one sequenced inbox of every
movement and anchor that concerns it. It holds nothing about any holder. A counterparty deposits to an
opaque account reference. The deployment withdraws by operation reference. The pallet ships with tests,
benchmarks and a runtime upgrade proposal ready for Kreivo's governance.

## Spec traces
- `REQ-SL-1`, `REQ-PR-1`, `REQ-PR-2`, `INV-19` — a bounded commitment with a per-deployment sequence,
  publicly checkable and ordered. It reveals nothing else.
- `REQ-SL-2`, `REQ-SL-7` — hold and move value; accept value with an opaque account reference.
- `REQ-SL-5`, `REQ-SL-10`, `REQ-PV-2`, `INV-17` — no record per operation, account or identity; nothing
  about holders.
- `REQ-SL-8`, `REQ-SO-3` — native, stable coordinates; outcomes found by operation reference, not position.
- `REQ-SL-9`, `REQ-SA-7` — a sequenced record readable by position.
- `REQ-SL-6` — another deployment is an ordinary counterparty.
- `REQ-SA-4` — a refusal is recorded, so "provably not executed" can be read. `NFR-8` — bounded withdrawal.
- `AC-D1.*`, `AC-E1.*` end to end; `AC-J1.2` at the pallet.

## Scope
**In:** the pallet; mock-runtime tests; benchmarks and weights; a Kreivo runtime branch with the pallet; a
zombienet setup; `try-runtime` checks; the upgrade proposal, drafted.
**Out:** the adapter (`F-016`); what an anchor commits to (`F-017`); observation semantics (`F-010`);
netting between deployments (`F-022`, `DEF-1`); proof verification on chain (`DEF-3`); opening upstream
pull requests. The owner decides when (CLAUDE.md, "Upstream projects").

## Design

**Config.**
```rust
pub trait Config: frame_system::Config {
    type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;
    type Assets: fungibles::Inspect<Self::AccountId> + fungibles::Mutate<Self::AccountId>;
    type PalletId: Get<PalletId>;
    type RegisterOrigin: EnsureOrigin<Self::RuntimeOrigin, Success = Self::AccountId>;
    type InstanceDeposit: Consideration<Self::AccountId, Footprint>;
    type MaxKeys: Get<u32>;
    type WeightInfo: WeightInfo;
}
```
The pallet is generic over `AssetIdOf<T>`. So it encodes whatever asset-id type the runtime uses. The
deployed runtime and the source snapshot use different enums.

**Types.** `InstanceId = u32`, `InboxSeq = u64`, `AnchorNumber = u64`, `AccountRef = [u8; 32]`,
`OperationRef = [u8; 32]`, `Commitment = [u8; 32]`. Every stored type has a `MaxEncodedLen`.

**Storage.** Every map is keyed by instance. Nothing is keyed by an account reference or an operation
reference (`REQ-SL-5`, `REQ-SL-10`).
- `NextInstanceId`.
- `Instances: InstanceId → InstanceInfo { admin, keys: BoundedVec<AccountId, MaxKeys>, attestation_key:
  Option<[u8; 32]>, last_anchor: AnchorNumber, next_seq: InboxSeq, limits }`.
- `Anchors: (InstanceId, AnchorNumber) → { commitment, at: BlockNumber }`. Never pruned.
- `Inbox: (InstanceId, InboxSeq) → { at: BlockNumber, event }`, where `event` is one of
  `Deposited{from, account_ref, asset, amount}`, `Withdrawn{operation_ref, to, asset, amount}`,
  `WithdrawRefused{operation_ref, to, asset, amount, error}` and `Anchored{number, commitment}`.
- Positions are not pallet storage. `pallet-assets` holds them at
  `PalletId.into_sub_account_truncating(instance)`.
`at` uses Kreivo's own block number. Other Kreivo pallets count relay blocks.

**Calls.**
1. `register_instance(keys, attestation_key)`. Origin: `RegisterOrigin`. Takes the deposit. Calls
   `inc_providers` on the position account, so it exists without a native balance, as `pallet-pass` does
   for its accounts. Emits `InstanceRegistered`.
2. `set_keys(instance, keys, attestation_key)`. Admin only. **Proposal:** it is not in `DEC-9`'s list, but
   key rotation after a compromise needs it.
3. `anchor(instance, number, commitment)`. Origin: an instance key. If `number == last_anchor + 1`, store
   it, append `Anchored` to the inbox and emit. If the same number and commitment are already stored,
   return `Ok` and do nothing, so a replay is safe. Otherwise `AnchorOutOfOrder` or `AnchorConflict`.
4. `deposit(instance, account_ref, asset, amount)`. Any signed origin; amount above zero. Transfer from the
   caller to the position. Append `Deposited`. Emit `Deposited{instance, seq, account_ref, asset, amount}`.
5. `withdraw(instance, operation_ref, to, asset, amount)`. Origin: an instance key. Try the transfer from
   the position to `to`. On success append `Withdrawn`. **Proposal:** if the transfer fails (funds, frozen
   asset, minimum balance, limit), append `WithdrawRefused` with the error and return `Ok`. The refusal
   then sits in the sequenced record as proof of non-execution (`REQ-SA-4`). *Alternative:* fail the
   extrinsic, and let the adapter find `ExtrinsicFailed` in block events. Either way the event is keyed by
   `operation_ref` (`REQ-SL-8`, `REQ-SO-3`).
6. **Proposal — `prune_inbox(instance, up_to)`.** Admin only. Removes movement entries up to `up_to` and
   keeps anchors. *Alternative:* never prune. Entries are about 150 bytes, and storage grows with external
   movements only.

**Proposal — withdrawal bound** (`NFR-8`). The admin sets a limit per asset and per window. A withdrawal
past it is refused with `LimitExceeded`. This bounds what a stolen adapter key can move.

**Proposal — deployment as counterparty** (`REQ-SL-6`). `to` may name another instance and an account
reference. The pallet moves value position to position. It writes `Withdrawn` in the sender's inbox and
`Deposited` in the receiver's. *Alternative:* plain accounts only in V0, with deployment transfers left to
`F-022`.

**Events.** `InstanceRegistered`, `KeysSet`, `Anchored{instance, number, commitment, seq}`,
`Deposited{…}`, `Withdrawn{instance, operation_ref, seq, to, asset, amount}`,
`WithdrawRefused{instance, operation_ref, seq, error}`, `InboxPruned`.
**Errors.** `UnknownInstance`, `NotInstanceKey`, `NotAdmin`, `AnchorOutOfOrder`, `AnchorConflict`,
`ZeroAmount`, `TooManyKeys`.

**Weights.** Every call has a frame-benchmarking v2 benchmark at its worst case: `MaxKeys` keys, the
largest error payload. On Kreivo the binding limit is proof size, not compute time.
Weights are generated on Kreivo's reference hardware.

**Kreivo integration.** A branch of `virto-network/kreivo` adds the pallet to `construct_runtime!` at a
free index. Index 82 is reserved. `Assets` is `pallet_assets<Instance1>`.
**Proposal — `RegisterOrigin`:** a signed origin with a deposit. *Alternative:* Root through governance.
Live Kreivo ran runtime 0.16.9 when this was planned, behind the source snapshot at spec 133. So the branch is
rebased onto Virto's next release, not onto the snapshot. The zombienet feature drops `PassAuthenticate`
and `SkipCheckIfFeeless`. CI records that difference.

**Governance.** Root is reached only through `KreivoReferenda` track 0: a 4-day decision, 15-minute
confirmation and a 90% approval floor, voted by `KreivoCollective`. The relay's executive can also reach it
by XCM. Kreivo is Virto's, so every runtime change needs a counterparty. The proposal is drafted separately. Pull requests to frame-contrib and Kreivo open only when the
owner says so.

## Test plan
- **Unit**, mock runtime with `pallet-balances` and `pallet-assets`: every call, every error, every event.
- **Property.** Inbox sequences are gapless from zero per instance (`REQ-SL-9`). Anchors are strictly
  ordered, with no gaps (`REQ-SL-1`, `AC-J1.2`). The stored commitment equals the submitted one (`INV-19`
  at this layer). Every value movement through a call has exactly one inbox entry.
- **Storage audit.** Enumerate every storage prefix after a random run. No key is derived from an account
  reference, an operation reference or any holder data (`REQ-SL-5`, `REQ-SL-10`, `INV-17`).
- **Benchmarks** run in CI, with a weight-regression check. **`try-runtime`** on the Kreivo branch against
  a snapshot of live state.
- **End to end** on zombienet with `F-016` and the core: `AC-D1.1`–`AC-D1.4` and `AC-E1.1`–`AC-E1.5`.

## Exit criterion
Pallet tests cover every call and every error. Benchmarks produce weights. The upgrade proposal is
drafted and the owner has reviewed it. The end-to-end zombienet run with the adapter closes with `F-016`,
which builds on this pallet.

## Open points
1. **Beyond `DEC-9`'s list:** `set_keys`, `prune_inbox`, the withdrawal bound, `attestation_key` (for
   `F-017`) and instance destinations. Each is a Proposal. The owner should confirm or cut them.
2. **Proposals:** recording refusals; a signed `RegisterOrigin`; the anchor number as a counter with the
   entry sequence inside the commitment (must match `F-017`).
3. **Account-reference privacy.** A stable reference per ledger account lets anyone group deposits by
   account, which `REQ-PV-2` forbids. The references should be single-use or rotating. The resolver is
   `F-010`'s, and it must agree.
4. **Value outside the pallet.** A plain asset transfer or an XCM arrival into a position writes no inbox
   entry. `F-017` and `F-018` see it as a difference. The book cannot take it in without an operation
   (`REQ-AT-3`). How it is resolved is open.
5. **Asset accounts.** If a settled asset is non-sufficient, each position account may owe an
   `AssetAccountDeposit`. Verify on the deployed runtime.
6. **Reproducing `AC-D1.3`** needs a best-block reorg under finality. Zombienet may not produce one
   reliably. A fallback is a two-collator fork, or a simulated fork at the adapter.
7. **Retention.** Pruning bounds storage, but it shortens what a `REQ-IN-5` re-scan can read on chain. The
   choice affects `F-016` gaps.
8. `RISK-4`: governance timing is an `M-3` gate. `M-2` runs on a local Kreivo.
