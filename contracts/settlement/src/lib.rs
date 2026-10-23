#![cfg_attr(not(feature = "std"), no_std, no_main)]
// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT

//! # aledgent settlement contract
//!
//! The on-chain side of settlement for one aledgent deployment in V0 (`PLAN.md` `DEC-17`): an ink!
//! contract on Kreivo, uploaded by a community and instantiated through Kreivo's contracts store.
//! One instance per deployment. It keeps the rules of `pallet-aledgent-settlement` (`F-019`):
//!
//! - **One position per deployment, never per holder.** The contract's own account holds the
//!   deployment's assets; its storage keeps the position per asset as its inbox accounts for it.
//! - **Deposits carry an account reference.** A depositor calls
//!   [`deposit`](settlement::Settlement::deposit); the contract pulls the amount from its caller
//!   through Kreivo's assets API, which keeps the depositor's asset account alive, and records the
//!   opaque account reference. No approval is needed and none is left behind.
//! - **Calls carry an operation reference and get one recorded outcome each.** A key submits a batch
//!   of [`Envelope`](types::Envelope)s to [`execute`](settlement::Settlement::execute). Each call
//!   is applied or refused on its own; a refusal moves nothing, is recorded, and does not undo the
//!   others. A withdrawal to another deployment runs in a frame of its own: if that frame fails (a
//!   trap, its weight limit) it is rolled back whole and recorded as refused too. The calls are the
//!   pallet's: withdraw to an account, withdraw to another deployment, and anchor; and one more,
//!   bind, which records a book-only asset's holding as the deployment's position, once, at zero
//!   divergence (amendment 0011, `REQ-AS-8`).
//! - **Withdrawals are bounded per asset per window** (`NFR-8`). No bound, no withdrawal.
//! - **Anchors by sequence**, from one, with no gaps. Only the latest is kept, overwritten in place
//!   by the next (owner's ruling of 2026-10-11): anchoring adds no storage item but its inbox
//!   entry, which the admin can prune once it is no longer the latest. An identical replay of the
//!   latest is a no-op; an older number is refused. A deployment that ran the earlier code, which
//!   kept every anchor by number, is migrated by its next state-changing messages
//!   ([`AnchorMigration`](types::AnchorMigration)).
//! - **An ordered inbox**: every movement, anchor, binding and refusal is one entry, numbered from
//!   zero with no gaps, readable by position (`REQ-SL-9`).
//! - **Keys and an admin.** The admin rotates keys, sets bounds and prunes the inbox.
//! - **Governance, and anchor sequences** (amendment 0039, `[PENDING AMENDMENT 0039]`). A
//!   deployment's governance (an account the admin names once, then only governance replaces:
//!   on Kreivo, the licence-owning community's account, reached by a referendum) can start a new
//!   anchor sequence for a re-founded book, as a relay chain's governance sets a parachain's head
//!   ([`set_anchor_head`](settlement::Settlement::set_anchor_head)). It sets the next anchor
//!   number and names the book; it moves no value and touches no position, bound, binding, key or
//!   inbox entry, and the inbox's numbering runs on. The previous sequence's latest anchor stays in
//!   current state; earlier ones stay where they were.
//!
//! Nothing is keyed by an account reference, an operation reference or anything a holder supplies
//! (`REQ-SL-5`, `REQ-SL-10`). Storage is keyed by inbox sequence and asset only (and, until a
//! deployment of the earlier code is migrated, by anchor number).
//!
//! Value moves through Kreivo's own contract API, its chain extension: `deposit` pulls from the
//! caller, `transfer` pays out, `balance` reads the contract's holding. The one exception is a
//! withdrawal that empties the contract's asset account, which the extension cannot make (it always
//! keeps the source alive): that one is `Assets::transfer` through `call_runtime`. See [`kreivo`].

pub mod kreivo;
pub mod types;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

/// The selector of [`credit`](settlement::Settlement::credit), which one deployment calls on
/// another after moving value to it.
pub const CREDIT_SELECTOR: [u8; 4] = [0x00, 0x00, 0x01, 0x02];

/// The selector of [`execute_with_limits`](settlement::Settlement::execute_with_limits).
pub const EXECUTE_WITH_LIMITS_SELECTOR: [u8; 4] = [0x00, 0x00, 0x01, 0x03];

/// The selector of [`apply`](settlement::Settlement::apply), which the contract calls on itself
/// to run each call of a batch in a frame of its own.
pub const APPLY_SELECTOR: [u8; 4] = [0x00, 0x00, 0x01, 0x10];

#[ink::contract(env = crate::kreivo::KreivoEnvironment)]
pub mod settlement {
    use crate::kreivo::{AssetsCall, MultiAddress, RuntimeCall};
    use crate::types::*;
    use ink::codegen::Env as _;
    use ink::prelude::vec::Vec;
    use ink::storage::{traits::ManualKey, Lazy, Mapping};

    /// The contract's storage. Every field has a fixed root key, so the layout survives code
    /// upgrades and an observer can compute any key without the contract's metadata (see
    /// `features/F-026-settlement-contract/NOTES.md`, "Storage layout").
    #[ink(storage)]
    pub struct Settlement {
        /// The deployment's identity: admin, keys, attestation key.
        registration: Lazy<Registration, ManualKey<0x0000_0001>>,
        /// Where the anchors and the inbox have reached.
        head: Lazy<Head, ManualKey<0x0000_0002>>,
        /// **Legacy.** Every anchor by number, as the code before 2026-10-11 kept it. Never written
        /// now: the migration reads the highest to seed `latest_anchor` and removes them all
        /// ([`migrate_anchors`](Self::migrate_anchors)). Declared so that its key stays reserved.
        anchors: Mapping<AnchorNumber, AnchorRecord, ManualKey<0x0000_0003>>,
        /// The ordered record of every movement, anchor and refusal.
        inbox: Mapping<InboxSeq, InboxEntry, ManualKey<0x0000_0004>>,
        /// The deployment's position per asset, as the inbox accounts for it.
        positions: Mapping<AssetId, Balance, ManualKey<0x0000_0005>>,
        /// The withdrawal bound per asset, with its current window.
        bounds: Mapping<AssetId, WithdrawalWindow, ManualKey<0x0000_0006>>,
        /// The inbox position of each bound asset's `Bound` entry (`Call::Bind`). Written once per
        /// asset, never removed; its entry is never pruned.
        bound: Mapping<AssetId, InboxSeq, ManualKey<0x0000_0007>>,
        /// The latest anchor, overwritten in place by each new one. Absent until the first anchor,
        /// or, on a deployment of the earlier code, until the migration seeds it.
        latest_anchor: Lazy<LatestAnchor, ManualKey<0x0000_0008>>,
        /// The legacy `anchors` map's removal, while it runs; absent before and after.
        anchor_migration: Lazy<AnchorMigration, ManualKey<0x0000_0009>>,
        /// The anchor sequence governance started last; absent while the instance runs the
        /// sequence it began with (amendment 0039). Absence is the earlier code's state, so a
        /// deployment of it needs no migration.
        anchor_epoch: Lazy<AnchorEpoch, ManualKey<0x0000_000A>>,
        /// The deployment's governance: the only caller of `set_anchor_head`. Absent until the
        /// admin names it; from then on only governance replaces it.
        governance: Lazy<AccountId, ManualKey<0x0000_000B>>,
    }

    /// The root key of `latest_anchor`.
    pub(crate) const LATEST_ANCHOR_KEY: u32 = 0x0000_0008;
    /// The root key of `anchor_migration`.
    pub(crate) const ANCHOR_MIGRATION_KEY: u32 = 0x0000_0009;
    /// The root key of `anchor_epoch`.
    pub(crate) const ANCHOR_EPOCH_KEY: u32 = 0x0000_000A;
    /// The root key of `governance` (read by the tests that pin the layout).
    #[cfg(test)]
    pub(crate) const GOVERNANCE_KEY: u32 = 0x0000_000B;

    /// The deployment registered: the contract was instantiated.
    #[ink(event)]
    #[derive(Debug, PartialEq, Eq)]
    pub struct Registered {
        /// The admin.
        pub admin: AccountId,
        /// The keys.
        pub keys: Vec<AccountId>,
        /// The attestation key.
        pub attestation_key: Option<AttestationKey>,
    }

