// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The writer's code in the hot row and the ring (`T-037-02`; `REQ-RT-15`, `REQ-RT-19`, `DEC-4`,
//! `DEC-26` (a); amendment 0033).
//!
//! - Every archived entry names the code that executed it, and the log hash covers it.
//! - The hot row stays bounded: the code fields add at most `MAX_CODE_BYTES`.
//! - `driver` and `observed_slot` are host-local: a book driven under another principal and
//!   another slot reaches the same roots, log hashes and anchor commitment.
//! - `T-037-03`: a checkpoint made through the Agent binds the book's genesis, the hot row's code
//!   and mode, and its construction (`v3`), and a host that rebuilds its replica from the archive
//!   makes the same.

mod common;
use common::*;
use core_binding::envelope::{decode, encode};
use core_binding::host::{AgentPort, NativePort};
use core_binding::hot::{HOT, Hot, Limits, MAX_BUDGET, MAX_CODE_BYTES, RingEntry};
use ledger_core::anchor::Checkpoint;
use ledger_core::chainspec::{ChunkLimits, GenesisCode};
use ledger_core::code::{
    Artefact, BuildProfile, CodeManifest, KernelIdentity, Platform, Role, WriterKind,
};
use ledger_core::identity::IdentityKind;
use ledger_core::operator::OperatorOpKind;
use ledger_core::principal::{Caller, Verifier};
use ledger_core::set::NonEmptySet;
use ledger_core::settlement::{SettlementConfig, SourceId};

fn manifest() -> CodeManifest {
    CodeManifest {
        writer: WriterKind::Core,
        platform: Platform {
            name: "vos-saga".into(),
            revision: "c54fc1ff13fcd55b86f2cf4edc77b041b62e245f".into(),
            identities: Vec::new(),
        },
        generation: 0,
        artefacts: vec![Artefact {
            role: Role::Package,
            digest: [0x11; 32],
            len: 1,
            identities: Vec::new(),
            profile: BuildProfile {
                target: "riscv64em-vos".into(),
                toolchain: "nightly-2026-03-20".into(),
                opt_level: "s".into(),
                features: Vec::new(),
                source_revision: "0123abcd".into(),
                cargo_lock: [0; 32],
            },
        }],
        kernel: KernelIdentity {
            revision: "e9a7b08".into(),
            checksum: None,
        },
        constructions: vec![ledger_core::anchor::construction()],
        messages: Vec::new(),
    }
}

/// A chainspec book whose header names its genesis code *H₀*, as a deployment's does.
fn named() -> (Fx<NativePort>, [u8; 32]) {
    named_on(NativePort::new())
}

/// The chainspec of [`named`]'s book.
fn spec() -> ledger_core::chainspec::ChainSpec {
    spec_of(op(0), 1, genesis().assets, 4, ChunkLimits::DEFAULT)
        .with_code(GenesisCode {
            manifest: manifest(),
            construction: ledger_core::anchor::construction(),
        })
        .unwrap()
}

fn named_on(port: NativePort) -> (Fx<NativePort>, [u8; 32]) {
    let mut g = genesis();
    let spec = spec();
    let h0 = manifest().version();
    g.chainspec = Some(spec.commitment());
    g.spec = Some(std::sync::Arc::new(spec));
    let limits = Limits {
        budget: MAX_BUDGET,
        ..Limits::DEFAULT
    };
    (Fx::with_genesis(port, limits, g), h0)
}

fn hot<P: AgentPort>(fx: &mut Fx<P>) -> Hot {
    fx.driver.port_mut().hot().unwrap().unwrap()
}

/// T-037-02: the hot row holds the genesis code, its generation and construction; every archived
/// entry names the code that executed it, and its log hash covers that name.
#[test]
fn t037_02_every_archived_entry_names_its_code() {
    let (mut fx, h0) = named();
    let h = hot(&mut fx);
    assert_eq!(h.code, h0);
    assert_eq!(h.generation, 0);
    assert_eq!(h.construction, ledger_core::anchor::construction());
    assert_eq!(
        (h.upgrade, h.authority_next, h.continued_at),
        (None, None, None)
    );
    assert_eq!(fx.driver.code(), h0);
    let (p, _) = fx.funded(1_000);
    let _ = fx.account(&p);
    assert!(fx.archive.len() >= 4);
    for e in &fx.archive {
        assert_eq!(e.code, h0, "input {} names its code", e.seq);
        // The log hash covers it: the same entry under other code is another link of the chain.
        let mut other: RingEntry = e.clone();
        other.code = [0xEE; 32];
        assert_ne!(other.log_hash(), e.log_hash());
    }
    // The chain holds: each entry's `prev` is the one before's log hash.
    for w in fx.archive.windows(2) {
        assert_eq!(w[1].prev, w[0].log_hash());
    }
    // And the Agent's own ring agrees with the host's archive.
    let last = fx.archive.last().unwrap().clone();
    assert_eq!(hot(&mut fx).log_head, last.log_hash());
}

