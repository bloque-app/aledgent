<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# aledgent settlement contract (ink!)

The on-chain side of settlement for one aledgent deployment in V0 (`PLAN.md` `DEC-17`, feature
`F-026`): a `pallet-contracts` contract on Kreivo, uploaded by a community and instantiated through
Kreivo's contracts store. It ports the rules of the settlement pallet of `F-019`. Design
choices, the storage layout and how the adapter reads it are in
[`features/F-026-settlement-contract/NOTES.md`](../../features/F-026-settlement-contract/NOTES.md).

| Message | Who | What |
|---|---|---|
| `new(admin, keys, attestation_key)` (constructor) | the contracts store | Register the deployment |
| `execute(Vec<Envelope>)` | a key | A batch of calls, each with its operation reference; one recorded outcome per call; a withdrawal to another deployment in a frame of its own |
| `execute_with_limits(Vec<Envelope>, Vec<Weight>)` | a key | The same, each framed call given its own limit (plus an eighth), as a dry run of the call alone reports it; entries of inline calls are ignored |
| `apply(head, envelope)` | the contract itself, only | One call of a batch, in its own frame (`execute` calls it on the contract) |
| `deposit(account_ref, asset, amount)` | anyone | Pull `amount` from the caller through Kreivo's assets API (no approval; the caller is kept alive) and record it under the account reference |
| `credit(account_ref, asset, amount)` | another deployment, only | Record value it has just moved here |
| `set_keys(keys, attestation_key)` | the admin | Rotate keys |
| `set_withdrawal_bound(asset, Option<bound>)` | the admin | Bound withdrawals per asset per window; an asset with no bound cannot leave (`NoWithdrawalBound`) |
| `prune_inbox(up_to)` | the admin | Remove old movement, refusal and superseded anchor entries; bind entries and the latest anchor's entry stay. **Never past the adapter's cursor**: an unread `Anchored` entry is what makes its checkpoint final |
| `set_governance(account)` | the admin while none is set; then governance only | Name the deployment's governance (on Kreivo: the licence-owning community's account, reached by a referendum through `Communities.dispatch_as_account`). Amendment 0039 |
| `set_anchor_head(expected_last, base, book)` | governance only | Start a new anchor sequence for a re-founded book: the next anchor is `base + 1`; refused `HeadMoved` unless the head's `last_anchor` is `expected_last`. Moves no value; touches no position, bound, binding, key or inbox entry. Amendment 0039 |
| `registration`, `head`, `inbox_entry`, `latest_anchor`, `position`, `withdrawal_window`, `bound`, `anchor_epoch`, `governance` | anyone | Reads (`bound(asset)`: the inbox position of the asset's `Bound` entry, if bound; `latest_anchor()`, selector `0x0000_0307`, replaces `anchor_record(number)`; `anchor_epoch()` (`0x0000_0308`): the sequence governance started last, `None` for the instance's first) |

Calls (`Call`): `Withdraw { to: Account(..) | Deployment { contract, account_ref }, asset, amount }`
(0), `Anchor { number, commitment }` (1) and `Bind { asset, amount }` (2). A bind runs inline,
like an anchor.

**Anchors: the latest only, in place** (owner's ruling of 2026-10-11; decisions 25 and 26 in
`NOTES.md`). An anchor `{ number, commitment }` is applied if `number` is the last one plus one: it
writes its `Anchored` inbox entry and overwrites `latest_anchor` (`{ number, commitment, at, seq
}`, one 52-byte cell, rewritten at the same size). The latest anchor again with the same commitment
is a replay, applied at its original entry, writing nothing; with another commitment it is
`AnchorConflict`; any other number, a gap or one below the latest, is `AnchorOutOfOrder`. So an
anchor adds one storage item, its entry, and `prune_inbox` removes that once a later anchor
supersedes it. Earlier anchors are in their entries until pruned, in their `Anchored` events, and
in the chain's finalized history (`latest_anchor` at the blocks where each was the latest), which is
where `crates/verify` reads them.