    /// The keys, or the attestation key, changed.
    #[ink(event)]
    #[derive(Debug, PartialEq, Eq)]
    pub struct KeysSet {
        /// The new keys.
        pub keys: Vec<AccountId>,
        /// The new attestation key.
        pub attestation_key: Option<AttestationKey>,
    }

    /// Value arrived at the position, recorded at inbox position `seq`.
    #[ink(event)]
    #[derive(Debug, PartialEq, Eq)]
    pub struct Deposited {
        /// The inbox position.
        pub seq: InboxSeq,
        /// The opaque account reference it was addressed to.
        pub account_ref: AccountRef,
        /// The asset.
        pub asset: AssetId,
        /// The amount.
        pub amount: Balance,
    }

    /// Value left the position, recorded at inbox position `seq`.
    #[ink(event)]
    #[derive(Debug, PartialEq, Eq)]
    pub struct Withdrawn {
        /// The inbox position.
        pub seq: InboxSeq,
        /// The deployment's operation reference. Also the event's topic.
        #[ink(topic)]
        pub operation_ref: OperationRef,
        /// Where it went.
        pub to: Destination,
        /// The asset.
        pub asset: AssetId,
        /// The amount.
        pub amount: Balance,
    }

    /// An anchor was stored, recorded at inbox position `seq`.
    #[ink(event)]
    #[derive(Debug, PartialEq, Eq)]
    pub struct Anchored {
        /// The inbox position.
        pub seq: InboxSeq,
        /// The deployment's operation reference. Also the event's topic.
        #[ink(topic)]
        pub operation_ref: OperationRef,
        /// The anchor's number.
        pub number: AnchorNumber,
        /// The commitment.
        pub commitment: Commitment,
    }

    /// A book-only asset was bound at zero divergence, recorded at inbox position `seq`: the
    /// deployment's holding of it is now its position (`REQ-AS-8`).
    #[ink(event)]
    #[derive(Debug, PartialEq, Eq)]
    pub struct Bound {
        /// The inbox position.
        pub seq: InboxSeq,
        /// The deployment's operation reference. Also the event's topic.
        #[ink(topic)]
        pub operation_ref: OperationRef,
        /// The asset.
        pub asset: AssetId,
        /// The amount bound.
        pub amount: Balance,
    }

    /// A call was refused and moved nothing, recorded at inbox position `seq`.
    #[ink(event)]
    #[derive(Debug, PartialEq, Eq)]
    pub struct Refused {
        /// The inbox position.
        pub seq: InboxSeq,
        /// The deployment's operation reference. Also the event's topic.
        #[ink(topic)]
        pub operation_ref: OperationRef,
        /// Why.
        pub reason: Refusal,
    }

    /// The withdrawal bound for an asset was set, changed or removed.
    #[ink(event)]
    #[derive(Debug, PartialEq, Eq)]
    pub struct WithdrawalBoundSet {
        /// The asset.
        pub asset: AssetId,
        /// The new bound, or `None` if removed.
        pub bound: Option<WithdrawalBound>,
    }

    /// The anchor records the earlier code kept by number (`anchors[1..=through]`) are all
    /// removed, and their storage deposits released to the signers of the messages that removed
    /// them; the latest is kept in place. Emitted once, by the message that removes the last one.
    #[ink(event)]
    #[derive(Debug, PartialEq, Eq)]
    pub struct AnchorsMigrated {
        /// The last anchor when the migration started.
        pub through: AnchorNumber,
    }

    /// The deployment's governance was named or replaced.
    #[ink(event)]
    #[derive(Debug, PartialEq, Eq)]
    pub struct GovernanceSet {
        /// The new governance.
        pub governance: AccountId,
    }

    /// Governance started a new anchor sequence: the next anchor is `base + 1`, and inbox entries
    /// from `from_seq` on belong to it. Nothing else changed. A verifier reads this event (or
    /// `anchor_epoch()`) to tell one book's anchors from another's on the same instance.
    #[ink(event)]
    #[derive(Debug, PartialEq, Eq)]
    pub struct AnchorHeadSet {
        /// The new sequence's number. Also the event's topic.
        #[ink(topic)]
        pub epoch: u32,
        /// The number the new sequence starts after.
        pub base: AnchorNumber,
        /// The inbox position of the new sequence's first entry.
        pub from_seq: InboxSeq,
        /// The opaque name of the book the new sequence anchors.
        pub book: Commitment,
        /// The previous sequence's latest anchor, if it had one.
        pub previous: Option<LatestAnchor>,
    }

    /// Movement, refusal and superseded anchor entries in `[from, to)` were pruned; `removed` of
    /// them existed.
    #[ink(event)]
    #[derive(Debug, PartialEq, Eq)]
    pub struct InboxPruned {
        /// The first position covered.
        pub from: InboxSeq,
        /// One past the last position covered.
        pub to: InboxSeq,
        /// How many entries were removed.
        pub removed: u64,
    }

    impl Settlement {
        /// Register the deployment. Called once, when the contracts store instantiates the contract.
        ///
        /// `admin` rotates keys, sets withdrawal bounds and prunes the inbox; `keys` submit calls.
        /// At most [`MAX_KEYS`] keys, no duplicates. An empty set is allowed: nothing can be withdrawn
        /// or anchored until the admin sets keys.
        #[ink(constructor, selector = 0x0000_0001)]
        pub fn new(
            admin: AccountId,
            keys: Vec<AccountId>,
            attestation_key: Option<AttestationKey>,
        ) -> Result<Self, Error> {
            check_keys(&keys)?;
            let mut this = Self::handle();
            this.registration.set(&Registration {
                admin,
                keys: keys.clone(),
                attestation_key,
            });
            this.head.set(&Head::default());
            this.env().emit_event(Registered {
                admin,
                keys,
                attestation_key,
            });
            Ok(this)
        }

        /// Execute a batch of calls. Key only.
        ///
        /// Each call gets exactly one outcome, returned in order and recorded in the inbox: applied,
        /// or refused with a reason. A refused call moves nothing and does not undo the calls before
        /// or after it. The message itself fails, and records nothing, only when the batch as a whole
        /// is at fault: the caller is not a key, it is empty, or it has more than [`MAX_CALLS`] calls.
        ///
        /// **A withdrawal to another deployment runs in a frame of its own** ([`apply`](Self::apply),
        /// called by the contract on itself): it is the one call that calls another contract, and
        /// the one whose invariants can trap after value moved. Its frame has its own weight limit:
        /// at most [`CALL_WEIGHT_LIMIT`], and never more than leaves the batch enough to record each
        /// such call still to come as refused ([`REFUSAL_RESERVE`] each) and to finish
        /// ([`BATCH_FINISH_RESERVE`]). A withdrawal to an account and an anchor run inline, as
        /// before: every way they can fail is a recorded refusal. If the frame traps,
        /// reverts or runs out of its limit, `pallet-contracts` rolls back everything it did (value
        /// moved included), and the batch records the call as refused with
        /// [`Refusal::CallFailed`], under its operation reference, and goes on with the next one.
        /// Only the batch's own exhaustion, too little weight or storage deposit for the batch
        /// itself, can still revert it whole. See `features/F-026-settlement-contract/NOTES.md`,
        /// decision 23.
        #[ink(message, selector = 0x0000_0100)]
        pub fn execute(&mut self, calls: Vec<Envelope>) -> Result<Vec<Outcome>, Error> {
            self.run_batch(calls, None)
        }

        /// Execute a batch of calls, each with the weight limit its frame is given. Key only.
        ///
        /// As [`execute`](Self::execute), except that call `i`'s frame, if it has one (a withdrawal
        /// to another deployment), gets `limits[i]` plus an eighth ([`call_limit`]) instead of
        /// [`CALL_WEIGHT_LIMIT`], and still never more than leaves the batch its reserve. The
        /// entries of calls that run inline are read for their count only. The submitter knows each call's weight ahead of time: a
        /// dry run of the call alone (`ContractsApi_call` of a batch of one) returns a
        /// `gas_required` that covers the call's frame and the batch's own cost besides. A part of
        /// a limit that is zero takes `CALL_WEIGHT_LIMIT`'s. `limits` must have one entry per
        /// call (`LimitsMismatch`). A call whose frame runs out of its limit is refused as
        /// `CallFailed(OutOfGas)` and cannot take what the calls after it were given.
        #[ink(message, selector = 0x0000_0103)]
        pub fn execute_with_limits(
            &mut self,
            calls: Vec<Envelope>,
            limits: Vec<Weight>,
        ) -> Result<Vec<Outcome>, Error> {
            self.run_batch(calls, Some(limits))
        }

