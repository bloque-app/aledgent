// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The openings vault (`F-001` §6, `DEC-11`, `REQ-RT-6`).
//!
//! Amounts in the book are Pedersen commitments; the core holds every opening
//! (`DEC-11`, L0). The vault keeps, per kernel account and layer, the
//! plaintext running sums of value and blinding on each side. From them the
//! core knows every balance in minor units, and can open the net the kernel
//! asks about when it checks a flagged account.
//!
//! Openings are private inputs (`REQ-RT-6`). The vault is a store of its own,
//! kept apart from the leaf store; nothing in it is written into the book, the
//! delta log or any export, and its `Debug` output hides every sum.
//!
//! Durability follows the book's commit order (`F-001` §9): the vault commits
//! sequence number `seq` first, then the leaf store commits it. The vault
//! keeps the undo record of its last commit, so after a crash between the two
//! it can be rolled back to the store's last committed `seq`.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

#[cfg(feature = "std")]
pub mod file;
#[cfg(feature = "std")]
pub use file::FileVault;

use cipher_clerk::crypto::pedersen_h;
use cipher_clerk::{AccountId, BalancePair, Direction};
use curve25519_dalek::constants::RISTRETTO_BASEPOINT_TABLE;
use curve25519_dalek::scalar::Scalar;

/// One vault row: a kernel account on one layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RowKey {
    /// The kernel account.
    pub account: AccountId,
    /// The layer index (`Layer::as_index`).
    pub layer: u8,
}

impl RowKey {
    /// The row of `account` on the layer with index `layer`.
    pub const fn new(account: AccountId, layer: u8) -> Self {
        Self { account, layer }
    }

    /// The 17-byte encoding: account id, then layer.
    pub fn to_bytes(&self) -> [u8; 17] {
        let mut out = [0u8; 17];
        out[..16].copy_from_slice(&self.account.0);
        out[16] = self.layer;
        out
    }

    /// Decode a 17-byte row key.
    pub fn from_bytes(b: &[u8; 17]) -> Self {
        let mut id = [0u8; 16];
        id.copy_from_slice(&b[..16]);
        Self {
            account: AccountId(id),
            layer: b[16],
        }
    }
}

/// The plaintext opening of one side's running sum.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SideSum {
    /// Sum of the values, in minor units. Wide, so it never wraps.
    pub value: u128,
    /// Sum of the blindings.
    pub blinding: Scalar,
}

impl SideSum {
    /// Nothing on this side.
    pub const ZERO: Self = Self {
        value: 0,
        blinding: Scalar::ZERO,
    };

    /// The commitment this sum opens: `value·G + blinding·H`.
    pub fn commitment(&self) -> [u8; 32] {
        let p =
            &Scalar::from(self.value) * RISTRETTO_BASEPOINT_TABLE + self.blinding * pedersen_h();
        p.compress().to_bytes()
    }
}

/// The plaintext openings of one row: debit and credit running sums.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct LayerSums {
    /// The debit side.
    pub dr: SideSum,
    /// The credit side.
    pub cr: SideSum,
}

impl Default for LayerSums {
    fn default() -> Self {
        Self::ZERO
    }
}

impl core::fmt::Debug for LayerSums {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("LayerSums(<private>)")
    }
}

/// A row whose sums would wrap or go below zero: the vault and the book
/// disagree, which is a fault, never a balance.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SumFault;

impl LayerSums {
    /// Nothing on either side.
    pub const ZERO: Self = Self {
        dr: SideSum::ZERO,
        cr: SideSum::ZERO,
    };

    fn side_mut(&mut self, side: Direction) -> &mut SideSum {
        match side {
            Direction::Debit => &mut self.dr,
            Direction::Credit => &mut self.cr,
        }
    }

    /// Add an entry's opening to one side.
    pub fn add(&mut self, side: Direction, value: u64, blinding: Scalar) -> Result<(), SumFault> {
        let s = self.side_mut(side);
        s.value = s.value.checked_add(u128::from(value)).ok_or(SumFault)?;
        s.blinding += blinding;
        Ok(())
    }