*Per anchor, at Kreivo's prices* (`DepositPerItem` 6 666 666 660, `DepositPerByte` 333 333, the
value's bytes counted): before, two items and 121 bytes, **13 373 666 613 (0.0134 KSM) held for
good**; now one item and 77 bytes, **6 692 333 301 (0.0067 KSM) until it is pruned**, and once per
deployment 6 683 999 976 for `latest_anchor`.

**Upgrading a deployment of the earlier code.** That code kept every anchor by number
(`anchors[n]`, root key `03000000`). The new code keeps every existing cell's key and encoding and
migrates by itself, since `ContractsStore.upgrade` swaps the code without running any of it: the
next state-changing message a key or the admin signs (`execute`, `execute_with_limits`, `set_keys`,
`set_withdrawal_bound`, `prune_inbox`; never `deposit` or `credit`) seeds `latest_anchor` from the
highest record and removes up to 32 records (`MIGRATION_STEP`), with each one's inbox entry if the
earlier code kept it below the pruning watermark; each later such message removes 32 more; the last
emits `AnchorsMigrated { through }` and from then on the check writes nothing. The released storage
deposits go to the signer of each of those messages (`pallet-contracts` refunds to the call's
origin). At most about 9 G of ref time and 20 KB of proof size per message, by Kreivo's weights.
**Upgrade when nothing is in flight, and migrate at once**: let pending instructions settle, then
`publish_upgrade` and `upgrade`, then immediately the admin's `set_keys` (same keys) until
`AnchorsMigrated`. A batch dry-run on one side of the code swap or of a migration step and included on
the other can fail whole, and a failed withdrawal batch is released. Every deployment that withdraws to another is upgraded in
one window.

**Binding a book-only asset** (amendment 0011, `REQ-AS-8`; `PLAN.md` `DEC-20`; decision 24 in
`NOTES.md`). An asset that started in the book alone is bound to Kreivo once, at zero divergence.
First the issuer puts exactly the book's outstanding balance of the asset in the contract's account,
by a plain transfer or a mint on Kreivo: that records no deposit, so no account is credited. Then a
key submits `Bind { asset, amount }` with that amount. The contract reads its own holding of the
asset and applies the bind only if the holding equals the position its inbox already records plus
`amount`, to the unit: it records `amount` into the position, writes a `Bound` entry under the
call's operation reference, and remembers the entry's position (`bound(asset)`). Otherwise the call
is refused with `HoldingMismatch { holding }`, the holding it read, and nothing but the inbox
changes; a second bind of the same asset is refused with `AlreadyBound`. The equality is checked at
the block that records the binding, so it is final with it (`REQ-SL-3`). A bind of zero is allowed.
`Bound` entries are never pruned. Withdrawing the asset still needs a withdrawal bound.

**How value moves.** Through Kreivo's own contract API, its chain extension (`KreivoChainExtensions`,
assets API): `deposit` pulls from the contract's caller, `transfer` pays out, `balance` reads the
contract's holding. Both transfers keep their source's asset account alive. The one exception is a
withdrawal of the contract's whole holding of an asset, which the extension cannot make: that one is
`Assets::transfer` through `call_runtime`. A withdrawal Kreivo refuses is recorded under its
operation reference with Kreivo's reason (`Refusal::Kreivo(..)`); the whole-holding transfer, which
`call_runtime` reports without a reason, as `Refusal::TransferFailed`. See decisions 3, 4, 14, 19 and
22 in `NOTES.md`.