        /// Apply one call of a batch, in a frame of its own. **The contract's own, and nobody
        /// else's**: `execute` calls it on the contract itself; any other caller is refused
        /// (`NotKey`) and nothing happens.
        ///
        /// Takes the inbox's head as the batch has it and returns it as the call leaves it, with the
        /// call's outcome. The head itself is stored by the batch, once, at its end. A refusal the
        /// call earns (a bound, a position, Kreivo's reason) is recorded here like any other. If
        /// this frame fails instead, everything it did is rolled back and the batch records the
        /// call as [`Refusal::CallFailed`].
        #[ink(message, selector = 0x0000_0110)]
        pub fn apply(&mut self, head: Head, envelope: Envelope) -> Result<(Head, Outcome), Error> {
            if self.env().caller() != self.env().account_id() {
                return Err(Error::NotKey);
            }
            let mut head = head;
            let outcome = self.apply_call(&mut head, envelope)?;
            Ok((head, outcome))
        }

        /// Send value to the deployment, addressed to one of its opaque account references.
        ///
        /// Anyone. The contract pulls `amount` of `asset` from its caller through Kreivo's assets
        /// API (`deposit`) and records the arrival. The call itself, with the amount in its
        /// arguments, is the depositor's consent: no approval is needed and none is left behind.
        /// Only the caller's own funds can be pulled: the API takes them from whoever called this
        /// contract, the same account recorded as `from`. Returns the inbox position.
        ///
        /// Kreivo keeps the depositor's asset account alive: a deposit that would leave it below
        /// the asset's minimum balance is refused (`DepositRefused`), and records nothing.
        #[ink(message, selector = 0x0000_0101)]
        pub fn deposit(
            &mut self,
            account_ref: AccountRef,
            asset: AssetId,
            amount: Balance,
        ) -> Result<InboxSeq, Error> {
            if amount == 0 {
                return Err(Error::ZeroAmount);
            }
            let from = self.env().caller();
            let position = self.credited_position(asset, amount)?;
            let mut head = self.load_head();
            head.next_seq
                .checked_add(1)
                .ok_or(Error::SequenceOverflow)?;

            // A refusal returns `Err`, which reverts the message: nothing moved, nothing recorded.
            let moved = self
                .host_deposit(asset, amount)
                .map_err(Error::DepositRefused)?;
            if moved != amount {
                return Err(Error::UnexpectedAmount);
            }
            self.record_arrival(&mut head, position, from, account_ref, asset, amount)
        }

        /// Record value another deployment has just moved to this one. Only a deployment running
        /// this contract's code can call it, and it does so only right after moving `amount` of
        /// `asset` here, in the same call. Returns the inbox position.
        #[ink(message, selector = 0x0000_0102)]
        pub fn credit(
            &mut self,
            account_ref: AccountRef,
            asset: AssetId,
            amount: Balance,
        ) -> Result<InboxSeq, Error> {
            let from = self.env().caller();
            if from == self.env().account_id() || !self.host_is_deployment(from) {
                return Err(Error::NotADeployment);
            }
            if amount == 0 {
                return Err(Error::ZeroAmount);
            }
            let position = self.credited_position(asset, amount)?;
            let mut head = self.load_head();
            head.next_seq
                .checked_add(1)
                .ok_or(Error::SequenceOverflow)?;
            self.record_arrival(&mut head, position, from, account_ref, asset, amount)
        }

        /// Replace the keys and the attestation key. Admin only.
        ///
        /// An empty set of keys is allowed: it stops every call until the admin sets new keys, which
        /// is what a compromise calls for.
        #[ink(message, selector = 0x0000_0200)]
        pub fn set_keys(
            &mut self,
            keys: Vec<AccountId>,
            attestation_key: Option<AttestationKey>,
        ) -> Result<(), Error> {
            let mut registration = self.ensure_admin()?;
            check_keys(&keys)?;
            self.migrate_anchors(&self.load_head());
            registration.keys = keys.clone();
            registration.attestation_key = attestation_key;
            self.registration.set(&registration);
            self.env().emit_event(KeysSet {
                keys,
                attestation_key,
            });
            Ok(())
        }

        /// Set, change or remove the withdrawal bound for an asset. Admin only.
        ///
        /// The first bound's window starts now. Changing a bound keeps the current window and what
        /// it has used, so tightening it cannot reset it. Removing it stops every withdrawal of the
        /// asset.
        #[ink(message, selector = 0x0000_0201)]
        pub fn set_withdrawal_bound(
            &mut self,
            asset: AssetId,
            bound: Option<WithdrawalBound>,
        ) -> Result<(), Error> {
            self.ensure_admin()?;
            self.migrate_anchors(&self.load_head());
            match bound {
                Some(bound) => {
                    if bound.period == 0 {
                        return Err(Error::InvalidBound);
                    }
                    let window = match self.bounds.get(asset) {
                        Some(window) => WithdrawalWindow { bound, ..window },
                        None => WithdrawalWindow {
                            bound,
                            started: self.env().block_number(),
                            spent: 0,
                        },
                    };
                    self.bounds.insert(asset, &window);
                }
                None => self.bounds.remove(asset),
            }
            self.env().emit_event(WithdrawalBoundSet { asset, bound });
            Ok(())
        }

        /// Remove the entries of the inbox in `[pruned_to, up_to)`: movements, refusals and
        /// anchors other than the latest. Admin only.
        ///
        /// Bind entries stay, and so does the latest anchor's entry (owner's ruling of 2026-10-11;
        /// before it, every anchor entry stayed). At most [`MAX_PRUNE`] positions per call. Returns
        /// how many entries were removed. Each removed entry's storage deposit is released to the
        /// signer of the call (`pallet-contracts`: a refund goes to the call's origin).
        #[ink(message, selector = 0x0000_0202)]
        pub fn prune_inbox(&mut self, up_to: InboxSeq) -> Result<u64, Error> {
            self.ensure_admin()?;
            let mut head = self.load_head();
            let from = head.pruned_to;
            if up_to <= from || up_to > head.next_seq {
                return Err(Error::InvalidPruneRange);
            }
            if up_to.saturating_sub(from) > MAX_PRUNE {
                return Err(Error::TooManyToPrune);
            }
            self.migrate_anchors(&head);
            let latest = self.load_latest_anchor(&head).map(|a| a.seq);
            let mut removed = 0u64;
            for seq in from..up_to {
                if Some(seq) == latest {
                    continue;
                }
                if let Some(entry) = self.inbox.get(seq) {
                    if !entry.event.is_kept() {
                        self.inbox.remove(seq);
                        removed = removed.saturating_add(1);
                    }
                }
            }
            head.pruned_to = up_to;
            self.head.set(&head);
            self.env().emit_event(InboxPruned {
                from,
                to: up_to,
                removed,
            });
            Ok(removed)
        }

        /// Name or replace the deployment's governance (amendment 0039). While none is set, the
        /// admin names it; once set, only governance replaces it, and the admin can no longer
        /// touch it. On Kreivo, governance is the licence-owning community's account, which a
        /// passed referendum reaches through `Communities::dispatch_as_account` (the call then
        /// runs signed by that account).
        ///
        /// *Trade-off, stated plainly.* Until the admin names governance, nobody can start an
        /// anchor sequence; the admin could name itself, which is why naming the community's
        /// account is one step of the deployment checklist, right after the upgrade or the
        /// instantiation.
        #[ink(message, selector = 0x0000_0400)]
        pub fn set_governance(&mut self, governance: AccountId) -> Result<(), Error> {
            let caller = self.env().caller();
            match self.governance.get() {
                Some(current) if current == caller => {}
                Some(_) => return Err(Error::NotGovernance),
                None => {
                    self.ensure_admin()?;
                }
            }
            self.migrate_anchors(&self.load_head());
            self.governance.set(&governance);
            self.env().emit_event(GovernanceSet { governance });
            Ok(())
        }

