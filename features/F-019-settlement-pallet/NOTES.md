<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-019 — implementation notes

Written while implementing `F-019` (issue #19), 2026-09-23. The plan's proposals were followed as
accepted: `set_keys`, `prune_inbox`, the withdrawal bound, `attestation_key`, instance destinations,
recorded refusals and a signed `RegisterOrigin`. Where the plan was silent, the most conservative
behaviour was chosen (refuse rather than allow); each such choice is listed here for review.

## Change of home

**The pallet lives in this repository, at `pallets/aledgent-settlement`, not in frame-contrib.** The
plan's home was `frame-contrib/pallets/aledgent-settlement`. On 2026-09-23 the owner ruled that
nothing is pushed to, or proposed on, `virto-network/frame-contrib`: it is another team's repository
and nobody there has asked for it. So:

- `pallets/` is a Cargo workspace of its own. The root `Cargo.toml` lists it under
  `[workspace] exclude`, so the Substrate dependency tree never enters the root workspace's builds.
- `.github/workflows/pallets.yml` builds, tests and lints it (and runs the benchmarks, see T-019-06)
  only when `pallets/**`, the workflow or its weight check change. `ci.yml` is untouched.
- The layout mirrors frame-contrib's root, so the crate can be proposed there later **unchanged**:
  the crate is named `fc-pallet-aledgent-settlement`, its manifest inherits every key from its
  workspace, `pallets/Cargo.toml` carries the subset of frame-contrib's `[workspace.package]` and
  `[workspace.dependencies]` it needs, at the same versions, and `pallets/.maintain/` holds
  frame-contrib's weight template. `pallets/kitchensink` plays the part of frame-contrib's
  `fc-kitchensink-runtime`; upstream, the pallet joins that runtime instead and this one is dropped.
- **License.** The crate inherits `Apache-2.0 OR MIT` from `pallets/Cargo.toml`, this repository's
  license. Upstream it would inherit frame-contrib's `GPL-3.0-only`. Apache-2.0 code can be included
  in a GPL-3.0 project, but relicensing is the owner's call when the time comes.

**Pins.** Polkadot SDK `stable2606-2` (`pallets/polkadot-sdk-version`): the release frame-contrib pins
at `4524639` and Kreivo pins at `4aef055` (spec 133), both read on 2026-09-23. The dependency
versions are frame-contrib's; `pallets/Cargo.lock` was seeded from frame-contrib's lockfile, so the
transitive crates resolve as they do there. Live Kreivo reported spec 132 on 2026-09-23, one behind
`main`.

## Decisions taken during implementation

### Shape

1. **`Config` differs from the plan's sketch in three places.** `RuntimeEvent` is expressed as
   `frame_system::Config<RuntimeEvent: From<Event<Self>>>`, the current FRAME form frame-contrib
   uses. `MaxPrune` is added, so `prune_inbox` has a bounded weight. `BenchmarkHelper` (benchmarks
   only) supplies an asset the benchmarks can mint.
2. **A `HoldReason::InstanceRegistration` composite enum**, so a runtime can take the instance
   deposit with `HoldConsideration`, as `pallet-pass` does for its registrations.
3. **Instance ids start at 1.** `PalletId::into_sub_account_truncating(0u32)` is byte-for-byte the
   pallet's own account (`into_account_truncating()`), so instance 0's position would be the pallet
   account. The position formula is the plan's; zero is simply never assigned.
4. **Withdrawal bounds are their own map**, `WithdrawalLimits: (instance, asset) → window`, not a
   `limits` field inside `InstanceInfo`. A field would need a bound on the number of assets per
   instance; the map is keyed by instance and asset only, never by a reference. **A call the plan
   implies but does not name**, `set_withdrawal_limit(instance, asset, Option<limit>)`, admin only,
   sets, changes or removes a bound.
5. **Prune watermark.** `InstanceInfo` gains `pruned_to`. `prune_inbox(instance, up_to)` removes the
   movement entries in `[pruned_to, up_to)`, at most `MaxPrune` positions per call, keeps every
   `Anchored` entry and every anchor, and charges for the positions it covered.
6. **Events and errors beyond the plan's lists.** Event `WithdrawalLimitSet`; `KeysSet` carries the
   keys and the attestation key; `InboxPruned` carries `from`, `to` and `removed`. `Deposited` (the
   event) follows the plan and omits `from`; the inbox entry carries it. Errors added: `DuplicateKey`,
   `LimitExceeded` (named in the plan's prose), `NoWithdrawalLimit`, `InvalidLimit`, `SelfTransfer`,
   `PositionAsAccount`, `InvalidPruneRange`, `TooManyToPrune`, `NoInstanceIdLeft`,
   `SequenceOverflow`.
7. **Deposit sizing.** The instance deposit is priced on one item of `InstanceInfo`'s max encoded
   length, so it covers the record at `MaxKeys` keys and never needs topping up on `set_keys`. There
   is no deregistration in the plan, so the deposit is never released.

### Behaviour

8. **No bound, no withdrawal.** An asset with no withdrawal bound cannot be withdrawn at all: the
   withdrawal is refused (`NoWithdrawalLimit`) and recorded. `NFR-8` says no single process may move
   value without bound; the plan says the admin sets a bound and says nothing about an asset without
   one. Refusing is the conservative reading. The admin sets a bound per asset before the first
   withdrawal.
9. **Windows.** A bound is `{amount, period}` in Kreivo's own blocks. The window starts when the bound
   is set and restarts at the first withdrawal after `period` blocks. A refused withdrawal uses
   nothing. Changing a bound keeps the current window and what it has used, so tightening it cannot
   be used to reset it.
10. **What is recorded as a refusal and what fails the call.** Recorded as `WithdrawRefused` with the
    chain's error, call succeeds: every failure of the transfer itself (funds, frozen asset or
    account, minimum balance, an account that cannot be created), the bound (`LimitExceeded`,
    `NoWithdrawalLimit`), an unknown receiving instance, and dust (item 11). The call fails and
    records nothing when the call itself is at fault: unknown instance, not a key, zero amount, a
    withdrawal to its own instance, a position account named as a plain account, a bad origin. The
    first group is "provably not executed" readable by position (`REQ-SA-4`); the second never had a
    coordinate to record against. The failed transfer runs in its own storage layer, so a refusal
    leaves no partial effect.
11. **A position leaves whole or above the minimum.** Found by the random runs: with
    `Preservation::Expendable`, a withdrawal that leaves less than the asset's minimum balance reaps
    the position's asset account, and the remainder disappears with no inbox entry. A withdrawal now
    either empties the position exactly or leaves at least the minimum; anything else is refused and
    recorded (`TokenError::NotExpendable`).
12. **Depositors are kept alive.** `deposit` transfers with `Preservation::Preserve`, so a counterparty
    cannot reap its own account through the pallet.
13. **A position account is not a plain destination.** `withdraw` refuses `Destination::Account` when
    the account is any instance's position (or the pallet's own account): the value would arrive with
    no inbox entry on the receiving side. Another deployment is named as
    `Destination::Instance { instance, account_ref }`, which writes `Withdrawn` in the sender's inbox
    and `Deposited` in the receiver's (`REQ-SL-6`).
