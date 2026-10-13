// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-014` T-014-09: export, import and compare at scale, and an opening load of many rows.
//!
//! ```text
//! f014-scale [--entries N] [--accounts A] [--openings R] [--dir DIR] [--keep]
//! ```
//!
//! A book of about `N` kernel entries is built through the command surface on `F-006`'s leaf
//! store on disk (`BookStore`: redb, archive without fsync, store commits `Visible`), with the
//! openings store in memory. It is anchored, exported, imported into a second, empty `BookStore`,
//! compared query by query (`AC-K5.2`), re-derived (`AC-K5.1`), and exported again. Then a fresh
//! book loads `R` opening rows. Every phase is timed; the sizes on disk are measured. Everything
//! under `DIR` is deleted at the end unless `--keep`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use conformance::tools::book::{self, ROOT, RandomBook};
use leaf_store::engine::Durability;
use leaf_store::{ArchiveConfig, BookStore, OpeningsStore, StoreConfig};
use ledger_core::Ledger;
use ledger_core::asset::Minor;
use ledger_core::export::compare::compare;
use ledger_core::export::{Checkpoints, Export};
use ledger_core::ledger::operator::{ImportTarget, openings_of};
use ledger_core::opening::OpeningRow;
use ledger_kernel::keys::KeyRing;
use ledger_kernel::store::LeafStore;

struct Args {
    entries: u64,
    accounts: usize,
    openings: usize,
    dir: PathBuf,
    keep: bool,
}