    /// Take an entry's opening back off one side (a pending release).
    pub fn sub(&mut self, side: Direction, value: u64, blinding: Scalar) -> Result<(), SumFault> {
        let s = self.side_mut(side);
        s.value = s.value.checked_sub(u128::from(value)).ok_or(SumFault)?;
        s.blinding -= blinding;
        Ok(())
    }

    /// The net in minor units and its blinding, as the kernel computes it for
    /// an account with this normal balance: `cr − dr` when credit-normal,
    /// `dr − cr` when debit-normal.
    pub fn net(&self, normal: Direction) -> Result<(i128, Scalar), SumFault> {
        let (plus, minus) = match normal {
            Direction::Credit => (&self.cr, &self.dr),
            Direction::Debit => (&self.dr, &self.cr),
        };
        let p = i128::try_from(plus.value).map_err(|_| SumFault)?;
        let m = i128::try_from(minus.value).map_err(|_| SumFault)?;
        Ok((p - m, plus.blinding - minus.blinding))
    }

    /// Whether these sums open the account's stored commitments on this layer.
    pub fn opens(&self, pair: &BalancePair) -> bool {
        self.dr.commitment() == pair.dr.0 && self.cr.commitment() == pair.cr.0
    }

    /// The 96-byte encoding: dr value, dr blinding, cr value, cr blinding.
    pub fn to_bytes(&self) -> [u8; 96] {
        let mut out = [0u8; 96];
        out[..16].copy_from_slice(&self.dr.value.to_be_bytes());
        out[16..48].copy_from_slice(self.dr.blinding.as_bytes());
        out[48..64].copy_from_slice(&self.cr.value.to_be_bytes());
        out[64..].copy_from_slice(self.cr.blinding.as_bytes());
        out
    }

    /// Decode the 96-byte encoding; `None` if a blinding is not canonical.
    pub fn from_bytes(b: &[u8; 96]) -> Option<Self> {
        let side = |v: &[u8], s: &[u8]| -> Option<SideSum> {
            let mut value = [0u8; 16];
            value.copy_from_slice(v);
            let mut bl = [0u8; 32];
            bl.copy_from_slice(s);
            Some(SideSum {
                value: u128::from_be_bytes(value),
                blinding: Scalar::from_canonical_bytes(bl).into_option()?,
            })
        };
        Some(Self {
            dr: side(&b[..16], &b[16..48])?,
            cr: side(&b[48..64], &b[64..])?,
        })
    }
}

/// How to reverse one vault commit.
#[derive(Clone, PartialEq, Eq)]
pub struct Undo {
    /// The commit this undoes.
    pub seq: u64,
    /// The vault's sequence number before it.
    pub prev_seq: Option<u64>,
    /// Each row the commit changed, with its value before (`None`: absent).
    pub rows: Vec<(RowKey, Option<LayerSums>)>,
}

impl core::fmt::Debug for Undo {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Undo")
            .field("seq", &self.seq)
            .field("prev_seq", &self.prev_seq)
            .field("rows", &self.rows.len())
            .finish()
    }
}

/// Everything a vault backend persists.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VaultImage {
    /// The last committed sequence number.
    pub seq: Option<u64>,
    /// Every row.
    pub rows: BTreeMap<RowKey, LayerSums>,
    /// How to reverse the last commit.
    pub undo: Option<Undo>,
}

/// Why the vault failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VaultError {
    /// Vault commits are numbered 0, 1, 2… like the store's.
    SeqOutOfOrder {
        /// The sequence number the vault expects next.
        expected: u64,
        /// The one it was given.
        got: u64,
    },
    /// There is no commit to reverse.
    NothingToRevert,
    /// The storage engine failed.
    Io(String),
    /// Stored bytes do not decode.
    Corrupt(String),
}

/// Where the vault persists (`F-001` §9). `MemVault` keeps the image in
/// memory; `FileVault` in a file of its own, apart from the leaf store.
pub trait VaultBackend {
    /// Read the whole persisted image.
    fn read(&self) -> Result<VaultImage, VaultError>;

    /// Persist commit `undo.seq`: write `rows` and keep `undo`. Atomic.
    fn write(&mut self, rows: &[(RowKey, LayerSums)], undo: &Undo) -> Result<(), VaultError>;

