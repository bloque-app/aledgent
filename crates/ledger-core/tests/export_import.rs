// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Export, import and re-derivation after a restore (`F-014` T-014-01 to T-014-05, `AC-K5.1`,
//! `AC-K5.2`, `REQ-RC-3`), over random books driven through the command surface.

use cipher_clerk::crypto::Amount;
use conformance::tools::book::{self, ROOT, RandomBook};
use ledger_core::Ledger;
use ledger_core::export::compare::compare;
use ledger_core::export::rederive::{Finding, rederive};
use ledger_core::export::{Checkpoints, Export, ExportError, Leaves, Openings};
use ledger_core::ledger::Ctx;
use ledger_core::ledger::operator::{ImportTarget, openings_of};
use ledger_kernel::keys::KeyRing;
use ledger_kernel::store::{FileStore, LeafStore, MemStore};
use ledger_kernel::vault::{FileVault, MemVault};

type Mem = RandomBook<MemStore, MemVault>;

fn random(seed: u64, steps: usize) -> Mem {
    let mut b = RandomBook::new(MemStore::new(), MemVault::new(), seed);
    b.populate(steps);
    b
}

fn target(b: &Mem) -> ImportTarget<MemStore, MemVault> {
    ImportTarget {
        store: MemStore::new(),
        vault: MemVault::new(),
        keys: KeyRing::from_root(ROOT),
        openings: openings_of(&b.ledger),
        config: book::config(),
    }
}

/// Anchor, then export.
fn export(b: &mut Mem, anchors: &mut Checkpoints) -> Export {
    anchors.anchor(&b.ledger).expect("a checkpoint");
    let ctx = b.ctx(b.operator());
    b.ledger
        .export_book(&ctx, anchors, b"export for a restore drill", &mut b.audit)
        .expect("the export")
}

fn import(b: &mut Mem, e: &Export, anchors: &Checkpoints) -> Ledger<MemStore, MemVault> {
    let ctx = b.ctx(b.operator());
    let t = target(b);
    Ledger::import_book(e, t, anchors, &ctx, b"restore drill", &mut b.audit)
        .expect("the import")
        .0
}

#[test]
fn ac_k5_2_export_and_reimport_give_identical_observable_state() {
    let mut b = random(0x514, 700);
    let mut anchors = Checkpoints::new();
    let e = export(&mut b, &mut anchors);
    // Every section of the export has something in it but the kernel's pending and voided
    // statuses, which aledgent's postings never make (`DEC-2`), and the seed's record, which a
    // book founded without a seed has none of (`F-036`; `ledger::tests::seeding` exports one).
    const EMPTY: [&str; 5] = [
        "kernel.pending",
        "kernel.voided",
        "service.chainspec",
        "service.chainspec-aliases",
        "service.chainspec-openings",
    ];
    for s in &e.sections {
        if !EMPTY.contains(&s.name()) {
            assert!(s.count() > 0, "section {} is empty", s.name());
        }
    }
    // Through bytes, as it would cross between deployments.
    let bytes = e.to_bytes();
    let e = Export::from_bytes(&bytes).expect("the bytes read back");
    let imported = import(&mut b, &e, &anchors);
    let now = b.now();
    let c = compare(&b.ledger, &imported, now);
    assert!(
        c.identical(),
        "{} differences, first: {:?}",
        c.differences.len(),
        c.differences.first()
    );
    assert!(c.queries > 1_000, "only {} queries", c.queries);
    assert!(c.movements > 1_000, "only {} movements", c.movements);

    // A second export, of the import, is the same book: every section and both roots
    // byte-identical. The manifest differs only in the audit head, which the import's own
    // record advanced (`REQ-AT-1`).
    let mut imported = imported;
    let mut again = Checkpoints::new();
    again.anchor(&imported).expect("a checkpoint of the import");
    let ctx = b.ctx(b.operator());
    let e2 = imported
        .export_book(&ctx, &again, b"export the import", &mut b.audit)
        .expect("the second export");
    assert_eq!(e2.sections, e.sections);
    assert_eq!(e2.manifest.kernel_root, e.manifest.kernel_root);
    assert_eq!(e2.manifest.service_root, e.manifest.service_root);
    assert_eq!(e2.manifest.sequence, e.manifest.sequence);
    assert_eq!(e2.manifest.sections, e.manifest.sections);
    assert_eq!(e2.manifest.audit.emitted, e.manifest.audit.emitted + 2);
}