        /// Start a new anchor sequence for a re-founded book: the next anchor is `base + 1`.
        /// Governance only (amendment 0039; `OQ-68`).
        ///
        /// `expected_last` must be the head's `last_anchor` as it stands (`HeadMoved` otherwise),
        /// so a reset decided days before it is enacted cannot land after an anchor it did not
        /// know of. `book` is an opaque 32-byte name of the book the new sequence anchors (its
        /// genesis or seed digest).
        ///
        /// It writes the head's `last_anchor` (to `base`), the sequence's record and nothing else
        /// of the contract's state: no position, bound, binding, key, attestation key or
        /// governance changes, no value moves, the inbox's `next_seq` and `pruned_to` stay, and no
        /// inbox entry is written or rewritten. The previous sequence's latest anchor moves from
        /// `latest_anchor` into the sequence's record (`previous`), so it stays in current state;
        /// its inbox entry, which `prune_inbox` kept while it was the latest, is removed if a prune
        /// has already passed it (nothing else would reach it), and is prunable otherwise. Earlier
        /// anchors are where they were: in their entries until pruned, their events and the
        /// layer's finalized history.
        ///
        /// Refused while the earlier code's anchor records are unmigrated (`MigrationPending`).
        #[ink(message, selector = 0x0000_0401)]
        pub fn set_anchor_head(
            &mut self,
            expected_last: AnchorNumber,
            base: AnchorNumber,
            book: Commitment,
        ) -> Result<(), Error> {
            if self.governance.get() != Some(self.env().caller()) {
                return Err(Error::NotGovernance);
            }
            let mut head = self.load_head();
            if head.last_anchor != expected_last {
                return Err(Error::HeadMoved);
            }
            if base.checked_add(1).is_none() {
                return Err(Error::InvalidAnchorBase);
            }
            let current = self.anchor_epoch.get();
            if self.anchor_migration.get().is_some()
                || (current.is_none()
                    && head.last_anchor > 0
                    && ink::env::contains_contract_storage(&LATEST_ANCHOR_KEY).is_none())
            {
                return Err(Error::MigrationPending);
            }
            let epoch = current
                .map_or(0, |e| e.epoch)
                .checked_add(1)
                .ok_or(Error::EpochOverflow)?;
            // The latest anchor of the most recent sequence that has one: a sequence reset again
            // before its first anchor (a wrong `book`, say) carries its own `previous` forward,
            // so the last anchor of the book before stays in current state.
            let previous = self
                .load_latest_anchor(&head)
                .or_else(|| current.and_then(|e| e.previous));
            if let Some(old) = previous {
                if old.seq < head.pruned_to {
                    self.inbox.remove(old.seq);
                }
            }
            ink::env::clear_contract_storage(&LATEST_ANCHOR_KEY);
            head.last_anchor = base;
            self.head.set(&head);
            let record = AnchorEpoch {
                epoch,
                base,
                from_seq: head.next_seq,
                at: self.env().block_number(),
                book,
                previous,
            };
            self.anchor_epoch.set(&record);
            self.env().emit_event(AnchorHeadSet {
                epoch,
                base,
                from_seq: record.from_seq,
                book,
                previous,
            });
            Ok(())
        }

        /// The anchor sequence governance started last, if any (amendment 0039). `None`: the
        /// instance still runs the sequence it began with.
        #[ink(message, selector = 0x0000_0308)]
        pub fn anchor_epoch(&self) -> Option<AnchorEpoch> {
            self.anchor_epoch.get()
        }

        /// The deployment's governance, if named.
        #[ink(message, selector = 0x0000_0309)]
        pub fn governance(&self) -> Option<AccountId> {
            self.governance.get()
        }

        /// The deployment's identity.
        #[ink(message, selector = 0x0000_0300)]
        pub fn registration(&self) -> Registration {
            self.load_registration()
        }

        /// Where the anchors and the inbox have reached.
        #[ink(message, selector = 0x0000_0301)]
        pub fn head(&self) -> Head {
            self.load_head()
        }

        /// The inbox entry at `seq`, if it exists and has not been pruned.
        #[ink(message, selector = 0x0000_0302)]
        pub fn inbox_entry(&self, seq: InboxSeq) -> Option<InboxEntry> {
            self.inbox.get(seq)
        }

        /// The latest anchor, if any: the only one the contract keeps (owner's ruling of
        /// 2026-10-11). An earlier anchor is read from its `Anchored` inbox entry while it is not
        /// pruned, from its `Anchored` event, or from this read at a block before the next anchor.
        /// On a deployment of the earlier code not yet migrated, it is read from the legacy record.
        ///
        /// A new selector: `anchor_record(number)` (`0x0000_0303`) is gone, so a caller that still
        /// sends it fails to dispatch rather than reading another type.
        #[ink(message, selector = 0x0000_0307)]
        pub fn latest_anchor(&self) -> Option<LatestAnchor> {
            self.load_latest_anchor(&self.load_head())
        }

        /// The deployment's position in an asset, as its inbox accounts for it.
        #[ink(message, selector = 0x0000_0304)]
        pub fn position(&self, asset: AssetId) -> Balance {
            self.positions.get(asset).unwrap_or(0)
        }

        /// The withdrawal bound for an asset and its current window, if set.
        #[ink(message, selector = 0x0000_0305)]
        pub fn withdrawal_window(&self, asset: AssetId) -> Option<WithdrawalWindow> {
            self.bounds.get(asset)
        }

        /// The inbox position of the asset's `Bound` entry, if it was bound (`REQ-AS-8`).
        #[ink(message, selector = 0x0000_0306)]
        pub fn bound(&self, asset: AssetId) -> Option<InboxSeq> {
            self.bound.get(asset)
        }
    }

    impl Settlement {
        /// A handle on the contract's storage. Writes nothing.
        pub(crate) fn handle() -> Self {
            Self {
                registration: Lazy::new(),
                head: Lazy::new(),
                anchors: Mapping::new(),
                inbox: Mapping::new(),
                positions: Mapping::new(),
                bounds: Mapping::new(),
                bound: Mapping::new(),
                latest_anchor: Lazy::new(),
                anchor_migration: Lazy::new(),
                anchor_epoch: Lazy::new(),
                governance: Lazy::new(),
            }
        }

        fn load_registration(&self) -> Registration {
            self.registration
                .get()
                .unwrap_or_else(|| panic!("the constructor stores the registration"))
        }

        fn load_head(&self) -> Head {
            self.head.get().unwrap_or_default()
        }

        /// The latest anchor: its record in place, or, on a deployment of the earlier code that
        /// no message has migrated yet, the legacy record of `head.last_anchor`.
        ///
        /// In a sequence governance started (amendment 0039), only the record in place counts:
        /// absent, the sequence has no anchor yet and `last_anchor` is its base, which names no
        /// anchor of this sequence. The legacy records belong to the instance's first sequence.
        fn load_latest_anchor(&self, head: &Head) -> Option<LatestAnchor> {
            if head.last_anchor == 0 {
                return None;
            }
            let latest = self.latest_anchor.get();
            if latest.is_some() || ink::env::contains_contract_storage(&ANCHOR_EPOCH_KEY).is_some()
            {
                return latest;
            }
            latest.or_else(|| {
                self.anchors.get(head.last_anchor).map(|r| LatestAnchor {
                    number: head.last_anchor,
                    commitment: r.commitment,
                    at: r.at,
                    seq: r.seq,
                })
            })
        }