    /// Persist the reversal of the last commit, described by `undo`. Atomic.
    fn revert(&mut self, undo: &Undo) -> Result<(), VaultError>;
}

/// A vault backend in memory. Dropping the vault but keeping the backend is
/// how tests simulate a restart.
#[derive(Clone, Debug, Default)]
pub struct MemVault {
    image: VaultImage,
}

impl MemVault {
    /// An empty backend.
    pub fn new() -> Self {
        Self::default()
    }
}

impl VaultBackend for MemVault {
    fn read(&self) -> Result<VaultImage, VaultError> {
        Ok(self.image.clone())
    }

    fn write(&mut self, rows: &[(RowKey, LayerSums)], undo: &Undo) -> Result<(), VaultError> {
        for (k, v) in rows {
            self.image.rows.insert(*k, *v);
        }
        self.image.seq = Some(undo.seq);
        self.image.undo = Some(undo.clone());
        Ok(())
    }

    fn revert(&mut self, undo: &Undo) -> Result<(), VaultError> {
        apply_undo(&mut self.image.rows, undo);
        self.image.seq = undo.prev_seq;
        self.image.undo = None;
        Ok(())
    }
}

pub(crate) fn apply_undo(rows: &mut BTreeMap<RowKey, LayerSums>, undo: &Undo) {
    for (k, before) in &undo.rows {
        match before {
            Some(v) => {
                rows.insert(*k, *v);
            }
            None => {
                rows.remove(k);
            }
        }
    }
}

/// The openings vault over a backend.
pub struct OpeningVault<B: VaultBackend> {
    backend: B,
    rows: BTreeMap<RowKey, LayerSums>,
    seq: Option<u64>,
    undo: Option<Undo>,
}

impl<B: VaultBackend> core::fmt::Debug for OpeningVault<B> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OpeningVault")
            .field("seq", &self.seq)
            .field("rows", &self.rows.len())
            .finish()
    }
}

impl<B: VaultBackend> OpeningVault<B> {
    /// Open the vault over what the backend has persisted.
    pub fn open(backend: B) -> Result<Self, VaultError> {
        let image = backend.read()?;
        Ok(Self {
            backend,
            rows: image.rows,
            seq: image.seq,
            undo: image.undo,
        })
    }

    /// The last committed sequence number.
    pub fn last_committed(&self) -> Option<u64> {
        self.seq
    }

    /// The committed sums of a row (zero if the row was never touched).
    pub fn sums(&self, key: &RowKey) -> LayerSums {
        self.rows.get(key).copied().unwrap_or(LayerSums::ZERO)
    }

    /// Every committed row, in key order.
    pub fn rows(&self) -> impl Iterator<Item = (&RowKey, &LayerSums)> {
        self.rows.iter()
    }

    /// The backend.
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Give the backend back, for a simulated restart.
    pub fn into_backend(self) -> B {
        self.backend
    }

    /// Commit `changes` as sequence number `seq`, first to the backend, then
    /// in memory.
    pub fn commit(
        &mut self,
        seq: u64,
        changes: &BTreeMap<RowKey, LayerSums>,
    ) -> Result<(), VaultError> {
        let expected = self.seq.map_or(0, |s| s + 1);
        if seq != expected {
            return Err(VaultError::SeqOutOfOrder { expected, got: seq });
        }
        let undo = Undo {
            seq,
            prev_seq: self.seq,
            rows: changes
                .keys()
                .map(|k| (*k, self.rows.get(k).copied()))
                .collect(),
        };
        let rows: Vec<(RowKey, LayerSums)> = changes.iter().map(|(k, v)| (*k, *v)).collect();
        self.backend.write(&rows, &undo)?;
        for (k, v) in rows {
            self.rows.insert(k, v);
        }
        self.seq = Some(seq);
        self.undo = Some(undo);
        Ok(())
    }

    /// Overwrite a row in memory only: lets tests make the vault lie.
    #[cfg(test)]
    pub(crate) fn tamper(&mut self, key: RowKey, sums: LayerSums) {
        self.rows.insert(key, sums);
    }