/// `F-005`: the service store's leaves are a function of what it holds, not of how it came to
/// hold it: a book rebuilt from its export holds exactly the original's leaves, derived indexes
/// included, so the core Agent's service root does not depend on the path.
#[test]
fn inv_6_an_imported_book_holds_the_same_service_leaves() {
    let mut b = random(0x516, 700);
    let mut anchors = Checkpoints::new();
    let e = export(&mut b, &mut anchors);
    let imported = import(&mut b, &e, &anchors);
    let before = b.ledger.state().all_leaves();
    let after = imported.state().all_leaves();
    assert!(before.len() > 1_000, "only {} leaves", before.len());
    let differ: Vec<_> = before
        .iter()
        .filter(|l| !after.contains(l))
        .chain(after.iter().filter(|l| !before.contains(l)))
        .map(|(kind, _, _)| *kind)
        .collect();
    assert!(differ.is_empty(), "leaves of kinds {differ:?} differ");
}

#[test]
fn ac_k5_2_the_imported_book_takes_commands_as_the_original_does() {
    let mut b = random(0x515, 300);
    let h = b.accounts[1];
    let to = b.accounts[2];
    b.issue(h.account, book::assets()[0], 50);
    let t = ledger_core::ledger::Transfer::single(
        h.account,
        to.account,
        book::assets()[0],
        ledger_core::asset::Minor::new(7),
    );
    // A transfer accepted before the export.
    let before = b.ctx(h.owner);
    let first = b.ledger.transfer(&before, &t).map(|a| a.value).unwrap();
    let mut anchors = Checkpoints::new();
    let e = export(&mut b, &mut anchors);
    let mut imported = import(&mut b, &e, &anchors);
    // Its repeat, under the same client key, replays on the import: the original outcome, and
    // nothing written (`REQ-OP-10`): the client-key entries crossed.
    let replayed = imported.transfer(&before, &t).map(|a| a.value);
    assert_eq!(replayed, Ok(first.clone()));
    assert_eq!(b.ledger.transfer(&before, &t).map(|a| a.value), Ok(first));
    let c = compare(&b.ledger, &imported, b.now());
    assert!(c.identical(), "{:?}", c.differences.first());
    // A new command on both books: the same outcome and the same posting sequence number.
    let ctx = b.ctx(h.owner);
    let left = b.ledger.transfer(&ctx, &t).map(|a| a.value);
    let right = imported.transfer(&ctx, &t).map(|a| a.value);
    assert_eq!(left, right);
    let c = compare(&b.ledger, &imported, b.now());
    assert!(c.identical(), "{:?}", c.differences.first());
    // And the harness sees a book that moved on alone.
    let ctx = b.ctx(h.owner);
    b.ledger.transfer(&ctx, &t).unwrap();
    let c = compare(&b.ledger, &imported, b.now());
    assert!(!c.identical());
    assert!(c.differences.iter().any(|d| d.query == "checkpoint"));
}

#[test]
fn two_exports_of_the_same_state_are_byte_identical_and_hold_no_secret() {
    let mut b = random(0x516, 400);
    let mut anchors = Checkpoints::new();
    let e1 = export(&mut b, &mut anchors);
    let e2 = export(&mut b, &mut anchors);
    // The audit head moved by the second export's own record; nothing else did.
    assert_eq!(e1.sections, e2.sections);
    assert_eq!(e1.manifest.service_root, e2.manifest.service_root);
    let bytes = e1.to_bytes();
    // No blinding and no opening: scan the export for every blinding the vault holds, and for
    // the key root.
    let openings = openings_of(&b.ledger);
    assert!(!openings.rows().is_empty());
    for sums in openings.rows().values() {
        for secret in [sums.dr.blinding.as_bytes(), sums.cr.blinding.as_bytes()] {
            if *secret == [0u8; 32] {
                continue;
            }
            assert!(
                !bytes.windows(32).any(|w| w == secret),
                "a blinding is in the export"
            );
        }
    }
    assert!(
        !bytes.windows(32).any(|w| w == ROOT),
        "the key root is in the export"
    );
    // Every opening is a secret the export does not hold, but the commitments it opens are there.
    let leaves = Leaves::from_sections(e1.kernel_sections()).unwrap();
    assert_eq!(openings.check(&leaves), Ok(()));
}