        /// Migrate a deployment of the code before 2026-10-11, which kept every anchor by number
        /// (`anchors[n]`), one bounded step at a time. Runs at the start of every state-changing
        /// message a key or the admin signs (`execute`, `execute_with_limits`, `set_keys`,
        /// `set_withdrawal_bound`, `prune_inbox`), after its caller is checked; never in
        /// `deposit` or `credit`, whose signers are strangers to the deployment and would
        /// receive the released deposits.
        ///
        /// - **Start**, once: no migration in progress, `head.last_anchor > 0` and no
        ///   `latest_anchor`. Seeds `latest_anchor` from `anchors[last_anchor]` and records
        ///   `AnchorMigration { next: 1, end: last_anchor }`.
        /// - **Step**: takes `anchors[next..]`, at most [`MIGRATION_STEP`] of them, and for each
        ///   one other than the latest whose inbox entry lies below the pruning watermark, removes
        ///   that entry too: the earlier code kept anchor entries when it pruned, and nothing
        ///   could reach them again. Each removed item's storage deposit goes back to the signer
        ///   of the message (`pallet-contracts` refunds to the call's origin, pro rata of the
        ///   contract's deposit per item and per byte).
        /// - **End**: with the last record, removes its own progress record and emits
        ///   [`AnchorsMigrated`]. From then on it reads two absent or unchanged cells and writes
        ///   nothing.
        ///
        /// A fresh deployment never starts it: its `latest_anchor` is written with its first
        /// anchor. Head, inbox and every other cell keep their keys and encodings.
        fn migrate_anchors(&mut self, head: &Head) {
            let mut migration = match self.anchor_migration.get() {
                Some(migration) => migration,
                None => {
                    // Never in a sequence governance started: `set_anchor_head` refuses to start
                    // one before the legacy records are gone, and a started sequence's
                    // `last_anchor` may be a base with no record in place.
                    if head.last_anchor == 0
                        || ink::env::contains_contract_storage(&LATEST_ANCHOR_KEY).is_some()
                        || ink::env::contains_contract_storage(&ANCHOR_EPOCH_KEY).is_some()
                    {
                        return;
                    }
                    let latest = self
                        .load_latest_anchor(head)
                        .unwrap_or_else(|| panic!("the earlier code stored every anchor"));
                    self.latest_anchor.set(&latest);
                    AnchorMigration {
                        next: 1,
                        end: head.last_anchor,
                    }
                }
            };
            let last = migration
                .end
                .min(migration.next.saturating_add(MIGRATION_STEP - 1));
            let latest = migration.end;
            for number in migration.next..=last {
                if let Some(record) = self.anchors.take(number) {
                    if number != latest && record.seq < head.pruned_to {
                        self.inbox.remove(record.seq);
                    }
                }
            }
            migration.next = last.saturating_add(1);
            if migration.next > migration.end {
                ink::env::clear_contract_storage(&ANCHOR_MIGRATION_KEY);
                self.env().emit_event(AnchorsMigrated {
                    through: migration.end,
                });
            } else {
                self.anchor_migration.set(&migration);
            }
        }

        fn ensure_admin(&self) -> Result<Registration, Error> {
            let registration = self.load_registration();
            if registration.admin != self.env().caller() {
                return Err(Error::NotAdmin);
            }
            Ok(registration)
        }

        /// Write the next inbox entry. The caller stores `head` afterwards.
        fn append(&mut self, head: &mut Head, event: InboxEvent) -> Result<InboxSeq, Error> {
            let seq = head.next_seq;
            head.next_seq = seq.checked_add(1).ok_or(Error::SequenceOverflow)?;
            self.inbox.insert(
                seq,
                &InboxEntry {
                    at: self.env().block_number(),
                    event,
                },
            );
            Ok(seq)
        }

        /// `execute` and `execute_with_limits`: check the batch, then run each call in its own
        /// frame, with its limit if one was given.
        fn run_batch(
            &mut self,
            calls: Vec<Envelope>,
            limits: Option<Vec<Weight>>,
        ) -> Result<Vec<Outcome>, Error> {
            let caller = self.env().caller();
            if !self.load_registration().keys.contains(&caller) {
                return Err(Error::NotKey);
            }
            if calls.is_empty() {
                return Err(Error::EmptyBatch);
            }
            if calls.len() > MAX_CALLS {
                return Err(Error::TooManyCalls);
            }
            if limits.as_ref().is_some_and(|l| l.len() != calls.len()) {
                return Err(Error::LimitsMismatch);
            }
            let mut head = self.load_head();
            // Each call writes at most one entry. Check there is room for all of them first, so that
            // nothing is applied and then undone.
            head.next_seq
                .checked_add(calls.len() as u64)
                .ok_or(Error::SequenceOverflow)?;
            // Before any call, so that an anchor below sees the latest in place.
            self.migrate_anchors(&head);

            let mut outcomes = Vec::with_capacity(calls.len());
            // Only withdrawals to another deployment get a frame of their own (the owner's
            // ruling of 2026-09-29, F-026 decision 23); the batch reserves for recording each of
            // them still to come.
            let mut pending = calls.iter().filter(|e| in_own_frame(&e.call)).count() as u64;
            for (i, envelope) in calls.into_iter().enumerate() {
                if !in_own_frame(&envelope.call) {
                    // Inline, as before: every way it can fail is a recorded refusal.
                    outcomes.push(self.apply_call(&mut head, envelope)?);
                    continue;
                }
                let limit = match &limits {
                    Some(limits) => call_limit(limits[i]),
                    None => CALL_WEIGHT_LIMIT,
                };
                let outcome = match self.isolate(head, envelope, limit, pending) {
                    Ok((next, outcome)) => {
                        head = next;
                        outcome
                    }
                    Err(failure) => self.refuse(
                        &mut head,
                        envelope.operation_ref,
                        envelope.call,
                        Refusal::CallFailed(failure),
                    )?,
                };
                outcomes.push(outcome);
                pending = pending.saturating_sub(1);
            }
            self.head.set(&head);
            Ok(outcomes)
        }

        /// Run one call of a batch in a frame of its own (`apply`). `Ok`: the head as the call left
        /// it, and its outcome. `Err`: the frame failed, or could not be given any weight, and
        /// nothing it did remains.
        ///
        /// The frame's limit is `wanted`, or less if that would not leave the batch
        /// what making the call costs it ([`CALL_FRAME_COST`]), what recording this call and each
        /// one after it as refused costs ([`REFUSAL_RESERVE`] each, `pending` of them), and what it
        /// needs to finish ([`BATCH_FINISH_RESERVE`]). So a call that runs out of its limit leaves
        /// the batch enough to record it and every call still to come.
        fn isolate(
            &mut self,
            head: Head,
            envelope: Envelope,
            wanted: Weight,
            pending: u64,
        ) -> Result<(Head, Outcome), CallFailure> {
            let before = self.host_gas_left();
            let kept = CALL_FRAME_COST
                .saturating_add(REFUSAL_RESERVE.saturating_mul(pending))
                .saturating_add(BATCH_FINISH_RESERVE);
            let limit = wanted.min(before.saturating_sub(kept));
            // A zero limit would give the frame all that is left, reserve included.
            if limit.any_zero() {
                return Err(CallFailure::OutOfGas);
            }
            match self.host_apply(limit, head, envelope) {
                Some(done) => Ok(done),
                None => {
                    // `pallet-contracts` says only that the frame trapped. It ran out if the batch
                    // spent at least the frame's limit on it: a frame that runs out has used its
                    // limit bar the one charge it could not pay, which is smaller than what the
                    // batch paid to make the call (its base cost and the code's load).
                    let used = before.saturating_sub(self.host_gas_left());
                    Err(if used.any_at_least(limit) {
                        CallFailure::OutOfGas
                    } else {
                        CallFailure::Trapped
                    })
                }
            }
        }

