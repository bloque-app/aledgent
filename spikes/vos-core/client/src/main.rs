// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `aledgent-g1 <tcp:host:port> <mix> [--n N] [--grow K]`: the core's command classes through the
//! spike's vos-core host, one input a request (W2, gate G1). The host records every input's gas,
//! argument bytes and peak heap; this side only drives the book, and checks every input commits
//! with the replica's roots (`Driver::sync`).
//!
//! The book is founded, an issuance grant made, two people with funded accounts opened (as
//! `aledgent-core-host measure` does), then `--grow K` more people with accounts (a larger book,
//! to see what grows with it), then `--n` operations of `mix`:
//!
//! - `reserve`: reservations on one account, left open (the 72 M saga reference);
//! - `card`: a reservation, then its capture or, every other one, its release;
//! - `transfers`: transfers each way in turn;
//! - `open`: an identity and its account, each an input;
//! - `tick`: reservations, then a lifecycle tick past every deadline;
//! - `issue`: issuances and redemptions in turn.

use std::collections::BTreeMap;
use std::num::NonZeroU64;

use core_binding::host::{AgentPort as _, Driver, HostKeys, RemotePort, Replica};
use core_binding::hot::Limits;
use ledger_core::account::{AccountKind, Custody};
use ledger_core::asset::{AssetId, AssetKind, Minor, NonZeroMinor};
use ledger_core::authority::{GrantKind, OperationKind, Subject, Window};
use ledger_core::id::{AccountId, OperationId};
use ledger_kernel::posting::OperationRef;
use ledger_core::identity::IdentityKind;
use ledger_core::ledger::{AssetDecl, Ctx, Genesis, GrantRequest, OpenAccount, Transfer};
use ledger_core::lifecycle::LifecycleConfig;
use ledger_core::principal::{Caller, Verifier};
use ledger_core::set::NonEmptySet;

fn op(n: u64) -> OperationId {
    let mut b = [0u8; 16];
    b[8..].copy_from_slice(&n.to_be_bytes());
    OperationId::from_ref(OperationRef(b))
}

fn usd() -> AssetId {
    AssetId::parse("USD").unwrap()
}

fn genesis() -> Genesis {
    Genesis {
        assets: vec![AssetDecl {
            id: usd(),
            location: Some(ledger_core::asset::AssetLocation::parse("Here(1)").unwrap()),
            precision: 2,
            kind: AssetKind::Fiat,
            deprecated: false,
        }],
        operator_verifier: Verifier::new(vec![0x0F]),
        lifecycle: LifecycleConfig::new([7; 16], 60, 3_600)
            .with_default_deadline(OperationKind::Reserve, 1_000),
        chainspec: None,
        spec: None,
    }
}

fn arg(args: &[String], name: &str, default: u64) -> u64 {
    args.windows(2)
        .find(|w| w[0] == name)
        .and_then(|w| w[1].parse().ok())
        .unwrap_or(default)
}

/// `--tee`: every invocation also run natively (the same transition, over rows in memory, with
/// RemotePort's blob names), and the two replies compared: a diagnostic for a divergence.
struct Tee {
    remote: RemotePort,
    rows: core_binding::transition::MemRows,
    tee: bool,
    leaves: bool,
}

mod diag {
    extern crate alloc;
    include!("../../diag_leaves.rs");
}