#[test]
fn t_014_02_the_manifest_root_is_the_anchored_root() {
    let mut b = random(0x517, 200);
    let mut anchors = Checkpoints::new();
    let e = export(&mut b, &mut anchors);
    use ledger_core::export::Anchors;
    assert_eq!(
        anchors.anchored(e.manifest.sequence),
        Some(e.manifest.book_root())
    );
    // A command after the checkpoint moves the book off the anchored root: no export.
    b.step();
    b.step();
    let ctx = b.ctx(b.operator());
    let stale = b
        .ledger
        .export_book(&ctx, &anchors, b"late export", &mut b.audit);
    assert_eq!(stale.err(), Some(ExportError::NotAnchored));
    // Without the privileged-read kind, nothing is exported, and the attempt is on the trail.
    let stranger = b.accounts[0].owner;
    let ctx = b.ctx(stranger);
    anchors.anchor(&b.ledger);
    let refused = b
        .ledger
        .export_book(&ctx, &anchors, b"curious", &mut b.audit);
    assert!(matches!(refused, Err(ExportError::Refused(_))));
}

#[test]
fn t_014_03_import_refuses_a_non_empty_target_a_bad_digest_and_a_root_mismatch() {
    let mut b = random(0x518, 200);
    let mut anchors = Checkpoints::new();
    let e = export(&mut b, &mut anchors);
    let ctx = b.ctx(b.operator());

    // A target that is not empty: refused, and given back as it was.
    let other = ledger_kernel::book::Book::bootstrap(
        MemStore::new(),
        MemVault::new(),
        KeyRing::from_root(ROOT),
    )
    .unwrap();
    let (store, vault, _) = other.into_parts();
    let root_before = store.root().unwrap();
    let t = ImportTarget {
        store,
        vault,
        ..target(&b)
    };
    let f = Ledger::import_book(&e, t, &anchors, &ctx, b"x", &mut b.audit)
        .err()
        .unwrap();
    assert_eq!(f.error, ExportError::NotEmpty);
    let (store, _) = f.parts.expect("untouched");
    assert_eq!(store.root().unwrap(), root_before);

    // A bad section digest.
    let mut bad = e.clone();
    bad.section_mut("service.journal").unwrap().tamper(40);
    let f = Ledger::import_book(&bad, target(&b), &anchors, &ctx, b"x", &mut b.audit)
        .err()
        .unwrap();
    assert_eq!(f.error, ExportError::Digest);
    let (store, _) = f.parts.expect("untouched");
    assert_eq!(store.last_committed().unwrap(), None);

    // A root mismatch: the digests re-sealed over a changed record, so only the roots can catch
    // it (the anchor is recorded for the forged root too, as a lying exporter would).
    let mut forged = e.clone();
    forged.section_mut("kernel.external-ids").unwrap().tamper(8);
    reseal(&mut forged);
    anchors.record(forged.manifest.sequence, forged.manifest.book_root());
    let f = Ledger::import_book(&forged, target(&b), &anchors, &ctx, b"x", &mut b.audit)
        .err()
        .unwrap();
    assert_eq!(f.error, ExportError::RootMismatch);
    let (store, _) = f.parts.expect("untouched");
    assert_eq!(store.last_committed().unwrap(), None);

    // A forgery consistent with itself — its manifest names its own roots — is not the book the
    // anchor holds: refused before anything is decoded.
    forged.manifest.kernel_root = Leaves::from_sections(forged.kernel_sections())
        .unwrap()
        .root();
    let mut honest = Checkpoints::new();
    honest.anchor(&b.ledger);
    let f = Ledger::import_book(&forged, target(&b), &honest, &ctx, b"x", &mut b.audit)
        .err()
        .unwrap();
    assert_eq!(f.error, ExportError::NotAnchored);

    // Openings that do not open the book: the sealed step carried the wrong secrets.
    let mut t = target(&b);
    let (row, sums) = t
        .openings
        .rows()
        .iter()
        .next()
        .map(|(k, v)| (*k, *v))
        .unwrap();
    let mut wrong = sums;
    wrong.cr.value += 1;
    t.openings.tamper(row, wrong);
    let f = Ledger::import_book(&e, t, &anchors, &ctx, b"x", &mut b.audit)
        .err()
        .unwrap();
    assert_eq!(f.error, ExportError::Secrets);

    // Keys from another root derive another journal.
    let t = ImportTarget {
        keys: KeyRing::from_root([1; 32]),
        ..target(&b)
    };
    let f = Ledger::import_book(&e, t, &anchors, &ctx, b"x", &mut b.audit)
        .err()
        .unwrap();
    assert_eq!(f.error, ExportError::Secrets);

    // Another deployment would derive other operation references.
    let mut t = target(&b);
    t.config.deployment = [0x99; 16];
    let f = Ledger::import_book(&e, t, &anchors, &ctx, b"x", &mut b.audit)
        .err()
        .unwrap();
    assert_eq!(f.error, ExportError::Deployment);

    // A caller without the import kind in the imported book.
    let stranger = b.accounts[0].owner;
    let sctx = Ctx {
        caller: stranger,
        ..ctx
    };
    let f = Ledger::import_book(&e, target(&b), &anchors, &sctx, b"x", &mut b.audit)
        .err()
        .unwrap();
    assert!(matches!(f.error, ExportError::Refused(_)));

    // An audit store that is not the export's.
    let mut elsewhere = ledger_core::audit::AuditTrail::new();
    let f = Ledger::import_book(&e, target(&b), &anchors, &ctx, b"x", &mut elsewhere)
        .err()
        .unwrap();
    assert_eq!(f.error, ExportError::AuditNotShipped);

    // And the untampered export imports.
    import(&mut b, &e, &anchors);
}

