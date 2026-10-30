// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The oracle the kernel reads openings from, and the per-chain negativity
//! openings (`F-001` §6–§7).
//!
//! The kernel checks a flagged account by asking the oracle to open the
//! account's net after each transfer (CipherClerk's source): if no `u64` opening
//! exists, the net is negative and the transfer is refused with
//! `BalanceMustNotBeNegative`. So before a chain runs, the core registers the
//! opening of every flagged account's net after every event, on the
//! assumption that the whole chain succeeds. A net below zero gets no opening,
//! and the kernel refuses; the binding never refuses in its place.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use cipher_clerk::crypto::{Amount, Blinding};
use cipher_clerk::state::{Opening, Oracle, Reveal};
use cipher_clerk::{AccountFlags, AccountId, Layer, Transfer, TransferFlags, TransferId};
use curve25519_dalek::scalar::Scalar;

use crate::keys::blinding_of;
use crate::vault::{OpeningVault, Overlay, RowKey, VaultBackend};
use crate::working_set::{WorkingSet, is_finalize};

/// The oracle for one unit: every opening the core knows for it, matched by
/// the commitment's bytes (as CipherClerk's `OpeningsOracle` does).
///
/// An unknown commitment gets the reveal `(0, 0)`, which opens only the
/// identity point, so the kernel rejects it. Its `Debug` output hides the
/// openings.
#[derive(Clone, Default)]
pub struct OpeningBook {
    openings: BTreeMap<[u8; 32], (u64, Blinding)>,
}

impl core::fmt::Debug for OpeningBook {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "OpeningBook({} openings)", self.openings.len())
    }
}

impl OpeningBook {
    /// An empty book.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that `amount` opens to `(value, blinding)`.
    pub fn register(&mut self, amount: Amount, value: u64, blinding: Blinding) {
        self.openings.insert(amount.0, (value, blinding));
    }

    /// Record a caller-supplied opening.
    pub fn register_opening(&mut self, o: &Opening) {
        self.register(o.amount, o.value, o.blinding);
    }

    /// The opening of `amount`, if known.
    pub fn get(&self, amount: &Amount) -> Option<(u64, Blinding)> {
        self.openings.get(&amount.0).copied()
    }
}

impl Oracle for OpeningBook {
    fn reveal_amount(&mut self, amount: &Amount) -> Reveal {
        match self.openings.get(&amount.0) {
            Some((value, blinding)) => Reveal {
                value: *value,
                blinding: *blinding,
            },
            None => Reveal {
                value: 0,
                blinding: Blinding::ZERO,
            },
        }
    }
}

/// One flagged account's plaintext net after one event, as the simulation
/// expects it if the chain succeeds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetCheck {
    /// The kernel account.
    pub account: AccountId,
    /// The layer checked.
    pub layer: Layer,
    /// Its net in minor units, by the account's normal balance.
    pub net: i128,
}

/// What the simulation of one chain found.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChainPlan {
    /// Per event, the nets the kernel will check.
    pub nets: Vec<Vec<NetCheck>>,
    /// The first event whose effect the core could not compute (an amount
    /// with no known opening, or sums that would not stay in range). The
    /// kernel must refuse at or before it.
    pub unknown_from: Option<usize>,
}

fn scalar(b: &Blinding) -> Option<Scalar> {
    Scalar::from_canonical_bytes(b.0).into_option()
}

/// Walk `chain` as the kernel will, applying each event's openings to the
/// unit's vault overlay, and register in `book` the opening of every flagged
/// account's net after every event. The overlay ends in the state the chain
/// leaves if it succeeds; the caller restores a savepoint if it does not.
pub(crate) fn simulate_chain<B: VaultBackend>(
    ws: &WorkingSet,
    vault: &OpeningVault<B>,
    overlay: &mut Overlay,
    book: &mut OpeningBook,
    chain: &[Transfer],
) -> ChainPlan {
    let mut plan = ChainPlan::default();
    // Transfers added earlier in this chain, for a finalisation that refers
    // to them: the kernel stages them outside the working set.
    let mut earlier: BTreeMap<TransferId, &Transfer> = BTreeMap::new();
    for (i, t) in chain.iter().enumerate() {
        let nets = if plan.unknown_from.is_some() {
            None
        } else if is_finalize(t) {
            simulate_finalize(ws, vault, overlay, book, t, &earlier).map(|()| Vec::new())
        } else {
            simulate_entries(ws, vault, overlay, book, t)
        };
        match nets {
            Some(nets) => plan.nets.push(nets),
            None => {
                plan.unknown_from.get_or_insert(i);
                plan.nets.push(Vec::new());
            }
        }
        earlier.insert(t.id, t);
    }
    plan
}