14. **Replay covers any stored anchor**, not only the latest: `anchor(n, c)` with `(n, c)` already
    stored succeeds, writes nothing and emits nothing. A stored `n` with another `c` is
    `AnchorConflict`.
15. **Keys.** A duplicate key is refused. An empty set is allowed: it freezes anchoring and
    withdrawals until the admin sets new keys, which is what a compromised key calls for. The admin is
    not a key unless it is listed.
16. **`try_state` invariants.** The pallet checks, under `try-runtime` and in every unit test: ids
    below `NextInstanceId` and never 0; every position account exists; anchors `1..=last_anchor` all
    present and no others; inbox gapless from `pruned_to` to `next_seq`, only anchor entries below
    `pruned_to`; each `Anchored` entry matches its anchor; bounds belong to instances and have a
    non-zero window.

## Benchmarks and weights (T-019-06)

- Every call has a frame-benchmarking v2 benchmark at its worst case (`src/benchmarking.rs`):
  `MaxKeys` keys with the acting key last; `withdraw` to another instance (both instances, both
  positions, both inboxes); `withdraw_refused` failing at the transfer after every read;
  `prune_inbox(n)` over `n` movement entries. The weight of `withdraw` is the larger of the two.
  The benchmarks also run as unit tests (`cargo test --features runtime-benchmarks`).