        /// Apply one call: check it, make it and record its outcome, applied or refused. The
        /// caller stores `head` afterwards.
        fn apply_call(&mut self, head: &mut Head, envelope: Envelope) -> Result<Outcome, Error> {
            let Envelope {
                operation_ref,
                call,
            } = envelope;
            Ok(match call {
                Call::Withdraw { to, asset, amount } => {
                    match self.try_withdraw(to, asset, amount) {
                        Ok(()) => {
                            let seq = self.append(
                                head,
                                InboxEvent::Withdrawn {
                                    operation_ref,
                                    to,
                                    asset,
                                    amount,
                                },
                            )?;
                            self.env().emit_event(Withdrawn {
                                seq,
                                operation_ref,
                                to,
                                asset,
                                amount,
                            });
                            Outcome::Applied { seq }
                        }
                        Err(reason) => self.refuse(head, operation_ref, call, reason)?,
                    }
                }
                Call::Anchor { number, commitment } => {
                    match self.try_anchor(head, number, commitment) {
                        Ok(Some(seq)) => Outcome::Applied { seq },
                        Ok(None) => {
                            let seq = self.append(
                                head,
                                InboxEvent::Anchored {
                                    operation_ref,
                                    number,
                                    commitment,
                                },
                            )?;
                            // The superseded anchor's entry, if a prune has already passed it
                            // (it kept it while it was the latest): nothing else would reach it.
                            if head.pruned_to > 0 {
                                if let Some(old) = self.load_latest_anchor(head) {
                                    if old.seq < head.pruned_to {
                                        self.inbox.remove(old.seq);
                                    }
                                }
                            }
                            // Overwritten in place: the same cell, the same size, no new deposit.
                            self.latest_anchor.set(&LatestAnchor {
                                number,
                                commitment,
                                at: self.env().block_number(),
                                seq,
                            });
                            head.last_anchor = number;
                            self.env().emit_event(Anchored {
                                seq,
                                operation_ref,
                                number,
                                commitment,
                            });
                            Outcome::Applied { seq }
                        }
                        Err(reason) => self.refuse(head, operation_ref, call, reason)?,
                    }
                }
                Call::Bind { asset, amount } => match self.try_bind(asset, amount) {
                    Ok(position) => {
                        let seq = self.append(
                            head,
                            InboxEvent::Bound {
                                operation_ref,
                                asset,
                                amount,
                            },
                        )?;
                        // Positions are removed at zero: a bind of zero over no position stores
                        // none.
                        if position != 0 {
                            self.positions.insert(asset, &position);
                        }
                        self.bound.insert(asset, &seq);
                        self.env().emit_event(Bound {
                            seq,
                            operation_ref,
                            asset,
                            amount,
                        });
                        Outcome::Applied { seq }
                    }
                    Err(reason) => self.refuse(head, operation_ref, call, reason)?,
                },
            })
        }

        /// Record a refused call.
        fn refuse(
            &mut self,
            head: &mut Head,
            operation_ref: OperationRef,
            call: Call,
            reason: Refusal,
        ) -> Result<Outcome, Error> {
            let seq = self.append(
                head,
                InboxEvent::Refused {
                    operation_ref,
                    call,
                    reason,
                },
            )?;
            self.env().emit_event(Refused {
                seq,
                operation_ref,
                reason,
            });
            Ok(Outcome::Refused { seq, reason })
        }

        /// The position after `amount` arrives.
        fn credited_position(&self, asset: AssetId, amount: Balance) -> Result<Balance, Error> {
            self.positions
                .get(asset)
                .unwrap_or(0)
                .checked_add(amount)
                .ok_or(Error::PositionOverflow)
        }

        /// Store the new position and the `Deposited` entry, and emit.
        fn record_arrival(
            &mut self,
            head: &mut Head,
            position: Balance,
            from: AccountId,
            account_ref: AccountRef,
            asset: AssetId,
            amount: Balance,
        ) -> Result<InboxSeq, Error> {
            self.positions.insert(asset, &position);
            let seq = self.append(
                head,
                InboxEvent::Deposited {
                    from,
                    account_ref,
                    asset,
                    amount,
                },
            )?;
            self.head.set(head);
            self.env().emit_event(Deposited {
                seq,
                account_ref,
                asset,
                amount,
            });
            Ok(seq)
        }

        /// Decide an anchor. `Ok(Some(seq))`: it is the latest, with this commitment, at `seq`: a
        /// replay, which writes nothing. `Ok(None)`: it is the next one; store it. The latest
        /// with another commitment is `AnchorConflict`; any other number, older than the latest
        /// or past the next, is `AnchorOutOfOrder`.
        fn try_anchor(
            &self,
            head: &Head,
            number: AnchorNumber,
            commitment: Commitment,
        ) -> Result<Option<InboxSeq>, Refusal> {
            if number != 0 && number == head.last_anchor {
                // No record: `last_anchor` is the base of a sequence governance started, which
                // names no anchor of this sequence (amendment 0039). An anchor of an earlier
                // sequence never replays into a later one.
                let Some(latest) = self.load_latest_anchor(head) else {
                    return Err(Refusal::AnchorOutOfOrder);
                };
                return if latest.commitment == commitment {
                    Ok(Some(latest.seq))
                } else {
                    Err(Refusal::AnchorConflict)
                };
            }
            if head.last_anchor.checked_add(1) != Some(number) {
                return Err(Refusal::AnchorOutOfOrder);
            }
            Ok(None)
        }

        /// Decide a binding (`REQ-AS-8`). `Ok(position)`: the position once `amount` is bound.
        ///
        /// The asset must never have been bound, and the contract's holding of it on Kreivo must be
        /// exactly its recorded position plus `amount`: the book's outstanding balance, placed in
        /// the contract's account by a plain transfer or mint (which records no deposit), and
        /// nothing else unrecorded. The check is made here, at the block that records the binding,
        /// so the equality is the layer's and final with it (`REQ-SL-3`). Nothing moves.
        fn try_bind(&self, asset: AssetId, amount: Balance) -> Result<Balance, Refusal> {
            if self.bound.contains(asset) {
                return Err(Refusal::AlreadyBound);
            }
            let held = self
                .env()
                .extension()
                .balance(asset, self.env().account_id());
            let mismatch = Refusal::HoldingMismatch { holding: held };
            let position = self
                .positions
                .get(asset)
                .unwrap_or(0)
                .checked_add(amount)
                .ok_or(mismatch)?;
            if position != held {
                return Err(mismatch);
            }
            Ok(position)
        }

        /// Check a withdrawal, move the value and charge the bound. On a refusal nothing has
        /// changed: the bound and the position are written only after the transfer succeeded.
        fn try_withdraw(
            &mut self,
            to: Destination,
            asset: AssetId,
            amount: Balance,
        ) -> Result<(), Refusal> {
            if amount == 0 {
                return Err(Refusal::ZeroAmount);
            }
            let me = self.env().account_id();
            let receiver = match to {
                Destination::Account(account) => {
                    if account == me {
                        return Err(Refusal::SelfTransfer);
                    }
                    if self.host_is_deployment(account) {
                        return Err(Refusal::DeploymentAsAccount);
                    }
                    account
                }
                Destination::Deployment { contract, .. } => {
                    if contract == me {
                        return Err(Refusal::SelfTransfer);
                    }
                    if !self.host_is_deployment(contract) {
                        return Err(Refusal::NotADeployment);
                    }
                    contract
                }
            };

            let mut window = self.bounds.get(asset).ok_or(Refusal::NoWithdrawalBound)?;
            let now = self.env().block_number();
            if now >= window.started.saturating_add(window.bound.period) {
                window.started = now;
                window.spent = 0;
            }
            window.spent = window
                .spent
                .checked_add(amount)
                .filter(|spent| *spent <= window.bound.amount)
                .ok_or(Refusal::BoundExceeded)?;
            let position = self
                .positions
                .get(asset)
                .unwrap_or(0)
                .checked_sub(amount)
                .ok_or(Refusal::InsufficientPosition)?;

            // A position leaves whole or stays at or above the asset's minimum balance. Emptying the
            // contract's asset account exactly is allowed; leaving dust is not, because `Assets`
            // would sweep the dust along with no inbox entry for it. The extension's `transfer`
            // keeps the contract alive, so it refuses anything that would leave dust; only the
            // whole holding needs `call_runtime`, with `Assets::transfer`.
            let held = self.env().extension().balance(asset, me);
            if held == amount {
                let call = RuntimeCall::Assets(AssetsCall::Transfer {
                    id: asset,
                    target: MultiAddress::Id(receiver),
                    amount,
                });
                if !self.host_call_runtime(&call) {
                    return Err(Refusal::TransferFailed);
                }
            } else {
                // A refused transfer has changed nothing: Kreivo checks that the source can pay
                // and the receiver can hold the amount before it writes anything.
                let moved = self
                    .host_transfer(asset, amount, receiver)
                    .map_err(Refusal::Kreivo)?;
                // The value has moved, and not as asked: it can be neither recorded as asked nor
                // refused. Trap: the call's frame reverts, transfer included, and the batch records
                // the call as `CallFailed(Trapped)`.
                assert!(
                    moved == amount,
                    "Kreivo moved an amount other than the one asked for"
                );
            }

            self.bounds.insert(asset, &window);
            if position == 0 {
                self.positions.remove(asset);
            } else {
                self.positions.insert(asset, &position);
            }
            if let Destination::Deployment {
                contract,
                account_ref,
            } = to
            {
                // Value must never arrive unrecorded. Trap: the call's frame reverts, transfer
                // included, and the batch records the call as `CallFailed`.
                if !self.host_credit(contract, account_ref, asset, amount) {
                    panic!("the receiving deployment did not record the value it received");
                }
            }
            Ok(())
        }
    }