**A failed call never fails its batch** (owner, 2026-09-29; hybrid, by the owner's ruling on #102;
decision 23 in `NOTES.md`). A withdrawal to another deployment, the one call that calls another
contract, runs in a frame of its own: a call to `apply` on the contract itself, with its own weight
limit (`CALL_WEIGHT_LIMIT`, or less so that the batch keeps a reserve to record every framed call
still to come). If the frame traps (Kreivo moved another amount than asked; the receiving
deployment did not record the credit), reverts or runs out, `pallet-contracts` rolls back
everything it did, value moved and the receiver's record included, and the batch records the call
as `Refused` with `Refusal::CallFailed(Trapped | OutOfGas)`, entry and event under its operation
reference, and goes on. A withdrawal to an account and an anchor run inline, as before: everything
that refuses them is already a recorded refusal. Only the batch's own exhaustion (weight or storage
deposit the transaction or the key cannot cover) reverts it whole; a dry-run-sized batch does not
reach it. Storage deposit has no per-call limit: `pallet-contracts` turns a frame's exceeded deposit
limit into a trap of its caller. `execute_with_limits` takes one limit per call (a dry run of the
call alone) for the framed calls, so that one given too little runs out alone. **Send a batch with
more gas than its dry run**: at least 2 G ref time and 96 KiB proof size more (the contract's
reserve), or half the dry run again.

## Layout

```
contracts/settlement/
  Cargo.toml            its own workspace (excluded from the root one)
  rust-toolchain.toml   Rust 1.91: cargo-contract 5.0.3's WASM build fails on 1.92 and later
  src/lib.rs            the contract
  src/types.rs          what it stores, takes, returns and emits
  src/kreivo.rs         Kreivo's environment, its chain extension (assets API) and the one runtime call
  src/mock.rs           (tests) a model of pallet-assets, the chain extension, other deployments and call frames
  src/tests.rs          (tests) every message, every refusal, invariants over a long run
  scripts/drive.mjs     deploy through the contracts store and drive it end to end
  scripts/e2e.mjs       the zombienet suite's phases (deploy, drive, check, faults), on drive.mjs's steps
```

## Build and test

Versions are Kreivo's at `4aef055`: ink! 5.1.1, `pallet-contracts` 49.0.0. `cargo-contract` 5.0.3.
Mainnet's Kreivo `0.16.9` runs `pallet-contracts` 43.0.0; everything the contract relies on reads the
same in both (`NOTES.md`, decision 23).

```sh
cd contracts/settlement
cargo install cargo-contract --version 5.0.3 --locked   # once
cargo test                                              # unit tests
cargo clippy --all-targets -- -D warnings
cargo contract build --release                          # target/ink/aledgent_settlement_contract.{contract,wasm,json}
```

The toolchain comes from `rust-toolchain.toml` (1.91, with `rust-src` and `wasm32-unknown-unknown`).
CI runs the same in `.github/workflows/contracts.yml`. The optimised code is about 36 KiB; Kreivo's
`MaxCodeLen` is 192 KiB.

## Upgrade authority

The contract's code can be upgraded by the community that owns its licence, through the contracts
store (`ContractsStore.upgrade`, which sets the instance's code to the app's latest published code;
the publisher chooses that code with `publish_upgrade`). Whoever can upgrade the code can move
everything the contract holds, without its keys and past its withdrawal bounds. **By the owner's
ruling of 2026-09-24, that community must be governed by Kreivo Communities' own membership-based
governance (`pallet-communities`: members, a decision method, its track, referenda) before the
contract holds any real value.** The contract has no governance code of its own.

**One community publishes, deploys and upgrades (owner, 2026-09-24).** Kreivo's contracts store is
permissioned, so the contract is published, instantiated and upgraded by the same community: the
operator's. No third-party community ever publishes code this contract runs. With that, upgrading is
a matter of the operator's own governance, and the remaining steps are on the checklist below.

As `CommunitiesManager.register` leaves it, a community is administered by one signed key, and that
key alone can upgrade the contract. Only that key can hand the community over to its own origin.
The details, and what stays outside the community's control (Kreivo's Root; the publisher, if it is
another community), are in `NOTES.md`, "Upgrade authority". Before real value, check that the Kreivo
runtime in production carries every upstream fix to the communities pallet that the handover relies on.