impl core_binding::host::AgentPort for Tee {
    fn invoke(
        &mut self,
        message: Vec<u8>,
        blobs: Vec<Vec<u8>>,
    ) -> Result<Vec<u8>, core_binding::host::PortError> {
        let native = self.tee.then(|| {
            let by: Vec<_> = blobs.iter().map(|b| (self.remote.blob_key(b), b.clone())).collect();
            let fetch = |k: &core_binding::envelope::BlobKey| {
                by.iter().find(|(key, _)| key == k).map(|(_, b)| b.clone())
            };
            let inv = core_binding::transition::Invocation {
                origin: Some([0xD7; 32]),
                slot: None,
                message: &message,
                blob: &fetch,
                generation: 2,
            };
            core_binding::transition::transition(&mut self.rows, &inv)
        });
        if self.leaves {
            let by: Vec<_> = blobs.iter().map(|b| (self.remote.blob_key(b), b.clone())).collect();
            let fetch = |k: &core_binding::envelope::BlobKey| {
                by.iter().find(|(key, _)| key == k).map(|(_, b)| b.clone())
            };
            let native = diag::genesis_leaves(&message, &fetch);
            let remote = self.remote.invoke(message, blobs)?;
            let n: Vec<(u8, Vec<u8>, Vec<u8>)> = core_binding::envelope::decode(&native).unwrap();
            let r: Vec<(u8, Vec<u8>, Vec<u8>)> = core_binding::envelope::decode(&remote).unwrap();
            eprintln!("leaves: native {} vos {}", n.len(), r.len());
            for (i, (a, b)) in n.iter().zip(r.iter()).enumerate() {
                if a != b {
                    eprintln!("leaf {i} differs:\n native {a:?}\n    vos {b:?}");
                }
            }
            std::process::exit(0);
        }
        let reply = self.remote.invoke(message, blobs)?;
        if let Some(native) = native {
            if native != reply {
                let n: Result<core_binding::envelope::Reply, _> = core_binding::envelope::decode(&native);
                let r: Result<core_binding::envelope::Reply, _> = core_binding::envelope::decode(&reply);
                eprintln!("TEE MISMATCH\n native: {n:?}\n   vos: {r:?}");
            }
        }
        Ok(reply)
    }
    fn blob_key(&self, bytes: &[u8]) -> core_binding::envelope::BlobKey {
        self.remote.blob_key(bytes)
    }
    fn hot(&mut self) -> Result<Option<core_binding::hot::Hot>, core_binding::host::PortError> {
        self.remote.hot()
    }
    fn entry(
        &mut self,
        seq: u64,
    ) -> Result<Option<core_binding::hot::RingEntry>, core_binding::host::PortError> {
        self.remote.entry(seq)
    }
}

struct Book {
    driver: Driver<Tee>,
    replica: Replica,
    next: u64,
    now: u64,
}

impl Book {
    fn ctx(&mut self, caller: Caller) -> Ctx {
        self.next += 1;
        Ctx {
            caller,
            op: op(1_000 + self.next),
            now: self.now,
        }
    }

    fn sync(&mut self, what: &str) {
        self.driver
            .sync(&mut self.replica, Vec::new())
            .unwrap_or_else(|e| panic!("{what}: the Agent commits: {e:?}"));
        self.driver.archived();
    }

    fn person(&mut self, operator: Caller, key: u64) -> (Caller, AccountId) {
        let c = self.ctx(operator);
        let mut k = vec![0u8; 32];
        k[..8].copy_from_slice(&key.to_be_bytes());
        let o = self
            .replica
            .open_identity(&c, IdentityKind::Person, None, Verifier::new(k))
            .expect("identity")
            .value;
        self.sync("open_identity");
        let person = Caller::new(o.identity, o.credential);
        let c = self.ctx(person);
        let account = self
            .replica
            .open_account(
                &c,
                OpenAccount {
                    owner: person.principal,
                    kind: AccountKind::Customer,
                    custody: Custody::Delegated,
                    treasury_limits: BTreeMap::new(),
                    acknowledged: None,
                },
            )
            .expect("account")
            .value;
        self.sync("open_account");
        (person, account)
    }