    // What the contract asks of Kreivo beyond its own storage. The off-chain test environment cannot
    // dispatch runtime calls, read code hashes or call other contracts, so under `cfg(test)` these go
    // to `crate::mock`, a model of `pallet-assets` and of other deployments. The runtime call is
    // still SCALE-encoded here and decoded there, so the tests exercise the encoding. The chain
    // extension is called for real in the tests too: ink!'s off-chain engine hands its bytes to the
    // mock, which decodes them as Kreivo's runtime does.
    impl Settlement {
        /// The assets API's `deposit`: pull `amount` from the caller into the contract.
        fn host_deposit(&self, asset: AssetId, amount: Balance) -> Result<Balance, KreivoApiError> {
            // The mock's extension runs inside ink!'s engine and cannot ask it who is calling.
            #[cfg(test)]
            crate::mock::enter_frame(self.env().caller(), self.env().account_id());
            self.env().extension().deposit(asset, amount)
        }

        /// The assets API's `transfer`: pay `amount` from the contract to `beneficiary`.
        fn host_transfer(
            &self,
            asset: AssetId,
            amount: Balance,
            beneficiary: AccountId,
        ) -> Result<Balance, KreivoApiError> {
            #[cfg(test)]
            crate::mock::enter_frame(self.env().caller(), self.env().account_id());
            self.env().extension().transfer(asset, amount, beneficiary)
        }

        #[cfg(not(test))]
        fn host_call_runtime(&self, call: &RuntimeCall) -> bool {
            self.env().call_runtime(call).is_ok()
        }

        #[cfg(test)]
        fn host_call_runtime(&self, call: &RuntimeCall) -> bool {
            crate::mock::call_runtime(self.env().account_id(), &scale::Encode::encode(call))
        }

        /// Whether `who` is a contract running this contract's code.
        #[cfg(not(test))]
        fn host_is_deployment(&self, who: AccountId) -> bool {
            match (self.env().code_hash(&who), self.env().own_code_hash()) {
                (Ok(theirs), Ok(ours)) => theirs == ours,
                _ => false,
            }
        }

        #[cfg(test)]
        fn host_is_deployment(&self, who: AccountId) -> bool {
            crate::mock::same_code(self.env().account_id(), who)
        }

        /// Call `credit` on another deployment. True only if it recorded the value.
        #[cfg(not(test))]
        fn host_credit(
            &self,
            receiver: AccountId,
            account_ref: AccountRef,
            asset: AssetId,
            amount: Balance,
        ) -> bool {
            use ink::env::call::{build_call, ExecutionInput, Selector};
            let result = build_call::<Environment>()
                .call(receiver)
                .exec_input(
                    ExecutionInput::new(Selector::new(crate::CREDIT_SELECTOR))
                        .push_arg(account_ref)
                        .push_arg(asset)
                        .push_arg(amount),
                )
                .returns::<Result<InboxSeq, Error>>()
                .try_invoke();
            matches!(result, Ok(Ok(Ok(_))))
        }

        #[cfg(test)]
        fn host_credit(
            &self,
            receiver: AccountId,
            account_ref: AccountRef,
            asset: AssetId,
            amount: Balance,
        ) -> bool {
            crate::mock::credit(
                self.env().account_id(),
                receiver,
                account_ref,
                asset,
                amount,
            )
        }

        /// The weight left to this frame, both parts: `seal1::gas_left`. ink! 5 reads only the
        /// ref time (`seal0`), and proof size is what binds a batch, so this reads the host
        /// function directly. It is one of `pallet-contracts`' unstable functions, which Kreivo
        /// enables (`UnsafeUnstableInterface`, 0.16.9 and main); see NOTES, decision 23.
        #[cfg(all(not(test), target_arch = "wasm32"))]
        fn host_gas_left(&self) -> Weight {
            use pallet_contracts_uapi::{HostFn, HostFnImpl};
            let mut buffer = [0u8; 20];
            let mut output = &mut buffer[..];
            #[allow(deprecated)]
            HostFnImpl::gas_left_v1(&mut output);
            scale::Decode::decode(&mut &output[..])
                .unwrap_or_else(|_| panic!("pallet-contracts writes the weight left as a Weight"))
        }

        /// Only the WASM build runs on a chain; a native build of the contract (its metadata, its
        /// checks) never calls this.
        #[cfg(all(not(test), not(target_arch = "wasm32")))]
        fn host_gas_left(&self) -> Weight {
            unimplemented!("the weight left is read on-chain only")
        }

        #[cfg(test)]
        fn host_gas_left(&self) -> Weight {
            crate::mock::gas_left()
        }

        /// Call `apply(head, envelope)` on this contract, in a frame of its own with `limit`.
        /// `Some`: it returned. `None`: the frame trapped, ran out of its limit or reverted, and
        /// `pallet-contracts` rolled back everything it did.
        #[cfg(not(test))]
        fn host_apply(
            &self,
            limit: Weight,
            head: Head,
            envelope: Envelope,
        ) -> Option<(Head, Outcome)> {
            use ink::env::call::{build_call, ExecutionInput, Selector};
            use ink::env::{CallFlags, ReturnErrorCode};
            let result = build_call::<Environment>()
                .call(self.env().account_id())
                .ref_time_limit(limit.ref_time)
                .proof_size_limit(limit.proof_size)
                // The contract calls itself: `pallet-contracts` refuses that unless the caller's
                // frame allows reentry, for this one call.
                .call_flags(CallFlags::ALLOW_REENTRY)
                .exec_input(
                    ExecutionInput::new(Selector::new(crate::APPLY_SELECTOR))
                        .push_arg(head)
                        .push_arg(envelope),
                )
                .returns::<Result<(Head, Outcome), Error>>()
                .try_invoke();
            match result {
                Ok(Ok(Ok(done))) => Some(done),
                // `apply` returned an error, or ink! could not dispatch it: the frame reverted.
                Ok(Ok(Err(_))) | Ok(Err(_)) => None,
                // The frame trapped or ran out (`CalleeTrapped`), or never ran.
                Err(ink::env::Error::ReturnError(code)) if code != ReturnErrorCode::Unknown => None,
                // Anything else may follow a frame that completed, whose effects stand: it can be
                // neither recorded as refused nor as applied. Trap, and the batch reverts whole.
                Err(_) => panic!("a call's frame returned what the batch cannot read"),
            }
        }

        #[cfg(test)]
        fn host_apply(
            &self,
            limit: Weight,
            head: Head,
            envelope: Envelope,
        ) -> Option<(Head, Outcome)> {
            crate::mock::apply(self.env().account_id(), limit, head, envelope)
        }
    }

    /// Whether a call runs in a frame of its own: a withdrawal to another deployment, the one call
    /// that calls another contract and the one whose invariants can trap after value moved. A
    /// withdrawal to an account, an anchor and a bind run inline in the batch's frame: whatever
    /// refuses them is a recorded refusal, and only the batch's own exhaustion can stop them
    /// (decisions 23 and 24).
    pub(crate) fn in_own_frame(call: &Call) -> bool {
        matches!(
            call,
            Call::Withdraw {
                to: Destination::Deployment { .. },
                ..
            }
        )
    }

    /// The frame limit for a call the submitter gave `given` for: `given` plus an eighth, the
    /// margin for a state that moved between the dry run and inclusion; a zero part takes
    /// [`CALL_WEIGHT_LIMIT`]'s.
    pub(crate) fn call_limit(given: Weight) -> Weight {
        let part = |given: u64, default: u64| match given {
            0 => default,
            given => given.saturating_add(given / 8),
        };
        Weight::new(
            part(given.ref_time, CALL_WEIGHT_LIMIT.ref_time),
            part(given.proof_size, CALL_WEIGHT_LIMIT.proof_size),
        )
    }