fn args() -> Args {
    let mut a = Args {
        entries: 1_000_000,
        accounts: 10_000,
        openings: 500_000,
        dir: std::env::temp_dir().join(format!("f014-scale-{}", std::process::id())),
        keep: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(k) = it.next() {
        let mut v = || it.next().expect("a value");
        match k.as_str() {
            "--entries" => a.entries = v().parse().expect("a count"),
            "--accounts" => a.accounts = v().parse().expect("a count"),
            "--openings" => a.openings = v().parse().expect("a count"),
            "--dir" => a.dir = PathBuf::from(v()),
            "--keep" => a.keep = true,
            other => panic!("unknown argument {other}"),
        }
    }
    a
}

fn store(dir: &Path) -> BookStore {
    let (s, _) = BookStore::open(
        dir,
        StoreConfig {
            durability: Durability::Visible,
            verify_reads: true,
        },
        ArchiveConfig {
            segment_bytes: 64 * 1024 * 1024,
            fsync: false,
        },
    )
    .expect("the store opens");
    s
}

fn du(path: &Path) -> u64 {
    fn walk(p: &Path) -> u64 {
        match std::fs::metadata(p) {
            Ok(m) if m.is_dir() => std::fs::read_dir(p)
                .map(|d| d.flatten().map(|e| walk(&e.path())).sum())
                .unwrap_or(0),
            Ok(m) => m.len(),
            Err(_) => 0,
        }
    }
    walk(path)
}

fn entries_of(l: &Ledger<BookStore, OpeningsStore>) -> u64 {
    l.state()
        .journal()
        .iter()
        .map(|p| p.lines.len() as u64)
        .sum()
}

/// Elapsed milliseconds: integers only, like every other number here (`INV-7`'s gate).
fn ms(t: Instant) -> u128 {
    t.elapsed().as_millis()
}

fn main() {
    let a = args();
    std::fs::create_dir_all(&a.dir).expect("the directory");
    println!(
        "# f014-scale: {} entries over {} accounts; {} opening rows; dir {}",
        a.entries,
        a.accounts,
        a.openings,
        a.dir.display()
    );

    if a.entries > 0 {
        book_phases(&a);
    }
    openings_phase(&a);
    if !a.keep {
        let _ = std::fs::remove_dir_all(&a.dir);
    }
    println!("done");
}

/// Build, export, import, compare, re-derive and export again.
fn book_phases(a: &Args) {
    let t = Instant::now();
    let mut b = RandomBook::new(
        store(&a.dir.join("source")),
        OpeningsStore::in_memory().expect("the openings store"),
        0xF014,
    );
    b.populate(2_000);
    let among = b.funded(a.accounts, 1_000_000_000);
    let legs = 10;
    let mut filled = entries_of(&b.ledger);
    while filled < a.entries {
        let left = (a.entries - filled).div_ceil(2 * legs as u64);
        let commands = left.min(5_000) as usize;
        b.bulk(&among, commands, legs);
        filled = entries_of(&b.ledger);
        eprintln!("  filled {filled} entries ({} ms)", ms(t));
    }
    let postings = b.ledger.state().journal().len();
    println!(
        "fill: {filled} entries in {postings} postings, {} ms ({} entries/s)",
        ms(t),
        u128::from(filled) * 1000 / ms(t).max(1)
    );
    let source_bytes = du(&a.dir.join("source"));
    println!(
        "source store on disk: {} MB ({} B per entry)",
        source_bytes / 1_000_000,
        source_bytes / filled.max(1)
    );

    // ---------------------------------------------------------------- export
    let mut anchors = Checkpoints::new();
    let t = Instant::now();
    anchors.anchor(&b.ledger).expect("a checkpoint");
    println!("checkpoint (book root): {} ms", ms(t));
    let t = Instant::now();
    let ctx = b.ctx(b.operator());
    let e = b
        .ledger
        .export_book(&ctx, &anchors, b"scale run", &mut b.audit)
        .expect("the export");
    println!("export: {} ms", ms(t));
    let t = Instant::now();
    let bytes = e.to_bytes();
    let e = Export::from_bytes(&bytes).expect("the export reads back");
    println!(
        "export bytes: {} MB ({} B per entry), written and read back in {} ms",
        bytes.len() / 1_000_000,
        bytes.len() as u64 / filled.max(1),
        ms(t)
    );
    for s in &e.sections {
        println!(
            "  section {:<28} {:>10} records {:>12} B",
            s.name(),
            s.count(),
            s.size()
        );
    }
    drop(bytes);

    // ---------------------------------------------------------------- import
    let t = Instant::now();
    let ictx = b.ctx(b.operator());
    let target = ImportTarget {
        store: store(&a.dir.join("target")),
        vault: OpeningsStore::in_memory().expect("the openings store"),
        keys: KeyRing::from_root(ROOT),
        openings: openings_of(&b.ledger),
        config: book::config(),
    };
    let (restored, _) =
        Ledger::import_book(&e, target, &anchors, &ictx, b"scale run", &mut b.audit)
            .map_err(|f| f.error)
            .expect("the import");
    println!(
        "import: {} ms, {} commits",
        ms(t),
        restored.book().store().last_committed().unwrap().unwrap() + 1
    );
    println!(
        "target store on disk: {} MB",
        du(&a.dir.join("target")) / 1_000_000
    );

    // ---------------------------------------------------------------- compare, re-derive
    let t = Instant::now();
    let c = compare(&b.ledger, &restored, b.now());
    println!(
        "compare: {} ms, {} queries, {} movements, {} differences",
        ms(t),
        c.queries,
        c.movements,
        c.differences.len()
    );
    assert!(c.identical(), "{:?}", c.differences.first());
    let t = Instant::now();
    restored.rederive(&b.acks).expect("everything re-derives");
    println!("re-derive (AC-K5.1): {} ms", ms(t));
    let mut restored = restored;
    let mut again = Checkpoints::new();
    again.anchor(&restored).expect("a checkpoint");
    let t = Instant::now();
    let ctx = b.ctx(b.operator());
    let e2 = restored
        .export_book(&ctx, &again, b"scale run, again", &mut b.audit)
        .expect("the second export");
    assert_eq!(e2.sections, e.sections, "the second export differs");
    assert_eq!(e2.manifest.service_root, e.manifest.service_root);
    assert_eq!(e2.manifest.kernel_root, e.manifest.kernel_root);
    println!(
        "second export, byte-identical sections and roots: {} ms",
        ms(t)
    );
    drop((e, e2, restored, b));
    let _ = std::fs::remove_dir_all(a.dir.join("source"));
    let _ = std::fs::remove_dir_all(a.dir.join("target"));
}

/// Load `a.openings` opening rows into a fresh book.
fn openings_phase(a: &Args) {
    if a.openings > 0 {
        let t = Instant::now();
        let mut o = RandomBook::new(
            store(&a.dir.join("openings")),
            OpeningsStore::in_memory().expect("the openings store"),
            0x0E14,
        );
        let people = a.openings.div_ceil(2);
        let mut rows = Vec::with_capacity(a.openings);
        for i in 0..people {
            // Delegated custody: an owned account's opening lists the owner's recovery
            // credentials, a scan of every credential (`F-012`), which this run is not measuring.
            let h = o.person_with(ledger_core::account::Custody::Delegated);
            rows.push(OpeningRow {
                account: h.account,
                asset: book::assets()[0],
                amount: Minor::new(1 + (i as u64 * 7919) % 1_000_000),
            });
            if rows.len() < a.openings {
                rows.push(OpeningRow {
                    account: h.account,
                    asset: book::assets()[1],
                    amount: Minor::new(1 + (i as u64 * 104_729) % 50_000),
                });
            }
        }
        rows.sort_by_key(|r| (r.account, r.asset));
        println!("opening accounts: {people} opened in {} ms", ms(t));
        let t = Instant::now();
        let (set, rec) = book::opening_set(&rows, o.ledger.state().registry(), o.now());
        println!("opening set and its reconciliation built: {} ms", ms(t));
        let t = Instant::now();
        let ctx = o.ctx(o.operator());
        let r = o
            .ledger
            .load_opening_balances(&ctx, &set, &rec, b"scale run", None)
            .expect("the load");
        assert!(r.value.complete);
        println!(
            "opening load: {} rows posted in {} ms ({} rows/s)",
            r.value.posted,
            ms(t),
            u128::from(r.value.posted) * 1000 / ms(t).max(1)
        );
        for asset in book::assets() {
            assert_eq!(o.ledger.trial_balance(&asset), Ok(0));
        }
        println!(
            "opening book on disk: {} MB",
            du(&a.dir.join("openings")) / 1_000_000
        );
        drop(o);
        let _ = std::fs::remove_dir_all(a.dir.join("openings"));
    }
}