fn simulate_entries<B: VaultBackend>(
    ws: &WorkingSet,
    vault: &OpeningVault<B>,
    overlay: &mut Overlay,
    book: &mut OpeningBook,
    t: &Transfer,
) -> Option<Vec<NetCheck>> {
    // Apply every entry's opening; remember which (account, layer) it touched.
    let mut touched: Vec<RowKey> = Vec::new();
    for e in &t.entries {
        let (value, blinding) = book.get(&e.amount)?;
        let key = RowKey::new(e.account_id, e.layer.as_byte());
        let mut sums = overlay.get(vault, &key);
        sums.add(e.direction, value, scalar(&blinding)?).ok()?;
        overlay.set(key, sums);
        if !touched.contains(&key) {
            touched.push(key);
        }
    }
    // Register the post-event net of every flagged account, per touched layer.
    let mut nets = Vec::new();
    for key in touched {
        let Some(account) = ws.peek_account(&key.account) else {
            continue; // the kernel refuses with AccountNotFound
        };
        if !account
            .flags
            .contains(AccountFlags::BALANCE_MUST_NOT_BE_NEGATIVE)
        {
            continue;
        }
        let (net, net_blinding) = overlay.get(vault, &key).net(account.normal_balance).ok()?;
        let layer = layer_of(key.layer)?;
        nets.push(NetCheck {
            account: key.account,
            layer,
            net,
        });
        if let Ok(value) = u64::try_from(net) {
            let b = blinding_of(&net_blinding);
            book.register(Amount::commit(value, &b), value, b);
        }
    }
    Some(nets)
}

fn simulate_finalize<B: VaultBackend>(
    ws: &WorkingSet,
    vault: &OpeningVault<B>,
    overlay: &mut Overlay,
    book: &OpeningBook,
    t: &Transfer,
    earlier: &BTreeMap<TransferId, &Transfer>,
) -> Option<()> {
    let pending_id = t.pending_id?;
    let pending = earlier
        .get(&pending_id)
        .copied()
        .or_else(|| ws.peek_transfer(&pending_id))?;
    let post = t.flags.contains(TransferFlags::POST_PENDING_TRANSFER);
    for e in &pending.entries {
        let (value, blinding) = book.get(&e.amount)?;
        let b = scalar(&blinding)?;
        // Release from Pending on the entry's own side.
        let key = RowKey::new(e.account_id, Layer::Pending.as_byte());
        let mut sums = overlay.get(vault, &key);
        sums.sub(e.direction, value, b).ok()?;
        overlay.set(key, sums);
        // A post applies the same delta to Settled.
        if post {
            let key = RowKey::new(e.account_id, Layer::Settled.as_byte());
            let mut sums = overlay.get(vault, &key);
            sums.add(e.direction, value, b).ok()?;
            overlay.set(key, sums);
        }
    }
    Some(())
}