- `src/weights.rs` was generated with `frame-omni-bencher` 0.24.0 (the stable2606 line) from
  `pallets/kitchensink`, 50 steps, 20 repeats, on a developer machine (Apple M2 Pro). These are
  **not** reference-hardware weights, and the kitchensink uses `u32` asset ids where Kreivo uses
  `FungibleAssetLocation`, which is larger and changes the proof sizes of `WithdrawalLimits` and the
  inbox entries. A runtime upgrade requires regenerating them on Kreivo's runtime and reference
  hardware (proof size, not time, is Kreivo's binding limit).
- **Regression check in CI.** The `weights` job builds the kitchensink, downloads the
  `frame-omni-bencher` of the pinned SDK release (checksum verified, as frame-contrib's `/cmd bench`
  does), runs every benchmark at 2 steps and 1 repeat, and `tools/ci/check-pallet-weights.py` fails
  if the fresh run needs more proof size, reads or writes (base or per component) than the committed
  weights charge. Execution time is reported, not checked: CI runners are not the reference
  hardware.
- The locally installed `frame-omni-bencher` 0.11.1 cannot run a stable2606 runtime (it fails to
  decode `TrackedStorageKey`); the matching 0.24.0 was built into a scratch directory, leaving the
  installed one untouched.

## Kreivo branch, try-runtime and zombienet (T-019-07)

Kreivo is Virto's repository, so the branch is not pushed anywhere. It is kept in this repository as
a patch, `pallets/kreivo/0001-kreivo-runtime-add-the-aledgent-settlement-pallet.patch`, against
Kreivo `main` at `4aef055` (`pallets/kreivo/KREIVO_REV`). `pallets/kreivo/kreivo-branch.sh`
rebuilds the branch (`apply`) and runs the checks (`check`, `try-runtime`, `zombienet`).

- **The patch.** The pallet at index **90**, a new group (82 stays reserved), with `Assets =
  pallet_assets<Instance1>`, a signed `RegisterOrigin` paying a `HoldConsideration` deposit priced
  with Kreivo's `deposit()`, `PalletId` `kv/aldst`, `MaxKeys = 16`, `MaxPrune = 128`, a benchmark
  helper that creates a sufficient `FungibleAssetLocation::Here(9_001)`, and `spec_version` 134. The
  dependency is a path into a sibling aledgent checkout until the pallet is published; the script
  rewrites it.
- **Compiles.** `cargo check -p kreivo-runtime --features try-runtime,runtime-benchmarks`: pass (the
  one warning is Kreivo's own, in its XCM weights). The release WASM with `try-runtime` builds.
- **try-runtime: pass**, 2026-09-23, `try-runtime` CLI 0.10.1,
  `on-runtime-upgrade --checks all --blocktime 6000 live --uri <a Kreivo RPC endpoint>`. The live
  runtime was spec **132**; the new one is 134. 273,385 keys, 17.2 MB of state, all of it decodes.
  The pallet is detected as new and its storage version initialised to 0; the upgrade is idempotent;
  the multi-block migrations finish in one block; every pallet's try-state passes, the settlement
  pallet's included. Upgrade weight 2.4 ms (0.12% of the block), PoV 9.3 KiB. The one notice is
  Kreivo's own (`Aura` declares internal migrations with no storage version set). Because the live
  chain is at 132 and `main` is at 133, the run also exercises Virto's own changes between the two.
- **Zombienet: configured, not run.** `pallets/kreivo/zombienet/` holds Kreivo's own
  `kusama-local` network (unchanged), a `.zndsl` test that waits for parachain 2281 to be registered,
  produce 10 blocks and finalize 5, and a check that the pallet is in the metadata at index 90 with
  `NextInstanceId = 1`. Running it needs the stable2606 `polkadot`, its two workers,
  `polkadot-parachain` and the Kusama chain-spec generator. None is installed here (the local
  `polkadot` is 1.5.0), and downloading release binaries was not in this session's remit. The run
  closes with `F-016`, together with the end-to-end run (`T-019-08`) that needs the adapter anyway.
  The `zombienet` cargo feature, which dropped `PassAuthenticate` and
  `SkipCheckIfFeeless`, no longer exists at `4aef055`, so a local network now encodes transactions
  as production does.
- **`AC-D1.3`** (a reorg under finality) is `T-019-08`'s, and so is its fallback; nothing is
  recorded here beyond the plan's open point 6.

## Open points for the owner

These are the plan's open points as they stand after implementation, plus one found on the way.

- **Inbox growth by strangers.** Anyone can `deposit` to any instance, and each deposit writes an
  inbox entry for the price of a transaction fee and the asset's minimum balance. Storage grows with
  external movements, as the plan says, but a counterparty chooses how many. `prune_inbox` bounds it
  after the fact. A per-deposit minimum or a small storage fee would bound it before. Not built: the
  plan does not ask for it.
- **Account-reference privacy** (plan open point 3). The pallet never keys
  storage by an account reference, and the storage audit proves it. Whether references are single-use
  or rotating is decided by `F-010`'s resolver, not here.
- **Value outside the pallet** (open point 4). Unchanged: a plain transfer into a position writes no
  entry (`deposits_move_through_fungibles_only` pins this down). `F-017` and `F-018` see it as a
  difference.
- **Asset accounts** (open point 5). In the mock, a non-sufficient asset reaches a position that has
  no native balance, and no asset-account deposit is charged: the provider reference the pallet adds
  at registration is enough. Still to verify on the deployed runtime.
