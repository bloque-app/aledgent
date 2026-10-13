// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Balance reads at scale (`F-006` §Balance read at scale, `NFR-3`, `REQ-BA-3`, `REQ-BA-6`).
//!
//! A balance read is three point reads, whatever the account's history: the kernel account's
//! leaf, its opening in the openings store, and the reserved total from its index. So an account
//! holding 70% of every entry of the book reads as fast as any other. The opening is checked
//! against the leaf's commitments before its value is believed.
//!
//! Both reads are pinned to one applied sequence: the openings store commits before the leaf
//! store, so it is at the leaf view's sequence or one ahead, and one ahead the row's value before
//! that commit comes from the openings store's undo record.

use cipher_clerk::{AccountId, Layer};
use ledger_kernel::store::StoreError;
use ledger_kernel::vault::VaultError;

use crate::engine::{Engine, Snapshot};
use crate::openings::{NotPinned, OpeningsStore, OpeningsView};
use crate::store::{BookStore, ReadView};

/// Why a balance could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BalanceError {
    /// The leaf store failed.
    Store(StoreError),
    /// The openings store failed.
    Vault(VaultError),
    /// The two stores were not at the same commit, several times running.
    NotPinned(NotPinned),
    /// The opening does not open the account's stored commitments: the stores disagree.
    OpeningMismatch(AccountId),
}

impl From<StoreError> for BalanceError {
    fn from(e: StoreError) -> Self {
        BalanceError::Store(e)
    }
}

impl From<VaultError> for BalanceError {
    fn from(e: VaultError) -> Self {
        BalanceError::Vault(e)
    }
}

/// One account's balance in one asset, from one applied sequence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Balance {
    /// The applied sequence it was read at.
    pub at: Option<u64>,
    /// The plaintext net of the kernel account's settled layer, by its normal balance: the
    /// number the kernel checks a spend against (`available`).
    pub available: i128,
    /// The ledger account's reserved total in the asset (`REQ-BA-6`).
    pub reserved: u64,
}

/// The settled net of a kernel account from a pair of pinned views; `None` if it does not
/// exist.
pub fn settled_net<S: Snapshot>(
    view: &ReadView<S>,
    openings: &OpeningsView,
    account: &AccountId,
) -> Result<Result<Option<i128>, NotPinned>, BalanceError> {
    let Some(a) = view.account(account)? else {
        return Ok(Ok(None));
    };
    let layer = Layer::Settled;
    let sums = match openings.sums_at(account, layer.as_byte(), view.applied_sequence())? {
        Ok(s) => s,
        Err(np) => return Ok(Err(np)),
    };
    if !sums.opens(&a.balances[layer.as_index()]) {
        return Err(BalanceError::OpeningMismatch(*account));
    }
    let (net, _) = sums
        .net(a.normal_balance)
        .map_err(|_| BalanceError::OpeningMismatch(*account))?;
    Ok(Ok(Some(net)))
}

/// Read a balance: the kernel account's available net, and the ledger account's reserved total
/// in the asset, pinned to one applied sequence. Retries a few times if a commit lands between
/// the two snapshots.
pub fn read_balance<E: Engine>(
    store: &BookStore<E>,
    openings: &OpeningsStore,
    kernel_account: &AccountId,
    ledger_account: &[u8; 16],
    asset: u32,
) -> Result<Option<Balance>, BalanceError> {
    let mut last = None;
    for _ in 0..4 {
        let ov = openings.view()?;
        let view = store.view()?;
        match settled_net(&view, &ov, kernel_account)? {
            Ok(None) => return Ok(None),
            Ok(Some(available)) => {
                return Ok(Some(Balance {
                    at: view.applied_sequence(),
                    available,
                    reserved: view.reserved(ledger_account, asset)?,
                }));
            }
            Err(np) => last = Some(np),
        }
    }
    Err(BalanceError::NotPinned(last.expect("the loop ran")))
}