    /// Reverse the last commit, in the backend and in memory. Only the last
    /// one can be reversed: the vault is never more than one commit ahead of
    /// the store.
    pub fn revert_last(&mut self) -> Result<(), VaultError> {
        let undo = self.undo.take().ok_or(VaultError::NothingToRevert)?;
        if let Err(e) = self.backend.revert(&undo) {
            self.undo = Some(undo);
            return Err(e);
        }
        apply_undo(&mut self.rows, &undo);
        self.seq = undo.prev_seq;
        Ok(())
    }
}

/// A unit's changes to the vault, on top of its committed rows. Cloned to
/// take a savepoint.
#[derive(Clone, Default)]
pub(crate) struct Overlay {
    pub(crate) changed: BTreeMap<RowKey, LayerSums>,
}

impl Overlay {
    pub(crate) fn get<B: VaultBackend>(&self, vault: &OpeningVault<B>, key: &RowKey) -> LayerSums {
        self.changed
            .get(key)
            .copied()
            .unwrap_or_else(|| vault.sums(key))
    }

    pub(crate) fn set(&mut self, key: RowKey, sums: LayerSums) {
        self.changed.insert(key, sums);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cipher_clerk::crypto::{Amount, Blinding};

    fn sc(n: u64) -> Scalar {
        Scalar::from(n)
    }

    #[test]
    fn sums_open_the_commitments_they_track() {
        let mut s = LayerSums::ZERO;
        s.add(Direction::Credit, 10, sc(3)).unwrap();
        s.add(Direction::Credit, 5, sc(4)).unwrap();
        s.add(Direction::Debit, 7, sc(2)).unwrap();
        let cr = Amount::commit(15, &Blinding::from_bytes(sc(7).to_bytes()).unwrap());
        let dr = Amount::commit(7, &Blinding::from_bytes(sc(2).to_bytes()).unwrap());
        assert!(s.opens(&BalancePair { dr, cr }));
        let (net, b) = s.net(Direction::Credit).unwrap();
        assert_eq!(net, 8);
        assert_eq!(b, sc(5));
        assert_eq!(s.net(Direction::Debit).unwrap().0, -8);
        assert!(LayerSums::ZERO.opens(&BalancePair::ZERO));
    }

    #[test]
    fn sums_round_trip_and_hide_in_debug() {
        let mut s = LayerSums::ZERO;
        s.add(Direction::Debit, u64::MAX, sc(9)).unwrap();
        s.add(Direction::Debit, u64::MAX, sc(9)).unwrap();
        assert_eq!(LayerSums::from_bytes(&s.to_bytes()), Some(s));
        assert_eq!(alloc::format!("{s:?}"), "LayerSums(<private>)");
        assert_eq!(s.sub(Direction::Credit, 1, sc(1)), Err(SumFault));
    }

    #[test]
    fn commit_and_revert_restore_the_previous_state() {
        let mut v = OpeningVault::open(MemVault::new()).unwrap();
        let k = RowKey::new(AccountId([1; 16]), 0);
        let mut s = LayerSums::ZERO;
        s.add(Direction::Credit, 4, sc(1)).unwrap();
        let mut changes = BTreeMap::new();
        changes.insert(k, s);
        assert_eq!(
            v.commit(1, &changes),
            Err(VaultError::SeqOutOfOrder {
                expected: 0,
                got: 1
            })
        );
        v.commit(0, &changes).unwrap();
        s.add(Direction::Credit, 4, sc(1)).unwrap();
        changes.insert(k, s);
        v.commit(1, &changes).unwrap();
        assert_eq!(v.sums(&k).cr.value, 8);

        // A restart over the same backend sees the same state.
        let mut v = OpeningVault::open(v.into_backend()).unwrap();
        assert_eq!(v.last_committed(), Some(1));
        v.revert_last().unwrap();
        assert_eq!(v.last_committed(), Some(0));
        assert_eq!(v.sums(&k).cr.value, 4);
        assert_eq!(v.revert_last(), Err(VaultError::NothingToRevert));
        let v = OpeningVault::open(v.into_backend()).unwrap();
        assert_eq!(v.last_committed(), Some(0));
        assert_eq!(v.sums(&k).cr.value, 4);
    }
}