/// Recompute every digest and the service root after a record was changed.
fn reseal(e: &mut Export) {
    for (s, entry) in e.sections.iter().zip(e.manifest.sections.iter_mut()) {
        entry.count = s.count();
        entry.digest = s.digest();
    }
    e.manifest.service_root = ledger_core::export::service_root(e.service_sections());
}

#[test]
fn req_at_1_the_import_is_on_the_audit_trail() {
    let mut b = random(0x519, 150);
    let mut anchors = Checkpoints::new();
    let e = export(&mut b, &mut anchors);
    let before = b.audit.len();
    let ctx = b.ctx(b.operator());
    let t = target(&b);
    let (imported, _) =
        Ledger::import_book(&e, t, &anchors, &ctx, b"restore drill", &mut b.audit).unwrap();
    assert_eq!(b.audit.len(), before + 1);
    let r = b.audit.iter().last().unwrap();
    assert_eq!(r.operation_ref, ctx.op);
    assert_eq!(r.principal(), Some(b.founded.operator));
    assert_eq!(
        r.justification.as_ref().map(|j| j.as_bytes().to_vec()),
        Some(b"restore drill".to_vec())
    );
    assert_eq!(
        r.action,
        ledger_core::audit::AuditAction::Command(ledger_core::state::Command::ImportBook)
    );
    assert_eq!(imported.state().audit().outbox().count(), 0);
    // The export's own read is on the trail too, as a privileged read (`REQ-PV-4`).
    assert!(b.audit.iter().any(|r| matches!(
        r.action,
        ledger_core::audit::AuditAction::PrivilegedRead(ledger_core::audit::ReadKind::Export)
    )));
}

