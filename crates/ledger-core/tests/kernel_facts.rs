// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The three kernel facts of `notes/FIT-VOS-CIPHERCLERK.md`, pinned on raw kernel transfers (`F-004` T-004-01).
//!
//! `DEC-2` realises reserved value as holding positions on the settled layer **because** of these
//! three facts. Each test below builds raw CipherClerk transfers against the kernel's own in-memory
//! reference backend, with no aledgent code in between, and asserts what the pinned revision
//! (`e9a7b08`) does. If a CipherClerk upgrade changes any of them, the matching test fails, and the
//! reservation design has to be looked at again before its meaning changes silently (`RISK-3`,
//! `DEP-4`).
//!
//! - **(a)** zero-sum is per `(currency, layer)`: a move between layers of one account cannot
//!   balance (`src/kernel/invariants.rs:22`, `:36-40`).
//! - **(b)** a `PENDING` debit on a flagged, credit-normal account is refused: negativity is
//!   checked per touched layer from that layer alone (`src/kernel/apply.rs:175-182`,
//!   `src/kernel/invariants.rs:90-99`, `tests/kernel_pending_flagged_invariants.rs:78-114`).
//! - **(c)** there is no kernel notion of available: the settled check ignores an outstanding
//!   `PENDING` debit (`src/kernel/invariants.rs:90-99`, `src/kernel/apply.rs:353-365`).

use cipher_clerk::helpers::{MemLedger, MemOracle};
use cipher_clerk::prelude::*;

/// A book with its registrar and journal.
struct Kernel {
    ledger: MemLedger,
    oracle: MemOracle,
    registrar: Keypair,
    journal: JournalId,
    ts: u64,
}

impl Kernel {
    fn new() -> Kernel {
        let mut ledger = MemLedger::new();
        let registrar = Keypair::generate();
        let journal = ledger.bootstrap_journal(registrar.public, 1);
        Kernel {
            ledger,
            oracle: MemOracle::new(),
            registrar,
            journal,
            ts: 1_000,
        }
    }

    /// A credit-normal account (a liability, like every aledgent kernel account), flagged
    /// `BALANCE_MUST_NOT_BE_NEGATIVE` or not.
    fn account(&mut self, flagged: bool) -> (Account, Keypair) {
        let kp = Keypair::generate();
        let mut account = Account::open(
            AccountKind::Liability,
            self.journal,
            kp.public,
            Iso4217::USD.as_ledger_id(),
            BankCode::Checking,
        );
        if flagged {
            account = account.with_flags(AccountFlags::BALANCE_MUST_NOT_BE_NEGATIVE);
        }
        assert_eq!(account.normal_balance, Direction::Credit);
        let ts = self.tick();
        let r = cipher_clerk::apply_account_creations(
            &mut self.ledger,
            &[CreateAccount::signed(
                account.clone(),
                &self.registrar.secret,
            )],
            &mut self.oracle,
            ts,
        );
        assert_eq!(r[0].status, EventStatus::Created);
        (account, kp)
    }

    fn tick(&mut self) -> u64 {
        self.ts += 10;
        self.ts
    }

    fn apply(&mut self, t: Transfer) -> EventStatus {
        let ts = self.tick();
        self.ledger
            .apply_batch_with_openings(&[t], &mut self.oracle, ts)[0]
            .status
    }

    /// Move `value` from `from` to `to`, both on `layer`, with `flags`.
    fn transfer(
        &mut self,
        from: &(Account, Keypair),
        to: &Account,
        layer: Layer,
        value: u64,
        flags: TransferFlags,
    ) -> EventStatus {
        let amount = self.oracle.commit(value);
        let t = Transfer::builder(self.journal)
            .flags(flags)
            .debit(&from.0, layer, amount)
            .credit(to, layer, amount)
            .signed_with(&[(&from.0, &from.1.secret)]);
        self.apply(t)
    }
}

/// Fact (a), `notes/FIT-VOS-CIPHERCLERK.md`. Debiting an account on `Settled` and crediting the **same**
/// account on `Pending` forms two `(currency, layer)` groups, each with one side only, and the
/// kernel refuses it as unbalanced. Only with a counter-account on each layer (four entries) does
/// a move between layers balance. This is why a reservation cannot be a layer move inside the
/// payer's account (`REQ-KER-3`).
#[test]
fn kernel_fact_a_a_move_between_layers_of_one_account_fails_zero_sum() {
    let mut k = Kernel::new();
    let issuer = k.account(false);
    let payer = k.account(false);
    let counter = k.account(false);
    assert_eq!(
        k.transfer(&issuer, &payer.0, Layer::Settled, 100, TransferFlags::NONE),
        EventStatus::Created
    );

    // Two entries, one account, two layers: refused.
    let amount = k.oracle.commit(40);
    let layer_move = Transfer::builder(k.journal)
        .debit(&payer.0, Layer::Settled, amount)
        .credit(&payer.0, Layer::Pending, amount)
        .signed_with(&[(&payer.0, &payer.1.secret)]);
    assert_eq!(k.apply(layer_move), EventStatus::UnbalancedTransfer);

    // Four entries, with a counter-account on each layer: each group balances on its own.
    let amount = k.oracle.commit(40);
    let four = Transfer::builder(k.journal)
        .debit(&payer.0, Layer::Settled, amount)
        .credit(&counter.0, Layer::Settled, amount)
        .debit(&counter.0, Layer::Pending, amount)
        .credit(&payer.0, Layer::Pending, amount)
        .signed_with(&[(&payer.0, &payer.1.secret), (&counter.0, &counter.1.secret)]);
    assert_eq!(k.apply(four), EventStatus::Created);
}