### Deployment checklist, before real value

- [ ] The community is registered (`CommunitiesManager.register`); its first admin key is held
      under the same custody as the contract's keys.
- [ ] Its governing members hold memberships (`Communities.add_member`, and `promote` if the method
      is `Rank`), and its decision method is set (`Communities.set_decision_method`).
- [ ] The same community published the app, holds the licence and instantiated the contract
      (`publish`, `license`, `instantiate` below).
- [ ] **Handover:** the admin key has called `Communities.set_admin_origin` with the community's own
      origin, `Communities(RawOrigin { community_id, subset: None })`. `Communities.CommunityIdFor`
      no longer maps the old signed origin, and a dry run of `ContractsStore.upgrade` signed by the
      old key fails with `BadOrigin`.
- [ ] **Governance named** (amendment 0039): the contract's admin has called
      `set_governance(<the community's account>)`, and `governance()` reads it back. Until then
      nobody can start an anchor sequence; after it, the admin cannot change it. On an upgraded
      instance this is the first message after `ContractsStore.upgrade`.
- [ ] A first referendum has replaced the default track (`CommunityTracks.update`): the default one
      passes a proposal with a single aye in about 25 minutes.
- [ ] The community id, members, decision method and track are recorded with the deployment, and
      the decision method is re-checked before every upgrade vote.

`drive.mjs` covers the third item only. The others are governance acts by
the community's members; a zombienet or testnet run may leave them out, a deployment that holds real
value may not.

## Deploy and drive (T-026-04)

**Run on zombienet by the release gate**, `.github/workflows/e2e-zombienet.yml` (`e2e/README.md`):
Kreivo 0.17.0 (spec 133), the runtime mainnet runs from 2026-09-30 (0.16.9 stays selectable), with
exactly the steps below, in order, from a test community. What it found is in
`features/F-026-settlement-contract/NOTES.md`, "The first real run".

`scripts/drive.mjs` takes each step in turn and keeps what it learns (app id, license id, contract
address) in `scripts/.state.json` (gitignored):

| Step | Signed by | Does |
|---|---|---|
| `preflight` | nobody | The chain is Kreivo; `Assets` is pallet 13 with `transfer` 8 (the one call the contract dispatches, for a whole position); the contract's byte layout for it equals the runtime's; the code fits `MaxCodeLen` |
| `community` | `ADMIN` | Registers `COMMUNITY_ID` with `ADMIN` as first admin, unless it exists; funds the community account |
| `publish` | `ADMIN` (community origin) | `ContractsStore.publish(code, None, None)` |
| `license` | `ADMIN` (community origin) | `ContractsStore.request_license(app)` |
| `instantiate` | `ADMIN` (community origin) | `ContractsStore.instantiate(app, license, 0, new(ADMIN, [KEY], None), salt)` |
| `bound` | `ADMIN` | `set_withdrawal_bound(ASSET, { amount: BOUND, period: BOUND_PERIOD })` |
| `deposit` | `DEPOSITOR` | `deposit(ref, ASSET, DEPOSIT)`: the contract pulls it through the chain extension, no approval |
| `execute` | `KEY` | A batch: withdraw `WITHDRAW` to `PAYEE` (applied), withdraw `u128::MAX` (refused by the bound), the next anchor (applied) |
| `read` | nobody | Every unpruned inbox entry, by contract query and by raw child-trie storage at the finalized block; fails if they differ |
| `events` | nobody | The contract's events in the last `BLOCKS` finalized blocks |
| `all` | | All of the above, in order |

Every transaction waits for finality and fails on a dispatch error. Every contract message is dry-run
first, fails on a revert, and is sent with the gas the dry run reported.

### On a local Kreivo (zombienet)

