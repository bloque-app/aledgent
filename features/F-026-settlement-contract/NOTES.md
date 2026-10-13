<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-026 — implementation notes

Written while implementing `F-026` (issue #75), 2026-09-24, after the owner's ruling of the same day
(`DEC-17`, `DEC-18`): V0 settles through an ink! contract on Kreivo instead of `F-019`'s pallet. The
contract lives at `contracts/settlement`, a Cargo workspace of its own excluded from the root one,
like `pallets/` and `spikes/`, built and tested by `.github/workflows/contracts.yml`. Where the plan
was silent, the most conservative behaviour was chosen (refuse rather than allow); each such choice
is listed here for review.

**Follow-up of the same day (owner rulings of 2026-09-24, still `F-026`, issue #75).** Value now
moves through Kreivo's own contract API, its chain extension, wherever that API can do the job
(decisions 3, 4, 14); refusals carry Kreivo's reason (decision 19); the V0 behaviours around refused
transfers and refused arrivals are accepted as they are (decision 22); and the contract's upgrade
authority must be a community governed by `pallet-communities`' own membership governance before
the contract holds real value ("Upgrade authority" below). Tests: 71.

**Follow-up of 2026-09-29 (owner's ruling, `T-026-08`, #97).** No single call can fail its batch.
A withdrawal to another deployment runs in a frame of its own (hybrid, by the owner's ruling on
#102); if that frame traps or runs out, the call is refused under its operation reference and the
rest applies (decision 23). Tests: 84.

**Follow-up of 2026-10-03 (amendment 0011, `PLAN.md` `DEC-20`, `T-029-10`).** A new call, `Bind`,
records a book-only asset's holding as the deployment's position, once, at zero divergence
(decision 24). Tests: 93.

**Follow-up of 2026-10-11 (owner's ruling, option (a) "overwrite in place", `T-026-09`).** The
contract keeps only its latest anchor, overwritten in place: an anchor adds no storage item but its
inbox entry, which the admin can now prune once a later anchor supersedes it. A deployment of the
earlier code, which kept every anchor by number, is migrated by its own next messages, a bounded
step at a time, and its storage deposits released (decisions 25 and 26). Tests: see "Tests".

## What Kreivo offers a contract (read at `4aef055`, spec 133)

- **`pallet-contracts` 49.0.0**, and ink! **5.1.1** on the contract side (`kreivo-apis`). The
  contract uses exactly those; `cargo-contract` 5.0.3 builds it.
- **The chain extension is `KreivoChainExtensions`** (`apis/`), extension id 0. Its assets API
  (`apis/src/runtime/api_impls/assets.rs`), on `T::Assets = pallet_assets<Instance1>`:
  - `balance(asset, who)` (`0x0000`): `T::Assets::balance`.
  - `deposit(asset, amount)` (`0x0001`): `T::Assets::transfer(asset, caller, contract, amount,
    Preserve)`. **The caller is `e.caller().account_id()`**: the immediate caller of the contract's
    frame, which is exactly the account `seal_caller` (ink!'s `self.env().caller()`) returns — the
    signer of the extrinsic when a person calls, the calling contract's account when a contract
    calls. A `Root` caller has no account and gets `ExtQueryError`. **`Preserve` keeps the caller's
    asset account alive**: `fungibles::Mutate::transfer` (frame-support 48.1.0) first checks
    `can_withdraw(..).into_result(keep_nonzero = true)`, and `pallet-assets`' `can_decrease` answers
    `ReducedToZero` for any remainder below the asset's minimum, zero included, which that refuses.
    No approval exists or is left behind.
  - `transfer(asset, amount, beneficiary)` (`0x0002`): `T::Assets::transfer(asset, contract,
    beneficiary, amount, Preserve)`. The contract's own asset account is always kept alive.
  - On success the runtime writes the amount moved and returns status 0. On failure it writes
    nothing and returns a status code: `0x0001_0000` (`CannotDeposit`), `0x0001_0001`
    (`CannotTransfer`), `2` (`ExtQueryError`). Every `pallet-assets` error is mapped to the one
    `Cannot…` code (`.map_err(|_| …)`), so the code says which step failed, not why.
  - `fungibles::Mutate::transfer` checks `can_withdraw` and `can_deposit` before its first write, so
    a refused transfer has written nothing, although the extension call is not a transactional
    dispatch of its own.
- **`call_runtime` is enabled.** It is a stable host function of `pallet-contracts` 49, dispatched
  with the contract's account as a signed origin through `Config::CallFilter`. Kreivo's filter
  allows `Balances::transfer_allow_death` and every `Assets` call
  (`runtime/kreivo/src/config/contracts/mod.rs`); `UnsafeUnstableInterface` is on besides.
- **`Assets` is `pallet_assets<Instance1>` at pallet index 13**, `pallet-assets` 53.0.0, asset id
  `FungibleAssetLocation` (also the call parameter), `Lookup` source `MultiAddress<AccountId,
  CommunityId>`. The contract encodes only `transfer` (8) itself now (`src/kreivo.rs`); a unit test
  pins the bytes, another proves the contract's mirror of `FungibleAssetLocation` encodes as
  Kreivo's own `virto-common` at the pinned commit, and the driving script's `preflight` compares
  the layout with the live runtime's metadata.
- **Upload is by community origin** (`ContractsStore.publish`), **instantiation only through the
  contracts store** (`pallet_contracts::Config::InstantiateOrigin` is `EnsureNever`;
  `ContractsStore.instantiate` calls `bare_instantiate` with the community account as caller, after
  `request_license`). A signed origin that is a community's admin origin passes `EnsureCommunity`.
  Who can then change the code is in "Upgrade authority" below.
- `MaxCodeLen` is 192 KiB; the optimised contract is about 28 KiB.

## Decisions taken during implementation

### Shape

1. **One contract instance per deployment.** The contract's own account is the deployment's
   position; its storage holds one deployment. A pallet is a singleton and needed instances; a
   contract is instantiated once per deployment through the store, one license each. No
   deployment's value shares an account with another's, so a fault in one cannot drain another.
   The pallet's `register_instance` is the constructor, `new(admin, keys, attestation_key)`.
2. **Positions are kept in storage, per asset, as the inbox accounts for them.** The plan's storage
   list names positions. A deposit or credit adds; an applied withdrawal subtracts. **A withdrawal
   is refused (`InsufficientPosition`) when it exceeds the recorded position**, even if the
   contract's account holds more: value that reached the account by any other path (a plain
   transfer, an XCM arrival) has no inbox entry, and the book cannot spend it. That leaves `F-019`
   open point 4 (value outside the venue) exactly where it was, and reconciliation can compare the
   recorded position with the account's balance.
3. **Value moves through Kreivo's contract API; `call_runtime` only where that API has no
   equivalent.** *(Revised by the owner's ruling of 2026-09-24; the first version used
   `call_runtime` with `Assets` calls for everything, and the extension only for `balance`.)*
   - Deposits: the extension's `deposit` (decision 14).
   - Withdrawals that leave the contract's asset account alive, to an account or to another
     deployment: the extension's `transfer`.
   - **Kept on `call_runtime`: a withdrawal of the contract's whole holding of an asset**, as
     `Assets::transfer` (call 8). The extension's `transfer` always preserves the source, so it
     cannot empty the account, and a position must be able to leave whole (decision 4). Nothing
     else is dispatched: `transfer_keep_alive` (9) and `transfer_approved` (25) are gone.
   - Reads of the contract's own holding: the extension's `balance`, as before.

   **The binding is the contract's own, not Kreivo's `kreivo-apis` crate.** The owner asked for
   Kreivo's published bindings if they build against ink! 5.1.1. They do not build into this
   contract: `kreivo-apis` depends on `frame-support` unconditionally (even with only the `contract`
   feature), which brings `sp-io`, `sp-core` and `secp256k1-sys` into the WASM build. A probe
   contract calling `KreivoApi`'s `deposit` compiled but did not link with `cargo contract build`
   and the pinned 1.91 toolchain: `secp256k1-sys`' C code, compiled by the host's clang (LLVM 22),
   is bitcode rustc's LLVM 21 cannot read. Another machine's clang might link it, but the contract
   would still carry the substrate primitives and a C library it never calls. Its bindings also
   fix the contract's environment to its own `KreivoApiEnvironment` (the extension type is
   private) and its asset id to `virto-common`'s. So `src/kreivo.rs`
   declares the same three functions with the same ids and argument order (`apis/src/contract/
   chain_extension.rs`), and the tests pin it byte for byte against the runtime's decoding: the
   mock extension reads each input with Kreivo's own asset type as `AssetsApiInfo::try_from` does
   and answers with the status codes `kreivo-apis`' own `KreivoApisErrorCode` builds;
   `kreivo_api_calls_encode_as_kreivo_decodes_them` checks the ids and bytes; and
   `kreivo_api_errors_decode_as_kreivo_encodes_them` checks that the contract reads every status
   code as Kreivo does and that its error type encodes as Kreivo's `KreivoApisError`.
   `kreivo-apis` is a dev-dependency only, with neither its `runtime` nor its `contract` feature.
4. **A position leaves whole or stays at or above the asset's minimum** (`F-019` decision 11). The
   contract reads its holding through the extension: if the withdrawal equals it, it dispatches
   `Assets::transfer` through `call_runtime` (the account may be emptied); otherwise the extension's
   `transfer`, which Kreivo refuses if dust would remain (`Preserve`). `pallet-assets`' `prep_debit`
   would otherwise send the dust along with no inbox entry. Tested against a model of both.
5. **No separate `anchor` message.** The plan's scope lists "anchor" among the messages; it is a
   call kind in `execute` instead, so every call the deployment makes, anchors included, goes through
   one envelope and gets one recorded outcome (`DEC-18`'s single call outbox). An anchor submitted
   alone is a batch of one.
6. **The envelope.** `Envelope { operation_ref, call }`, with `Call` an enum: `Withdraw { to, asset,
   amount }` (to `Destination::Account` or `Destination::Deployment { contract, account_ref }`) and
   `Anchor { number, commitment }`. New call kinds are new variants at the next codec index; the
   envelope, `Outcome` and the inbox's `Refused` entry (which carries the call as submitted) do not
   change. `Refusal` reasons likewise only grow. A unit test pins the encoding of envelopes and
   outcomes, since the adapter builds and reads those bytes.
7. **No per-call expiry.** `REQ-SL-4`'s expiry is the transaction's mortality, as in `F-016`: a
   batch included after its era is impossible, a batch never included is provably not executed.
   A per-call deadline would need a new field in the envelope; it is not built.
8. **Limits.** `MAX_KEYS` 16 (the pallet's Kreivo value), `MAX_CALLS` 32 per batch, `MAX_PRUNE` 128
   per call (the pallet's). `MAX_CALLS` is a bound, not the size that fits: proof size decides
   that, by the batch's dry run (decision 23, "The price").
9. **Selectors are explicit** (`0x0000_0001` for the constructor, `0x0000_01xx` for value messages,
   `0x0000_02xx` for admin, `0x0000_03xx` for reads), so the adapter can encode calls without the
   metadata and the interface survives renames. `credit`'s selector is also a constant the
   contract uses to call another deployment; a test proves the dispatcher routes it.

### Behaviour

10. **Every call in a batch gets exactly one outcome, recorded in the inbox.** Applied: the inbox
    entry that records it (`Withdrawn`, `Anchored`). Refused: a `Refused { operation_ref, call,
    reason }` entry. **One refusal does not undo the rest**: every check is made before anything
    is written; a refused transfer has itself written nothing (the extension's `transfer` checks
    `can_withdraw` and `can_deposit` before its first write; the one `call_runtime` transfer is a
    FRAME call, which is transactional); and the bound and the position are written only after the
    transfer succeeded.
11. **More refusals are recorded than in the pallet.** The pallet failed the whole extrinsic for
    faults in the call itself (zero amount, a transfer to its own instance, a position account
    named as a plain account). In a batch that would undo the other calls, so here they are
    recorded refusals like any other (`ZeroAmount`, `SelfTransfer`, `DeploymentAsAccount`,
    `NotADeployment`). A conflicting or out-of-order anchor is also a recorded refusal
    (`AnchorConflict`, `AnchorOutOfOrder`), where the pallet failed the call.
12. **What still fails the whole message, and records nothing:** the caller is not a key; the batch
    is empty or has more than `MAX_CALLS` calls; the inbox's sequence cannot fit the batch. In
    ink! 5 a message that returns `Err` reverts, and the extrinsic fails with `ContractReverted`.
    *(Revised by decision 23, 2026-09-29.)* Two traps used to revert it too, as `ContractTrapped`:
    a `credit` that fails after the transfer (decision 15), and a transfer Kreivo reports as having
    moved an amount other than the one asked for (value moved, and not as asked, can be neither
    recorded as asked nor refused; Kreivo at `4aef055` always reports the amount asked for). They
    still trap, but only the call's own frame, which rolls the transfer back; the batch records the
    call as `CallFailed(Trapped)` and goes on. The batch's own exhaustion still reverts it whole
    (decision 23).
13. **An identical anchor replay writes nothing** and returns `Applied` with the inbox position of
    the original entry (`AnchorRecord` keeps it). Any stored anchor, not only the latest, as in the
    pallet (`F-019` decision 14). No event is emitted for a replay. *(Revised by decision 25,
    2026-10-11: only the latest anchor replays; an older number is refused.)*
14. **A deposit is pulled from its caller by Kreivo's assets API, which keeps the depositor alive.**
    *(Revised by the owner's ruling of 2026-09-24; the first version pulled with
    `Assets::transfer_approved` under an approval, which could reap the depositor.)* `deposit`
    calls the extension's `deposit(asset, amount)`, which moves the amount from the contract's
    immediate caller to the contract with `Preserve`:
    - **Who pays.** The runtime takes the funds from `e.caller()`, the same account the contract
      records as `from` (`self.env().caller()`): nobody can deposit another's funds into an
      account reference of their choosing. A contract that calls `deposit` pays from its own
      account, never from whoever called it.
    - **Consent.** The call, with the amount in its arguments, is the depositor's consent. No
      approval is needed, and none is left behind to be spent later.
    - **Keep-alive.** A deposit that would leave the depositor's asset account below the asset's
      minimum balance, emptied included, is refused. The depositor-reaping trade-off of the first
      version is gone: the pallet's protection (`F-019` decision 12) is back. The price is that a
      depositor can never deposit its entire holding of an asset: the minimum stays behind.
    - **Refusal.** A refused pull returns `Error::DepositRefused(reason)`, with Kreivo's reason
      (decision 19), which reverts the message: nothing moved, nothing is recorded, and the
      extrinsic fails with `ContractReverted`. A pull Kreivo reports as other than the amount asked
      for returns `Error::UnexpectedAmount`, which reverts the pull with it.
15. **Withdrawal to another deployment: transfer, then `credit`.** The sender checks that the
    receiver runs the same code (`code_hash` equals `own_code_hash`), transfers the value, then
    calls the receiver's `credit(account_ref, asset, amount)`, which records a `Deposited` entry from
    the sender's contract account (`REQ-SL-6`: another deployment is an ordinary counterparty). The
    receiver accepts `credit` only from a contract running its own code, and our code calls it only
    right after moving that amount. **If `credit` fails after the transfer, the contract traps**, so
    value never arrives unrecorded. *(Revised by decision 23.)* The trap is the call's own frame's:
    the transfer and whatever the receiver wrote are rolled back with it, and the batch records the
    call as `CallFailed`. A receiver whose inbox cannot advance, or a frame that runs out during
    `credit`, causes it.
    **Trade-off:** two deployments at different code versions cannot transfer to each other
    (`NotADeployment`) until both are upgraded; and a deployment at another version named as a
    plain `Account` is not recognised as one, so its inbox would not record the arrival.
16. **Withdrawal bounds are the pallet's** (`F-019` decisions 8 and 9): no bound, no withdrawal;
    `{amount, period}` in Kreivo's blocks; the window starts when the bound is set and restarts at
    the first withdrawal after `period`; a refused withdrawal uses nothing; changing a bound keeps
    the window and what it used. Bounds also apply to transfers to another deployment.
17. **Keys and admin are the pallet's** (`F-019` decision 15): no duplicate keys, at most 16, an
    empty set freezes every call, the admin is not a key unless listed, and there is no admin
    rotation (the pallet had none either).
18. **No deduplication by operation reference.** The contract cannot key storage by it
    (`REQ-SL-5`), so it cannot tell a resubmitted call from a new one. Exactly-once is the
    adapter's: store before submit, replay the same signed bytes (`F-016`, carried into `DEC-18`).
19. **Refusal detail: a refused withdrawal carries Kreivo's reason.** *(Revised by the owner's
    ruling of 2026-09-24; the first version recorded a bare `TransferFailed` for every transfer
    Kreivo refused.)* A withdrawal the extension's `transfer` refuses is recorded as
    `Refusal::Kreivo(KreivoApiError)` (new, codec index 10), Kreivo's own error type
    (`kreivo_apis::KreivoApisError`), decoded from the status code as Kreivo decodes it and encoded
    as Kreivo encodes it. A refused deposit carries the same type in `Error::DepositRefused`.
    `Refusal::TransferFailed` (index 7, unchanged) now means only the withdrawal of a whole
    position, the one transfer still made through `call_runtime`, which reports nothing but "the
    call failed". **What the reason can say, stated plainly:** Kreivo's runtime maps every
    `pallet-assets` error of the transfer to `CannotTransfer` (or `CannotDeposit`), so the reason
    names which step Kreivo refused, not why (a frozen asset or account, the receiver's minimum,
    dust left behind, an asset the receiver cannot hold all read the same). `REQ-SA-3` asks for the
    network's own error text *where one exists*; this is the text Kreivo gives a contract. The
    contract's own reasons (`ZeroAmount` to `AnchorConflict`) are exact.
20. **Toolchain.** `cargo-contract` 5.0.3 builds `core` with `-Zbuild-std-features=panic_immediate_abort`,
    which Rust 1.92 and later reject ("panic_immediate_abort is now a real panic strategy"). The
    contract pins Rust **1.91** in `contracts/settlement/rust-toolchain.toml`; the root workspace
    keeps `stable`. When `cargo-contract` is upgraded, drop the pin.
21. **Lints.** `cargo contract build` runs clippy with `cast_possible_truncation` and
    `arithmetic_side_effects` denied. `parity-scale-codec`'s derive casts enum variant indices to
    `u8` under the variant's span, so `types.rs` and `kreivo.rs` allow `cast_possible_truncation`
    (nothing else in them casts). ink! generates `#[no_mangle]` exports, so the crate cannot
    `forbid(unsafe_code)`; CI greps `src` for `unsafe` instead, and runs `check-no-float.sh`.

### V0 behaviours accepted by the owner (2026-09-24)

22. **Refusals and failed arrivals are reported, not repaired, in V0.**
    - **A refused transfer is just that.** The contract records the refusal under the call's
      operation reference, with its reason (decisions 10, 19), and moves nothing; the adapter
      (`F-027`) reports it as an observation, "provably not executed" (`REQ-SA-3`, `REQ-SA-4`).
      The contract does not retry it, and does not choose another way to move the value.
    - **If funds arrive and are refused or fail, aledgent notices from the inbox and informs the
      holder.** The contract records every arrival it takes in (`Deposited`, with `from`, the
      account reference, the asset and the amount) and never judges it: it cannot know whether the
      reference resolves to a ledger account that can be credited. When aledgent refuses an arrival
      it has read (an unknown or unusable account reference) or fails to credit it, it notices from
      that inbox entry and informs the holder; the value stays in the deployment's position until
      then, and the contract has no path that returns it on its own. A deposit Kreivo refuses never
      arrives: the depositor's own transaction fails (decision 14) and the depositor's wallet sees
      it; there is nothing in the inbox to notice. Value that reaches the contract's account outside
      `deposit` and `credit` (a plain transfer, an XCM arrival) has no inbox entry either
      (decision 2); reconciliation sees it as the difference between the account's balance and the
      recorded position.

### Per-call frames (owner's ruling of 2026-09-29, `T-026-08`, #97)

> "Why would a failed call fail the entire contract? We should emit an event indicating that this
> specific call failed, alongside its well-recorded correlation id." (the owner, 2026-09-29)

23. **A withdrawal to another deployment runs in a frame of its own; if that frame fails, the call
    is refused and the batch goes on.** *(Hybrid, by the owner's ruling on #102 of 2026-09-29; the
    first version gave every call a frame, which doubled the cost of every withdrawal: "The
    price".)* Until now two things reverted a whole batch: the two invariant traps of
    `try_withdraw` (Kreivo moved an amount other than the one asked for; the receiving deployment
    did not record the credit, decision 15), and one call exhausting the transaction's weight.

    **Which calls get a frame.** Only `Withdraw { to: Destination::Deployment { .. } }`: it is the
    one call that calls another contract, and where the credit trap lives. For it, `execute` calls
    `apply(head, envelope)` (selector `0x0000_0110`) **on the contract itself**, with
    `CallFlags::ALLOW_REENTRY` and a weight limit of the call's own (ref time and proof size,
    through ink!'s `call_v2`). `apply` is the per-call body: it checks, moves, records the applied
    outcome or an ordinary refusal, and returns the inbox's head as the call left it; the batch
    stores the head once, at its end. Anyone but the contract itself calling `apply` is refused
    (`NotKey`) and nothing happens.

    **A withdrawal to an account and an anchor run inline, as before.** Everything that can refuse
    them is already a recorded `Refused` outcome (decisions 10, 11, 19); the only way either can
    sink the batch is the batch running out of weight, which the adapter's dry-run sizing prevents
    (`F-027`, T-027-09). **Stated plainly:** the misreported-amount trap also guards a withdrawal to
    an account, and inline it still reverts the batch. Kreivo's assets API cannot reach it: its
    `transfer` returns what `fungibles::Mutate::transfer` returns, the amount it was asked to move
    (`apis/src/runtime/api_impls/assets.rs`, 0.16.9 and 0.17.0); the test that exercises it with a
    mock that lies now expects the batch to trap, as before `T-026-08`.

    **When the frame fails** (it traps, reverts or runs out of its limit), `pallet-contracts` has
    already rolled back everything the frame did, and the batch records the call as
    `Refused { operation_ref, call, reason: CallFailed(kind) }`, entry and event, and moves on to
    the next call. `Refusal::CallFailed(CallFailure)` is the one new reason, appended at codec
    index 11 (bytes `[11, 0]` for `Trapped`, `[11, 1]` for `OutOfGas`); no earlier index moved, and
    the encoding test pins all twelve. For the adapter it is a refusal like any other: a proved
    rejection, nothing moved, recorded at finality.

    **What `pallet-contracts` guarantees, read in its source.** Mainnet's Kreivo `0.16.9` locks
    `pallet-contracts` **43.0.0** (`Cargo.lock` at the `0.16.9` tag; polkadot-sdk `stable2509`);
    **Kreivo `0.17.0` (`3e0a5c3`, spec 133, transaction version 13), which mainnet enacts on
    2026-09-30 and the canary deploys on, locks 49.0.0**, as `4aef055` does. Every line below reads the same in 43.0.0 and
    49.0.0 (line numbers are 43.0.0's; 49.0.0's differ by at most one where noted):

    - **A frame is a storage transaction.** `exec.rs` `run`, lines 1064-1070: the frame's whole
      execution runs in `with_transaction`, and anything but a successful, non-reverted return is
      `TransactionOutcome::Rollback` (49.0.0: 1064-1070). Its effects are storage writes in that
      transaction: a transfer by Kreivo's chain extension (`pallet-assets` writes, from
      `seal_call_chain_extension` inside the frame, `wasm/runtime.rs` 2256-2266), a runtime call
      (`call_runtime` dispatches in the frame, `exec.rs` 1550-1554), a call into another deployment
      (a nested frame inside this one), and the contract's events (`deposit_event` writes
      `frame_system`'s events, `exec.rs` 1493-1497, `lib.rs` 1913). All of them go with a failed
      frame. `pop_frame(persist: false)` (`exec.rs` 1115) also drops its storage-deposit charges.
    - **The caller survives a callee that traps, reverts or runs out.** An error raised while the
      callee executes carries `ErrorOrigin::Callee` (`exec.rs` 988), and `exec_into_return_code`
      turns any callee error into the return code `CalleeTrapped` for the caller
      (`wasm/runtime.rs` 737-749; 49.0.0: 745-757). Running out of gas inside the frame is such an
      error (`OutOfGas`, from the frame's own meter). A revert returns normally with the revert
      flag (`CalleeReverted`).
    - **A frame's own limit.** `GasMeter::nested` gives the frame the lesser of its limit and what
      the caller has left, and a zero part means "all that is left" (`gas.rs` 171-187). So the
      contract never passes a zero part.
    - **What the caller pays before the frame's limit starts.** `new_frame` loads the callee's code
      against the **caller's** meter (`exec.rs` 861; `wasm/mod.rs` 466, `CodeLoadToken`: the code's
      length in proof size), and only then carves out the frame's meter (`exec.rs` 900). With
      `seal_call`'s base cost (`CallBase`), that is the extra cost of a frame: see "The price"
      below. If the caller cannot pay it, the error is the caller's own and the whole message
      traps; the batch reserves for it (below).
    - **Reentry.** `call` sets the caller's frame to allow reentry for this call only, from the
      flag, and refuses a call into a contract that has a frame on the stack not allowing it
      (`exec.rs` 1241-1242, 1273-1277). `apply`'s own call to another deployment's `credit` passes
      no flag, so while it runs no frame may call back into the sender.

    **Storage deposit cannot be isolated per call; it is a batch-level limit only.** A frame can be
    given its own storage-deposit limit, but exceeding it is checked when the frame returns
    (`enforce_subcall_limit`, `exec.rs` 1044) and raised with `?`, which makes it the **caller's**
    error (`From<DispatchError> for ExecError` sets `ErrorOrigin::Caller`, `exec.rs` 125-128), and
    `err_into_return_code` passes any caller error but three through as a trap of the caller
    (`wasm/runtime.rs` 721-734). `pallet-contracts`' own test `deposit_limit_in_nested_calls`
    (49.0.0 `tests.rs` 3579-3589) shows it: a nested call over its own limit fails the extrinsic
    with `StorageDepositLimitExhausted`. So a per-call deposit limit would turn one call's excess
    into a reverted batch, the opposite of the ruling. The contract gives each frame **no** limit of
    its own (the batch's applies), and there is no `CallFailed` kind for it. Nothing is lost by
    that: what a call can put down is fixed by the code, not by the caller (at most one inbox entry,
    one anchor record, one position cell here and one entry and position cell at a receiving
    deployment), so no single call can run away with the deposit; the batch as a whole runs out
    only when the transaction's `storage_deposit_limit`, or the key's free balance, is short for
    the whole batch, which the adapter's dry run prices exactly.

    **Each framed call's weight, known ahead of time (the owner, 2026-09-29).** A dry run
    (`ContractsApi_call`) of a call alone, as a batch of one, returns its `gas_required`, which
    covers the call's frame and the batch's own cost besides. So there are two messages:

    - `execute_with_limits(calls, limits)` (selector `0x0000_0103`): one `Weight` per call, SCALE
      as `sp_weights::Weight` (both parts compact), in a vector alongside the envelopes. Call `i`'s
      frame gets `limits[i]` **plus an eighth** (the margin for state that moved between the dry
      run and inclusion, `call_limit`); a zero part takes `CALL_WEIGHT_LIMIT`'s; a count other than
      the calls' is refused whole (`Error::LimitsMismatch`, appended). **The limits apply to the
      calls with a frame only**; an inline call's entry is read for the count and nothing else, so
      the vector keeps one entry per call whatever the batch holds. A call given too little runs
      out alone and cannot take what the calls after it were given. This is the path for the
      adapter: it has each call's dry run anyway when it sizes a batch (`F-027`, T-027-09, the
      other agent's), and it need not know anything of the contract's costs.
    - `execute(calls)`, unchanged in shape: every frame gets the contract's own
      `CALL_WEIGHT_LIMIT`. Kept so that nothing that speaks the contract today breaks, and for an
      operator without a dry run at hand.

    **Why the limits travel beside the envelope, not in it:** the call inside `Envelope` is what the
    inbox's `Refused` entry records and the adapter decodes; a limit is a submission parameter, not
    part of what the call is; and a SCALE struct cannot gain a field without every old decoder
    breaking. A vector alongside it changes nothing that exists.

    **Why the contract still derives something itself:** whatever the limits say, only the contract
    knows at run time how much weight is left, so the reserve for recording refusals is kept inside
    it (`gas_left`, below), and a frame never gets more than leaves that reserve.

    **The frame's limit and the batch's reserve.** The frame gets `CALL_WEIGHT_LIMIT` (25 G ref time,
    512 KiB proof size: about twice the costliest call measured, a withdrawal to another
    deployment), or less, so that the batch always keeps what it needs besides: `CALL_FRAME_COST`
    for making the call (1 G, 80 KiB: `seal_call`'s base of 148 M ps, three reads and a write and
    4 019 B, a code load of 1 040 ps and 1 B per byte, 33 KB, and what else the batch is charged for
    it; measured, below),
    `REFUSAL_RESERVE` for recording this call and each framed call still to come as refused (0.6 G, 8 KiB
    each: an inbox entry, about 140 M ps and 254 B, and an event with two topics, about 265 M ps and
    5 940 B), and `BATCH_FINISH_RESERVE` to store the head and return (0.4 G, 4 KiB). Each is an
    upper bound read from Kreivo `0.16.9`'s own `pallet_contracts` weights
    (`runtime/kreivo/src/weights/pallet_contracts.rs`, with `RocksDbWeight`) and checked on
    zombienet. If nothing is left to give a frame, the call is refused as `CallFailed(OutOfGas)`
    without one. So a withdrawal to another deployment that the batch cannot afford is refused under
    its reference instead of reverting the batch. An inline call the batch cannot afford still runs
    it out of weight, which reverts it whole (`F-027`'s finding 2): that is the
    batch's own exhaustion, which a dry-run-sized batch does not reach.

    The zombienet runs set these. The first had a whole G of ref time in every reserve: a one-call
    batch sent with half its dry run again had its only call refused as out of gas (ref time, never
    what binds a batch, did). The second had `CALL_FRAME_COST` at 44 KiB, the weights' base and code
    load alone: a batch whose last call ran out was then itself short, because the batch pays
    between 49 and 61 KB of each frame (bisected against the reserves that held and the ones that
    did not). 80 KiB covers that and 20 KB of code growth.

    **Reading the weight left needs `seal1::gas_left`.** ink! 5.1.1's `gas_left` reads the ref time
    only (`seal0`), and proof size is what binds a batch. The contract reads both through
    `pallet-contracts-uapi` 9.0.0 (the version ink! 5.1.1 is built on), `HostFn::gas_left_v1`,
    which `pallet-contracts` marks `#[unstable]` (`wasm/runtime.rs` 1835-1836; 49.0.0: 1843-1844).
    Unstable functions link only if the runtime sets `UnsafeUnstableInterface`; Kreivo does, at
    `0.16.9` and at `0.17.0` alike: `type UnsafeUnstableInterface = ConstBool<true>;` at
    `runtime/kreivo/src/config/contracts/mod.rs:131` in both tags (`CallStack` is 23 frames in
    both, line 111). **Trade-off, stated
    plainly:** if Kreivo ever turned that off, or `pallet-contracts` changed the function, every
    call into the contract would fail to link (`CodeRejected`) until the contract is upgraded
    through the contracts store. `pallet-contracts` is in maintenance (superseded by
    `pallet-revive`), so a change is unlikely, but it is Kreivo's decision, not ours.

    **The kind.** `pallet-contracts` tells the caller only `CalleeTrapped`, whether the frame
    trapped or ran out. The batch tells them apart by what it spent on the frame: a frame that ran
    out used its whole limit in one part of the weight, bar the one charge it could not pay, and the
    batch's own cost of making the call (at least the code's load, in proof size) is larger than any
    single charge a frame can fail on. So `OutOfGas` when the batch spent at least the frame's limit
    in either part, `Trapped` otherwise (a revert reads as `Trapped` too). The kind is for
    diagnosis; both are the same proved rejection.

    **What still reverts a whole batch:** the batch's own faults (not a key, empty, too many calls,
    an inbox that cannot fit it: decision 12), and **the batch's own exhaustion**: less weight than
    the reserve for recording its calls, or a storage deposit the transaction or the key cannot
    cover. Both are the submitter's, and a dry run finds them.

    **A submitter must send more than the dry run's gas.** The dry run reports what the batch used,
    not the reserve it kept back. Sent with exactly that, the last call can find less than its frame
    needs plus the reserve, and be refused as `CallFailed(OutOfGas)`. The margin that never lets
    that happen is at most `CALL_FRAME_COST + REFUSAL_RESERVE + BATCH_FINISH_RESERVE` (2 G ref time,
    92 KiB proof size) above the dry run, and in practice what the reserves hold beyond the costs
    they cover (about 1.2 G and 35 KB). `drive.mjs` adds 2 G and 96 KiB; `e2e.mjs` the larger of
    that and half the dry run again. `F-027`'s batch sizing (half the dry run again) covers it for
    any batch with a framed call in it (a batch of one withdrawal to another deployment dry-runs at
    3.8 to 4.3 G and 336 KB). A batch with no framed call keeps no reserve and needs no margin.

    **The price.** Measured on zombienet with the same suite, from dry runs of batches of one and of
    several calls (the difference over the extra calls is the cost of one call; the rest is the
    batch's own: eight calls in the measurement runs, `MAX_CALLS` since). Before: `main`'s contract on `0.16.9` (run 36589866155).
    Hybrid (this code, 33 477 B): on `0.16.9` (run 36614340594)
    and on `0.17.0` with `polkadot-stable2606-2` (run 36614345555).

    | One call, ref time and proof size | Before (`0.16.9`) | Hybrid, `0.16.9` | Hybrid, `0.17.0` |
    |---|---|---|---|
    | Withdrawal to an account (inline) | 1.04 G, 61 216 B | 1.04 G, 61 216 B | 0.96 G, 60 972 B |
    | Withdrawal to another deployment (own frame) | 2.70 G, 172 617 B | 3.47 G, 235 971 B | 3.08 G, 234 995 B |
    | The batch alone | 0.81 G, 95 961 B | 0.84 G, 101 292 B | 0.74 G, 101 292 B |

    A withdrawal to an account costs exactly what it did before `T-026-08`. A withdrawal to another
    deployment costs 63 KB (+37 %) and 0.77 G more for its frame; the batch itself pays 49 to 61 KB
    of that (see the reserves above), of which Kreivo's weights account for 37 KB (`seal_call`'s
    base, 4 019 B, and a fresh load of the contract's code, its length). The batch alone costs 5 KB
    more (the larger code). Storage deposits do not change. The first version, a frame for every
    call, cost a withdrawal to an account 119 KB (+94 %), and 32 of them no longer fit.

    **Kreivo `0.17.0`'s extrinsic limit is not a constant.** Its `MaximumBlockWeight` is
    `MaxParachainBlockWeight<Runtime, TargetBlockRate>` (`runtime/kreivo/src/config/system.rs:45-46`):
    a block's share of the cores the parachain has, and the first block of a core may take the
    whole core (`DynamicMaxBlockWeight`, `lib.rs:95-98`). The suite read a normal extrinsic's proof
    size as 2 446 677 B in one run (36614345555) and 7 340 032 B in another (36628062170), against
    `0.16.9`'s fixed 3 670 016 B. What mainnet allows depends on its cores at the time.

    **The two batches, against the extrinsic limit** (proof size, dry runs; half again on top is
    what the adapter sends, `F-027` T-027-09; "fits" means under the limit less 64 KiB for the call's
    own weight):

    | Batch | `0.16.9` (3 670 016 B) | `0.17.0` at 2 446 677 B | `0.17.0` at 7 340 032 B |
    |---|---|---|---|
    | 32 withdrawals to accounts, inline | 2.06 MB, 56 %: lands | 2.05 MB, 84 %: fits, not with half again | 2.05 MB, 28 %: lands |
    | Withdrawals to another deployment that fit with half again | 9 (landed: 2.23 MB, 61 %) | 6 (1.51 MB) | 20 (landed: 4.80 MB, 65 %) |
    | Withdrawals to accounts that fit with half again | 37 | 24 | 77 |

    **`MAX_CALLS` stays 32**, the generous bound (it was 32 before `T-026-08`). One constant cannot
    serve both kinds: the worst case, all withdrawals to another deployment, allows 6 when
    `0.17.0` gives an extrinsic 2.45 MB, and a cap of 6 would take three quarters of the capacity
    the hybrid design won back for withdrawals to accounts. Nor can a constant follow `0.17.0`'s
    moving limit (6 to 20 of the costliest calls in two runs). It does not need to: the cap is not
    what keeps a batch inside an extrinsic. Proof size is, and it depends on the calls. The adapter fills each batch only while
    its dry run, with half again on top, fits the runtime's extrinsic limit (`F-027`, T-027-09,
    the other agent's). A batch that is still too large is refused by the transaction pool and
    changes nothing on-chain (the first measurement run found exactly that: "would exhaust the
    block limits", nonce unconsumed). So a larger cap cannot cost value or a record; at worst it
    lets a mis-sized batch be refused before inclusion. The suite shows both on each runtime: a
    full batch of 32 withdrawals to accounts lands, and so does the largest batch of withdrawals to
    another deployment that the adapter's rule allows, which it sizes from its own measurement.

    **For `F-027`:** its batch cap, `NORMAL_EXTRINSIC_MAX` (3 670 016 B), is `0.16.9`'s; on `0.17.0`
    a batch given that much gas is refused by the pool. It must read the limit from the runtime
    (`BlockWeights::per_class.normal.max_extrinsic`) at the block it sizes against, since on
    `0.17.0` it moves. On `0.17.0`,
    32 withdrawals to accounts need its sizing: with half again on top they do not fit.

    **Tests.** Each failure kind in its own unit test: a withdrawal to another deployment in a batch
    of five, between anchors and withdrawals to accounts, whose frame traps because Kreivo
    misreports the amount; traps because the receiving deployment refuses the credit; runs out right
    after the transfer; runs out after the receiver recorded the value (entry, position and event
    rolled back on both sides). Each checks that the other calls applied and moved exactly their
    amounts, that only the framed call had a frame, and that the failed call's `Refused` entry and
    event carry its reference. Besides: only withdrawals to a deployment get a frame; a frame whose
    limit is smaller than the call needs; a batch with weight for two framed calls of five; limits
    from `execute_with_limits` (plus an eighth, framed calls only; a call given too little runs out
    alone); `apply` refused to anyone but the contract; a misreported withdrawal to an account
    traps the batch, as before. The mock models the frame as `pallet-contracts` runs it: the batch
    pays for making it, the frame gets the lesser of its limit and what is left, and a failed frame
    is rolled back, cells, assets and events alike. On zombienet, `e2e.mjs` starves the second of
    two withdrawals to another deployment at the very end of its frame, runs `execute_with_limits`
    with dry-run limits and with one halved, and lands a full batch of 32 withdrawals to accounts
    and the largest batch of withdrawals to
    another deployment that fits, sized from its own measurement, on both runtimes ("The price").

### Binding a book-only asset (amendment 0011, `DEC-20`, `T-029-10`)

24. **`Call::Bind { asset, amount }` (codec index 2) records a book-only asset's holding as the
    deployment's position, once, at zero divergence** (`REQ-AS-8`, `REQ-SL-3`). Before it is
    submitted, the asset's issuer puts exactly the book's outstanding balance of the asset in the
    contract's account by a plain transfer or a mint on Kreivo. That records no deposit, so no
    account is credited, and no deposit has to be told apart from a holder's. The bind then:

    - is refused with `Refusal::AlreadyBound` (index 12) if the asset was bound before, whatever
      the holding: a binding is never repeated or undone;
    - reads the contract's holding of the asset through the chain extension's `balance` (as
      `try_withdraw` does), and is refused with `Refusal::HoldingMismatch { holding }` (index 13,
      `holding: u128`, the holding it read) unless the holding equals the position the inbox
      already records plus `amount`, exactly. An `amount` that overflows the position is the same
      refusal. **The recorded position counts**, beyond `DEC-20`'s first wording ("never recorded
      a position"): value deposited before the binding is already in the book and in the
      position, and the issuer funds only the rest. Asking for no position at all would refuse a
      binding whenever a deposit of the asset had arrived first, and an asset whose position
      already holds the whole outstanding balance binds at zero;
    - otherwise appends `InboxEvent::Bound { operation_ref, asset, amount }` (index 4), sets the
      position to the recorded position plus `amount` (none stored at zero, as for every
      position), stores the entry's sequence in `bound[asset]`, emits the ink! event
      `Bound { seq, operation_ref (topic), asset, amount }`, and returns `Applied { seq }`.

    A refusal is recorded like every other (decision 10), under the call's operation reference,
    and changes nothing but the inbox. Nothing moves in either case. **The equality is the
    layer's**: it is checked at the block that records the binding, and is final with it, so no
    read by the adapter or the core leaves a gap before the binding. A bind of zero is allowed
    (an asset with nothing outstanding). A bind runs **inline**, never in its own frame: it calls
    no other contract and nothing in it can trap after a change (decision 23). After the binding
    the asset is like any other: deposits, credits and withdrawals, which still need a withdrawal
    bound.

    **`Bound` entries are never pruned**, like anchor entries: `bound[asset]` points at its entry,
    and it is the binding's proof on the layer. `InboxEvent::is_kept()` (anchors and binds) now
    decides what `prune_inbox` keeps; `is_anchor()` keeps its meaning. A new read,
    `bound(asset) -> Option<InboxSeq>`, selector `0x0000_0306`. The debug invariant check adds that
    every `Bound` entry is the one its asset's `bound` points at.

    **The contract's code hash changes.** A deployment that is to bind takes the new code by the
    contracts store's upgrade (`ContractsStore.upgrade`, "Upgrade authority"); storage is unchanged
    except for the new root key `07000000`, so no migration is needed. The adapter decodes the new
    call, refusals and inbox entry kind (`F-027`/`services/settle`'s SCALE mirror).

### The latest anchor in place (owner's ruling of 2026-10-11, `T-026-09`)

The ruling: until now each anchor wrote two storage items, each holding a storage deposit for good:
its `Anchored` inbox entry, which `prune_inbox` kept, and its record in `anchors[n]`, which nothing
ever removed. The owner ruled option (a), "overwrite in place", plus a migration for deployed
instances: storage and deposits must not grow with the number of anchors.

25. **One anchor in storage: the latest, overwritten in place.** `latest_anchor: Lazy<LatestAnchor,
    ManualKey<0x0000_0008>>`, `LatestAnchor { number: u64, commitment, at: u32, seq: u64 }`
    (52 bytes), rewritten by every applied anchor at the same size, so after the first anchor it
    adds no item and no deposit. `Head` is unchanged (its `last_anchor` is the latest's number): it
    is stored at `02000000` and passed between frames, so its encoding cannot change. `try_anchor`:

    - `number == last_anchor` (and not zero), same commitment: a replay, `Applied` at the original
      entry's position, nothing written, no event;
    - `number == last_anchor`, another commitment: `AnchorConflict`;
    - `number == last_anchor + 1`: stored (overwrite);
    - anything else, a gap or **a number below the latest**: `AnchorOutOfOrder`, recorded. The
      contract no longer holds an older anchor, so it can neither replay one nor tell a conflict
      from a replay. A new refusal was considered and not taken: it would change the inbox's
      encoding for every reader (the adapter's SCALE mirror, the zombienet suite) for a case no
      key of ours produces (the adapter submits only `last_anchor + 1`, and an extrinsic's bytes
      are resent under the same nonce, so a stale anchor cannot land after a later one).

    **Pruning** *(ruled by the owner on 2026-10-11, on #230, over the review's alternative of keeping
    every anchor entry)*. `prune_inbox` now removes superseded `Anchored` entries like movements; it
    keeps `Bound` entries (`InboxEvent::is_kept()`, now binds only) and the latest anchor's entry,
    which `latest_anchor.seq` points at. A prune that passes the latest's entry keeps it, and the watermark
    never goes back, so the next anchor removes the entry it supersedes when it lies below the
    watermark (one read of `latest_anchor` when anything was ever pruned). So whatever the number of
    anchors, a pruned deployment holds two cells for them: `latest_anchor` and its entry.

    **Where an earlier anchor lives.** In its `Anchored` inbox entry until the admin prunes it; in
    its `Anchored` event; and in the layer's finalized history: `latest_anchor` read at any block
    from its own up to the next anchor's. The verifier (`crates/verify`, contract venue) reads the
    latest at the finalized head and finds an older one by a binary search over finalized blocks
    for the first that holds its number or a later one, which needs a node that keeps past state. A
    node that has discarded it makes the anchor **unreadable, never missing** *(ruled by the owner on
    2026-10-11, on #230)*. Amendment 0029 (`SPEC.md` v0.4.23, ruled the same day) states it in
    `REQ-SL-1`, on the owner's assumption that readers keeping the layer's finalized history are run
    by parties other than the operator. The read `anchor_record(number)` (`0x0000_0303`) is replaced by `latest_anchor()`
    (`0x0000_0307`), a new selector so that an old caller fails to dispatch instead of decoding
    another type.

26. **The migration of a deployed instance, lazy and bounded.** A deployment is upgraded by
    `ContractsStore.upgrade`, which runs `pallet_contracts::set_code` as Root: no constructor and no
    message runs at upgrade time ("Upgrade authority"). So the new code migrates on its own, in the
    ordinary messages that follow:

    - **Trigger.** Every state-changing message a key or the admin signs, after its caller is
      checked: `execute`, `execute_with_limits`, `set_keys`, `set_withdrawal_bound`,
      `prune_inbox`. Never `deposit` or `credit`: their signers are strangers to the deployment, and
      the deposits the migration releases go to whoever signs (below). Never `apply`, which runs
      inside a batch that has already migrated.
    - **Start**, once, when no migration is recorded, `head.last_anchor > 0` and `latest_anchor` is
      absent (a fresh deployment writes `latest_anchor` with its first anchor and never starts it):
      seed `latest_anchor` from `anchors[last_anchor]`, and record `AnchorMigration { next: 1, end:
      last_anchor }` at `ManualKey<0x0000_0009>`.
    - **Step**: take (`seal_take_storage`) at most `MIGRATION_STEP` = 32 records `anchors[next..]`,
      and for each one but the latest whose entry lies below the pruning watermark, remove that
      entry too: the earlier code kept anchor entries when it pruned, and `prune_inbox` never goes
      back below its watermark, so nothing else could ever release them. Entries above the
      watermark stay for `prune_inbox`.
    - **End**: with the last record, clear the progress record (its deposit released too) and emit
      `AnchorsMigrated { through }`, an event with no inbox entry. From then on the check reads one
      absent cell (`anchor_migration`) and, if anchors exist, the size of `latest_anchor`
      (`seal_contains_storage`): about 0.08 G of ref time and 0.6 KB of proof size a message, and
      nothing is written.
    - Until the migration starts, `latest_anchor()` and `try_anchor` read the latest from its legacy
      record, so a read between the upgrade and the next message is already right.
    - **Layout.** Every existing cell keeps its key and encoding (`Head`, `inbox`, `positions`,
      `bounds`, `bound`, `registration`). The legacy map stays declared at `03000000` so its key is
      reserved and it can be read and removed; the two new cells take `08000000` and `09000000`.
      ink!'s root cell (`00000000`, the storage struct itself) encodes to nothing for a struct of
      `Lazy` and `Mapping` fields only, before and after.
    - **Who gets the deposits.** `pallet-contracts` (49.0.0, Kreivo's at `4aef055`, spec 133;
      Kreivo runs `pallet-contracts`, not `pallet-revive`) nets each call's storage changes per
      contract and refunds a net removal **to the call's origin, the transaction's signer**:
      `transfer_on_hold(StorageDepositReserve, contract → origin)`, as free balance, pro rata of the
      contract's deposit per item and per byte (`storage/meter.rs`, `update_contract` and
      `charge`). So the key that submits the batch, or the admin, receives them; the key accounts
      paid most of them in the first place. A batch that both anchors and migrates is charged its
      new entry and refunded the records it removed, net.
    - **Cost per step**, from Kreivo's weights at `4aef055` (`seal_take_storage(44)`: 15.5 M ps +
      one read and one write of `RocksDbWeight`, 297 B of proof; `seal_clear_storage(77)`: 14.2 M ps
      + one read and one write, 330 B): about 0.14 G and 0.3 KB per record, 0.28 G and 0.63 KB with
      its entry; **at most about 9 G of ref time and 20 KB of proof size per message** for a step of
      32, under a third of one withdrawal's proof size (61 KB). The start adds a read and two writes
      (about 0.3 G and 0.6 KB). Not measured on a chain: the zombienet suite should record it.
    - **Nothing is paused across the upgrade: it is made when nothing is in flight** (review of #230,
      F1). The adapter sets a batch's
      storage deposit limit from its dry run's `Charge` with a margin, zero for a `Refund`, and its gas
      from the dry run. A batch dry-run while a migration step was still to take, and included after
      another message took it, nets a charge against a limit sized for a refund and fails whole with
      `StorageDepositLimitExhausted`; a batch dry-run before the code swap and included after it pays
      for a step it was not sized for. No outside sender is needed: every lane dry-runs at the same
      finalized head, so all but the first lane's batch included can fail. Nothing executes twice (the
      failure reverts everything, and the nonce is spent), but a failed withdrawal batch is marked
      `NotExecuted(DispatchFailed)` and its operations are **released**: nothing re-signs a withdrawal
      instruction. **The owner's ruling of 2026-10-11** ("if there are changes, let's settle and
      migrate"): nothing special is paused; the upgrade is made when nothing is in flight (pending
      instructions settled, every signed transaction decided), `publish_upgrade` and `upgrade` follow,
      then at once the admin's `set_keys` (same keys) until `AnchorsMigrated`, then normal operation. *Trade-off, stated plainly:* a
      batch the adapter dry-runs between `upgrade` and the admin's `set_keys` can still fail whole,
      once; running `set_keys` at once keeps that window to a block or two. Sizing the deposit limit
      without counting refunds would close the deposit side of it; it is not done here.
    - **Every deployment that withdraws to another is upgraded in one window** (F5): a deployment
      recognises another by its code hash (`host_is_deployment`), so between two upgrades withdrawals
      between them are refused (`NotADeployment`, or `CallFailed`), as at the `T-029-10` upgrade.
    - **Never prune past the adapter's cursor** (F2). A superseded anchor's `Anchored` entry is now
      prunable, and it is the only thing that makes its checkpoint `Final` in the book (`ledger-core`'s
      `anchor_final_writes`, from the adapter's observation of it; the adapter's anchor step only
      checks `last_anchor`). Pruned unread, the range is a gap and the checkpoint can never be cited
      (`ERR-AnchorNotFinal`). This is rule 5 of "How the adapter reads outcomes at finality", which
      now also guards anchors. **The guard is the runbook's** (owner's ruling of 2026-10-11:
      the deployment's runbook, "Pruning the inbox"): no tool prunes today, and the contract cannot
      know the adapter's cursor; a tool that prunes later reads the cursor and refuses past it.

## Upgrade authority (owner's ruling of 2026-09-24)

**Ruling.** The contract's code can be upgraded by the community that owns its licence, through
Kreivo's contracts store. That community must be governed by Kreivo Communities' own
membership-based governance — its members, its decision method, its track and its referenda, from
`pallet-communities` — and that must be set up **before the contract holds any real value**. The
contract itself has no governance code; this section says who can change it, read at Kreivo
`4aef055` with `fc-pallet-communities` and `fc-pallet-referenda-tracks` 2.2.0 and `pallet-referenda`
49.0.0.

**Publisher = deployer = upgrader (owner, 2026-09-24).** Kreivo's contracts store is permissioned.
The deploying community publishes the contract, instantiates it and holds its licence, so it alone can
upgrade it. The contract never runs code published by a third-party community. The upgrade risk is
then that community's own governance: the admin-key handover and the voting track (checklist). An
upstream fix to the communities pallet, in a Kreivo release enacted on mainnet, is a precondition of
handover.

**Who can change the code, and how.**

- **The licence owner upgrades.** `ContractsStore::upgrade(app_id, license_id)` requires
  `InstantiateOrigin` (`EnsureCommunity`, mapped to the community's account) and that account to
  own the licence item; it then runs `Contracts::set_code` as Root on the instance, to the app's
  **current** code hash (`pallets/contracts-store/src/lib.rs`, `upgrade`). It takes no hash: a vote
  to upgrade installs whatever the app's latest code is when it is enacted.
- **The publisher chooses that code.** `ContractsStore::publish_upgrade(app_id, code)` requires
  `UploadOrigin` (`EnsureCommunity`) and the caller to be the app's publisher; it only replaces the
  app's code hash. It cannot touch an instance by itself, but it decides what the licence owner's
  next `upgrade` installs.
- **`EnsureCommunity` accepts two kinds of origin** (`pallet_communities::origin`): the community's
  own origin (`Communities(RawOrigin { community_id, subset })`, which a passed referendum
  dispatches with), and any origin registered in `CommunityIdFor` as the community's **admin
  origin**. `CommunitiesManager::register` registers `Signed(first_admin)` as that admin origin.
- **So the handover comes before value.** Whoever can upgrade the code can move everything the
  contract holds, without its keys and past its withdrawal bounds: that is the real ceiling of `NFR-8`
  in this venue. As registered, the first admin's signed origin acts for the community without a
  vote, so the handover below is a step of the deployment checklist, before the contract holds value.
- **Only the admin key can hand over.** `Communities::set_admin_origin(new)` requires the caller to
  be the community's current admin origin; it removes that origin from `CommunityIdFor` and
  registers `new`. Setting `new` to the community's own origin (`Communities(RawOrigin {
  community_id, subset: None })`) removes the single-key path: from then on `upgrade`,
  `publish_upgrade`, member management and `set_admin_origin` itself need a passed referendum.
  A referendum cannot do the handover, since its origin is not the admin origin until then.

**How the community then decides** (`pallet-communities`, `CommunityReferenda` =
`pallet_referenda<Instance2>`, `CommunityTracks` = `pallet_referenda_tracks<Instance2>`):

- **Members** hold a membership (an NFT of the community's collection, `add_member` /
  `remove_member`, ranks 0–100 with `promote` / `demote`). Only a member can submit a referendum
  on the community's track (`SubmitOrigin` is `EnsureCommunityMember`) and vote on it (`vote`, one
  vote per membership, replaceable until the poll ends).
- **Decision method** (`set_decision_method`): `Membership` (one member, one vote; the default),
  `Rank` (each aye weighs the member's rank, so rank 0 counts for nothing), `NativeToken` or
  `CommunityAsset(asset, min)` (votes weigh locked balance). Approval is ayes over ayes plus nays;
  support is the bare ayes over the method's total (members, ranks, or issuance).
- **Track.** `register` inserts one track per community, keyed by its origin. Its default
  (`CommunitiesManager::default_tack`) lets one deciding referendum at a time, a zero decision
  deposit, a decision period of 255 relay-chain blocks (about 25 minutes), and approval falling from
  100% to 50% and support from 50% to 0% over it: **one aye and no nays passes by the end.** The
  track is changed by `CommunityTracks::update`, whose origin must be the community's own origin
  (a passed referendum); only Kreivo's Root can insert or remove tracks.
- **A referendum** is `CommunityReferenda::submit(Communities(RawOrigin { community_id, None }),
  proposal, enactment)` by a member, then `place_decision_deposit`, then votes; once it passes, the
  scheduler dispatches the proposal with the community origin. The periods count relay-chain
  blocks.

**What remains outside the community's governance, stated plainly:**

- **Kreivo's Root** can `Contracts::set_code` any contract directly, dispatch as the community's
  origin (`Utility::dispatch_as`), cancel or kill referenda, and insert or remove tracks. The
  contract is only as safe as Kreivo's own governance.
- **The publisher, if it is another community**, decides what an approved `upgrade` installs. The
  same community should publish and hold the licence; the scripts do (`publish`, `license` and
  `instantiate` all run as `COMMUNITY_ID`). A single referendum can carry both calls in one
  `Utility::batch_all`, so that the code voted on is the code installed.
- **The contract cannot change its own code**: it never calls `set_code_hash`.
- **Liveness**: a community with no members after the handover can never decide anything again.

**Deployment checklist, before the contract holds real value** (also in the contract's README):

1. Register the community with `CommunitiesManager::register`; the first admin is a key held under
   the same custody as the contract's keys.
2. Give it its governing members (`add_member`; `promote` if the method is `Rank`), and set the
   decision method (`set_decision_method`).
3. Publish, license and instantiate from that same community (the driving script's `community`,
   `publish`, `license`, `instantiate` steps).
4. Hand over: the admin key calls `Communities::set_admin_origin(Communities(RawOrigin {
   community_id, subset: None }))`. Check that `Communities::CommunityIdFor` no longer maps the
   admin's signed origin, and that a dry run of `ContractsStore::upgrade` signed by the old admin
   key fails with `BadOrigin`.
5. Through a first referendum, replace the default track (`CommunityTracks::update`) with periods,
   deposit and curves fit to hold real value: the default passes with one aye in about 25 minutes.
6. Record the community id, its members, its decision method and its track in the deployment's
   records, and re-check the decision method before each upgrade vote.

The driving script stops at step 3: steps 2, 4 and 5 are governance acts for the community's
members, not for a test script, and a zombienet or testnet run may leave them out.

## Storage layout

One contract instance, one child trie. `pallet-contracts` keeps the trie id in
`Contracts::ContractInfoOf[contract].trie_id`; the child trie is `:child_storage:default:` ++
`trie_id`. ink! 5 writes each item under a variable-length key `k`, and `pallet-contracts` stores it
at **`blake2_128(k) ++ k`** (`Blake2_128Concat`). Every root key is fixed with `ManualKey`, so the
layout survives code upgrades and a reader needs no metadata.

| Item | `k` | Value (SCALE) | Written by |
|---|---|---|---|
| `registration` | `01000000` | `Registration { admin, keys: Vec<AccountId>, attestation_key: Option<[u8; 32]> }` | constructor, `set_keys` |
| `head` | `02000000` | `Head { last_anchor: u64, next_seq: u64, pruned_to: u64 }` | every entry, `prune_inbox` |
| `anchors[n]` (legacy) | `03000000` ++ `n: u64 LE` | `AnchorRecord { commitment, at: u32, seq: u64 }` | the code before 2026-10-11, at each anchor; never written now, removed by the migration (decision 26) |
| `inbox[seq]` | `04000000` ++ `seq: u64 LE` | `InboxEntry { at: u32, event: InboxEvent }` | every movement, anchor and refusal |
| `positions[asset]` | `05000000` ++ `FungibleAssetLocation` | `u128` | deposits, credits, withdrawals; removed at zero |
| `bounds[asset]` | `06000000` ++ `FungibleAssetLocation` | `WithdrawalWindow { bound: { amount: u128, period: u32 }, started: u32, spent: u128 }` | `set_withdrawal_bound`, applied withdrawals |
| `bound[asset]` | `07000000` ++ `FungibleAssetLocation` | `u64` (the inbox position of the asset's `Bound` entry) | an applied bind (decision 24); never removed |
| `latest_anchor` | `08000000` | `LatestAnchor { number: u64, commitment, at: u32, seq: u64 }` | an applied anchor, overwriting it in place; the migration's start (decision 25) |
| `anchor_migration` | `09000000` | `AnchorMigration { next: u64, end: u64 }` | the migration, while it runs; removed when it ends (decision 26) |

`InboxEvent` is `Deposited { from, account_ref, asset, amount }` (0), `Withdrawn { operation_ref,
to, asset, amount }` (1), `Anchored { operation_ref, number, commitment }` (2), `Refused {
operation_ref, call, reason }` (3), `Bound { operation_ref, asset, amount }` (4). `Bound` entries
and the latest anchor's entry are never pruned; earlier anchor entries are (decision 25). Nothing is
keyed by an account reference, an operation reference, or anything a holder supplies (`REQ-SL-5`,
`REQ-SL-10`): the long-run test counts the contract's storage cells after every step and finds
exactly two, plus one if any anchor was stored (however many), plus one per unpruned entry, per
asset with a position, per withdrawal bound and per bound asset, however many distinct references
went by.

## How the adapter reads outcomes at finality (for `F-027`)

**Read storage, by position, at a finalized block. Use events only as a wake-up.**

1. At a finalized block `B`, read `ContractInfoOf[contract]` (main trie) for the trie id, then
   `head` from the child trie. Both come with proofs against `B`'s state root; smoldot serves child
   tries (`chainHead_v1_storage` takes a `childTrie`).
2. Read `inbox[seq]` for every `seq` from the adapter's cursor to `head.next_seq - 1`. The inbox is
   gapless (`REQ-SL-9`), so the range read is complete by construction (`REQ-SA-7`). The
   coordinate is `(contract, seq, hash of the entry)`, as `F-016` used `(instance, seq, hash)`.
3. Each `Withdrawn`, `Anchored`, `Bound` and `Refused` entry carries the operation reference: the outcome is
   found by identity (`REQ-SL-8`, `REQ-SO-3`). `Refused` is "provably not executed" for that call
   (`REQ-SA-4`). An anchor replay (of the latest only, decision 25) has no new entry; its outcome
   is `latest_anchor`, whose `seq` points at the original.
4. A batch that failed as a whole (not a key, empty, too large, or its own exhaustion: decision
   23) writes nothing and shows as `System.ExtrinsicFailed` with `Contracts.ContractReverted`,
   `ContractTrapped`, `OutOfGas` or `StorageDepositLimitExhausted`: every call in it is provably not
   executed. A batch never included before its mortality ends is the same. A single call that
   traps or runs out is not a batch failure: it is a `Refused` entry, `CallFailed`, like any other
   refusal.
5. **Pruning must not pass the adapter's finalized cursor.** `prune_inbox` is the admin's; the
   adapter's re-scan (`REQ-IN-5`) can only read what is still there. `F-027` should own when the
   admin prunes.

Why not events: `Contracts.ContractEmitted { contract, data }` (topics: the event signature and, for
`Withdrawn`, `Anchored` and `Refused`, the operation reference) sits in `System::Events`, one value
rewritten every block. Reading it needs that block's own state, which full nodes prune; it cannot
be read as a range from a cursor; and it adds nothing the inbox does not have (`Deposited` events
carry no `from`). It is a good trigger to read storage, and a good trace. The plan's open point
(which is more reliable at finality) is settled by the zombienet run: the script's `read` step
compares the contract query with the raw child-trie read at the finalized block, and `events` shows
the same outcomes from events.

## Tests

`cargo test` in `contracts/settlement`: **104 tests**, all passing (93 before `T-026-09`, 84 before `T-029-10`, 71 before `T-026-08`, 66 in the first version; the
follow-up replaced the three approval tests, renamed three refusal tests and added eight). They run
in ink!'s off-chain environment; `src/mock.rs` stands in for what it cannot do (runtime calls, code
hashes, calls to other contracts) with a model of `pallet-assets` 53 (minimum balances, frozen
assets, `prep_debit`'s dust rule for the expendable `transfer`, `Preserve` for the extension's
transfers), Kreivo's chain extension registered with ink!'s engine (`balance`, `deposit`,
`transfer`, decoding each input with Kreivo's own asset type and answering with `kreivo-apis`' own
status codes), and other deployments run against their own storage. The contract's extension calls
and runtime calls reach the model as bytes, so the encoding is exercised on every transfer.

| Covers | Tests |
|---|---|
| Every message | `new`, `execute`, `deposit`, `credit`, `set_keys`, `set_withdrawal_bound`, `prune_inbox`, every read |
| Every refusal (`REQ-SA-4`) | `refusal_*`: each `Refusal` variant, each checked to record the call under its reference, emit one event with the reference as topic, and move nothing |
| Every message error | each `Error` variant the message can return |
| `T-026-02` | `t_026_02_a_batch_with_one_refused_call_applies_the_rest`, `consecutive_anchors_in_one_batch`, `batch_emits_one_event_per_call_with_the_reference_as_topic` |
| `T-026-03`, `T-026-05` (deposits) | `t_026_03_deposit_lands_with_its_account_reference_in_the_inbox`, `deposit_pulls_only_from_its_caller`, `deposit_keeps_the_depositor_alive`, `deposit_beyond_the_callers_balance_is_refused_with_kreivos_reason_and_records_nothing`, `deposit_refuses_an_amount_kreivo_did_not_move_as_asked` |
| `T-026-05` (withdrawals) | `req_sl_8_withdrawal_to_an_account_is_recorded_under_its_operation_reference` (the extension's `transfer`), `a_position_can_leave_whole` (`call_runtime`), `a_position_never_leaves_dust`, `refusal_kreivo_*` (Kreivo's reason recorded), `refusal_transfer_failed_for_a_whole_position`, `a_withdrawal_kreivo_did_not_move_as_asked_traps` |
| `REQ-SL-1`, `INV-19` | `req_sl_1_anchors_are_stored_in_order_from_one`, `inv_19_the_stored_commitment_is_the_submitted_one`, `anchor_replay_of_the_latest_writes_nothing_and_points_at_the_original`, `refusal_anchor_conflict`, `refusal_anchor_out_of_order` (gaps), `refusal_anchor_older_than_the_latest` |
| `T-026-09` (latest anchor in place, migration) | `anchoring_adds_no_storage_item_but_its_prunable_entry` (cells and bytes per anchor: one item, 77 B; the latest rewritten at 52 B), `storage_deposit_per_anchor_before_and_after` (2 items and 121 B held for good, 13 373 666 613; now 1 item and 77 B until pruned, 6 692 333 301), `prune_inbox_keeps_only_the_latest_anchors_entry`, `a_superseded_anchor_entry_below_the_watermark_goes_with_the_next_anchor`, `the_root_keys_of_the_new_cells_are_fixed`, `the_earlier_layout_reads_unchanged_under_the_new_code` (the earlier layout written by raw key and raw SCALE tuples, read by the new code), `the_migration_runs_in_bounded_steps_and_then_never_again` (69 legacy anchors, three messages of three kinds, the latest kept throughout, entries below the watermark removed, one `AnchorsMigrated`, then every trigger a no-op), `anchor_refusals_hold_on_an_upgraded_deployment`, `the_migration_waits_for_a_key_or_the_admin` (not in `deposit`, `credit` or a refused message), `the_migration_keeps_the_latests_entry_below_the_watermark`, `a_deployment_with_no_anchors_has_nothing_to_migrate`. The off-chain engine (ink! 5.1.1) does not drop a cell from `count_used_storage_cells` when `take_storage` removes it, so the migration tests count cells by key |
| `REQ-SL-6` | `req_sl_6_withdrawal_to_another_deployment_writes_both_inboxes` |
| `REQ-SL-8` | `req_sl_8_withdrawal_to_an_account_is_recorded_under_its_operation_reference` |
| `REQ-SL-5`, `REQ-SL-9`, `REQ-SL-10` | `req_sl_9_req_sl_5_invariants_hold_over_a_long_run`: 400 pseudo-random steps; after each, the inbox is gapless, anchors are ordered, each outcome points at its entry, positions equal the account's holding less what an issuer placed there and has not yet bound, every bound asset points at its `Bound` entry, and the storage-cell audit holds |
| `NFR-8` | bound window restart, refused withdrawals use nothing, per-asset bounds, changing and removing bounds |
| `T-026-08` (per-call frames) | `t_026_08_*`: a frame that traps (Kreivo misreports; the receiver refuses the credit), one that runs out after the transfer, one that runs out after the receiver recorded the value, one given less than it needs, a batch short of weight, limits from `execute_with_limits` (plus an eighth; a call given too little runs out alone), `apply` refused to anyone but the contract; each checks the other calls applied and the failed call's entry and event carry its reference. `a_withdrawal_kreivo_did_not_move_as_asked_traps_its_frame` replaces the test that expected the whole batch to trap |
| `REQ-AS-8`, `REQ-SL-3` (`T-029-10`, binding) | `req_as_8_*`: a bind at zero divergence sets the position and writes a `Bound` entry, moving nothing; a holding one unit off either way is refused with `HoldingMismatch` and changes nothing but the inbox; a second bind is `AlreadyBound`; a bind counts the position already recorded; an overflowing amount is refused; a bind of zero; a bound asset is withdrawn like any other; a bind runs inline in its batch; `Bound` entries survive `prune_inbox`. The long run binds both assets (some binds one unit off, the rest after the first `AlreadyBound`), and `envelopes_and_outcomes_keep_their_encoding` pins the new indices |
| Kreivo encoding | `asset_id_encodes_as_kreivos_fungible_asset_location` (against `virto-common` at `4aef055`), `kreivo_api_calls_encode_as_kreivo_decodes_them` (the extension's ids and input bytes, read with Kreivo's types), `kreivo_api_errors_decode_as_kreivo_encodes_them` (against `kreivo-apis` at `4aef055`), `runtime_calls_encode_as_kreivo_decodes_them`, `envelopes_and_outcomes_keep_their_encoding`, `credit_selector_dispatches_to_credit` |

**No ink! e2e tests.** They need a contracts node binary, and none is installed here; downloading
one was not in this session's remit. More to the point, a generic `substrate-contracts-node` cannot
run what matters: it has no `KreivoChainExtensions` (reading the balance traps, so every withdrawal
traps), its `Assets` pallet sits at another index with `u32` asset ids (every `call_runtime` the
contract encodes is undecodable there), and its call filter and contracts store differ. It could
cover registration, keys, anchors, pruning and reads, which the unit tests already cover, and would
give false confidence about value movement. The zombienet run is the e2e test.

## The first real run: zombienet (T-026-04)

Run by `.github/workflows/e2e-zombienet.yml` on the self-hosted `zombienet` fleet, 2026-09-29,
run 36526889582 (`e2e/README.md`
says what each phase does). The network: `rococo-local` and Kreivo from the **0.16.9 release**
(`kreivo-parachain` spec 132, transaction version 12, wasm sha256 `e0870211…`), collators from
`polkadot-stable2603-4`. **Kreivo mainnet runs the same runtime** (spec 132, transaction version
12, read from `kreivo.io` during the run), so this is the runtime a mainnet deployment targets. Bring-up took 175 s on the fleet, the whole job about 21 minutes.

**It ran with the steps a mainnet deployment runs**, `drive.mjs`'s own, in the README's order:
`preflight`, `community` (finding the community and funding its account), `publish`, `license`,
`instantiate`, `bound`. The community (id 4242 here) was registered first, since a
fresh network has none. Then `drive.mjs`'s `deposit`, `execute`, `read` and `events`, and the suite's
own cases. Every case passed, 46 inbox entries in all; one fault case is pending (below).

**The run found a bug in the deployment script.** `instantiate` passed the salt to
`ContractsStore.instantiate` as a `Uint8Array`, which `@polkadot/api` reads as length-prefixed
`Bytes`: the call could not even be built. It now goes as hex (the constructor data was right: the
ABI's `toU8a` already prefixes it). A mainnet deployment would have failed at this step. The
script's dependencies also mixed `@polkadot/util` 13 with the API's 14; they are aligned now.

### Weights and storage deposits

From the dry runs and the inclusion events. Amounts in Kreivo's native unit, 12 decimals (KSM).
`DepositPerItem` is 6 666 666 660 and `DepositPerByte` 333 333.

| Message | Dry-run ref time | Dry-run proof size | Included weight (ref time, proof size) | Storage deposit | Fee |
|---|---|---|---|---|---|
| `deposit` (one inbox entry, existing position) | 1 284 028 231 | 119 920 B | 2 847 388 231, 130 991 B | 6 696 666 630 (0.0067) | 5 068 526 701 (0.0051) |
| `execute`, 32 withdrawals (`MAX_CALLS`) | 34 062 459 742 | 2 054 873 B | 35 625 819 742, 2 065 944 B | 214 303 998 816 (0.214) | 78 587 388 622 (0.0786) |

- **Proof size is the limit, not time.** A full batch uses 2.4 % of a normal extrinsic's ref time
  and **56 % of its proof size** (3 670 016 B). `MAX_CALLS` = 32 fits with less than twice the
  headroom; about 57 withdrawals would not fit in one extrinsic. *(Kreivo `0.17.0` allows an extrinsic
  2.45 MB of proof size, and a withdrawal to another deployment in its own frame costs 235 KB:
  batches are sized by their dry run's proof size, decision 23.)* `F-027`'s batching per block should
  size batches by proof size.
- **Each inbox entry costs its writer about 0.0067 KSM of storage deposit** (one item plus about
  90 bytes), held on the contract's account and charged to the transaction's signer: the depositor
  for a deposit, the key for a batch (0.214 KSM for 32 entries). A new position cell costs another
  item: the first deposit of an asset holds about twice as much. Pruning releases it to the pruning
  admin. The key's account must hold that deposit plus fees; this is a funding requirement for any
  deployment.

### Events versus storage at finality

**Storage, as decided above ("How the adapter reads outcomes at finality").** At the finalized head,
every entry read from the child trie by key equals the contract's own query (`drive.mjs read`), and
all 46 entries have their event (`Contracts.ContractEmitted`, decoded, same kind and `seq`) in the
block the entry records. So the two agree; what differs is how long each can be read:

- Through `kreivo-pruned`, a full node keeping 16 blocks of state, the events of the oldest entry's
  block (#47) were gone by its finalized #101 ("State already discarded"), while the whole inbox
  (44 entries) read at its finalized head matched the collator's byte for byte.
- A node with default pruning keeps 256 blocks of state. An
  adapter that reads events must never fall further behind than that; one that reads the inbox by
  position from its cursor at the finalized head can fall behind by any amount. Events stay a
  wake-up signal only.

### Kreivo's assets API, observed

- **Deposit** through the chain extension, with no approval: the amount moves from the caller and
  the entry records the caller as `from` and the account reference. A deposit of the depositor's
  whole balance is refused: the message returns `DepositRefused(Assets(CannotDeposit))` and
  reverts, so nothing is recorded (decision 14, `Preserve`).
- **Withdrawal** through the extension's `transfer`: `Assets.Transferred` from the contract's
  account, the payee credited exactly the amount.
- **A withdrawal Kreivo refuses** (the contract's asset account frozen by the asset's freezer) is
  recorded under its operation reference as `Refused` with reason `Kreivo(Assets(CannotTransfer))`,
  and the rest of its batch applies (decision 19).
- **A withdrawal past the bound** is recorded as `Refused` with `BoundExceeded`, in the same batch as
  an applied withdrawal and an anchor (`T-026-02`).
- **A non-sufficient asset reaches the contract without an asset-account deposit**: the contract's
  asset account exists with reason `Consumer` (it rides on the native account created at
  instantiation), and the only event is `Assets.Transferred` (`F-019` open point 5). The native
  amount reserved on the contract grew by 13 368 666 618 on that deposit: that is the contract's
  storage deposit for two new cells (the entry and the new position), not an asset deposit.
- **`PassAuthenticate` needs nothing more than `None`**: every signed extrinsic of the run, from
  both signers (//Alice and //Bob), was accepted with the extension declared as `Option<Bytes>`. Mainnet declares the
  same extensions, in the same order.
- **Not exercised:** a withdrawal of the contract's whole holding, which goes through `call_runtime`
  (`Assets::transfer`); the suite always leaves a position behind.

### Fault injection with the script driver (T-027-06, first part)

Each case checks both proofs of "not executed" (the owner's ruling of 2026-09-24): no inbox entry
for the reference at finality, and the key's nonce unconsumed.

- **Connection dropped mid-submit**, after the pool accepted the extrinsic and before any answer:
  both executed exactly once, found by reference at finality with the nonce consumed; the same bytes
  resubmitted are refused (`1012: Transaction is temporarily banned`, the pool's answer for an
  extrinsic it has just included).
- **Submitter killed between signing and submitting** (a child process signs, writes the bytes and
  dies by SIGKILL): both proofs hold, **and the bytes still apply** (a dry run at the best block
  succeeds). "Not executed" is not final at that point: whoever holds the bytes can still land them.
  It became final only once the key's nonce was consumed by another extrinsic (a remark); the bytes
  were then refused as outdated and never landed. This is why the nonce is one of the two proofs:
  the adapter must consume the nonce (or let the extrinsic's mortality pass) before it releases.
- **Pruned node state**: as above.
- **A deposit in the same gap as a withdrawal**, from two signers submitted together: both landed in
  the same block; one range read from the cursor at finality returned exactly the two entries, and
  the position moved by both.
- **Starving a light client of peers**: pending. The script driver has no light client; it needs the
  aledgent driver (services/settle over smoldot, `F-027`).

### Against Kreivo main (0.17), and through aledgent

`F-027` (#92) landed while the suite was being built, so step 3 now has its aledgent driver
(`e2e/aledgent-driver`, `e2e/README.md`), and the suite can build Kreivo's runtime from a commit
(`kreivo_ref`) for a runtime with no release yet. Runs, 2026-09-29:

| Run | Kreivo | Driver | Result |
|---|---|---|---|
| run 36546310620 | 0.16.9 release (spec 132, mainnet's) | script | green |
| run 36552362545 | built from `1b849b3` (main, with kreivo#501; spec 133), `polkadot-stable2606-2` | aledgent, over the collator's RPC | green |
| run 36541928029 | the same | aledgent, over the light client | the deposit is credited; the withdrawal is never signed (below) |
| run 36564643083 | 0.16.9 release, with the adapter signing for spec 132 (#94) | aledgent, over the collator's RPC | green |
| run 36552359085, run 36556566367 | the same | the same | the refused withdrawal's batch was dropped (point 6) |

**Through aledgent, over RPC, step 3 passes end to end, on Kreivo main and on 0.16.9.** Through ingress and the
in-process core, with the adapter as the community PASS account: a customer's deposit to alice's
account reference is credited at finality; a withdrawal is settled (33 to 46 s from submission); a
withdrawal paid to the contract itself is refused with `SelfTransfer`, reported in those words,
and nothing is released (alice keeps it reserved); the core's checkpoint is anchored and final.
`e2e.mjs check` then finds each of those in the inbox at the finalized head, with its event, and
the position equal to the contract's holding. The fault cases then run as with the script driver,
all passing on both runtimes.

Findings, each for `F-027` unless it says otherwise:

1. **The adapter now signs for mainnet's runtime, and the suite confirms it.** Kreivo 0.16.9, which
   mainnet runs, verifies a PASS device's signature over `context ‖ authority ‖ challenge`
   (`pass-authenticators-substrate-keys` 0.1.0); Kreivo main, over `context ‖ challenge ‖ authority`
   (1.x). The suite's PASS registration tries both and records which one the runtime took: 0.1.0's
   on 0.16.9, 1.x's on main. #94 (T-027-07, merged during this work) made the adapter sign in the
   format of the runtime it finds; on 0.16.9 its batches were then accepted and step 3 passed
   (run 36564643083).
2. **A full batch does not fit the adapter's gas limit.** `CallLimits::default()` gives a batch
   1 000 000 bytes of proof size; 32 withdrawals need 2 054 873 (above, "Weights and storage
   deposits"), about 60 KB each. A batch of more than about 15 withdrawals would run out of gas and
   revert whole, sending every call in it to Needs-Review. Raise the limit to
   about 3 MB (a normal extrinsic allows 3 670 016) or cap batches by proof size. Relatedly, a batch
   dry-run against a state where its withdrawal would be refused prices the cheap path: when a
   deposit landed first in the same block, the script driver's batch ran out of gas. The suite
   sends half again the dry run's gas.
3. **Kreivo 0.17 reorganises short forks often** (several parachain blocks per relay-chain slot,
   kreivo#492: reorganisations of up to six blocks on the fleet). A transaction whose mortal era is
   born at the best block turns invalid when that block is retracted: `drive.mjs`'s first
   transactions on 0.17 never became final. It now signs with the era born at the finalized head,
   settles by the signer's nonce at finality, and signs again only with the same nonce. The adapter
   still births its era at the best block (`Mortality::within(best.number, …)`), which on 0.17 can
   leave a batch invalid for no fault of its own.
4. **Over the light client the adapter cannot sign.** It resolves its era's birth block hash by
   number (`chain_getBlockHash`), which the embedded smoldot does not answer ("no block N" for
   every era it tried, 24 blocks apart, until the step's deadline). Birth at the finalized head,
   read by a hash the light client has pinned, would fix this and point 3 together. Until then the
   aledgent driver runs over RPC (`aledgent_connection=rpc`), and "starve the light client of
   peers" stays pending.
5. **smoldot-light 0.17.2 panicked** (`network_service.rs:1277`, `event_pending_send.is_none()`)
   when its chain specs named two boot nodes per chain on the local network; with one per chain it
   connected in 24 s and served the deposit. Not reproduced on purpose; worth watching against
   mainnet's many peers.

6. **Two of three runs on 0.16.9 lost the refused withdrawal's batch.** The pool took it
   ("Transaction Already Imported" on every rebroadcast), then dropped it without including it
   ("temporarily banned"); its era ended with the PASS account's nonce unconsumed, and the adapter
   declared it not executed, with both proofs, as it should. The refusal the step waits for never
   happened, so the step failed after ten minutes. Point 3 fits: the adapter births the era at the
   best block, and a batch whose birth block a reorganisation retracts turns invalid. The collator
   now logs its pool at debug level (`-l txpool=debug`, kept with the logs of a failed run) to say
   so the next time. Until the adapter births at the finalized head, the aledgent driver can fail
   this way on either runtime; the script driver, which does, has not.

**Fixed in #96** (`F-027` NOTES, "The first zombienet runs' findings, fixed", decisions 25 to 41):
finding 2 (batches sized and priced by their dry run, storage deposit included), findings 3, 4
and 6 (eras born at the finalized head, nothing looked up by number). Finding 5 is upstream: the
same smoldot panic came back with **one** boot node per chain, among other smoldot panics, so the
adapter now aborts on any panic and is restarted over its store. Those runs also found that
smoldot cannot dry-run `execute` (first read as call proofs lacking `System::EventTopics`; corrected
in `F-027` NOTES decision 42: smoldot recalculates the contract's child-trie root after the call):
dry runs go through a node's RPC (`dry_run_endpoint`). The runs, over the light client on both runtimes, with the
adapter as its own process and every fault case through it, are in `F-027` NOTES.

### Still not done

- **The Kreivo testnet.** Replaced, for the canary, by Kreivo mainnet with a dedicated test
  community (the owner, 2026-09-29). The suite gates it: a release tag fails unless the runtime under
  test is the one mainnet runs.

## Open points for the owner

*Closed by the rulings of 2026-09-24:* who can change the code (the licence-owning community, under
`pallet-communities` governance, before real value: "Upgrade authority"); refusal text (Kreivo's
reason, decision 19, and the V0 behaviour of decision 22); depositor keep-alive (decision 14).

- **Inbox growth by strangers** (`F-019`'s open point) is now priced: each entry costs its writer a
  storage deposit (`DepositPerItem` plus per byte), charged to the transaction's origin — the
  depositor for a deposit. Pruning refunds it to the origin of the pruning transaction, the admin.
- **The contract's review** before it holds real value (plan open point). A `cargo contract build
  --verifiable` build gives reviewers a reproducible code hash to compare with the store's.