/// T-037-02, `DEC-4`: the hot row stays bounded. The code fields add at most `MAX_CODE_BYTES`
/// to its encoding, whatever they hold, and inputs do not grow it.
#[test]
fn t037_02_the_hot_row_stays_bounded() {
    let (mut fx, _) = named();
    let mut h = hot(&mut fx);
    let mut none = h.clone();
    (none.code, none.authority) = ([0; 32], [0; 32]);
    (none.generation, none.construction) = (0, 0);
    (none.upgrade, none.authority_next, none.continued_at) = (None, None, None);
    h.code = [0xFF; 32];
    h.generation = u64::MAX;
    h.upgrade = Some(([0xFF; 32], u64::MAX));
    h.authority = [0xFF; 32];
    h.authority_next = Some(([0xFF; 32], u64::MAX));
    h.continued_at = Some(u64::MAX);
    let widest = encode(&h).len() as u64;
    let narrowest = encode(&none).len() as u64;
    assert!(
        widest - narrowest <= MAX_CODE_BYTES,
        "{widest} - {narrowest} > {MAX_CODE_BYTES}"
    );
    // About 150 bytes (`DEC-4`, amendment 0023).
    const { assert!(MAX_CODE_BYTES <= 200) };
    // Inputs do not grow it: after more inputs, archived, only its counters have moved.
    let before = encode(&hot(&mut fx)).len();
    for _ in 0..3 {
        let _ = fx.funded(10);
    }
    let after = hot(&mut fx);
    assert!(encode(&after).len() <= before + 16);
    assert!(after.bytes() <= after.limits.budget);
}

/// Review of #37: the hot row and the log hash carry their layout. A row of an earlier layout is
/// refused by its layout, by the host and by the Agent, before any field is decoded; a ring
/// entry's log hash is under `aledgent/core-agent/log/v2`.
#[test]
fn t037_02_an_earlier_layout_is_refused_by_its_layout() {
    let (mut fx, _) = named();
    let _ = fx.person();
    // The log domain is version 2.
    let entry = fx.archive[0].clone();
    assert_eq!(core_binding::hot::LOG_DOMAIN, b"aledgent/core-agent/log/v2");
    assert_eq!(
        entry.log_hash(),
        cipher_clerk::crypto::blake2b_256(b"aledgent/core-agent/log/v2", &[&encode(&entry)])
    );
    let mut old = hot(&mut fx);
    assert_eq!(old.layout, core_binding::hot::LAYOUT);
    old.layout = 2;
    assert!(core_binding::hot::decode_hot(&encode(&old)).is_err());
    // A layout-2 row: its first field, then whatever followed it then.
    let mut bytes = encode(&2u32);
    bytes.extend_from_slice(b"an earlier row's fields");
    let e = core_binding::hot::decode_hot(&bytes).unwrap_err();
    assert!(e.contains("layout 2"), "{e}");
    // The Agent refuses an input over such a row, naming the layout.
    fx.driver.port_mut().rows.0.insert(HOT.to_vec(), bytes);
    let ctx = fx.ctx(fx.operator());
    let _ = fx
        .replica
        .open_identity(&ctx, IdentityKind::Person, None, Verifier::new(vec![2]));
    let e = format!("{:?}", fx.sync().unwrap_err());
    assert!(e.contains("layout 2"), "{e}");
}

/// `T-037-03`, `REQ-RT-15`: a checkpoint the Agent makes binds the book's genesis (its chainspec's
/// seed digest), the code version and the mode its hot row holds, and its construction; its anchor
/// commitment is the `v3` one over them, and a disclosure verifier recomputes it. A host that
/// rebuilds its replica from the archive (`Driver::recover`, each input under the code its entry
/// names) stands where the Agent does.
#[test]
fn t037_03_a_checkpoint_through_the_agent_binds_code_construction_and_mode() {
    let (archive, c, h) = drive(NativePort::new());
    let (_, h0) = named();
    let b = c.binding.expect("a v3 checkpoint");
    assert_eq!(b.code, h0);
    assert_eq!(b.code, h.code);
    assert_eq!(b.authority, h.authority);
    assert_eq!(b.genesis, spec().commitment().digest);
    assert_eq!(c.construction, h.construction);
    assert!(c.consistent());
    assert_eq!(
        c.commitment,
        ledger_core::anchor::commitment_v3(
            c.construction,
            &b,
            c.instance,
            c.number,
            c.entry_sequence,
            &c.kernel_root
        )
    );
    assert!(!archive.is_empty());
    // A host that lost its replica rebuilds it from the archive, each input under the code its
    // entry names: it reaches the Agent's roots (the checkpoint's leaf among them) and the same
    // checkpoint, and runs the next inputs under the hot row's code.
    let (fx, made) = drive_book(NativePort::new());
    let mut archive = fx.archive.clone();
    let mut g = genesis();
    let s = spec();
    g.chainspec = Some(s.commitment());
    g.spec = Some(std::sync::Arc::new(s));
    let port = fx.driver.into_port();
    let (driver, replica) =
        core_binding::host::Driver::recover(port, keys(), g, op(0), 1, &mut archive).unwrap();
    assert_eq!(driver.code(), h0);
    assert_eq!(replica.code_binding(), b);
    let rebuilt = replica
        .state()
        .anchoring()
        .checkpoint(made.number)
        .copied()
        .unwrap();
    assert_eq!(rebuilt, made);
}