/// Fact (b), `notes/FIT-VOS-CIPHERCLERK.md`. A `PENDING` transfer must place every entry on `Pending`, and the
/// negativity check of a flagged account reads the touched layer's own pair. A customer balance is
/// a flagged, credit-normal account; reserving on `Pending` debits it there, the `Pending` net is
/// `-amount`, and the kernel refuses, **however much the settled layer holds**. The same move on
/// an unflagged account is accepted. This is why `DEC-2` keeps every entry on `Settled`.
#[test]
fn kernel_fact_b_a_pending_debit_on_a_flagged_credit_normal_account_fails() {
    let mut k = Kernel::new();
    let issuer = k.account(false);
    let customer = k.account(true);
    let unflagged = k.account(false);
    let hold = k.account(false);
    for payer in [&customer, &unflagged] {
        assert_eq!(
            k.transfer(
                &issuer,
                &payer.0,
                Layer::Settled,
                1_000,
                TransferFlags::NONE
            ),
            EventStatus::Created
        );
    }

    // A PENDING transfer with a settled entry is refused before anything else.
    let amount = k.oracle.commit(10);
    let mixed = Transfer::builder(k.journal)
        .flags(TransferFlags::PENDING)
        .debit(&customer.0, Layer::Settled, amount)
        .credit(&hold.0, Layer::Settled, amount)
        .signed_with(&[(&customer.0, &customer.1.secret)]);
    assert_eq!(k.apply(mixed), EventStatus::PendingFlagRequiresPendingLayer);

    // Reserving 10 of a settled 1 000 on Pending: refused for the flagged account.
    assert_eq!(
        k.transfer(
            &customer,
            &hold.0,
            Layer::Pending,
            10,
            TransferFlags::PENDING
        ),
        EventStatus::BalanceMustNotBeNegative
    );
    // Accepted for the unflagged one.
    assert_eq!(
        k.transfer(
            &unflagged,
            &hold.0,
            Layer::Pending,
            10,
            TransferFlags::PENDING
        ),
        EventStatus::Created
    );
}

/// Fact (c), `notes/FIT-VOS-CIPHERCLERK.md`. With a `PENDING` debit of the whole balance outstanding (possible
/// only on an unflagged account, by fact (b)), an ordinary settled spend of the whole balance is
/// still accepted: the check reads `balances[Settled]` and never `balances[Pending]`. The same
/// value is spent twice. The kernel has no "available = settled − reserved".
///
/// Under `DEC-2` this cannot happen: reserved value **leaves** the payer's settled layer, so the
/// only number the kernel checks is the number aledgent calls available. The last part of the test
/// shows that shape: a flagged payer, a settled move into a holding account, and a second spend of
/// the full balance refused.
#[test]
fn kernel_fact_c_the_settled_check_ignores_an_outstanding_pending_debit() {
    let mut k = Kernel::new();
    let issuer = k.account(false);
    let payer = k.account(false);
    let hold = k.account(false);
    let merchant = k.account(false);
    assert_eq!(
        k.transfer(&issuer, &payer.0, Layer::Settled, 100, TransferFlags::NONE),
        EventStatus::Created
    );
    // Reserve the whole balance on Pending.
    assert_eq!(
        k.transfer(&payer, &hold.0, Layer::Pending, 100, TransferFlags::PENDING),
        EventStatus::Created
    );
    // Spend the whole balance again on Settled: accepted.
    assert_eq!(
        k.transfer(
            &payer,
            &merchant.0,
            Layer::Settled,
            100,
            TransferFlags::NONE
        ),
        EventStatus::Created
    );

    // DEC-2's shape: a flagged payer, the reservation a settled move into a flagged holding.
    let customer = k.account(true);
    let holding = k.account(true);
    assert_eq!(
        k.transfer(
            &issuer,
            &customer.0,
            Layer::Settled,
            100,
            TransferFlags::NONE
        ),
        EventStatus::Created
    );
    assert_eq!(
        k.transfer(
            &customer,
            &holding.0,
            Layer::Settled,
            100,
            TransferFlags::NONE
        ),
        EventStatus::Created
    );
    assert_eq!(
        k.transfer(
            &customer,
            &merchant.0,
            Layer::Settled,
            100,
            TransferFlags::NONE
        ),
        EventStatus::BalanceMustNotBeNegative
    );
    assert_eq!(
        k.transfer(
            &customer,
            &merchant.0,
            Layer::Settled,
            1,
            TransferFlags::NONE
        ),
        EventStatus::BalanceMustNotBeNegative
    );
}
