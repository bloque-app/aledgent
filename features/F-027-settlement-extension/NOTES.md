<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-027 — notes

Facts recorded before implementation. Decisions taken while implementing go under their own heading when
the work starts.

## Rulings and facts

- **V0 transactions are v5 general transactions authorised by Kreivo's `PassAuthenticate` extension; they
  bump the PASS account's ordinary nonce** (owner, 2026-09-24). So the nonce the settlement adapter reads
  at the finalised head, for the owner's ruling of the same day that a release needs the signing key's
  nonce unconsumed (`F-010` decisions 29 to 35, `F-016` decisions 30 to 34, #88), is the PASS account's
  ordinary `System::Account(who).nonce`.
- **Reference implementation for the signing and the transaction extensions** (owner, 2026-09-24):
  a previous chain operator's PASS signer and its Kreivo `Config`. An owner-sanctioned exception to `CLAUDE.md`'s
  rule on the replaced services, limited to those mechanics; see the plan's Scope.

## Implementation notes (T-027-01 to T-027-04, issue #76, 2026-09-28)

Written while implementing `T-027-01` to `T-027-04` on top of `F-016`'s adapter, `F-026`'s contract and
`#89`'s signing record. `T-027-05` (the move into a VOS host extension) waits on upstream VOS, and
`T-027-06` (the zombienet fault-injection suite) is separate work. Where the plan was silent, the more
conservative behaviour was chosen; each such choice is listed here for review.

### What landed

| Where | What |
|---|---|
| `crates/ledger-core/src/settlement/envelope.rs` | `T-027-01`: the call envelope (`CallEnvelope`: reference, target, call, expiry), its references, the call outbox entry |
| `crates/ledger-core/src/ledger/settle_adapter.rs` | `Ledger::call_outbox`, `Ledger::adapter_calls` |
| `services/ingress` (`settlement.rs`, `boundary.rs`, `surface.rs`, `inprocess.rs`, `openapi.*`) | `GET /v1/settlement/calls` (`CallBody`), the adapter's principal only; `EventKindBody::AnchorRefused`; an anchor fact's venue |
| `services/settle/src/chain/kreivo.rs` | `T-027-02`, `T-027-03`: Kreivo at `4aef055` in plain SCALE — the contract's types and selectors, `Contracts::call`, `ContractsApi_call`, and the v5 general transaction authorised by `PassAuthenticate` |
| `services/settle/src/chain/fake.rs` | The fake layer now runs the contract (see "Tests") |
| `services/settle/src/chain/rpc.rs` | The contract venue over the light client (reads through `ContractsApi_call`, see decision 8) |
| `services/settle/src/adapter.rs` | Intake from the call outbox; batches; per-batch tracking; `AnchorRefused` observations; anchor venues |
| `services/settle/src/signer.rs` | `PassAccount`: a device key behind `Signer`, acting as a community PASS account |
| `services/settle/src/store/` | `store_batch` (all or nothing), `ExtrinsicRow::batch`, `NotExecuted::CallRefused`; `migrations/0003_batches.sql` |
| `services/settle/kreivo-pin/` | `T-027-03`: pins the transaction to Kreivo's crates (its own workspace, `vectors.json`) |
| `crates/verify` | `T-027-04`: `Venue`; the disclosure envelope's version 2 names it; the node read of the contract venue |
| `crates/ledger-core` (`anchor`, `export`, `ledger/anchoring.rs`, `ledger/disclosure.rs`) | `T-027-04`: an anchor fact's venue; a checkpoint's venue at finality; export format 5 |

### Decisions taken during implementation

**The call envelope and the outbox (`T-027-01`)**

1. **The envelope is `{ reference, target, call, expiry }`** (`CallEnvelope`). The reference is what the
   layer records the outcome under: for a withdrawal, the operation's 16 bytes and 16 zero bytes (the form
   `F-016` has used since its first version); for an anchor, `blake2b-256("aledgent/anchor-reference/v1" ‖
   instance ‖ number)`, the same for every emission of the anchor and naming no operation. The target is
   the deployment's instance. The call is `Withdraw { asset, amount, to }` or `Anchor { number,
   commitment }`, nothing else (`INV-17`). The expiry is the withdrawal's; **an anchor has none**: its
   transaction is still mortal, and one that provably did not store it is emitted again (`F-017`).
2. **One outbox, one read.** `GET /v1/settlement/calls` lists every call the executor has not taken over:
   the withdrawal instructions it has not acknowledged, then the checkpoint in flight until final, with
   whether it is acknowledged. The envelope is built only from an `OutboundInstruction` (which only a
   reservation yields, `INV-13`) or a checkpoint's `AnchorInstruction`. A withdrawal's instruction
   identity is recomputed from the envelope alone (`CallEnvelope::instruction`); the executor refuses a
   call whose envelope and identity disagree, as `F-016` refused an instruction. `GET
   /v1/settlement/instructions` and the anchor read stay: the latter still says whether a settlement
   comparison is due.

**The venue (`T-027-02`)**

3. **The venue is the contract; the pallet path is not kept.** The adapter spoke to `F-019`'s pallet with
   one extrinsic per operation; the contract takes batches, records refusals as inbox entries, and moves
   value through Kreivo's assets API. Keeping both behind configuration would have meant two executors
   (per-operation and batched), two decoders and two fakes. It was not cheap, so it was not done: the
   pallet stays built and tested in `pallets/` (`DEC-17`), and a later venue (Spaces) is a new
   implementation of `Chain`, not a flag. The deployment's instance number stays the book's (the
   coordinate's `instance`); the configuration binds it to the contract's account
   (`ProcessConfig::deployments`), together with every deployment a withdrawal may pay into
   (`Destination::Instance` → the contract's `Deployment { contract, account_ref }`).
4. **Batches.** Each step, each free lane takes up to 32 ready withdrawals (`MAX_CALLS`) into one
   `execute` transaction: one nonce, one mortal era, one signed body. The era ends at or before the
   earliest expiry among them. A lane still has one transaction in flight. The store keeps **one row per
   operation**, sharing the transaction's bytes, lane and nonce, written all or nothing
   (`Store::store_batch`), so `REQ-SO-7`'s "one transaction per operation reference, ever" is still the
   primary key it was. `settle_extrinsics.hash` is no longer unique (migration 3); a trigger keeps the rows
   of one transaction on one lane and nonce.
5. **Anchors keep their own key** (`F-017`: its nonces are the anchors' alone) and go out as a batch of
   one on it, through the same envelope and the same `execute`. Mixing them into the lanes' batches would
   have tied an anchor's fate to a withdrawal batch's (a revert of one sends both to their proofs).
6. **Liquidity** is what the contract will pay: the smaller of the contract account's holding and the
   position the contract accounts for (`F-026` decision 2), less what is signed and undecided. The
   settlement comparison (`REQ-PR-5`) still reports the account's holding, so value that reached the
   account outside the contract's calls shows as a difference.
7. **Outcomes, one per reference.** Each call's inbox entry is reported as an observation at its own
   coordinate: `Withdrawn`, `WithdrawRefused` (the contract's `Refused` of a withdrawal, with the
   contract's reason or Kreivo's, `Kreivo(Assets(CannotTransfer))`, as the payload, `REQ-SA-3`),
   `Anchored` (with its venue), and **`AnchorRefused`**, a new event kind (code 5, export format 5) for
   the contract's `Refused` of an anchor call. The core ingests it with no operation, like `Anchored`,
   so the range stays complete; nothing becomes final on it. A batch included in a finalized block whose
   anchor is not stored at the finalized head was refused by the contract (`NotExecuted::CallRefused`):
   the core emits the anchor again. **A batch that failed as a whole** (`Contracts::ContractReverted`:
   not a key, empty, too long; or trapped) wrote nothing and spent its nonce: every call in it is
   `DispatchFailed`, and under `#89`'s rule the core sends each to `Needs-Review` with
   `IncludedWithoutRecord` — it cannot tell that from an execution its reader missed. Trade-off, stated
   plainly: one revert now sends up to 32 operations to review, where the pallet sent one.
8. **Reading the contract's storage through the light client.** The contract's inbox is a child trie of
   `pallet-contracts`, and **smoldot 0.19.4 (subxt 0.44's light client) serves no child-trie read**:
   `childstate_getStorage` answers "not implemented", and `chainHead_v1_storage` with a `childTrie` "not
   supported yet" (`smoldot-light` 0.17.2, `json_rpc_service/background.rs`). `F-026`'s notes expected
   the second to work. Runtime calls are served, and smoldot proves every storage read they make, child
   tries included. So the adapter reads the contract with **`ContractsApi_call`**, a dry run of its read
   messages — `registration()`, `head()`, `inbox_entry(seq)`, `position(asset)` — at the finalized block
   (`state_call` at that hash). The messages return the stored values: an inbox entry's bytes, and so its
   digest and its coordinate, are the storage's. Chain state (nonces, balances, decimals, the contract's
   `ContractInfoOf`) is read through the metadata as before. The verifier's node read of the contract
   venue uses `childstate_getStorage`, which a node serves; it needs a node's RPC, not a light client.