/// The book the host-local test drives: a funded account, a settlement configuration, and an
/// on-demand checkpoint. Answers the archive and the checkpoint.
fn drive(port: NativePort) -> (Vec<RingEntry>, Checkpoint, Hot) {
    let (mut fx, c) = drive_book(port);
    let h = hot(&mut fx);
    (fx.archive, c, h)
}

fn drive_book(port: NativePort) -> (Fx<NativePort>, Checkpoint) {
    let (mut fx, _) = named_on(port);
    let _ = fx.funded(500);
    let ctx = fx.ctx(fx.operator());
    let o = fx
        .replica
        .open_identity(
            &ctx,
            IdentityKind::Organisation,
            None,
            Verifier::new(vec![9]),
        )
        .unwrap()
        .value;
    fx.synced();
    let adapter = Caller::new(o.identity, o.credential);
    let ctx = fx.ctx(fx.operator());
    fx.replica
        .configure_settlement(
            &ctx,
            SettlementConfig {
                authoritative: SourceId::of_identity(&adapter.principal),
                instance: 1,
                suspense: None,
                inbound_deadline: 1_000_000,
                expiry_window: 1_000,
                block_time: 1,
                finality_margin: 1,
            },
        )
        .unwrap();
    fx.synced();
    let op = fx.operator();
    let ctx = fx.ctx(op);
    fx.replica
        .grant_operator(
            &ctx,
            op.principal,
            NonEmptySet::one(OperatorOpKind::AnchorCheckpoint),
            None,
            b"T-037-02: the operator anchors",
        )
        .unwrap();
    fx.synced();
    let ctx = fx.ctx(fx.operator());
    let c = fx
        .replica
        .anchor_checkpoint(&ctx, b"T-037-02: host-local fields")
        .unwrap()
        .value
        .expect("a checkpoint");
    fx.synced();
    (fx, c)
}

/// T-037-02, `REQ-RT-19`: `driver` and `observed_slot` are the only host-local fields. A book
/// driven under another principal, with the platform reporting other slots, reaches the same
/// roots, the same log hashes and the same anchor commitment; only those two fields differ.
#[test]
fn t037_02_driver_and_observed_slot_change_no_root_log_hash_or_commitment() {
    let (a, ca, ha) = drive(NativePort::new());
    let other = NativePort {
        origin: [0x42; 32],
        slot: 7_000_000,
        ..NativePort::new()
    };
    let (b, cb, hb) = drive(other);
    assert_ne!(ha.driver, hb.driver);
    assert_ne!(ha.observed_slot, hb.observed_slot);
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(&b) {
        assert_eq!(x.roots, y.roots, "input {}", x.seq);
        assert_eq!(x.log_hash(), y.log_hash(), "input {}", x.seq);
    }
    assert_eq!(ca.commitment, cb.commitment);
    assert_eq!(ca, cb);
    // Everything else in the hot row is the same: it is writer state.
    let mut hb = hb;
    hb.driver = ha.driver;
    hb.observed_slot = ha.observed_slot;
    assert_eq!(ha, hb);
    // And the row the Agent stores is the encoding of exactly that.
    let _: Hot = decode(&encode(&ha)).unwrap();
    let _ = HOT;
}

/// `T-037-15`: the fence is in *H₀* and always on, and it changes nothing of a book that never
/// upgrades. The roots and log hash of every input, the checkpoint's anchor commitment, and the
/// writer state of the hot row (every field but the host-local two) of the book [`drive`] makes
/// are pinned to what the code before the fence computed (`develop` at `4f0b90c`), under the
/// default v2 roots (v1 roots give other values, and are not pinned here).
#[cfg(not(feature = "smt-v1"))]
#[test]
fn t037_15_the_fence_changes_nothing_of_a_book_that_never_upgrades() {
    let (archive, c, mut h) = drive(NativePort::new());
    let mut acc = Vec::new();
    for e in &archive {
        acc.extend_from_slice(&e.seq.to_be_bytes());
        acc.extend_from_slice(&e.roots.kernel);
        acc.extend_from_slice(&e.roots.service);
        acc.extend_from_slice(&e.roots.audit);
        acc.extend_from_slice(&e.log_hash());
    }
    acc.extend_from_slice(&c.commitment);
    (h.driver, h.observed_slot) = ([0; 32], 0);
    acc.extend_from_slice(&encode(&h));
    let pinned = cipher_clerk::crypto::blake2b_256(b"t037-15/pin", &[&acc]);
    assert_eq!(archive.len(), 8);
    assert_eq!(
        hex::encode(pinned),
        "885d56d7bd55e37b8c94a86ddc2309e2d45fe8b453b349b0f36bb60251a51336"
    );
}