    fn issue(&mut self, operator: Caller, account: AccountId, amount: u64) {
        let c = self.ctx(operator);
        self.replica
            .issue(&c, usd(), account, Minor::new(amount))
            .expect("issue");
        self.sync("issue");
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let endpoint = &args[1];
    let mix = args[2].as_str();
    let n = arg(&args, "--n", 50);
    let grow = arg(&args, "--grow", 0);
    let port = Tee {
        remote: RemotePort::connect(endpoint).expect("the spike host"),
        rows: Default::default(),
        tee: args.iter().any(|a| a == "--tee"),
        leaves: args.iter().any(|a| a == "--leaves"),
    };
    let (mut driver, replica, founded) = Driver::found(
        port,
        HostKeys::local([0xC5; 32], None),
        genesis(),
        op(0),
        1,
        Limits::DEFAULT,
    )
    .unwrap_or_else(|e| panic!("genesis: {e:?}"));
    driver.archived();
    let mut book = Book {
        driver,
        replica,
        next: 0,
        now: 10,
    };
    let operator = Caller::new(founded.operator, founded.operator_credential);
    let c = book.ctx(operator);
    book.replica
        .grant_authority(
            &c,
            GrantRequest {
                kind: GrantKind::Issuance,
                principal: operator.principal,
                subject: Subject::Asset(usd()),
                ops: NonEmptySet::new([OperationKind::Issue, OperationKind::Redeem].into())
                    .expect("two kinds"),
                per_operation: None,
                per_window: Some(Window {
                    length: NonZeroU64::new(1_000_000).expect("non-zero"),
                    limit: NonZeroMinor::new(Minor::new(u64::MAX / 2)).expect("non-zero"),
                }),
                assets: Some(NonEmptySet::one(usd())),
                counterparties: None,
                parent: None,
                expires: None,
            },
        )
        .expect("grant");
    book.sync("grant_authority");
    let (p, a) = book.person(operator, 1);
    book.issue(operator, a, 1_000_000_000);
    let (q, b) = book.person(operator, 2);
    book.issue(operator, b, 1_000_000_000);
    for i in 0..grow {
        let (_, acct) = book.person(operator, 1_000 + i);
        if i % 4 == 0 {
            book.issue(operator, acct, 1_000);
        }
        if i % 500 == 499 {
            eprintln!("grown {}", i + 1);
        }
    }
    eprintln!("mix {mix}, n {n}");
    for i in 0..n {
        match mix {
            "reserve" => {
                let c = book.ctx(p);
                book.replica
                    .reserve(&c, a, usd(), Minor::new(100 + i))
                    .expect("reserve");
                book.sync("reserve");
            }
            "card" => {
                let c = book.ctx(p);
                book.replica
                    .reserve(&c, a, usd(), Minor::new(100 + i))
                    .expect("reserve");
                book.sync("reserve");
                if i % 2 == 0 {
                    book.replica.capture(&c, b, Minor::new(50 + i)).expect("capture");
                    book.sync("capture");
                } else {
                    book.replica.release(&c).expect("release");
                    book.sync("release");
                }
            }
            "transfers" => {
                let (who, from, to) = if i % 2 == 0 { (p, a, b) } else { (q, b, a) };
                let c = book.ctx(who);
                let t = Transfer {
                    deadline: Some(1_000_000),
                    ..Transfer::single(from, to, usd(), Minor::new(10 + i))
                };
                book.replica.transfer(&c, &t).expect("transfer");
                book.sync("transfer");
            }
            "open" => {
                book.person(operator, 1_000_000 + i);
            }
            "tick" => {
                let c = book.ctx(p);
                book.replica
                    .reserve(&c, a, usd(), Minor::new(100 + i))
                    .expect("reserve");
                book.sync("reserve");
            }
            "issue" => {
                let c = book.ctx(operator);
                if i % 2 == 0 {
                    book.replica
                        .issue(&c, usd(), a, Minor::new(10 + i))
                        .expect("issue");
                } else {
                    // A refusal is an input too: the Agent records it.
                    let _ = book.replica.redeem(&c, usd(), a, Minor::new(10));
                }
                book.sync("issue/redeem");
            }
            other => panic!("mix {other}"),
        }
    }
    if mix == "tick" {
        // One tick past every deadline (1 000 ms after 10): it fires as many as one tick may.
        book.replica.tick(10_000).expect("tick");
        book.sync("tick");
        book.replica.tick(10_001).expect("tick");
        book.sync("tick again");
    }
    eprintln!("done: {} inputs", book.driver.seq());
}