9. **Provisional records of a reorged batch.** A provisional record whose position the finalized inbox
   has not reached is now checked once the finalized head passes the block that wrote it: that block did
   not write it on the finalized chain, and never will (the entry's digest binds its block), so
   `still_matches` is false. Before, it waited for the finalized inbox to reach its position, which an
   idle deployment may never do: a batch whose era's birth block was reorged away left its operations
   `Observed` for as long as the inbox was idle (found by the restart chaos run, seed 2026). A
   behaviour change in `F-016`'s observe step, made here because batches exposed it.

**The PASS account (`T-027-03`)**

10. **What is signed.** A batch is `Contracts::call { dest: Id(contract), value: 0, gas_limit,
    storage_deposit_limit, data: execute(envelopes) }` (pallet 80, call 6), in a v5 general transaction
    (`0x45`, extension version 0) with Kreivo's extensions at `4aef055`: `PassAuthenticate`,
    `CheckNonZeroSender`, `CheckSpecVersion`, `CheckTxVersion`, `CheckGenesis`, `CheckEra`,
    `CheckNonce` (compact `u32`), `CheckWeight`, `SkipCheckIfFeeless<ChargeGasTxPayment<
    ChargeAssetTxPayment>>` — all wrapped in `DynamicMaxBlockWeight`, which, like the two payment
    wrappers, encodes as what it wraps. **There is no `CheckMetadataHash`**: the reference's
    `KreivoConfig` lists one, from another runtime. The device signs `context ‖ challenge ‖ authority`,
    with `challenge = blake2_256(block_hash(context) ‖ blake2_256(0 ‖ call ‖ explicit ‖ implicit))` over
    every extension after `PassAuthenticate`.
11. **The preimage order is the one Kreivo `4aef055` verifies.** `pass-authenticators-substrate-keys`
    1.0.0, which that commit locks, signs `context ‖ challenge ‖ authority`. The reference signer
    (`rail-domain`'s `signed_message_preimage`) signs `context ‖ authority ‖ challenge`, the order of an
    older revision (`92c3484`) the runtime it was written for locked. The adapter speaks the pinned
    commit's order; a production runtime built from an older revision would refuse every credential with
    `BadSigner`. The zombienet run (`T-027-06`) is where the deployed runtime answers. **Superseded by
    `T-027-07` (decisions 18 to 24 below): live mainnet runs that older revision, so the adapter now
    speaks both orders, chosen by the runtime's `spec_version`.**
12. **The key behind `Signer`.** A lane is a `PassAccount`: a device key behind the existing signer
    boundary (a KMS in production, `F-016` decision 24), plus the PASS account's hashed user id.
    `Signer::account` is the PASS account (`blake2_256([0; 32] ‖ user)`, `fc-pallet-pass`'s address
    generator for `()`, the one Kreivo configures): the contract's key, the fee payer and the nonce the
    core's release rule reads. `Signer::device` is the key's own account, the device id. The device's
    secp256k1 signature is a `MultiSignature::Ecdsa` over `blake2_256(message)`, which is exactly what
    the signer boundary already signed. No key is copied or logged; the adapter still sees 32-byte
    digests only.
13. **The challenge's context is the finalized head** (its number and hash): stable under reorgs,
    and in `System::BlockHash`. Kreivo accepts a context at most 30 minutes of its blocks old at
    inclusion (`BlockHashChallenger`), so **no era is signed to outlive `challenge_window` blocks after
    that head** (configuration, 256 by default: 30 minutes at one block per 6 s is 300). If finality lags
    so far that no era fits, nothing is signed that step; the withdrawal is tombstoned only when its
    expiry, not the window, rules an era out.
14. **Not taken from the reference:** immortal eras (every transaction here is mortal, `REQ-SL-4`), the
    per-signing nonce fetch (the lane's nonce rule of `F-016` decision 13 is kept), the proxy and
    registration paths, and subxt's metadata-driven `AnyOf` encoding (Kreivo's runtime also lists
    `DynamicMaxBlockWeight` in its extension metadata; the encoder here is plain SCALE, pinned instead).

**The venue on every anchor (`T-027-04`)**

15. **`Venue`** is `Pallet { instance }` or `Contract { account }` (`crates/verify`). The adapter
    reports the venue it read an `Anchored` record in, on the observation's anchor fact; the core records
    it on the checkpoint when it becomes final (`Checkpoint::venue`), and a checkpoint final without one
    is the pallet's under its instance (`Checkpoint::anchored_in`). The commitment does not change.
16. **The disclosure envelope's version 2** names the venue right after the instance; version 1 is still
    read (as the pallet's) and re-encodes to the bytes it was signed over. The verifier reads the anchor
    and the published clerk key in the disclosure's venue: the pallet's storage, or the contract's
    `anchors[number]` and `registration` in its child trie. *(Since 2026-10-11 the contract keeps
    only its latest anchor, `latest_anchor`; the verifier finds an earlier one at the finalized
    blocks where it was the latest: `F-026` decision 25.)* A saved read names the venue as
    `contract:<account-hex>` or the pallet's instance.
17. **Export format 5**: an anchor fact's venue, a checkpoint's venue and the `AnchorRefused` kind. A
    format-4 export is refused by version, not misread.

### Tests

- **`services/settle/tests/contract.rs`** (new, 9): the call outbox's envelopes (`T-027-01`); a batch of
  three in one transaction with one outcome each; a refused call inside a batch, recorded under its
  reference with the contract's reason while the rest apply; a refusal with Kreivo's reason; a restart
  that replays the same batch bytes; a reader fault inside a batch going to review (`#89`); a batch whose
  era ends unincluded releasing every call on the unconsumed nonce; a batch sent as the PASS account (the
  PASS account's nonce moves, the device's does not); a tampered PASS transaction never included.
- **`services/settle/tests/anchoring.rs`** (2 new): a checkpoint records its venue and the disclosure
  names it (the verifier finds it there, not in the pallet); a refused anchor call observed with the
  contract's reason.
- **Unit**: `chain::kreivo` (eras, the authority and PASS account, the contract's encodings against
  `F-026`'s pinned bytes, assets, `Contracts::call`, a dry run's result, a PASS transaction's round trip,
  **the pinned vector**); `settlement::envelope`; `Ledger::call_outbox`; the anchor store's venue;
  `verify`'s venue, envelope version 2, contract keys, saved reads.
- **`services/settle/kreivo-pin`** (its own workspace): rebuilds `vectors.json` from the same inputs with
  `sp-runtime` 48.0.0, `sp-weights` 36.0.0, `fc-traits-authn` 2.2.0 and `pass-authenticators-substrate-keys`
  1.0.0 (the versions Kreivo `4aef055` locks): the era, the call, the extensions' explicit and implicit
  data, the inherited implication (`ImplicationParts`), Kreivo's challenger, `SignedMessage::message()`,
  `MultiSignature::Ecdsa::verify`, the substrate-keys `Device::verify_user` (and its refusal for another
  call), `PassAuthenticate`'s value, the PASS account, and the whole transaction decoded by
  `UncheckedExtrinsic` and re-encoded byte for byte. Run it with `cargo test --manifest-path
  services/settle/kreivo-pin/Cargo.toml`. It was not in CI at first; `T-027-07` added the `kreivo-pin`
  job to `.github/workflows/ci.yml`, which runs it and `kreivo-pin-132`.
- **The fake layer runs the contract.** Transactions are Kreivo's own bytes, built by the encoder and
  checked as the runtime checks them (era and birth hash, challenge context and window, the challenge,
  the device's signature, the PASS account's nonce, fees); the contract's keys, recorded positions,
  withdrawal bounds, one outcome per call, refusals with the contract's or Kreivo's reason, reverts, the
  inbox in the contract's own SCALE bytes, anchors. Which devices a PASS account registered is not
  modelled. `fund_position` stands for a deployment instantiated holding value; `top_up_position` stays a
  plain transfer, which the contract does not account for.
- **What changed in `F-016`'s and `F-017`'s tests**, and why: the lanes are PASS accounts; the fake's
  asset is Kreivo's `Here(9001)`; "liquidity returns" is `fund_position` (a plain transfer no longer
  raises what the contract pays); the failed-dispatch disposition is a batch the contract reverts (the
  lane's key removed before it lands), since the contract records a payment to a deployment as a refusal
  where the pallet failed the extrinsic; the scripted intake serves the call outbox; the verifier's read
  is in the contract venue; `AC-I1.1`'s entry is the contract's (`4 + 1 + 32 + 8 + 32` bytes, the 32 being
  the anchor's reference, checked to be its instance's and number's); `ops`' restore drill expects export
  format 5. No assertion was weakened. `Kit::pass` now waits for the core to tick at the new time (`#91`'s
  `Deployment::pass`), and `AC-E1.5` and `AC-E3.3` drive their own ticks.

### Not done

- **The runs on zombienet and the testnet** (`T-027-03`'s "a signed batch is accepted on zombienet", and
  the exit criterion): `T-027-06`. Nothing here has met a running Kreivo; the pin crate is the evidence
  the bytes are Kreivo's.
- `T-027-05`: the VOS host extension.
- Pruning the contract's inbox: `F-026`'s notes ask `F-027` to own when the admin prunes, never past the
  adapter's finalized cursor. Not built; the admin does not prune yet.
- Measured gas and storage-deposit limits for a full batch (`ProcessConfig::call_limits`; generous
  defaults until the zombienet run).

## PASS signing for the live mainnet runtime and the next release (T-027-07, issue #93, 2026-09-29)

aledgent's test environment goes live on Kreivo **mainnet** (a dedicated test community, the
canary). `T-027-03` signed as Kreivo `4aef055` (spec 133) verifies, and mainnet runs spec 132. This
section records what differs between the two, what mainnet runs, and how the adapter now chooses.

### What mainnet runs

Read-only RPC on 2026-09-29, against `https://kreivo.io` (other public endpoints answered the
same):

- `state_getRuntimeVersion`: `kreivo-parachain`, **`specVersion` 132, `transactionVersion` 12**.
- `state_getStorageHash(":code")` =
  `0x59c07b0c96efc943f41e083d101708984ec1f317de0cb8dc0902db4e58890647`, **exactly the code hash of Kreivo
  release `0.16.9`'s `authorizeUpgrade` call** (its release notes). Mainnet runs release `0.16.9`, commit
  `db3d8af`, the latest published release.
- Its metadata (v16; the v14 it also serves lists the same) supports extrinsic versions 4 and 5, and
  extension version 0 lists, in order: `PassAuthenticate`, `CheckNonZeroSender`, `CheckSpecVersion`,
  `CheckTxVersion`, `CheckGenesis`, `CheckMortality`, `CheckNonce`, `CheckWeight`,
  `ChargeAssetTxPayment`. **No `CheckMetadataHash`.**

### What changed between spec 132 and spec 133, and where

Read in `virto-network/kreivo`, `virto-network/pass-authenticators`, `virto-network/frame-contrib` and
`paritytech/polkadot-sdk` at the revisions named.

| What | Spec 132 (`0.16.9`, `db3d8af`) | Spec 133 (`4aef055`) | Where it changed |
|---|---|---|---|
| **The device's message** | `context ‖ authority ‖ challenge` (pass-authenticators `92c3484`) | `context ‖ challenge ‖ authority` (pass-authenticators 1.0.0) | pass-authenticators **PR #16**, "feat(substrate-keys): restructure encoding order", merged 2026-02-14 as `7f240c3`. Kreivo took it in **`a8714cc`** (kreivo#473, "update to `stable2512-2`", 2026-02-26), already spec 133. First release with it: `0.17.0-pre.1` (`9c5ac21`, a pre-release) |
| `spec_version`, `transaction_version` | 132, 12 | 133, 13 | Kreivo **`cf69b67`**, "update to `stable2512-1`" (2026-01-30, pushed without a PR) |
| `DynamicMaxBlockWeight` around the extensions | absent | wraps all nine | kreivo **#492** (`805bfa6`, 2026-09-22, block bundling). It encodes as the tuple it wraps and adds no implicit data, so **no byte changes**; its metadata appends an entry named `DynamicMaxBlockWeight` (type `()`, implicit `()`) after the nine |
| `CheckMetadataHash` | absent | absent | Never in Kreivo's `TransactionExtensions` at `0.16.9`, `4aef055` or master, nor in mainnet's metadata. The reference signer's `KreivoConfig` lists it because subxt's `AnyOf` matches extensions by the metadata's names and skips the absent ones: it adds nothing on Kreivo |
| The extensions' order and encoding | as in decision 10 | the same | Unchanged: `CheckNonce` compact `u32`, `ChargeAssetTxPayment { compact tip, Option<asset> }`, `CheckEra` named `CheckMortality`, `SkipCheckIfFeeless` and `ChargeGasTxPayment` named for what they wrap (fc-pallet-gas-transaction-payment at `26630fb` and 2.2.0 identical) |

Checked unchanged as well: `PassAuthenticate(Option<AuthenticateParams { device_id, credential }>)` and
the inherited implication it hashes (fc-pallet-pass 1.0.0 at frame-contrib `26630fb` against 2.2.0:
weight and refund changes only); the credential's verification (`MultiSignature::verify` over the
message, against the device's `AccountId32`: at `92c3484` it lived in `key_registration.rs`, from 1.0.0
in `key_signature.rs`, same code); the `Pass` composite (`WebAuthn`, then `SubstrateKey`), the authority
`kreivo_p`, the address generator `()`, `BlockHashChallenger`; `Contracts` at pallet index 80 and `call`
at call index 6 (`pallet-contracts` 43.0.0 and 49.0.0), and `ContractsApi_call`'s result types (the two
versions' `primitives.rs` differ only in derives); `Nonce = u32`. The challenger's window, 30 minutes of
blocks, is 3 600 blocks at `0.16.9` (12 blocks per relay slot) and 900 at `4aef055` (3 per slot,
`parachain::MINUTES`): the adapter's `challenge_window` (256) fits inside both.

**The release that carries kreivo#501** (master `1b849b3`) is still spec 133 and locks
pass-authenticators-substrate-keys 1.2.0 and fc-pallet-pass 2.3.1 (kreivo#496, #499): weights only;
`signed_message.rs` is identical from 1.0.0 to 1.2.0. It signs as `4aef055` does.

### Decisions (T-027-07)

18. **Two formats, chosen by `spec_version`, and no other.** `chain::kreivo::TxFormat` names what differs
    between the runtimes the executor signs for: the device's message order and the extension set.
    `TxFormat::of(132)` is Kreivo `0.16.9`'s (`context ‖ authority ‖ challenge`, the nine extensions);
    `TxFormat::of(133)` is `4aef055`'s (`context ‖ challenge ‖ authority`, the nine and
    `DynamicMaxBlockWeight`). Every other `spec_version` has none, and nothing is signed for it rather
    than guessed. The rest of the transaction is one encoder for both, since its bytes are the same.
19. **Where the rule is applied.** Each step's start-up assertions (`REQ-SA-2`, run on every step since
    `F-016` decision 7) read the runtime at the best head. Besides the operator's allow-list
    (`spec_versions`), they refuse a runtime with no known format (`Refusing::TxFormat`), and one whose
    metadata does not name exactly its format's extensions, in order, for extension version 0
    (`Refusing::Extensions`). While refusing, nothing is signed. `kreivo::sign` selects the format from
    the `spec_version` it signs for (the best head's, as before) and refuses an unknown one
    (`SignError::UnknownRuntime`), in case the runtime changes between the assertions and the signature.
    A configuration whose allow-list names a `spec_version` with no format is refused at start-up.
20. **A runtime upgrade switches the format of the next batch.** Nothing is cached across steps: the step
    after the layer reports the new `spec_version` signs in the new format. Over the light client or
    RPC, `RpcChain::runtime` sees the new runtime version at the best head and reloads the metadata
    there before it is checked or anything is signed with it (subxt's client otherwise keeps the
    metadata it connected with). Trade-off, stated plainly: in the few blocks between the upgrade at
    the best head and its finality, chain state read at the finalized head (nonces, balances, asset
    metadata) is decoded with the new runtime's metadata. A value that does not decode stops the step
    (`ChainError::Invalid`) and the next one retries; nothing is guessed. The entries read there were
    not compared field by field between the two runtimes.
21. **A batch in flight across the upgrade is not re-signed.** Its `spec_version` is in its challenge
    (`CheckSpecVersion`'s implicit), so the new runtime never includes it. It is decided as any batch
    that never lands: its era ends with the PASS account's nonce unconsumed and the inbox holds no record
    of its calls, so every call is released on both proofs (`F-016`, the owner's ruling of 2026-09-24).
    Trade-off, stated plainly: operations signed in the blocks before the upgrade are released, not
    settled, and their customers submit again; the era bounds how long that takes (`max_era`, and never
    past `challenge_window` blocks after the finalized head).
22. **Pinned against Kreivo's crates at both revisions.** `services/settle/kreivo-pin` (spec 133, its
    vector unchanged) and **`services/settle/kreivo-pin-132`** (new): the same test, with the crates
    release `0.16.9` locks — `sp-runtime` 44.0.0, `sp-weights` 33.1.0, `frame-support` 43.0.0,
    `fc-traits-authn` 0.1.0 at frame-contrib `26630fb`, `pass-authenticators-substrate-keys` 0.1.0 at
    `92c3484`. Its lock file is seeded from Kreivo `0.16.9`'s own, and a test checks the two git
    revisions; the git dependencies carry no revision in `Cargo.toml`, as in Kreivo's, so that
    pass-authenticators' own dependency on frame-contrib unifies with ours. Each crate also signs its
    vector's challenge in the **other** runtime's order with the same key and shows its runtime refuses
    it. The two cannot share a workspace: they lock incompatible SDK releases. Both run in CI (the
    `kreivo-pin` job: fmt, clippy, tests, all `--locked`).
23. **`spec_version` is a sound selector for published runtimes, not for every build.** Kreivo `cf69b67`
    and `5ccd253` are spec 133 with pass-authenticators `823e2ea`, before PR #16: the old order under the
    new number. No release was cut from them (`0.17.0-pre.1` already has PR #16). A network built from
    those commits would refuse the adapter's spec 133 transactions. Conservative reading: the rule covers
    every published release and pre-release; the extension check does not catch an order change, so a
    new `spec_version` is added only with its own pin crate.
24. **The fake layer runs spec 133 by default** (it ran 134, a number no Kreivo runtime of the contract
    venue has) and checks each transaction in the format of its current runtime; the test kit allows
    132 and 133. The README's example allows `[132, 133]`: mainnet's runtime and the release after it.

### Tests (T-027-07)

- **Unit** (`chain::kreivo`): the spec 132 vector
  (`t_027_07_the_spec_132_transaction_matches_its_pinned_vector`, `kreivo-pin-132/vectors.json`) beside
  the spec 133 one; the format table (both orders, both extension sets, no format for 0, 131, 134, 135,
  `u32::MAX`); an unknown runtime refused by `sign`. `config`: an allow-list naming a runtime with no
  format.
- **`tests/contract.rs`**: on spec 132 the device signs in 132's order and the batch settles; **a runtime
  upgrade from 132 to 133 seen mid-run switches the format for the next batch**, and both settle; a batch
  in flight across the upgrade is released on both proofs, never re-signed, and the next goes out in
  133's format. Each checks which order the stored signature verifies in, and that it is exactly one.
- **`tests/flow.rs`**: an allowed runtime with no known format stops service; a runtime whose extensions
  are not its format's stops service; the unlisted-runtime test now also reports the missing format.
- **`kreivo-pin-132`**: the whole vector against Kreivo `0.16.9`'s crates, the other order refused, the
  lock's revisions. **`kreivo-pin`**: the other order refused as well.
- **Once against mainnet, read-only, not committed**: `RpcChain::runtime` over `wss://kreivo.io`
  answered spec 132 with exactly `EXTENSIONS_132`, so the format check passes there. Nothing was
  signed or sent. The upgrade path of `RpcChain::runtime` (reloading the metadata) has not met a real
  upgrade; the zombienet suite is where it can.

### For the owner

- **When the release carrying kreivo#501 is cut, check its `spec_version`.** Master is spec 133 today,
  which the adapter already signs for. If the release bumps it (to 134), the adapter refuses that runtime
  (fail closed) from the block the upgrade is enacted until a format for it is added and pinned. That is
  a small change (a row in `FORMATS`, a vector, a pin crate at that release), but it must land before the
  upgrade is enacted on mainnet, not after.
- The zombienet suite (`e2e-zombienet`) takes Kreivo's latest release, `0.16.9`: the adapter speaks
  spec 132 there, which is also what mainnet answers.

## The first zombienet runs' findings, fixed (T-027-06, T-027-08, T-027-09, issue #96, 2026-09-29)

The first runs of the zombienet suite through aledgent (`features/F-026-settlement-contract/NOTES.md`,
"Against Kreivo main (0.17), and through aledgent", findings 1 to 6) found three defects in the
adapter, each a blocker for the canary on Kreivo mainnet (spec 132, release `0.16.9`): eras born at the best block (findings 3
and 6), the birth block looked up by number, which the light client does not answer (finding 4), and a
fixed gas limit a full batch did not fit (finding 2). The owner ruled on how a batch that fails its dry
run is handled (decision 30).

### Decisions

25. **Every era is born at the finalized head** (`T-027-08`; `Mortality::born_at_finalized`). The
    challenge's context was already the finalized head; the era's birth now is too, so the birth hash is
    the finalized head's own hash, which the adapter holds. A reorganisation above finality (Kreivo 0.17
    reorganises up to six blocks) can no longer retract the birth block and invalidate a batch, and
    nothing is looked up by block number. `sign_batch` refuses an era not born at its context (both
    implementations), so no path can look a birth block up. Every era of at most 4 096 blocks is born
    where it is built; `max_era` is 64 and never above `BlockHashCount`.
26. **The last-valid-height rule is unchanged, from a lower birth.** The era ends at or before the
    earliest expiry of its batch and within `challenge_window` of the finalized head, and it must leave a
    block above the best head (`last_valid > best`), so the core, which has seen finalized heads only up
    to the birth, has seen none at or past its last valid block. The era is shorter by the finality lag.
    When finality lags so far that no era fits, the batch waits for the next step. The tombstone rule
    keeps reading the best head (`NeverSigned` only when not even an era born at the best head could
    fit): the conservative reading, since a tombstone lets the core release and a later finalized head
    can still make an era possible. #89's rules are untouched: the signing is recorded before the
    broadcast, nothing is signed twice, a dead nonce is reused only after the core has taken in the head
    proving it dead, and nothing is released without both proofs.
27. **Finding an included transaction walks parent hashes** back from the finalized head to the era's
    birth (`RpcChain::find_extrinsic`), for the same reason: no lookup by number. When a block's events
    can no longer be read (a full node keeps state for 256 blocks by default; an adapter down longer
    than that), the transaction is not taken as a success: it is decided as failed locally, the inbox
    read at the finalized head then records what the batch did, and the core decides from the inbox and
    the nonce, never from this (a re-emitted anchor is a no-op replay). A block whose body is gone
    (`--blocks-pruning`) still stops the step; see "Open".
28. **Records that no longer match are checked first.** A batch born at the finalized head survives a
    reorganisation and can be included again at the same inbox position in another block, leaving two
    provisional records of one operation at one position (the chaos test found it). The core rebinds the
    operation to the survivor when the first is orphaned; had the survivor already been made final by an
    earlier check in the same step, nothing would have promoted the operation afterwards (it stayed
    `Observed`). The observe step now reports every record that no longer matches before any that does.
29. **Each batch is sized and priced by a dry run** (`T-027-09`; `Chain::dry_run`, `ContractsApi_call`
    as the lane's PASS account at the finalized head, where the adapter decides everything else; the
    batch runs a few blocks later, which the margin covers). Its gas limit is the
    dry run's `gas_required` with half again (`batch_gas_margin_percent`, 150, integer arithmetic,
    rounded up), never above the runtime's normal-extrinsic maximum at the block the batch is sized
    against (decision 41; on `0.16.9` 1 399 891 843 000 of ref time and **3 670 016 bytes of proof
    size, which is what binds**) or `batch_gas_cap`, if one is set lower (a full batch of 32 withdrawals needs about 2.05 MB; the old fixed limit of 1 MB reverted any
    batch above about 15). One dry run in the common case. The margin covers a batch that runs a costlier
    path than its dry run priced, such as a withdrawal the dry run saw refused applying because a deposit
    landed first in the same block; the suite needed half again for that (F-026 finding 2), and a test
    proves it against the fake's weights and shows the batch running out of gas without it. **The
    storage deposit** the dry run reports (its `Charge`) becomes the batch's storage deposit limit, with
    the same margin, and the lane's native balance must cover it besides `fee_floor`, or the batch
    waits as for fees. No constant enters any of this (the owner, 2026-09-29: dry-run reads through
    `ContractsApi_call` are the standard way, and weights are known before submitting). Every limit is
    computed in one function, `Adapter::batch_limits`, which is where per-call limits would be filled
    if the envelope gains them (the per-call isolation work on the contract). The gas limit is never
    less than the dry run plus 2 000 000 000 of ref time and 92 KiB of proof size, what #102's per-call
    isolation may use above a dry run: the percentage covers that for any batch with a withdrawal, the
    floor for a batch of anchors only.
30. **A batch whose dry run fails is split; a call no good alone goes to review** (the owner's ruling on
    #96). The contract turns every per-call problem into that call's `Refused` entry, so a batch fails as
    a whole only by running out of gas, proof size or storage deposit, or by an invariant trap. The
    adapter bisects: the longest prefix that applies and fits goes now, and the rest go back to be fitted
    again (by another lane, or the next step). A call whose own dry run fails, or which alone needs more
    than the cap, is **not refused on chain, since nothing moved**: it is left unsubmitted
    (`InstructionState::Held`, with the dry run's error and a `review:<operation>` alert), never signed
    and never tombstoned. With no signing record the core proves nothing about it, so it releases
    nothing, and the operation joins the review queue at its deadline, where it reads `Needs-Review`
    (amendment 0006, applied on `main` before this PR). An earlier draft of this task dropped such a call
    as a tombstone, which would have let the core release it; the ruling replaced that.
31. **A batch the contract reverts as a whole is no call's doing.** `execute` returns an error only for
    the batch (not a key, empty, too long, the inbox cannot advance). Nothing is held for it: nothing is
    signed on that lane in that step, the calls go back, and a `batch:<lane>` alert says why. With the dry
    run at the finalized head, where the start-up assertions already refuse to serve with a lane that is
    not a key, this is a guard more than a path; a key removed in a block not yet final is still signed
    for, and that batch fails on chain as a whole.
32. **The fake layer models gas** after the suite's measures (`EXECUTE_BASE` 100 000 bytes of proof size,
    60 000 per applied withdrawal, 45 000 per refusal, 30 000 per anchor): a batch whose cost exceeds its
    gas limit fails whole (`Contracts::OutOfGas`), its fee and nonce spent. The refusal's cost is an
    assumption chosen so that half again covers the cheap path, as the suite found on Kreivo; the suite,
    not the fake, is the evidence for the margin.
33. **`fault-injection`**, a feature of `services/settle` that only the suite's build enables: the binary
    dies (`abort`) the first time a step reaches the point `ALEDGENT_SETTLE_CRASH_AT` names
    (`after-sign`, `after-persist`, `after-broadcast`), for "kill the adapter between signing and
    submitting". A release build does not contain it.
34. **The aledgent driver runs the adapter as production does** (`T-027-06`; `e2e/README.md`): the
    `aledgent-settle` binary, its own process with its own PostgreSQL store, over its light client,
    reaching the core over ingress's HTTP surface, which the driver hosts with the in-process core. The
    adapter runs in a container in a network namespace of its own (a long-lived holder with `iptables`,
    `NET_ADMIN` in that namespace only), so the harness can cut every peer, kill and restart the adapter
    over the same store, and block the collator's parachain port, without touching the runner's network.
    Every fault case runs through it.
35. **No batch is signed without its dry run; a client that cannot give one pauses them.** The first
    runs of this work on the fleet (0.16.9 and main, both over the light client; below) found that the
    embedded smoldot answered a dry run of `execute` **at the best head** with "Error trying to access
    the storage required for the runtime call" (`RuntimeCallError::Inaccessible`), while it answers the
    contract's read messages at the finalized head. Dry runs are now at the finalized head (decision
    29), and the next runs (36588926409, 36588931699) showed smoldot fails there too: see decision 36
    for why, and what the adapter does about it. When the client cannot give one all the same (`ChainError::Unavailable`), nothing
    is signed, the calls go back, a `dry-run` alert is raised, and no dry run is asked for again for a
    minute: smoldot bans for 40 s every peer whose call proof it could not use (`runtime_service.rs`,
    "invalid-call-proof"; 10 s one that did not answer), and with one boot node per chain a ban stops
    every read. That is what made the start-up assertions answer "unreachable" every other step in
    those runs; they now log which read failed. Nothing is held for review on that account, since the
    call itself was never judged. (An interim version priced such a batch by the suite's measure; the
    owner's ruling that weights are known before submitting replaced it.)
36. **Dry runs go through a node's RPC (`dry_run_endpoint`); nothing else does.** Observed on both
    runtimes (0.16.9, whose `pallet-contracts` is 43.0.0, and main): smoldot answers every dry run of
    `execute` with an unusable call proof, and every read message with a result. *Corrected
    2026-10-02 (decision 42, the light-client probe):* the cause given here first, that the serving
    node appends to `System::EventTopics` without reading it so that its proof lacks the value, was
    wrong (`sp-state-machine` 0.53 reads the backend on the first append, and smoldot 3.3.1 dry-runs
    contract events with topics, byte for byte as the node). The cause is smoldot's: once a call has
    finished it recalculates the root of every child trie the call wrote, though nothing reads it,
    and that needs trie nodes and values the node never touched, so its proof lacks them; smoldot
    0.19.4 also fails earlier in the same work. A read message writes nothing, so it is never
    affected. So over the light client alone no batch can be dry-run reliably, and none signed.
    `ProcessConfig::dry_run_endpoint`
    names a node's RPC used for dry runs and for nothing else, at the finalized head's hash the light
    client gave; every read that decides an outcome (heads, nonces, the inbox, positions) stays on the
    light client. Trade-off, stated plainly: that node is trusted for a batch's limits only; a
    dishonest or stale one can make a batch fail whole on chain (nothing moves, its calls go to
    review) or be given more than it needs (a limit, not a charge), never move value or decide an
    outcome. It is an explicit field, never a fallback, and the canary's configuration must name one.
    A dry run that fails over that node is tried once more over a new connection: in the starve case
    of run 36610881899 (0.16.9) the node's websocket was cut with every other peer and stayed closed
    ("restart required"), so no dry run, and no batch, followed for the rest of the process's life.
    The lasting fix is upstream, in smoldot (decision 42); not attempted here.
37. **Any panic ends the adapter's process.** smoldot 0.17.2 panicked in its own threads on the fleet:
    `network_service.rs:1277` (`event_pending_send.is_none()`, finding 5 of the first runs, now with
    one boot node per chain), `sync_service.rs:240`, `runtime_service.rs:1615`, and a `swap_remove`
    out of bounds. Each left the process running with a dead light client, serving nothing and
    releasing nothing, until restarted by hand. The binary now aborts on any panic, so its supervisor
    starts it again over the same store, which resumes where it stopped (`REQ-SA-5`, `REQ-SA-6`, the
    restart matrix). The harness runs it with `--restart on-failure`, as a supervisor would. The
    canary's deployment needs that supervisor.
38. **A blind adapter restarts itself.** smoldot did not always reconnect after its peers were cut for
    a minute and a half on the fleet. After three minutes in which every step finds the settlement
    layer unreachable, the binary ends with an error, and its supervisor starts it with a new light
    client. Nothing is released meanwhile: a blind adapter reports no head.
39. **Per-call limits (`execute_with_limits`, #102) are not used.** #102, merged before this PR, runs
    only a withdrawal to another deployment in a frame of its own (hybrid isolation: about 236 KB of
    proof size and 3.1 to 3.5 G of ref time each, against about 61 KB and 1.0 G for a withdrawal to an
    account or an anchor, inline), keeps `MAX_CALLS` at 32, turns a framed call's trap or exhaustion
    into its `CallFailed` refusal (which the adapter decodes, from #102), and adds
    `execute_with_limits`, whose limits apply to framed calls only. The adapter keeps `execute`
    with one limit for the batch: the batch's dry run prices every frame, and its margin (half
    again, never less than 2 G and 92 KiB above the dry run, decision 29) covers what a batch with
    a framed call needs above its dry run (at most 2 G and 92 KiB, per #102). Per-call limits would
    cost a dry run per framed call and buy only an earlier `CallFailed`; `Adapter::batch_limits` is
    where they would be filled.
40. **One watch per submitted transaction, kept until it ends.** The most frequent smoldot panic on
    the fleet (`swap_remove index (is n) should be < len (is n)`, seven times in run 36615716251 on
    0.17.0, each a restart) is smoldot-light 0.17.2's `PendingTransaction::update_status`
    (`transactions_service.rs:1434`): it walks its status watchers by index while removing the closed
    ones, so a transaction with two watchers, one closed, panics at its next status. The adapter made
    that state every step: it rebroadcast each uncertain transaction with subxt's `submit`, which opens
    a watch, reads one status and closes it. `RpcChain::submit` now opens one watch per transaction and
    drains it until the client reports the end (finalized, dropped, invalid); while it lasts the same
    bytes are not submitted again, since the client itself keeps broadcasting them, and after it ends
    the next step submits them again (`REQ-SO-7` unchanged). The other smoldot panics (decision 37)
    are not the adapter's doing and are still handled by the restart.

41. **The cap is the runtime's extrinsic limit, read at the block the batch is sized against.** Kreivo
    `0.16.9`'s normal-extrinsic maximum is fixed (3 670 016 B of proof size); `0.17.0`'s moves with
    the cores the parachain has (`MaxParachainBlockWeight`, `DynamicMaxBlockWeight`; the suite read
    2 446 677 B in one run and 7 340 032 B in another, #102). `Chain::extrinsic_limit` reads
    `System::BlockWeights` (normal class, `max_extrinsic`) from the runtime's metadata as it is built
    in the state of the finalized head the batch is dry-run at (`Metadata_metadata_at_version`, through
    the dry-run node), and the adapter caps each batch there (`Adapter::batch_cap`; `batch_gas_cap`
    only lowers it, for tests). On `0.17.0` the value depends on where the block sits in its core: a
    block that opens a core gives the whole core's (7 340 032 B on the suite's network, which is the
    second figure above), any other the fraction the pool holds a transaction to (2 446 677 B, one
    core and three blocks to it). The adapter takes the lesser of the head's and its parent's, which
    is the fraction whenever a core carries more than one block. With half again, that is about 26
    withdrawals to accounts or 6 to a deployment in one batch on the suite's network; `MAX_CALLS`
    (32) is a bound, not a size. No limit to be read, nothing is signed (dry runs pause, as in
    decision 35). The constant `NORMAL_EXTRINSIC_MAX_0_16_9` stays as the fake layer's default and a
    placeholder; nothing signs with it.

42. **The light client is smoldot-light 2.1, and why dry runs of `execute` still need a node
    (T-027-10, a light-client probe, not kept in this repository).** The same `ContractsApi_call`
    bytes of `execute`, built with the adapter's own encoders, ran at one finalized block through
    every light-client path and against the collator's `state_call`, on Kreivo 0.17.0 and 0.16.9
    (runs below). What they showed:
    - **subxt 0.44's own light client (smoldot-light 0.17.2, smoldot 0.19.4), the adapter's until
      now, fails every dry run of `execute`**, a single withdrawal or an anchor included, and
      answers the read message `head()`. smoldot 3.3.1 (polkadot-api 2.1.8, `chainHead_v1_call` and
      raw `state_call`) and smoldot-light 2.1.0 (smoldot 4.0.0) answer one withdrawal to an account,
      one to another deployment (a framed call, child-trie writes in two contracts, an asset moved)
      and one anchor, **byte for byte as the collator**, output and gas.
    - **Every light client fails a batch of several withdrawals** (two sometimes, eight and sixteen
      always): `MissingProofEntry`. smoldot-light 2.1.0, patched only to say so, named the access the
      proof lacks: the value of an inbox entry the contract never reads (seq 47; in another run 29
      and 20), in the contract's child trie. It is asked for by smoldot's executor
      (`executor/runtime_call.rs`, `HostVm::Finished`): when a call finishes, smoldot recalculates
      the root of every child trie the call wrote, even with `calculate_trie_changes: false`, which
      every light-client runtime call sets and with which nothing reads the result. The node never
      calculates that root during a runtime call, so its call proof holds only what the call itself
      read; where the new inbox keys split an existing leaf, the root needs that leaf's value, which
      is not there. Whether a batch fails depends on where its keys land in the trie, hence one
      withdrawal mostly passing and sixteen never. smoldot 0.19.4 fails earlier in the same work:
      its `trie::proof_decode::next_key` walks out of the prefix it is given (it counts every
      descent toward the prefix as an ancestor that matches it; fixed in smoldot 0.20.0), and the
      root calculation looks for keys under a prefix, so even one inbox entry fails there.
    - **The proof:** path (d2), smoldot 4.0.0 with that one recalculation skipped when
      `calculate_trie_changes` is false (a patch in the probe, an
      experiment, never shipped), answered all seven cases, the batches of two, eight and sixteen
      included, byte for byte as the collator.

    What changed here: the adapter's light client is smoldot-light 2.1.0 behind subxt 0.44's
    `RpcClientT` (`chain/light.rs`, what subxt-lightclient does, about 300 lines; later subxt
    releases embed smoldot-light 1.3 under an API the adapter would have to be ported to). That
    removes smoldot 0.19.4's failure, so a batch of one is dry-run over the light client, and the
    smoldot-light 0.17.2 code whose panics decisions 37 and 40 handle. smoldot's debug assertions are
    off in dev and test builds (the workspace's `Cargo.toml`): `next_key` asserts something untrue,
    and a debug build panicked on these dry runs (run 37016565339, path d). **`dry_run_endpoint` is
    still required for the canary**: no batch is signed without its dry run, and over the light
    client a batch of several withdrawals cannot be dry-run until smoldot stops recalculating child
    tries' roots the caller did not ask for. That is a one-line change upstream (the guard path d2
    applied); carrying it in a fork of smoldot here, or sending it upstream, is the owner's call. Not done: a workaround in the adapter (dry-running batches of one), which
    would hide the cause and cost a dry run per call.

    With two Kreivo boot nodes (the collator and the pruned full node, run
    37038254965) every path gave the same results and no client panicked, smoldot-light 0.17.2
    and 2.1.0 included. The adapter on smoldot-light 2.1.0 also passed the whole suite with the
    aledgent driver, step 3 and all five fault cases, dry runs through the collator (run
    37038365582, 0.17.0); its first run (37033609241) failed only because the transport read a
    `null` result as no result, fixed before.

    Two more facts the probe found. The Rust smoldot-light platform (0.17.2 and 2.1.0 alike) speaks
    TCP and plain WebSocket, not secure WebSocket (`platform/default.rs`, "TODO: support WebSocket
    secure"): a `/tcp/443/wss` boot node is unreachable for the adapter, whatever a browser or
    Node.js client reaches through it. And smoldot-light 0.17.2 panicked against Kusama mainnet from
    a workstation (`network_service.rs:1277`, as on the fleet, decision 37).

43. **smoldot from a fork, and every dry run over the light client (T-027-11,
    the owner's ruling of 2026-10-02: carry the patch and propose it upstream).** The workspace
    builds smoldot and smoldot-light from `pandres95/smoldot`, pinned by commit (`67ad6c37`,
    `[patch.crates-io]` in `Cargo.toml`): upstream `main` at `b7f587e4` (smoldot 4.0.0 and
    smoldot-light 2.1.0 as released, plus seven commits since) and one change. The change is not
    decision 42's one-line guard: `runtime_call::Config` gets
    `calculate_child_tries_roots_on_finish`, which the light client sets to false, because block
    authoring in smoldot runs with `calculate_trie_changes: false` too and needs those roots, so
    reusing that flag would have broken it. Proposed upstream as paritytech/smoldot#3395, with
    a test that reproduces the failure without a network; drop the patch once a release carries
    it. smoldot 0.19.4, subxt's own light client under `probe-subxt-lightclient`, is not patched.

    Runs on Kreivo 0.17.0 (`polkadot-stable2606-2`):
    - **The probe** (run 37074411553, one peer): path (c2), the adapter's `RpcChain` on the patched
      smoldot, answers all seven cases, batches of two, eight and sixteen included, byte for byte as
      the collator (output and gas). In the same run, (d), released smoldot-light 2.1.0, still fails
      the three batches, and (d2) passes them, as before.
    - **The aledgent driver's suite with no dry-run node** (run 37074414497): step 3, all five fault
      cases and the final checks pass, every dry run over the light client. Its batches held one
      call each, so the probe is what covers batches of several.
    - Again with two Kreivo peers (probe, run 37077364588), and after rebasing onto F-028 (suite,
      run 37077814169): the same results.

    So `dry_run_endpoint` stays optional and leaves the canary's template (`deploy/templates`). The
    aledgent driver names none over the light client. In that run the adapter lost its peer only
    during the injected faults (the 30 s cut and the 90 s starvation): none of the twenty-second
    spells "Open" below describes, which came seconds after a batch was submitted; nor in run
    37077814169. That fits that item's suspected cause (validating a submitted transaction over a
    call proof met the same missing entry, and banned the peer), but two runs of seven batches do
    not prove it.

### Tests

- **`tests/batching.rs`** (new): a batch born at the finalized head survives a reorganisation of three
  blocks and settles, while one born at the best block is dropped for good by the same reorganisation;
  an era not born at its context is refused; a full batch of 32 goes out whole, its gas from one dry
  run, over 1 MB and under the normal-extrinsic maximum; with the old 1 MB as the cap, batches of nine
  (9 + 9 + 9 + 5), none out of gas; with the runtime's extrinsic limit at 1 MB, batches of nine, and once it
  rises (more cores), the other 23 in one, each within the limit read when it was sized; one call that traps its batch: the five before it go, then the other
  26, and it is held for review, never signed, never released, in the review queue at its deadline;
  a full batch's storage deposit limit is its dry run's deposit with the margin; with no dry run to
  be had nothing is signed, none is asked for again for a minute, then the withdrawal settles and the
  alert clears; half again covers a batch priced on the cheap path (dry-run at the
  finalized head inside a spent withdrawal window, run in a block after it started again), and
  without the margin it runs out of gas.
- **Unit**: `System::BlockWeights` decoded to the normal class's maximum, and refused when it is
  absent or the bytes are not that constant; `Mortality::born_at_finalized` against every finalized head, lag and expiry in a range; the
  dry run's decoding (applied, reverted, failed to dispatch); the margin in integers, rounded up.
- **`tests/restart.rs`**'s chaos run found decision 28: with eras that survive reorganisations it failed
  in each of three runs (at a different seed each time: operation references are random), and passed
  six runs of six since.

### Runs

On the `zombienet` fleet, the aledgent driver, the adapter as its own process (container, own network
namespace) with its PostgreSQL store, over its light client, dry runs through the collator's RPC
(`dry_run_endpoint`). Kreivo `0.16.9` (spec 132, mainnet today) with the workflow's default
polkadot-sdk; `0.17.0` (`3e0a5c3`, spec 133) with `polkadot-stable2606-2`.

| Run | Runtime | Commit | Step 3 | Faults (5) | Final checks | What it showed |
|---|---|---|---|---|---|---|
| 36588926409, 36588931699 | 0.16.9, main | early | 0.16.9 pass | not run | — | smoldot cannot dry-run `execute` (decision 36) |
| 36610881899 | 0.16.9 | pre-rebase | pass | 4: starve failed | pass | the dry-run node's websocket stayed closed after starving (decision 36) |
| 36615716251 | 0.17.0 | `1c6454b` | pass | 4: starve failed | pass | same; seven `swap_remove` panics, each a restart (decision 40) |
| 36622867191 | 0.17.0 | `6ab5244` | failed | 5 | pass | a withdrawal refused in a peerless spell ("Open") |
| 36622871033 | 0.16.9 | `6ab5244` | pass | 4: starve failed | pass | the same refusal, after feeding |
| 36627097148 | 0.17.0 | `97f8510` | pass | 5 | pass | green before #102 |
| 36630082167 | 0.16.9 | `97f8510` | pass | 5 | pass | green before #102 |
| **36636083178** | **0.17.0** | `facc286` | pass | 5 | pass | **final**: limit read 2 446 677 B at every head; no panic |
| **36636086877** | **0.16.9** | `facc286` | pass | 5 | pass | **final**: limit read 3 670 016 B; no panic |

The light-client probe (decision 42; one peer, script driver, step 4 skipped):

| Run | Runtime | What it showed |
|---|---|---|
| 37016565339, 37016568969 | 0.17.0, 0.16.9 | smoldot 0.19.4 (subxt) fails every `execute`; smoldot 3.3.1 answers one call of each kind byte for byte, not sixteen; a debug build of smoldot 4.0.0 panics (`proof_decode.rs:1329`) |
| 37020885522, 37020891250 | 0.17.0, 0.16.9 | the adapter on smoldot-light 2.1.0 answers one call of each kind byte for byte; every client fails eight and sixteen (`MissingProofEntry`) |
| 37025705757 | 0.17.0 | the access the proof lacks: an existing inbox entry's value (seq 29, 20) |
| **37033572534, 37033576413** | **0.17.0, 0.16.9** | **d2 (no end-of-call child-trie roots) answers all seven cases byte for byte; every other light client fails batches of two, eight, sixteen** |
| 37038254965 | 0.17.0, two boot nodes | the same results with two Kreivo peers; no panic in any client |
| 37038365582 | 0.17.0, aledgent driver | the adapter on smoldot-light 2.1.0, dry runs through the collator: step 3, all five fault cases and the final checks pass |
| **37074411553** | **0.17.0** | **decision 43: (c2), the adapter on the patched smoldot (`67ad6c37`), answers all seven cases byte for byte, batches of sixteen included; (d), unpatched, still fails the batches** |
| 37074414497 | 0.17.0, aledgent driver | decision 43: no dry-run node, every dry run over the light client: step 3, all five fault cases and the final checks pass; no peer lost outside the injected faults |
| 37077364588 | 0.17.0, two boot nodes | decision 43: the same as 37074411553 with two Kreivo peers |
| 37077814169 | 0.17.0, aledgent driver | decision 43 after rebasing onto F-028: the same as 37074414497 |

### Open

- **Over the light client the adapter loses its only peer for about twenty seconds, now and then.**
  In every run on both runtimes, the first adapter process had one or two spells in which smoldot
  answered "No node available" and the adapter refused to serve (`ERR-SettlementLayerUnavailable`
  to a withdrawal made then; run 36622867191 failed step 3 on one). Most came seconds after a batch
  was signed and submitted. Not proven, and the likely cause: smoldot validates a submitted
  transaction by a call proof from its peer, meets the same missing proof entry as a dry run
  (decisions 36, 42: validating a transaction runs it, and it writes the contract's child trie)
  and bans the peer, the only one on the suite's network. On mainnet, with many peers,
  that costs one peer, not availability; the canary should watch `unreachable` refusals. The driver
  asks again, as a client would, for up to two minutes. With the patched smoldot (decision 43, runs
  37074414497 and 37077814169) there were none outside the injected faults; more runs will tell.
- **No adapter-to-core path puts an operation in the review queue at once, with a reason.** A held call
  (decision 30) reaches review at its deadline; its reason is in the adapter's store and alert, not in
  the core's review entry. A `flag` capability for the adapter would make it immediate.
- **Deciding a batch from a pruned peer.** An adapter down longer than its peers keep block bodies, with
  a batch included meanwhile, cannot walk back to it (`find_extrinsic` needs the bodies): the step stops
  and retries, deciding nothing and releasing nothing. Deciding inclusion from the inbox and the nonce
  alone would remove the dependency.

## The format by the runtime's surface, confirmed by the layer (T-027-12, amendment 0031, 2026-10-14)

On 2026-10-14 at 21:01Z Kreivo mainnet enacted release `0.17.1`, spec 134. Its transaction extensions,
their types, its `transaction_version` (13) and its PASS crates are spec 133's, yet the adapter stopped:
`spec_versions` did not list 134 and `TxFormat::of(134)` had no entry. PR #7 added both as a stopgap.
Amendment 0031 (`REQ-SA-9`, `AC-E4.3`, `AC-E4.4`) makes the rule a requirement: decide from what an
instruction must carry, never from a version number alone, and have the layer confirm before the first
broadcast under rules not served before.

Read from mainnet's own metadata (`Metadata_metadata_at_version(15)` over `https://kreivo.io`, read
only, at blocks #38 653 511 (spec 132), #39 643 511 (spec 133) and #39 650 511 (spec 134)): specs 133
and 134 give the same surface digest, `2da44f41…`; spec 132 gives `08ee0c93…`, with
`transaction_version` 12 and no `DynamicMaxBlockWeight`.

### Decisions (T-027-12)

44. **The format is chosen by the runtime's transaction surface.** `kreivo::Surface` is three things
    read from live metadata on every step (`RpcChain::runtime`): the `transaction_version`, the
    identifiers of the transaction extensions of extension version 0 in order, and a digest of the
    types of what a transaction carries (`chain::surface`). `TxFormat` names the surface it applies
    to (`kreivo-132`: 12, `EXTENSIONS_132`, `TYPES_132`; `kreivo-133`: 13, `EXTENSIONS_133`,
    `TYPES_133`) besides the Kreivo revision it is pinned against (`kreivo-pin`, `kreivo-pin-132`,
    unchanged), and `TxFormat::by_surface` selects. The `spec_version` plays no part, so 134 needs
    no entry: PR #7's stopgap is removed, and `kreivo-133` covers 133 and 134. Signing takes the
    format chosen (`kreivo::sign(signer, format, …)`, `Chain::sign_batch(…, &Signing)`), with the
    `spec_version` and `transaction_version` of the runtime the step checked; `sign` no longer
    selects or refuses anything. A surface with no format is refused (`Refusing::UnknownSurface`,
    which replaces `SpecVersion`, `TxFormat` and `Extensions`).
45. **What the digest covers.** Each extension's identifier, its explicit type and its implicit
    type; the contracts pallet's index and its `call` call's index; and `call`'s arguments, names
    and types. A type's digest is its shape as SCALE encodes it: composites by field, in order;
    variants by index, with their names and fields; sequences, arrays with their length, tuples,
    primitives, compacts and bit sequences; field and variant names included; type paths, type
    parameters and docs excluded. Recursive types get a fixed marker. **What it does not cover**:
    anything the runtime does with the bytes rather than how they are typed, such as the order of
    the message a PASS device signs (the 132 → 133 change, decision 23), how an extension computes
    its implicit data, or what the contract does with `execute`; and the rest of the metadata
    (other calls, storage, events). Those are the confirmation's (decision 47).
46. **Not subxt's validation hash.** The brief preferred subxt's own mechanism
    (`subxt_metadata::utils::validation`) over a hand-written digest. It is not usable here: it
    XORs a composite's field hashes, on purpose (subxt encodes by field name, so field order does
    not matter to it), which makes it blind to two fields swapped, and so to a change of every byte
    the positional encoder in `kreivo.rs` writes. The digest is this crate's own
    (`chain/surface.rs`, about a hundred lines over `scale-info`'s registry), which also keeps the
    pinned values independent of subxt's releases. They are pinned in code and derived from reduced
    fixtures of mainnet's metadata (`services/settle/fixtures/surface-{132,133,134}.json`: the type
    registry cut down to the types the digest reads, renumbered; `SurfaceTypes::reduced` checks the
    reduced registry digests as the full one does). The ignored test
    `t_027_12_write_surface_fixtures` rewrites them from saved metadata when a format is added.
    Trade-off, stated plainly: field and variant names are in the digest, so a rename that moves no
    byte (or a variant added to a type the adapter encodes as `None`, such as the fee asset's
    location) stops the adapter until a release or the operator's mapping (decision 49) covers it.
    A false stop was preferred to a false start; the confirmation alone could not tell a type change
    that happens to validate from one that does not.
47. **The layer's confirmation before the first broadcast on a runtime.** (Where it validates, how a
    failed call is read, and what a "not now" answer does: amended by decision 51.) A runtime is its
    `spec_version`, its surface's fingerprint and the format chosen. Before the first transaction
    signed on a runtime the store does not record as served is broadcast, on every path that
    broadcasts something newly signed (lane batches, anchors, bindings), the runtime validates it:
    `TaggedTransactionQueue_validate_transaction(External, extrinsic, best block hash)` through
    `state_call` at the step's best head, over the adapter's own client (the light client by
    default; never `dry_run_endpoint`'s node, which is trusted for batch sizes only).
    `kreivo::transaction_validity` reads the answer:
    - **Valid**: the runtime is recorded as served (`settle_runtimes`, migration
      `0005_runtimes.sql`; `Store::confirm_runtime`, never removed), so a restart does not ask again,
      and the transaction is stored, recorded with the core and broadcast as before.
    - **Invalid for what it carries** (`BadProof`, `BadSigner`, `UnknownOrigin`,
      `IndeterminateImplicit`, `Call`, `Custom(_)`, `BadMandatory`, `MandatoryValidation`, or a
      runtime that cannot decode the bytes, which panics in the API's argument decoding): nothing
      is stored or broadcast, the calls stay acknowledged and unsigned, a `runtime:<spec_version>`
      alert is raised, and the adapter refuses to serve on that runtime
      (`Refusing::Unconfirmed { spec_version, format, error }`). The refusal is held in memory: a
      restart asks again, which is how a fix (a release, or the operator's mapping) is taken up.
      Before refusing, the runtime is read again: if it changed since the transaction was signed
      (whose `spec_version` is in its implicit data, so any upgrade makes it a `BadProof`), nothing
      is broadcast and nothing is refused; the next step signs for the new runtime.
    - **Invalid for now** (`Payment`, `Future`, `Stale`, `AncientBirthBlock`, `ExhaustsResources`,
      any `UnknownTransaction`): no evidence either way. The transaction goes out as any does (the
      pool validates it again), and the runtime is neither recorded nor refused: the next
      transaction signed on it is validated again.
    `Custom(_)` is classed with the refusals because it cannot be told which extension raised it
    and `PassAuthenticate` raises its own; that leans to a false stop, as decision 46 does.
    **What the confirmation catches that the surface does not**: a device message in another order
    (shown by the fake layer: a runtime with 133's surface whose authenticator verifies 132's order
    is refused with `BadProof`), a wrong `CheckSpecVersion`/`CheckTxVersion`/`CheckGenesis` implicit,
    a call the runtime cannot decode. **What it does not**: anything the runtime checks only on
    dispatch, after validation (the contract's own refusals are recorded per call in its inbox, as
    before).
48. **`spec_versions` gates nothing.** `AdapterConfig::spec_versions` is optional and
    informational: a file that has it still parses, and the adapter logs the list at start-up
    ("informational … gates nothing"). The README's example and the canary's template drop it.
    Start-up no longer checks it.
49. **The operator's mapping.** `adapter.formats` maps a `spec_version` to a known format's name
    (`{ "135": "kreivo-133" }`). The named format is used for that `spec_version` whatever its surface
    (explicit beats inferred), and the confirmation is still required before anything signed in it
    is broadcast (amendment 0031). A name the adapter does not know stops it at start-up
    (`AdapterConfig::unknown_formats`).
50. **The fake layer** runs Kreivo spec 133 in `kreivo-133` by default, now with its real
    `transaction_version` (13, not 1) and surface; `FakeConfig::order` is the order its
    authenticator actually verifies, independent of its surface, so a test can flip it under an
    unchanged surface. `set_spec_version` is an upgrade that changes nothing a transaction carries,
    `upgrade_to(spec, format)` one to another format, `upgrade(f)` any change. `Chain::validate`
    checks a transaction as the runtime would (decode, challenge and signature in its order, then
    nonce and fees), or answers what a test says (`answer_validations`).
51. **Validated by the runtime it was signed for; a failed call read by its kind; nothing out until
    confirmed** (review of PR #13, three findings).
    - **F1, where the validation runs.** The step read the best head at its start, but the runtime a
      transaction is signed for was read later (`Chain::runtime`, in `submit`, `anchor` and `binds`),
      at a newer best head. If Kreivo upgraded between the two reads (133 → 134), the transaction was
      signed for 134 and validated by 133 at the older head; 133 answered `BadSigner` or `BadProof`
      (`pallet-pass` answers `BadSigner` for any failed authentication, and the `spec_version` is in
      the challenge), and the check before refusing compared the runtime *now* (134) with the one
      chosen (134), found them equal, and refused with `Unconfirmed` until a restart. Now
      `RuntimeInfo::at` is the block `runtime()` read (hash and number), the chosen runtime carries
      it, and the validation runs at that block: the runtime that validates is the one signed for.
      Before a refusal is taken as one of the format, the runtime's version at that block
      (`Chain::version`, `state_getRuntimeVersion` at its hash) must equal the `spec_version` and
      `transaction_version` signed for; if not, nothing is confirmed, refused or broadcast, and the
      next step signs again. The check against the runtime *now* is gone: an upgrade after the
      validated block says nothing of what that block's runtime answered.
    - **F2, how a failed validation call is read.** An undecodable transaction was recognised by the
      text "Bad input data provided" in the error, which a Substrate node's answer carries but the
      light client's does not: live Kreivo over smoldot answers it as a bare wasm trap. It became
      `ChainError::Unavailable` on every step, with no refusal, no alert, and the step aborted
      before anchors and bindings. Now the failure is classed by its kind (`rpc::call_failure`):
      a transport error of the RPC client (unreachable, connection lost, a timeout) is
      `Transport`; an answer that the call was not run (a JSON-RPC protocol error, or a server error
      other than an execution failure: a call proof no peer served, a block it does not know, a
      runtime it could not load) is `NotRun`; an answer that the runtime ran the call and failed is
      `Executed`, recognised by each client's own name for that error variant (smoldot's
      `RuntimeCallError::Execution`, "Error during the execution of the runtime"; a Substrate node's
      `sp_blockchain::Error::Execution`, "Execution failed"), never by the trap's or the panic's
      text. `Executed` on a transaction the adapter built is `Refused` (a runtime fails
      `validate_transaction` only when it cannot decode its arguments: a wrong format); the other
      two stay `Unavailable` (`rpc::validation_failure`). Trade-off, stated plainly: the variant's
      name is still text, and a client that names an execution failure otherwise reads as
      `NotRun`, so `Unavailable`: the old failure mode, for that client only. The other way round, a
      network failure classed as an execution would be a false stop until restart, which is worse
      on the light client, whose call proofs fail routinely.
    - **F3, a "not now" answer.** `Stale`, `Future`, `Payment`, `ExhaustsResources`,
      `AncientBirthBlock` and every `UnknownTransaction` used to let the first transaction go out
      under a runtime nobody had confirmed. `REQ-SA-9` says the adapter does not serve until the
      layer confirms. Now any answer that neither confirms nor refuses broadcasts nothing: nothing
      is stored, no nonce moves, the calls stay acknowledged and unsigned, and the next step
      rebuilds the transaction (a new nonce, era and challenge) and asks again. A runtime already
      confirmed is unchanged: later transactions on it are not validated. Trade-off, stated
      plainly: a "not now" that persists (a lane short of fees on a new runtime, say) holds every
      withdrawal on that runtime with no alert of its own, only a `warn` log each step; the lane's
      fee alert, if it is short, is the signal. The runbook says so.
    The fake layer now keeps the runtime in each block's state: an upgrade applies to the best block
    from when it is made and to every block produced after, a block produced before keeps its own,
    and `validate(at)` checks against the runtime *at `at`*. `before_next_runtime_read` and
    `before_next_validation` let a test act at a precise point of a step.

### Tests (T-027-12)

- **Unit** (`chain::surface`): the fixtures' digests are the pinned `TYPES_132` and `TYPES_133`, 133 and
  134 identical, 132 different in all three parts, `Contracts::call` at (80, 6); the digest changes
  with two call arguments swapped (which subxt's hash would not see), a call or pallet index, an
  extension renamed, reordered or retyped. (`chain::kreivo`): each known surface selects its format,
  by name too, and any changed part (`transaction_version` 14, nine extensions, other types, no types,
  132's surface under 13) selects none; the validation's arguments, and every `InvalidTransaction`
  and `UnknownTransaction` classed. (`config`): `spec_versions` optional, old lists parse, `formats`
  with an unknown name reported. (`chain::rpc`, decision 51): a validation the runtime ran and failed
  is `Refused`, smoldot's bare trap and a Substrate node's answer with or without the panic's text;
  one the client did not run (a call proof not served, an unknown block, a JSON-RPC protocol error)
  and every transport failure (a timeout, a lost connection) are `Unavailable`, even one whose text
  reads like an execution failure.
- **`tests/contract.rs`**: `AC-E4.3` 133 → 134 keeps serving with no configuration and no format entry
  for 134, one validation per runtime, none after a restart; `AC-E4.4` an unchanged surface whose
  validation answers `BadProof`: nothing stored or broadcast, `Unconfirmed`, `runtime:134` alert,
  `ERR-SettlementLayerUnavailable` once declared; an order flipped under 133's surface is refused
  by the (fake) layer itself, and the operator's mapping to `kreivo-132` then settles after
  confirmation; a transient answer broadcasts, stores and signs nothing, twice, and the transaction
  goes out once the layer confirms (decision 51); an upgrade between the step's start and its signing
  is validated by the new runtime and confirmed, not refused; a refusal by another runtime than the
  one signed for refuses nothing, and the next step confirms and broadcasts; a runtime
  with unknown types is refused until mapped, then served only after the layer confirms. `T-027-07`'s
  three tests carry over (`upgrade_to` instead of `set_spec_version`; `signed_in` lists formats).
- **`tests/flow.rs`**: an unknown `transaction_version` and other extensions refuse with
  `UnknownSurface`; a `spec_versions` list gates nothing either way; an upgrade to an unknown surface
  stops submission and signing until the surface is known again.
- **`tests/anchoring.rs`**: an anchor the layer does not confirm is neither stored nor broadcast.
- **Conformance** (`crates/conformance`, `F-016`): `AC-E4.3` and `AC-E4.4` registered as service cases;
  `AC-E4.2` keeps the wrong genesis, and `ERR-SettlementLayerUnavailable` uses an unknown surface.

### Open (T-027-12)

- **Run against a live runtime, once (2026-10-15, on test).** The validation probe's request and the
  decoding of its answer are written from `sp-runtime`'s `TransactionValidity` and were tested against
  hand-built bytes. After Kreivo's runtime upgrade to spec 134, the adapter on test validated a
  transaction through the runtime, logged that it confirmed the format on `spec_version` 134
  (format `kreivo-133`), and the next anchor was signed and included. A validation runs the extensions' `validate`, not the call, so the missing child-trie proof entries
  of decisions 36 and 42 should not arise; the zombienet suite is where it can be seen. A probe that
  fails to answer (`ChainError::Unavailable`) ends the step, as a dry run's failure did before decision
  35, and nothing is signed or stored by it.
- **The aledgent driver** (`e2e/aledgent-driver`) still sets `spec_versions`, now informational. A local
  Kreivo built from a revision whose types differ from mainnet's (an asset location type, say) has
  another surface digest and is refused until its format is mapped (`adapter.formats`) or pinned.

## Follow-ups from T-027-12's review (2026-10-15)

Three findings of PR #13's review that did not block it: a "not now" that persists raised nothing
(decision 51, F3); nobody had compared how long a withdrawal waits unsigned with the page that was
meant to catch it; and the light client names a failure to start the runtime as it names a runtime
that ran and failed.

### Decisions

52. **A "not now" that persists pages, because the expiry comes long before the oldest-operation
    page.** The numbers, and where each comes from:
    - **The expiry.** `submit_outbound` gives a withdrawal's instruction the expiry *last finalized
      height the core holds + `expiry_window`* (`crates/ledger-core/src/ledger/settlement.rs`);
      `expiry_window` is the settlement configuration's, **150 heights** in
      `deploy/templates/settlement.json`, with `block_time` 2 000 ms. Kreivo mainnet's block time
      is 2.0 s at p50 (mean 2.1 s) and its finality lag 31.8 s at p50, 49.5 s at p99 (about 16 and
      25 blocks; `features/F-032-stress/NOTES-withdrawals.md`, 2026-10-04). When the core
      instructs, the best head is already a finality lag above the height the expiry counts from.
    - **The last moment to sign.** The adapter signs only an era that ends at or before the expiry
      and leaves a block above the best head (`REQ-SL-4`; `Mortality::within`, `born_at_finalized`).
      Once the best head is within a few blocks of the expiry no era fits, and the adapter
      tombstones the call (`NotExecuted::NeverSigned`, reported to the core as never signed).
      `max_era` (64) and `challenge_window` (256) bound how long a *signed* transaction lives; they
      do not shorten the unsigned wait. So a withdrawal can wait unsigned for about **150 − 16 =
      134 blocks, 4.5 minutes** (about 4.2 minutes at a p99 finality lag, 4.7 at the mean block
      time).
    - **Its end.** Once the finalized head passes the expiry, the core holds the non-inclusion
      proven (never signed, so no nonce to consume) and releases the reservation: the operation
      ends **`Released`, about 150 blocks (5 minutes) after it was instructed**. That is terminal
      and is not `Needs-Review`: the deadline the core requires is later still (after now +
      150 × 2 000 ms + `finality_margin` 120 000 ms = 7 minutes), so the review queue is never
      reached.
    - **The page.** `AledgentOldestNonTerminalOverServiceLevel` (`NFR-11`, `REQ-OP-8`) pages when
      the oldest non-terminal operation is over **3 600 000 ms for 5 minutes: 65 minutes**
      (`ops/prometheus/rules/aledgent.yml`). A withdrawal held by a "not now" is terminal after 5,
      so that page **never fires** for it; nor does `AledgentAdapterNotServing` (the adapter still
      serves), and `AledgentAnchorLag` (anchors held the same way) waits two hours. Every
      withdrawal on the runtime would end released, one after another, with nobody paged.
    So the "not now" answers now raise an alert of their own, and it pages. After
    `adapter.not_now_alert_after` (default **10**) "not now" answers in a row to the first
    transaction on a runtime not yet confirmed, the adapter raises that runtime's
    `runtime:<spec_version>` alert (`Adapter::confirmed`), the same key a refusal raises, its detail
    naming the count and the answer at that point (`Invalid(Payment)`, say; later answers are in the
    `warn` log, which counts them). The runtime's confirmation clears it, and with it any other
    runtime's `runtime:*` alert this process has not refused (one held by "not now" and upgraded
    away from, perhaps before a restart: nothing waits on it any more). A refusal of the runtime
    replaces a "not now" alert's detail with its own. The count is in memory, so a restart counts
    again from zero; an alert already raised is in the store and stays open. The new rule
    `AledgentAdapterRuntimeUnconfirmed` (`aledgent_adapter_alerts{kind="runtime"} > 0`, no `for`;
    `runtime` is now exported at zero) pages settlement-operations; it also pages a refusal
    (`Unconfirmed`) at once, five minutes before `AledgentAdapterNotServing` would.
    **Why 10.** At least one validation a step while calls wait (one per signing path with
    something to sign: each lane's batch, the anchor, a binding), and a step at least every
    `interval` (2 000 ms in `deploy/templates/settle.json`; 1 000 by default), sooner on news; a
    step itself takes about 0.4 s. Ten answers is then at most about 24 s, plus a scrape and an
    evaluation, against 4.5 minutes of signable window: the page comes with about four minutes to
    spare. Fewer would page on the odd transient (a `Future` nonce once), more would eat the
    window. **The ordering to keep**: `not_now_alert_after` × (`interval` + 0.4 s) + the scrape and
    evaluation interval must stay well under (`expiry_window` − the finality lag in blocks) ×
    the block time. An operator who lengthens `interval` or shortens `expiry_window` must check it;
    nothing checks it at start-up, since the adapter does not read the core's settlement
    configuration. Trade-off, stated plainly: the page tells someone within a minute, but the
    withdrawals instructed before it is fixed still end released when their expiry passes;
    nothing is held past it (`REQ-SL-4`), and nothing can be.

53. **The light client's failure to start the runtime reads as a refusal: documented, not told
    apart.** smoldot's `RuntimeCallError::Execution` carries three cases
    (`RuntimeCallExecutionError`): `Execution` (the runtime ran and trapped or panicked: what
    decision 51 means by a refusal), `Start` (the virtual machine could not start the function:
    `FunctionNotFound`, `NotAFunction`, `SignatureNotSupported`, `InvalidParameters`, or
    `DataSizeOverflow`) and `ForbiddenHostFunction` (the runtime called an off-chain host function,
    which a light client does not provide). Over `state_call` all three reach the adapter alike: a
    JSON-RPC server error, code -32000, whose message is the error's display, "Error during the
    execution of the runtime: …" (`light-base/src/json_rpc_service/background.rs`, at the pinned
    fork). No code, field or structured value tells them apart; only the text after the colon
    does. Decision 51 recognises the prefix and nothing else (`rpc::call_failure`), and matching
    the inner texts as well would tie the classification to wording smoldot does not promise, so
    it is not done. **The case**: `validate_transaction` on a new runtime answered with `Start` or
    `ForbiddenHostFunction` is classed `Refused`: the adapter refuses that runtime
    (`Unconfirmed`), raises `runtime:<spec_version>`, and pages. **How to recognise it**: the
    alert's detail and the `error` log "did not confirm the transaction format on its runtime"
    carry the client's message after "the runtime failed validating it (it cannot decode it):";
    a format the runtime cannot decode reads "Error during the execution of the runtime: Runtime
    has trapped: …" (or "Runtime has aborted: …"), whereas this case reads "Error during the
    execution of the runtime: " followed by one of "Function to start was not found.", "Symbol to
    start is not a function.", "Function to start uses unsupported signature.", "The types of the
    provided parameters don't match the signature.", "DataSizeOverflow" or
    "ForbiddenHostFunction". **What clears it**: a restart. The refusal is held in memory, the
    runtime is not recorded as served, and the restarted process asks the layer again; when the
    cause was the light client's, the runtime confirms then. If the same answer comes back after a
    restart, the light client cannot run that runtime's validation, which no restart fixes: record
    it as a finding, and the explicit fallback is a node's RPC (`"connection": "rpc"`, README),
    which validates in a node's executor. Trade-off, stated plainly: a false stop until restart,
    which decision 51 already preferred to a false start.

### Tests

- **`tests/contract.rs`**: `ac_e4_4_t_027_12_repeated_not_now_answers_raise_the_runtime_alert_until_confirmed`:
  nine "not now" answers (`Invalid(Stale)`) raise nothing; the tenth (`Invalid(Payment)`) raises
  `runtime:133` naming it and the count, with nothing broadcast and the adapter still serving; the
  confirmation clears it and the withdrawal settles.
  `t_027_12_a_runtime_upgraded_away_from_has_its_not_now_alert_cleared`: an alert raised on 133 is
  cleared by 134's confirmation.
- **`ops/tests/alerts.rs`**: `AledgentAdapterRuntimeUnconfirmed` pages the test route with an open
  `runtime` alert, and a healthy deployment (`runtime` at zero) pages no one.
- Decision 53 changes no code, so it has no test of its own; decision 51's tests of
  `rpc::call_failure` stand.

## Kreivo 0.18.1, spec 136 and `transaction_version` 14 (`REQ-SA-9`, 2026-10-28)

Kreivo mainnet moved to release `0.18.1` (`5cd5003`, spec 136; release `0.18.0`, spec 135, is where
`transaction_version` went to 14). Read from mainnet's own metadata
(`Metadata_metadata_at_version(15)` at #40 202 760, read only, `examples/surface_probe.rs`):
`transaction_version` 14, the same ten transaction extensions in the same order, and the same types
digest as specs 133 and 134 (`2da44f41…`, `fixtures/surface-136.json`). An adapter without this
change refuses 136 (`unknown_surface`), as decision 44 designed: a `transaction_version` no format
lists is a changed surface.

Why 14: Kreivo's release cut (kreivo#519) bumped it because two `CommunitiesManager` calls were
removed. What else changed that a transaction's metadata does not show, read off Kreivo's source:

- **The PASS crates**: `fc-traits-authn`/`fc-pallet-pass` 3.0.0-pre.2 (frame-contrib `4d16b1e`) and
  `pass-authenticators-substrate-keys` 2.0.0-pre.1 (`86674b2`), by git revision. The message the
  device signs is still `context ‖ challenge ‖ authority`; the challenger (`BlockHashChallenger`, 30
  minutes) and the extrinsic context `PassAuthenticate` hashes are unchanged.
- **Device filters**: `fc-pallet-pass` 3.x refuses a device with no call filter
  (`InvalidTransaction::Call`, which the confirmation reads as a refusal). `0.18.0`'s
  `MigrateV0ToV1` gave `Admin` (every call allowed) to every device; on mainnet at #40 202 760 the
  multi-block migration cursor is empty and all 5 484 `Pass::Devices` entries have a
  `Pass::DeviceFilters` entry.
- **The fee step** is `SkipCheckIfFeeless<ChargeUsageSubscription<ChargeAssetTxPayment>>`, which
  encodes, and is named, as `ChargeAssetTxPayment` (Kreivo's own test of it).

### Decisions

54. **A format lists the `transaction_version`s it is pinned for.** `TxFormat::transaction_versions`
    replaces `transaction_version`: `kreivo-132` is `[12]`, `kreivo-133` is `[13, 14]`. A version is
    added to a format only after its runtime's metadata gives the format's extensions and types
    (a fixture), its own crates rebuild the format's bytes (a pin crate) and mainnet validates a
    transaction signed in it. The name stays `kreivo-133`, so a runtime confirmed under an
    operator's `"formats": { "136": "kreivo-133" }` and one confirmed by the surface are recorded
    under the same name. A new format (`kreivo-136`) was the alternative; nothing it signs differs,
    and it would have made an interim operator mapping and the release disagree on the name.
    Trade-off, stated plainly: the next `transaction_version` bump stops every deployment again
    until a release lists it, which is decision 44's choice of a false stop over a false start.
55. **`kreivo-pin-136`** is `kreivo-pin` with the crates `0.18.1` locks (the Polkadot SDK crates are
    the same versions), its lock seeded from Kreivo `0.18.1`'s, and its vectors (`vectors.json`,
    `vectors-native.json`) written by `services/settle`'s encoder at spec 136 and
    `transaction_version` 14.

### Tests

- `chain::surface`: `t_027_12_the_pinned_surfaces_are_kreivos_own` reads `surface-136.json`: 14,
  and 134's extensions and types, chosen as `kreivo-133`.
- `chain::kreivo`: `t_027_12_each_known_surface_has_one_format_and_no_other_is_guessed` (14 is
  `kreivo-133`'s, 15 is no format's, 14 with 132's surface neither);
  `the_spec_136_transaction_matches_its_pinned_vector`,
  `f_045_the_spec_136_native_transaction_matches_its_pinned_vector`.
- `tests/contract.rs`: `ac_e4_3_kreivo_0_18_1_spec_136_transaction_version_14_keeps_serving`;
  `tests/native.rs`: `f_045_a_native_withdrawal_settles_on_spec_136`. The tests that used 14 as an
  unknown `transaction_version` (`tests/flow.rs`, conformance `AC-E4.4`) use 15.
- **Mainnet, read only** (`examples/surface_probe.rs` at #40 202 760, throwaway unfunded keys, nothing
  broadcast): a native transaction signed with 14 is `Invalid(Payment)` (the format accepted); with a
  signature byte flipped, `Invalid(BadProof)`; signed with 13, `Invalid(BadProof)`. A PASS
  transaction from an unregistered device is `Invalid(BadSigner)` all three ways: it decodes and
  reaches authentication, which is all a device the runtime does not know can show. The message
  order rests on `kreivo-pin-136` and the zombienet gate on `0.18.1`.