#[test]
fn ac_k5_1_a_restored_book_rederives_and_every_seeded_corruption_is_caught() {
    let mut b = random(0x51A, 500);
    let mut anchors = Checkpoints::new();
    let e = export(&mut b, &mut anchors);
    let restored = import(&mut b, &e, &anchors);
    // The three checks, on the restored book as it stands.
    restored.rederive(&b.acks).expect("everything re-derives");

    // The same checks over a copy of what the restored book holds, corrupted one thing at a time.
    let store = restored.book().store();
    let leaves = Leaves::from_log(store, store.last_committed().unwrap().unwrap()).unwrap();
    let openings = Openings::from_vault(restored.book().vault());
    rederive(&leaves, &openings, restored.state(), &b.acks).expect("clean");

    // One balance.
    let mut l = leaves.clone();
    let (id, acct) = l
        .accounts
        .iter_mut()
        .find(|(_, a)| a.balances[0].cr != Amount::ZERO)
        .unwrap();
    let id = *id;
    acct.balances[0].cr = acct.balances[0].dr;
    let f = rederive(&l, &openings, restored.state(), &b.acks).unwrap_err();
    assert!(
        f.iter()
            .any(|x| matches!(x, Finding::Balance { account, .. } if *account == id)),
        "{f:?}"
    );

    // One entry.
    let mut l = leaves.clone();
    let t = l.transfers.values_mut().nth(3).unwrap();
    let victim = t.entries[0].account_id;
    t.entries[0].amount = Amount::ZERO;
    let f = rederive(&l, &openings, restored.state(), &b.acks).unwrap_err();
    assert!(
        f.iter()
            .any(|x| matches!(x, Finding::Balance { account, .. } if *account == victim)),
        "{f:?}"
    );

    // One opening.
    let mut o = openings.clone();
    let (row, sums) = o.rows().iter().nth(2).map(|(k, v)| (*k, *v)).unwrap();
    let mut wrong = sums;
    wrong.dr.value += 1;
    o.tamper(row, wrong);
    let f = rederive(&leaves, &o, restored.state(), &b.acks).unwrap_err();
    assert_eq!(f, vec![Finding::Opening(row)]);

    // An acknowledged operation that is not in the book.
    let mut acks = b.acks.clone();
    let ghost =
        ledger_core::id::OperationId::from_ref(ledger_kernel::posting::OperationRef([0xEE; 16]));
    acks.push(ledger_core::export::rederive::Acknowledgement {
        op: ghost,
        state: None,
    });
    let f = rederive(&leaves, &openings, restored.state(), &acks).unwrap_err();
    assert_eq!(f, vec![Finding::Missing(ghost)]);
}

#[test]
fn ac_k5_1_a_file_store_copy_restores_and_rederives() {
    let dir = tempfile::tempdir().unwrap();
    let mut b = RandomBook::new(
        FileStore::open(dir.path().join("book.redb")).unwrap(),
        FileVault::open(dir.path().join("vault.redb")).unwrap(),
        0x51B,
    );
    b.populate(250);
    let acks = b.acks.clone();
    let audit_len = b.audit.len();
    assert!(audit_len > 0);
    let mut anchors = Checkpoints::new();
    anchors.anchor(&b.ledger);
    let ctx = b.ctx(b.operator());
    let e = b
        .ledger
        .export_book(&ctx, &anchors, b"backup", &mut b.audit)
        .unwrap();
    // Restore into files of their own.
    let t = ImportTarget {
        store: FileStore::open(dir.path().join("restored.redb")).unwrap(),
        vault: FileVault::open(dir.path().join("restored-vault.redb")).unwrap(),
        keys: KeyRing::from_root(ROOT),
        openings: openings_of(&b.ledger),
        config: book::config(),
    };
    let ctx = b.ctx(b.operator());
    let (restored, _) =
        Ledger::import_book(&e, t, &anchors, &ctx, b"restore", &mut b.audit).unwrap();
    restored.rederive(&acks).expect("everything re-derives");
    let c = compare(&b.ledger, &restored, b.now());
    assert!(c.identical(), "{:?}", c.differences.first());
}