    /// At most [`MAX_KEYS`] keys, no duplicates.
    fn check_keys(keys: &[AccountId]) -> Result<(), Error> {
        if keys.len() > MAX_KEYS {
            return Err(Error::TooManyKeys);
        }
        for (i, key) in keys.iter().enumerate() {
            if keys[..i].contains(key) {
                return Err(Error::DuplicateKey);
            }
        }
        Ok(())
    }

    /// The storage cells one call's frame can write, as they were before it ran (tests only):
    /// the mock rolls a failed frame back to them, as `pallet-contracts` rolls back a frame's
    /// storage transaction.
    #[cfg(test)]
    #[derive(Clone, Debug)]
    pub(crate) struct Cells {
        head: Option<Head>,
        inbox: (InboxSeq, Option<InboxEntry>),
        latest_anchor: Option<Option<LatestAnchor>>,
        asset: Option<(AssetId, Option<Balance>, Option<WithdrawalWindow>)>,
    }

    #[cfg(test)]
    impl Settlement {
        /// The cells a frame at this contract can write: its head, the inbox entry at its next
        /// position (`next_seq`), the latest anchor if it anchors, and the position and bound of
        /// `asset`.
        pub(crate) fn cells(
            &self,
            next_seq: InboxSeq,
            anchors: bool,
            asset: Option<AssetId>,
        ) -> Cells {
            Cells {
                head: self.head.get(),
                inbox: (next_seq, self.inbox.get(next_seq)),
                latest_anchor: anchors.then(|| self.latest_anchor.get()),
                asset: asset.map(|a| (a, self.positions.get(a), self.bounds.get(a))),
            }
        }

        /// Put the cells back as they were.
        pub(crate) fn restore(&mut self, cells: Cells) {
            match cells.head {
                Some(head) => self.head.set(&head),
                None => self.head.set(&Head::default()),
            }
            let (seq, entry) = cells.inbox;
            match entry {
                Some(entry) => self.inbox.insert(seq, &entry),
                None => {
                    self.inbox.remove(seq);
                    None
                }
            };
            if let Some(latest) = cells.latest_anchor {
                match latest {
                    Some(latest) => self.latest_anchor.set(&latest),
                    None => {
                        ink::env::clear_contract_storage(&LATEST_ANCHOR_KEY);
                    }
                }
            }
            if let Some((asset, position, window)) = cells.asset {
                match position {
                    Some(position) => self.positions.insert(asset, &position),
                    None => {
                        self.positions.remove(asset);
                        None
                    }
                };
                match window {
                    Some(window) => self.bounds.insert(asset, &window),
                    None => {
                        self.bounds.remove(asset);
                        None
                    }
                };
            }
        }
    }

    #[cfg(test)]
    impl Settlement {
        /// Move the inbox's head, to reach the sequence's end without writing 2^64 entries. Everything
        /// below it counts as pruned.
        pub(crate) fn force_next_seq(&mut self, next_seq: InboxSeq) {
            let mut head = self.load_head();
            head.next_seq = next_seq;
            head.pruned_to = next_seq;
            self.head.set(&head);
        }

        /// The contract's invariants over the storage it can enumerate:
        ///
        /// - with anchors, the latest is stored in place with `last_anchor`'s number, and its
        ///   inbox entry is the matching `Anchored` entry; with none, nothing is;
        /// - legacy records (`anchors[n]`) remain only while a migration runs, and then only in
        ///   its range `next..=end`; none past `last_anchor`;
        /// - `pruned_to <= next_seq`; from `pruned_to` up every inbox position is filled; below it
        ///   only bind entries, the latest anchor's entry and, while a migration runs, the entries
        ///   of the anchors it has still to remove remain;
        /// - every `Bound` entry is the one its asset's `bound` points at, so an asset is bound
        ///   at most once.
        pub(crate) fn check_invariants(&self) {
            let head = self.load_head();
            assert!(
                head.pruned_to <= head.next_seq,
                "pruned past the inbox's end"
            );
            let migration = self.anchor_migration.get();
            let latest = self.latest_anchor.get();
            match latest {
                None => assert!(
                    head.last_anchor == 0 || migration.is_none(),
                    "a migration runs with no latest anchor"
                ),
                Some(latest) => {
                    assert_eq!(
                        latest.number, head.last_anchor,
                        "the latest is not the last"
                    );
                    let entry = self
                        .inbox
                        .get(latest.seq)
                        .expect("the latest anchor's entry is never pruned");
                    match entry.event {
                        InboxEvent::Anchored {
                            number, commitment, ..
                        } => {
                            assert_eq!(number, latest.number, "anchor entry for another number");
                            assert_eq!(
                                commitment, latest.commitment,
                                "anchor entry and anchor differ"
                            );
                        }
                        other => panic!("the latest anchor points at {other:?}"),
                    }
                }
            }
            // In the instance's first sequence only: a later one starts only once the legacy
            // records are gone (`set_anchor_head`), and its numbers say nothing about them.
            let first_sequence = self.anchor_epoch.get().is_none();
            if first_sequence && head.last_anchor > 0 && migration.is_none() && latest.is_some() {
                // A fresh deployment, or a migrated one: no legacy record is left.
                for number in 1..=head.last_anchor {
                    assert!(
                        self.anchors.get(number).is_none(),
                        "legacy anchor {number} left after the migration"
                    );
                }
            }
            if let Some(m) = migration {
                assert!(m.next <= m.end && m.end <= head.last_anchor, "{m:?}");
                for number in 1..m.next {
                    assert!(
                        self.anchors.get(number).is_none(),
                        "anchor {number} migrated"
                    );
                }
            }
            assert!(
                self.anchors
                    .get(head.last_anchor.saturating_add(1))
                    .is_none(),
                "an anchor is stored past the last one"
            );
            for seq in head.pruned_to..head.next_seq {
                assert!(
                    self.inbox.get(seq).is_some(),
                    "gap in the unpruned inbox at {seq}"
                );
            }
            let latest_seq = latest.map(|a| a.seq);
            for seq in 0..head.pruned_to {
                if let Some(entry) = self.inbox.get(seq) {
                    // While a migration runs, the entries of the anchors whose records it has
                    // still to take.
                    let migrating = matches!(
                        (migration, entry.event),
                        (Some(m), InboxEvent::Anchored { number, .. }) if number >= m.next
                    );
                    assert!(
                        entry.event.is_kept() || Some(seq) == latest_seq || migrating,
                        "a pruned entry remains at {seq}"
                    );
                }
            }
            for seq in 0..head.next_seq {
                if let Some(InboxEntry {
                    event: InboxEvent::Bound { asset, .. },
                    ..
                }) = self.inbox.get(seq)
                {
                    assert_eq!(
                        self.bound.get(asset),
                        Some(seq),
                        "a Bound entry its asset does not point at"
                    );
                }
            }
            assert!(
                self.inbox.get(head.next_seq).is_none(),
                "an entry past next_seq"
            );
            // A sequence governance started (amendment 0039): it starts within the inbox, its
            // anchors are numbered from its base and written from its first position, no legacy
            // migration runs in it, and the previous sequence's latest is not the current one.
            if let Some(e) = self.anchor_epoch.get() {
                assert!(e.epoch > 0, "sequence 0 is never stored");
                assert!(
                    e.from_seq <= head.next_seq,
                    "a sequence starts past the inbox"
                );
                assert!(
                    e.base <= head.last_anchor,
                    "an anchor below the sequence's base"
                );
                assert!(
                    migration.is_none(),
                    "a legacy migration in a later sequence"
                );
                match latest {
                    None => assert_eq!(
                        head.last_anchor, e.base,
                        "anchors in the sequence but none in place"
                    ),
                    Some(latest) => {
                        assert!(latest.number > e.base, "the latest is not of this sequence");
                        assert!(latest.seq >= e.from_seq, "the latest predates its sequence");
                    }
                }
                for seq in e.from_seq..head.next_seq {
                    if let Some(InboxEntry {
                        event: InboxEvent::Anchored { number, .. },
                        ..
                    }) = self.inbox.get(seq)
                    {
                        assert!(
                            number > e.base,
                            "anchor {number} at {seq} is below the base"
                        );
                    }
                }
            }
        }
    }
}