fn layer_of(index: u8) -> Option<Layer> {
    match index {
        0 => Some(Layer::Settled),
        1 => Some(Layer::Pending),
        2 => Some(Layer::Encumbrance),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{AssetTag, BalanceRule, KernelAccountSpec, Role};
    use crate::build::BuiltTransfer;
    use crate::build::{build_creation, build_transfer};
    use crate::keys::KeyRing;
    use crate::posting::{EntryDraft, OperationRef, PostingDraft, PostingKind, PostingMeta};
    use crate::store::{BookDelta, LeafStore, MemStore};
    use crate::vault::MemVault;
    use alloc::vec;
    use cipher_clerk::{
        Direction, EventResult, EventStatus, Journal, JournalId, apply_account_creations,
        apply_batch,
    };
    use core::num::NonZeroU64;

    const JOURNAL: JournalId = JournalId([0x10; 16]);

    fn spec(n: u8, rule: BalanceRule) -> KernelAccountSpec {
        KernelAccountSpec::new(Role::LedgerAccount, [n; 16], AssetTag(1), rule)
    }

    fn draft(
        ordinal: u32,
        from: KernelAccountSpec,
        to: KernelAccountSpec,
        value: u64,
    ) -> PostingDraft {
        let v = NonZeroU64::new(value).unwrap();
        PostingDraft::balanced(
            PostingMeta::new(PostingKind::Transfer, OperationRef([0x33; 16]), ordinal),
            vec![EntryDraft::debit(from, v), EntryDraft::credit(to, v)],
        )
        .unwrap()
    }

    /// Create an issuance account and two flagged customers, issue 100 to C,
    /// then try to move 150 and 100 from C to D. Returns every kernel result,
    /// the unit's delta, and the nets the simulation expected.
    fn run(keys: &KeyRing) -> (Vec<EventResult>, BookDelta, Vec<Vec<NetCheck>>) {
        let mut store = MemStore::new();
        let mut boot = BookDelta::default();
        boot.journals
            .insert(JOURNAL, Journal::new(JOURNAL, keys.registrar_key(), 0));
        store.commit(0, &boot).unwrap();
        let vault = OpeningVault::open(MemVault::new()).unwrap();

        let issuer = spec(1, BalanceRule::Unchecked);
        let c = spec(2, BalanceRule::NeverNegative);
        let d = spec(3, BalanceRule::NeverNegative);
        let creations: Vec<_> = [issuer, c, d]
            .iter()
            .map(|s| build_creation(keys, JOURNAL, s))
            .collect();
        let chains = [
            draft(0, issuer, c, 100),
            draft(1, c, d, 150),
            draft(2, c, d, 100),
        ];
        let built: Vec<_> = chains
            .iter()
            .map(|p| build_transfer(keys, JOURNAL, p, false))
            .collect();
        let transfers: Vec<Transfer> = built.iter().map(|b| b.transfer.clone()).collect();

        let mut ws = WorkingSet::load_for(&store, &creations, &transfers).unwrap();
        let mut book = OpeningBook::new();
        let mut results = apply_account_creations(&mut ws, &creations, &mut book, 10);
        let mut overlay = Overlay::default();
        let mut all_nets = Vec::new();
        for b in &built {
            for o in &b.openings {
                book.register_opening(o);
            }
            let saved = overlay.clone();
            let plan = simulate_chain(
                &ws,
                &vault,
                &mut overlay,
                &mut book,
                core::slice::from_ref(&b.transfer),
            );
            let r = apply_batch(&mut ws, core::slice::from_ref(&b.transfer), &mut book, 20);
            if r[0].status != EventStatus::Created {
                overlay = saved;
            }
            results.extend(r);
            all_nets.extend(plan.nets);
        }
        (results, ws.into_delta().unwrap(), all_nets)
    }

    #[test]
    fn req_ker_4_a_flagged_overdraw_is_refused_by_the_kernel() {
        let (results, _, nets) = run(&KeyRing::from_root([5; 32]));
        let statuses: Vec<EventStatus> = results.iter().map(|r| r.status).collect();
        assert_eq!(
            statuses,
            vec![
                EventStatus::Created,
                EventStatus::Created,
                EventStatus::Created,
                EventStatus::Created,
                EventStatus::BalanceMustNotBeNegative,
                EventStatus::Created,
            ]
        );
        // The overdraw's plaintext net was negative; the others were not.
        let c = spec(2, BalanceRule::NeverNegative).id();
        assert_eq!(
            nets[0],
            vec![NetCheck {
                account: c,
                layer: Layer::Settled,
                net: 100
            }]
        );
        assert!(nets[1].iter().any(|n| n.account == c && n.net == -50));
        assert!(nets[2].iter().any(|n| n.account == c && n.net == 0));
    }

    #[test]
    fn req_rt_3_the_same_unit_with_the_same_inputs_gives_identical_bytes() {
        let a = run(&KeyRing::from_root([6; 32]));
        let b = run(&KeyRing::from_root([6; 32]));
        assert_eq!(a.0, b.0);
        assert_eq!(
            crate::store::codec::encode_delta(&a.1),
            crate::store::codec::encode_delta(&b.1)
        );
        // Another core root gives other keys, so other bytes.
        let c = run(&KeyRing::from_root([7; 32]));
        assert_ne!(
            crate::store::codec::encode_delta(&a.1),
            crate::store::codec::encode_delta(&c.1)
        );
    }
    /// A transfer of `value` from `from` to `to`, with every entry on `layer` and `flags` set,
    /// signed again: the binding builds Settled-layer postings only, and these are the
    /// kernel's own pending and encumbrance moves, which the simulation must follow.
    fn on_layer(
        keys: &KeyRing,
        ordinal: u32,
        from: KernelAccountSpec,
        to: KernelAccountSpec,
        value: u64,
        layer: Layer,
        flags: TransferFlags,
    ) -> crate::build::BuiltTransfer {
        let mut built = build_transfer(keys, JOURNAL, &draft(ordinal, from, to, value), false);
        for e in &mut built.transfer.entries {
            e.layer = layer;
        }
        built.transfer.flags = flags;
        built.transfer.signatures = crate::build::sign_debits(keys, &built.transfer);
        built
    }

    /// A finalisation of the pending transfer `of`: no entries, signed by `payer`.
    fn finalize(
        keys: &KeyRing,
        id: u8,
        of: &Transfer,
        payer: &KernelAccountSpec,
        flag: TransferFlags,
    ) -> Transfer {
        let mut t = Transfer::draft(TransferId([id; 16]), JOURNAL, Vec::new())
            .with_flags(flag)
            .finalizing(of.id);
        t.signatures = vec![keys.sign_account(&payer.id(), &t.signing_payload())];
        t
    }

    /// What the simulation of `chain` found, what the kernel did with it, and the working set
    /// and overlay both left behind.
    struct Followed {
        plan: ChainPlan,
        results: Vec<EventResult>,
        ws: WorkingSet,
        overlay: Overlay,
    }

    /// Create `issuer` (unchecked) and `holder` (flagged), then simulate `chain` as the book
    /// does (the openings registered first), then apply it to the kernel.
    fn follow(keys: &KeyRing, chain: &[crate::build::BuiltTransfer]) -> Followed {
        let mut store = MemStore::new();
        let mut boot = BookDelta::default();
        boot.journals
            .insert(JOURNAL, Journal::new(JOURNAL, keys.registrar_key(), 0));
        store.commit(0, &boot).unwrap();
        let vault = OpeningVault::open(MemVault::new()).unwrap();
        let creations: Vec<_> = [
            spec(1, BalanceRule::Unchecked),
            spec(2, BalanceRule::NeverNegative),
        ]
        .iter()
        .map(|s| build_creation(keys, JOURNAL, s))
        .collect();
        let transfers: Vec<Transfer> = chain.iter().map(|b| b.transfer.clone()).collect();
        let mut ws = WorkingSet::load_for(&store, &creations, &transfers).unwrap();
        let mut book = OpeningBook::new();
        let created = apply_account_creations(&mut ws, &creations, &mut book, 10);
        assert!(created.iter().all(|r| r.status == EventStatus::Created));
        for b in chain {
            for o in &b.openings {
                book.register_opening(o);
            }
        }
        let mut overlay = Overlay::default();
        let plan = simulate_chain(&ws, &vault, &mut overlay, &mut book, &transfers);
        let results = apply_batch(&mut ws, &transfers, &mut book, 20);
        Followed {
            plan,
            results,
            ws,
            overlay,
        }
    }

    /// Every row the simulation changed opens the commitments the kernel stored for it.
    fn assert_overlay_opens_the_kernels_balances(f: &Followed) {
        assert!(!f.overlay.changed.is_empty());
        for (row, sums) in &f.overlay.changed {
            let account =
                f.ws.peek_account(&row.account)
                    .expect("the account is loaded");
            let pair = account.balances[usize::from(row.layer)];
            assert!(
                sums.opens(&pair),
                "row {row:?} does not open the kernel's balance"
            );
        }
    }

    fn created(f: &Followed) {
        let statuses: Vec<EventStatus> = f.results.iter().map(|r| r.status).collect();
        assert!(
            statuses.iter().all(|s| *s == EventStatus::Created),
            "the kernel took every event: {statuses:?}"
        );
    }

    /// `REQ-KER-3`: a pending transfer posted is released from Pending and applied to
    /// Settled, and the vault's openings, followed through the simulation alone, open exactly
    /// the commitments the kernel then holds on both layers (the blinding taken off, not added).
    #[test]
    fn req_ker_3_a_posted_pending_transfer_leaves_openings_that_open_the_kernels_balances() {
        let keys = KeyRing::from_root([0x61; 32]);
        let (issuer, holder) = (
            spec(1, BalanceRule::Unchecked),
            spec(2, BalanceRule::NeverNegative),
        );
        let funding = on_layer(
            &keys,
            0,
            issuer,
            holder,
            100,
            Layer::Settled,
            TransferFlags::NONE,
        );
        let pending = on_layer(
            &keys,
            1,
            issuer,
            holder,
            30,
            Layer::Pending,
            TransferFlags::PENDING,
        );
        let post = finalize(
            &keys,
            0x71,
            &pending.transfer,
            &issuer,
            TransferFlags::POST_PENDING_TRANSFER,
        );
        let chain = [
            funding,
            pending.clone(),
            BuiltTransfer {
                transfer: post,
                openings: Vec::new(),
            },
        ];
        let f = follow(&keys, &chain);
        created(&f);
        assert_eq!(f.plan.unknown_from, None, "the finalisation is computable");
        assert_overlay_opens_the_kernels_balances(&f);
        // The Pending row of the holder is empty again, and its Settled row holds both credits.
        let (pending_row, settled_row) = (
            f.overlay.changed[&RowKey::new(holder.id(), Layer::Pending.as_byte())],
            f.overlay.changed[&RowKey::new(holder.id(), Layer::Settled.as_byte())],
        );
        assert_eq!(pending_row.net(Direction::Credit).unwrap().0, 0);
        assert_eq!(settled_row.net(Direction::Credit).unwrap().0, 130);
    }

    /// `REQ-KER-3`: a pending transfer voided is released from Pending and nothing is
    /// applied to Settled.
    #[test]
    fn req_ker_3_a_voided_pending_transfer_leaves_openings_that_open_the_kernels_balances() {
        let keys = KeyRing::from_root([0x62; 32]);
        let (issuer, holder) = (
            spec(1, BalanceRule::Unchecked),
            spec(2, BalanceRule::NeverNegative),
        );
        let funding = on_layer(
            &keys,
            0,
            issuer,
            holder,
            100,
            Layer::Settled,
            TransferFlags::NONE,
        );
        let pending = on_layer(
            &keys,
            1,
            issuer,
            holder,
            25,
            Layer::Pending,
            TransferFlags::PENDING,
        );
        let void = finalize(
            &keys,
            0x72,
            &pending.transfer,
            &issuer,
            TransferFlags::VOID_PENDING_TRANSFER,
        );
        let chain = [
            funding,
            pending.clone(),
            BuiltTransfer {
                transfer: void,
                openings: Vec::new(),
            },
        ];
        let f = follow(&keys, &chain);
        created(&f);
        assert_eq!(f.plan.unknown_from, None);
        assert_overlay_opens_the_kernels_balances(&f);
        let pending_row = f.overlay.changed[&RowKey::new(holder.id(), Layer::Pending.as_byte())];
        let settled_row = f.overlay.changed[&RowKey::new(holder.id(), Layer::Settled.as_byte())];
        assert_eq!(pending_row.net(Direction::Credit).unwrap().0, 0);
        assert_eq!(settled_row.net(Direction::Credit).unwrap().0, 100);
    }

    /// `REQ-KER-4`, `ERR-InsufficientAvailable`: the nets the simulation expects name the layer
    /// they are on, for each of the three layers; an entry on a layer the simulation cannot name
    /// would leave the chain uncomputable.
    #[test]
    fn req_ker_4_the_simulated_nets_name_the_settled_pending_and_encumbrance_layers() {
        let keys = KeyRing::from_root([0x63; 32]);
        let (issuer, holder) = (
            spec(1, BalanceRule::Unchecked),
            spec(2, BalanceRule::NeverNegative),
        );
        let chain = [
            on_layer(
                &keys,
                0,
                issuer,
                holder,
                100,
                Layer::Settled,
                TransferFlags::NONE,
            ),
            on_layer(
                &keys,
                1,
                issuer,
                holder,
                30,
                Layer::Pending,
                TransferFlags::PENDING,
            ),
            on_layer(
                &keys,
                2,
                issuer,
                holder,
                20,
                Layer::Encumbrance,
                TransferFlags::NONE,
            ),
        ];
        let f = follow(&keys, &chain);
        created(&f);
        assert_eq!(f.plan.unknown_from, None);
        let id = holder.id();
        assert_eq!(
            f.plan.nets,
            vec![
                vec![NetCheck {
                    account: id,
                    layer: Layer::Settled,
                    net: 100
                }],
                vec![NetCheck {
                    account: id,
                    layer: Layer::Pending,
                    net: 30
                }],
                vec![NetCheck {
                    account: id,
                    layer: Layer::Encumbrance,
                    net: 20
                }],
            ]
        );
        assert_overlay_opens_the_kernels_balances(&f);
    }
}