/// `REQ-RC-3`, `F-016`: what the settlement adapter changed crosses an export. The adapter is a
/// principal of the book, named by `configure_settlement`; its status declaration gates
/// `withdraw` and `submit_outbound`, so "declared, now stale" and "refusing" must survive an
/// import exactly — dropping the status would turn both into "never declared", which gates
/// nothing. Its observations, acknowledgements and the instructions it reads cross as the
/// settlement section, and the comparison asks the adapter's read and the gate at every time its
/// declaration changes meaning.
#[test]
fn req_rc_3_the_settlement_adapter_its_status_and_its_records_cross_an_export() {
    for (serving, lease, advance) in [(true, 50, 100), (false, 300_000, 0), (true, 300_000, 0)] {
        let mut b = random(0x5160 + advance, 300);
        // A holder of its own: a random one may be frozen or restricted.
        let h = b.person();
        b.issue(h.account, book::assets()[0], 5_000);
        b.declare_adapter(serving, lease);
        b.advance(advance);
        let mut anchors = Checkpoints::new();
        let e = export(&mut b, &mut anchors);
        let e = Export::from_bytes(&e.to_bytes()).expect("the bytes read back");
        let mut imported = import(&mut b, &e, &anchors);
        let now = b.now();
        let c = compare(&b.ledger, &imported, now);
        assert!(c.identical(), "{:?}", c.differences.first());

        // The configuration names the same adapter, and both books refuse its principal
        // everything but its capabilities.
        let (sa, sb) = (b.ledger.state().settlement(), imported.state().settlement());
        assert_eq!(sa.config(), sb.config());
        assert_eq!(
            sb.config().map(|c| c.authoritative.identity()),
            Some(b.adapter.principal)
        );
        assert_eq!(sa.status(), sb.status());
        assert!(sb.status().is_some(), "the declaration crossed");
        assert_eq!(
            ledger_core::principal::resolve(imported.state(), &b.adapter).map(|_| ()),
            Err(ledger_core::Refusal::NotAuthorised)
        );
        assert!(
            sb.all_ingested().count() > 0,
            "the adapter's observations crossed"
        );
        assert_eq!(
            b.ledger.adapter_instructions(&b.adapter),
            imported.adapter_instructions(&b.adapter)
        );

        // A withdrawal: the same answer on both, the gate's included.
        let w = ledger_core::ledger::settlement::Withdrawal {
            payer: h.account,
            asset: book::assets()[0],
            amount: ledger_core::asset::Minor::new(10),
            deadline: Some(now + 500),
        };
        let ctx = b.ctx(h.owner);
        let left = b.ledger.withdraw(&ctx, &w).map(|a| a.value);
        let right = imported.withdraw(&ctx, &w).map(|a| a.value);
        assert_eq!(left, right);
        let expect_open = serving && advance == 0;
        assert_eq!(
            left.is_ok(),
            expect_open,
            "serving {serving}, stale {}: {left:?}",
            advance > 0
        );
        if !expect_open {
            assert_eq!(left, Err(ledger_core::Refusal::SettlementLayerUnavailable));
        }
        let c = compare(&b.ledger, &imported, b.now());
        assert!(c.identical(), "{:?}", c.differences.first());
    }
}

/// `T-037-03`, review of #38: format 12 carries each checkpoint's construction and code binding, so
/// an export of an earlier format (11, whose checkpoints have neither) is refused at the version,
/// both when its bytes are read and when it is verified, and never decoded as format 12.
#[test]
fn t037_03_an_export_of_format_11_is_refused() {
    let mut b = random(0x037, 120);
    let mut anchors = Checkpoints::new();
    let e = export(&mut b, &mut anchors);
    assert_eq!(e.manifest.version, ledger_core::export::FORMAT_VERSION);
    assert_eq!(ledger_core::export::FORMAT_VERSION, 12);
    let mut old = e.clone();
    old.manifest.version = 11;
    assert_eq!(old.verify(), Err(ExportError::Format));
    assert!(Export::from_bytes(&old.to_bytes()).is_err());
    assert!(Export::from_bytes(&e.to_bytes()).is_ok());
}