Use the suite: `e2e/README.md`, "By hand", or dispatch the workflow on the `zombienet` fleet. It
spawns the network from release artifacts (no Kreivo checkout, no compiled relay), registers the
community, and runs these steps against it with `COMMUNITY_ID`, `COMMUNITY_NAME`, `BOUND` and
`BOUND_PERIOD` set. Kreivo's `development` genesis has no community and no assets; the suite's
network adds `Here(1)` (sufficient) and `Here(2)` (not sufficient), held by //Alice.

### On Kreivo mainnet

A mainnet deployment follows the checklist above with dedicated keys held in the operator's own
custody: register the community, create the accounts, give the governing members their
memberships, publish, license and instantiate the contract through the contracts store, set a
withdrawal bound, then hand the community over to its own origin and pass its first referendum,
which replaces the default track. Dry-run every call as the origin that will sign it first.
`drive.mjs` and `e2e.mjs` stay on `@polkadot/api` and serve the zombienet suite only.

### On a Kreivo testnet, with `drive.mjs`

Use dedicated keys, never `//Alice`. The community must exist with `ADMIN` as its admin (or be
registered by the `community` step, which charges `ADMIN` Kreivo's registration price), and the
depositor must hold `ASSET`.

```sh
cd contracts/settlement && cargo contract build --release
cd scripts && npm install
export KREIVO_WS=wss://<testnet endpoint>
export ADMIN_SURI='<admin secret URI>' KEY_SURI='<key secret URI>' DEPOSITOR_SURI='<depositor secret URI>'
export COMMUNITY_ID=<id> ASSET='{"Here":<asset id>}' PAYEE=<address>
export BUNDLE=../target/ink/aledgent_settlement_contract.contract STATE=.state.testnet.json
node drive.mjs preflight
node drive.mjs all
```

Secret URIs are read from the environment only and never printed.

### Verified on the first zombienet run (answers in NOTES)

- **Transaction extensions.** Kreivo's `PassAuthenticate` is declared to the API as
  `Option<Bytes>`, always `None`. If a transaction is rejected as a bad proof, compare
  `api.registry.signedExtensions` with the runtime metadata.
- **Community origin.** `EnsureCommunity` accepts a signed origin that is a community's admin
  origin. Once the community has been handed over to its own origin (the checklist above),
  `publish`, `license` and `instantiate` signed by the old admin fail with `BadOrigin`; they then go
  through the community's referenda.
- **Chain extension.** Kreivo's `KreivoChainExtensions` is not described by the metadata, so
  `preflight` cannot check it. `deposit` and `execute` exercise it: check that the deposit leaves
  the depositor at or above the asset's minimum, that the withdrawal is an `Assets.Transferred`
  event from the contract account, and that a withdrawal Kreivo refuses (for example a frozen
  asset) is recorded as `Refused` with the reason `Kreivo(Assets(CannotTransfer))`.
- **Contract account and assets.** The contract account exists from instantiation (existential
  deposit), so it can hold a non-sufficient asset. Check that DUSD or COPM arrive without an
  `AssetAccountDeposit` (F-019 open point 5).
- **Storage vs events at finality** (plan open point): `read` compares the contract query with the
  raw child-trie read at the finalized block; `events` shows the same outcomes from
  `Contracts.ContractEmitted`. Record which the adapter should rely on in F-026's `NOTES.md`.
- **Gas.** Record the weight and storage deposit of `deposit` and of a full `execute` batch
  (`MAX_CALLS` = 32 withdrawals to accounts, and the largest batch of withdrawals to another
  deployment that fits with half its dry run again) from the dry runs, and the cost of one call of
  each kind (NOTES, decision 23: a withdrawal to an account 61 KB of proof size,
  one to another deployment, in its own frame, 235 KB). Kreivo `0.17.0` allows a normal extrinsic
  2 446 677 B of proof size, `0.16.9` 3 670 016 B.
- **Per-call frames.** A call starved of gas at the very end of its frame, after it moved value and
  the receiving deployment recorded it, is refused as `CallFailed(OutOfGas)` and nothing of it
  remains; with `execute_with_limits`, a call given half its dry run's limit is refused alone.
