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
                generation: 0,
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
            type D = (
                Vec<(u8, Vec<u8>, Vec<u8>)>,
                Vec<([u8; 16], [u8; 32])>,
                Vec<[u8; 32]>,
                [u8; 32],
                [u8; 32],
                [u8; 32],
            );
            let n: D = core_binding::envelope::decode(&native).unwrap();
            let r: D = core_binding::envelope::decode(&remote).unwrap();
            eprintln!("leaves equal: {}", n.0 == r.0);
            eprintln!("keys+hashes equal: {}", n.1 == r.1);
            for (i, (a, b)) in n.1.iter().zip(r.1.iter()).enumerate() {
                if a != b {
                    eprintln!(" pair {i}: native {a:?}\n          vos {b:?}");
                }
            }
            eprintln!("leaf nodes equal: {}", n.2 == r.2);
            for (i, (a, b)) in n.2.iter().zip(r.2.iter()).enumerate() {
                if a != b {
                    eprintln!(" node {i}: native {a:?}\n          vos {b:?}");
                }
            }
            eprintln!("node(0,1) equal: {}", n.3 == r.3);
            eprintln!("service root equal: {} (native {:?}, vos {:?})", n.4 == r.4, &n.4[..4], &r.4[..4]);
            eprintln!("root_sorted equal: {} (native {:?}, vos {:?})", n.5 == r.5, &n.5[..4], &r.5[..4]);
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

/// SCALE's compact length prefix: how `(Vec<u8>, Vec<Vec<u8>>)` frames each part.
fn compact(n: usize) -> usize {
    match n {
        0..=63 => 1,
        64..=16_383 => 2,
        16_384..=1_073_741_823 => 4,
        _ => 5,
    }
}

/// The bytes `apply(message, blobs)`'s arguments take: what a vos-core request carries.
fn request_bytes(message: &[u8], blobs: &[Vec<u8>]) -> usize {
    compact(message.len())
        + message.len()
        + compact(blobs.len())
        + blobs.iter().map(|b| compact(b.len()) + b.len()).sum::<usize>()
}

/// The native Agent, each invocation's request size recorded.
struct Sizing {
    inner: core_binding::host::NativePort,
    sizes: Vec<(String, usize, usize, usize)>,
}

impl core_binding::host::AgentPort for Sizing {
    fn invoke(
        &mut self,
        message: Vec<u8>,
        blobs: Vec<Vec<u8>>,
    ) -> Result<Vec<u8>, core_binding::host::PortError> {
        use core_binding::envelope::Envelope;
        let what = match core_binding::envelope::decode::<Envelope>(&message) {
            Ok(Envelope::Input(i)) => i.calls.iter().map(|c| c.method().to_string()).collect::<Vec<_>>().join("+"),
            Ok(Envelope::Genesis(_)) => "genesis".into(),
            Ok(Envelope::Batch(b)) => format!("batch of {}", b.inputs.len()),
            Ok(Envelope::Archived { .. }) => "archived".into(),
            _ => "other".into(),
        };
        // A batch: each input as vos-core would carry it, alone in its request: its own envelope,
        // and its three blobs out of the packs.
        if let Ok(Envelope::Batch(b)) = core_binding::envelope::decode::<Envelope>(&message) {
            let mut by_key = std::collections::BTreeMap::new();
            for pack in &blobs {
                if let Ok(p) = core_binding::envelope::decode::<core_binding::envelope::Pack>(pack) {
                    for (k, bytes) in p.blobs {
                        by_key.insert((k.hash, k.len), bytes.to_vec());
                    }
                }
            }
            for input in b.inputs {
                let what = input.calls.iter().map(|c| c.method().to_string()).collect::<Vec<_>>().join("+");
                let own: Vec<Vec<u8>> = [input.witness, input.service, input.private]
                    .iter()
                    .map(|k| by_key.get(&(k.hash, k.len)).cloned().expect("an input's blob in a pack"))
                    .collect();
                let single = core_binding::envelope::encode(&Envelope::Input(input));
                let blob_bytes = own.iter().map(Vec::len).sum();
                self.sizes.push((what, request_bytes(&single, &own), single.len(), blob_bytes));
            }
            return self.inner.invoke(message, blobs);
        }
        let blob_bytes = blobs.iter().map(Vec::len).sum();
        self.sizes.push((what, request_bytes(&message, &blobs), message.len(), blob_bytes));
        self.inner.invoke(message, blobs)
    }
    fn blob_key(&self, bytes: &[u8]) -> core_binding::envelope::BlobKey {
        self.inner.blob_key(bytes)
    }
    fn hot(&mut self) -> Result<Option<core_binding::hot::Hot>, core_binding::host::PortError> {
        self.inner.hot()
    }
    fn entry(
        &mut self,
        seq: u64,
    ) -> Result<Option<core_binding::hot::RingEntry>, core_binding::host::PortError> {
        self.inner.entry(seq)
    }
    fn switch_to(&mut self, code: [u8; 32]) -> Result<(), String> {
        self.inner.switch_to(code)
    }
    fn holds_code(&self, code: &[u8; 32]) -> bool {
        self.inner.holds_code(code)
    }
}

/// `archive <dir>`: a recorded book (`founding.bin`, `entries.bin`, `code-map.json`, as
/// crates/core-binding/tests/fixtures/code-history holds one) re-driven from genesis natively,
/// one input an invocation, every request's size against vos-core's 64 KiB.
fn archive(dir: &str) {
    type Head = (Genesis, OperationId, u64, [u8; 32]);
    let d = std::path::Path::new(dir);
    let head: Head =
        core_binding::envelope::decode(&std::fs::read(d.join("founding.bin")).unwrap()).unwrap();
    let bytes = std::fs::read(d.join("entries.bin")).unwrap();
    let mut entries = Vec::new();
    let mut at = 0usize;
    while at < bytes.len() {
        let len = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        entries.push(
            core_binding::envelope::decode::<core_binding::hot::RingEntry>(&bytes[at + 4..at + 4 + len])
                .unwrap(),
        );
        at += 4 + len;
    }
    let map: core_binding::code_map::CodeMap =
        serde_json::from_slice(&std::fs::read(d.join("code-map.json")).unwrap()).unwrap();
    let (genesis, op, at, root) = head;
    let founding = core_binding::core::Founding {
        genesis,
        op,
        at,
        keys: HostKeys::local(root, None),
        limits: Limits::DEFAULT,
        genesis_roots: None,
    };
    let mut inner = core_binding::host::NativePort::new();
    inner.code_map = map.versions();
    let port = Sizing { inner, sizes: Vec::new() };
    let (port, _) = core_binding::core::roll_forward_agent(
        port,
        &founding,
        &entries,
        &core_binding::host::ReplicaStores::Memory,
    )
    .unwrap_or_else(|e| panic!("the archive does not replay: {e}"));
    println!("invocation,classes,request,message,blobs");
    for (i, (what, req, m, b)) in port.sizes.iter().enumerate() {
        println!("{i},{what},{req},{m},{b}");
    }
    let max = port.sizes.iter().max_by_key(|s| s.1).unwrap();
    eprintln!(
        "archive: {} entries, {} invocations; largest request {} B ({}), against 65536",
        entries.len(),
        port.sizes.len(),
        max.1,
        max.0
    );
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("archive") {
        archive(&args[2]);
        return;
    }
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
