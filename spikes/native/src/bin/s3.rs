// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! S3 — the witnessed-leaves invocation (T-002-04, T-002-05).
//!
//! 1. **Generate** a book of N accounts in `FileStore` (on disk), with
//!    history, reproducibly from a seed: every account is created and funded,
//!    then N transfers whose senders follow a Zipf distribution (`NFR-3`:
//!    extreme skew) and whose receivers are uniform. Every posting goes
//!    through `Book::apply`, so through the kernel.
//! 2. **Measure**, for a transfer and a reservation under each `DEC-2`
//!    option: the witness's size, its build time, its verify-and-apply time
//!    (CipherClerk's own `verify_transition`), and the root after, derived
//!    from the proofs alone and asserted equal to `FileStore`'s after commit.
//!    Also the point-read latency of one account leaf.
//!
//! ```text
//! cargo run --release --manifest-path spikes/Cargo.toml --bin s3 -- \
//!     [--store file|index] [--accounts 10000] [--seed 1] [--dir spikes/.books] \
//!     [--samples 1000] [--gen-only]
//! ```
//!
//! Writes `spikes/results/s3-<N>.csv`, the witnesses under
//! `spikes/results/witness/`, and prints the summary tables.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use cipher_clerk::crypto::{Amount, Blinding};
use cipher_clerk::merkle::{BatchProof, build_empty_chain};
use cipher_clerk::state::Opening;
use cipher_clerk::state_root::account_leaf_content;
use cipher_clerk::{
    AccountId, Direction, EventStatus, Layer, OpeningsOracle, Transfer, VecLedger,
    apply_account_creations,
};
use ledger_kernel::account::{BalanceRule, KernelAccountSpec};
use ledger_kernel::book::{Book, Step, Unit};
use ledger_kernel::build::{build_creation, build_transfer};
use ledger_kernel::keys::KeyRing;
use ledger_kernel::posting::{EntryDraft, OperationRef, PostingDraft, PostingKind, PostingMeta};
use ledger_kernel::store::{FileStore, KeySet, LeafStore};
use ledger_kernel::vault::{FileVault, MemVault, RowKey, VaultBackend};
use ledger_kernel::working_set::discover;
use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use spikes_native::index_store::IndexStore;
use spikes_native::shapes::{
    ASSET_A, ROOT, assert_applied, customer, holding, issuer, pool, subject, two, v,
};
use spikes_native::stats::{summarize, us};
use spikes_native::witness::{Model, WitnessParts, decode, leaf_hashes};
use spikes_native::{Sink, results_dir};

type FileBook = Book<FileStore, FileVault>;

struct Args {
    store: String,
    accounts: u64,
    seed: u64,
    dir: PathBuf,
    samples: usize,
    gen_only: bool,
    capture_only: bool,
}

fn args() -> Args {
    let mut a = Args {
        store: "file".into(),
        accounts: 10_000,
        seed: 1,
        dir: Path::new(env!("CARGO_MANIFEST_DIR")).join("../.books"),
        samples: 1_000,
        gen_only: false,
        capture_only: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(x) = it.next() {
        let mut val = || it.next().expect("a value");
        match x.as_str() {
            "--store" => a.store = val(),
            "--accounts" => a.accounts = val().parse().expect("N"),
            "--seed" => a.seed = val().parse().expect("seed"),
            "--dir" => a.dir = PathBuf::from(val()),
            "--samples" => a.samples = val().parse().expect("n"),
            "--gen-only" => a.gen_only = true,
            "--capture-only" => a.capture_only = true,
            other => panic!("unknown argument {other}"),
        }
    }
    a
}

/// Accounts created (and funded) per unit, and transfers per unit.
const CHUNK: u64 = 1_000;
/// What each account is funded with.
const FUNDING: u64 = 1_000_000_000_000;
/// The first unit's host time; each unit adds `TS_STEP`.
const TS0: u64 = 1_000_000_000;
const TS_STEP: u64 = 100_000;

/// Zipf (s = 1) sampling over ranks 1..=n with integer weights `2^40 / r`.
struct Zipf {
    cumulative: Vec<u64>,
}

impl Zipf {
    fn new(n: u64) -> Self {
        let mut total = 0u64;
        let cumulative = (1..=n)
            .map(|r| {
                total += (1u64 << 40) / r;
                total
            })
            .collect();
        Self { cumulative }
    }

    /// A 0-based rank.
    fn sample(&self, rng: &mut ChaCha20Rng) -> usize {
        let total = *self.cumulative.last().expect("n > 0");
        let u = rng.next_u64() % total;
        self.cumulative.partition_point(|c| *c <= u)
    }
}

/// The account index behind each Zipf rank: a seeded permutation, so the
/// hot accounts are not the first ones created.
fn permutation(n: u64, seed: u64) -> Vec<u64> {
    let mut rng = ChaCha20Rng::seed_from_u64(seed ^ 0x0005_EED0_FA11);
    let mut p: Vec<u64> = (0..n).collect();
    for i in (1..p.len()).rev() {
        let j = (rng.next_u64() % (i as u64 + 1)) as usize;
        p.swap(i, j);
    }
    p
}

fn book_dir(args: &Args) -> PathBuf {
    args.dir
        .join(format!("book-{}-seed-{}", args.accounts, args.seed))
}

fn open_book(dir: &Path) -> FileBook {
    Book::open(
        FileStore::open(dir.join("book.redb")).expect("store"),
        FileVault::open(dir.join("vault.redb")).expect("vault"),
        KeyRing::from_root(ROOT),
    )
    .expect("open book")
}

fn file_len(p: &Path) -> u64 {
    std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)
}

/// Generate the book (T-002-04) into `book`. Returns the last host time used
/// and the root.
fn generate<S: LeafStore, V: VaultBackend>(
    args: &Args,
    book: &mut Book<S, V>,
    sink: Option<&mut Sink>,
) -> (u64, [u8; 32], u64) {
    let n = args.accounts;
    let started = Instant::now();
    let mut ts = TS0;
    let mut unit_times = Vec::new();

    // Accounts and funding, CHUNK per unit.
    let fund_op = OperationRef(subject(0xF1, 0));
    let mut first = true;
    let mut next = 0u64;
    while next < n {
        let end = (next + CHUNK).min(n);
        let mut creations: Vec<KernelAccountSpec> =
            (next..end).map(|i| customer(i, ASSET_A)).collect();
        if first {
            creations.splice(0..0, [issuer(ASSET_A), pool(ASSET_A)]);
            first = false;
        }
        let mut steps = vec![Step {
            creations,
            chain: Vec::new(),
        }];
        steps.extend((next..end).map(|i| Step {
            creations: Vec::new(),
            chain: vec![two(
                PostingKind::Issue,
                fund_op,
                i as u32,
                issuer(ASSET_A),
                customer(i, ASSET_A),
                FUNDING,
            )],
        }));
        ts += TS_STEP;
        let t = Instant::now();
        let a = book.apply(&Unit { steps }, ts).expect("funding unit");
        unit_times.push(t.elapsed().as_nanos() as u64);
        assert_applied(&a);
        next = end;
    }

    // History: n transfers, Zipf senders, uniform receivers.
    let perm = permutation(n, args.seed);
    let zipf = Zipf::new(n);
    let mut rng = ChaCha20Rng::seed_from_u64(args.seed);
    let mut done = 0u64;
    while done < n {
        let end = (done + CHUNK).min(n);
        let steps = (done..end)
            .map(|k| {
                let from = perm[zipf.sample(&mut rng)];
                let mut to = rng.next_u64() % n;
                if to == from {
                    to = (to + 1) % n;
                }
                let value = 1 + rng.next_u64() % 1_000;
                Step {
                    creations: Vec::new(),
                    chain: vec![two(
                        PostingKind::Transfer,
                        OperationRef(subject(0xA0, k)),
                        0,
                        customer(from, ASSET_A),
                        customer(to, ASSET_A),
                        value,
                    )],
                }
            })
            .collect();
        ts += TS_STEP;
        let t = Instant::now();
        let a = book.apply(&Unit { steps }, ts).expect("history unit");
        unit_times.push(t.elapsed().as_nanos() as u64);
        assert_applied(&a);
        done = end;
    }
    let root = book.root().expect("root");
    let secs = started.elapsed().as_secs();
    let Some(sink) = sink else {
        return (ts, root, secs);
    };
    sink.series(
        "s3",
        "generate",
        &args.store,
        n,
        "unit_apply",
        "ns",
        &unit_times,
    )
    .expect("csv");
    let s = summarize(&unit_times);
    println!(
        "\n### Book generation, {n} accounts, seed {}, store {}\n",
        args.seed, args.store
    );
    println!(
        "| Accounts | Postings + creations | Units | Wall time s | Unit p50 ms | Unit p99 ms | Root |"
    );
    println!("|---:|---:|---:|---:|---:|---:|---|");
    println!(
        "| {n} | {} | {} | {secs} | {} | {} | `{}` |",
        3 * n,
        unit_times.len(),
        s.p50 / 1_000_000,
        s.p99 / 1_000_000,
        spikes_native::golden::hex(&root)
    );
    (ts, root, secs)
}

/// The post-state openings of the never-negative accounts `t` touches.
fn net_openings<S: LeafStore, V: VaultBackend>(book: &Book<S, V>, t: &Transfer) -> Vec<Opening> {
    let ids: BTreeSet<AccountId> = t.entries.iter().map(|e| e.account_id).collect();
    let mut out = Vec::new();
    for id in ids {
        let acct = book.kernel_account(&id).expect("load").expect("exists");
        if BalanceRule::of(&acct) != BalanceRule::NeverNegative {
            continue;
        }
        let (net, bl) = book
            .vault()
            .sums(&RowKey::new(id, Layer::Settled.as_byte()))
            .net(Direction::Credit)
            .expect("sums");
        let value = u64::try_from(net).expect("never negative");
        let blinding = Blinding::from_bytes(bl.to_bytes()).expect("canonical");
        out.push(Opening {
            amount: Amount::commit(value, &blinding),
            value,
            blinding,
        });
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Case {
    Transfer,
    ReservePooled,
    ReserveNew,
    /// Capture for less of a reservation into a new holding position: three
    /// entries with `CLOSING_DEBIT` (F-004 asked for its witness size).
    CaptureLess,
}

impl Case {
    fn name(self) -> &'static str {
        match self {
            Case::Transfer => "transfer",
            Case::ReservePooled => "reserve-pooled",
            Case::ReserveNew => "reserve-new-holding",
            Case::CaptureLess => "capture-for-less",
        }
    }
}

struct CaseResult {
    case: Case,
    size: spikes_native::witness::WitnessSize,
    creation_bytes: usize,
    build: Vec<u64>,
    verify: Vec<u64>,
    root_after: Vec<u64>,
}

#[allow(clippy::too_many_arguments)]
fn run_case<S: LeafStore, V: VaultBackend>(
    case: Case,
    book: &mut Book<S, V>,
    model: &mut Model,
    empty: &[[u8; 32]; 129],
    payer: KernelAccountSpec,
    payee: KernelAccountSpec,
    ts: u64,
    op: OperationRef,
    samples: usize,
    n: u64,
) -> CaseResult {
    let keys = KeyRing::from_root(ROOT);
    let journal = book.journal();
    let (creations, draft): (Vec<KernelAccountSpec>, PostingDraft) = match case {
        Case::Transfer => (
            Vec::new(),
            two(PostingKind::Transfer, op, 0, payer, payee, 7),
        ),
        Case::ReservePooled => (
            Vec::new(),
            two(PostingKind::Reserve, op, 0, payer, pool(ASSET_A), 7),
        ),
        Case::ReserveNew => {
            let h = holding(op, ASSET_A);
            (vec![h], two(PostingKind::Reserve, op, 0, payer, h, 7))
        }
        Case::CaptureLess => {
            // The reservation first, untimed, with the index kept in step.
            let h = holding(op, ASSET_A);
            let a = book
                .apply(
                    &Unit {
                        steps: vec![Step {
                            creations: vec![h],
                            chain: vec![two(PostingKind::Reserve, op, 0, payer, h, 7)],
                        }],
                    },
                    ts - TS_STEP / 2,
                )
                .expect("reserve");
            assert_applied(&a);
            let rec = book
                .store()
                .log_record(a.seq.expect("committed"))
                .expect("log")
                .expect("record");
            model.apply(&rec.delta);
            let capture = PostingDraft::balanced(
                PostingMeta::new(PostingKind::Capture, op, 1),
                vec![
                    EntryDraft::debit(h, v(7)),
                    EntryDraft::credit(payee, v(5)),
                    EntryDraft::credit(payer, v(2)),
                ],
            )
            .expect("capture balances")
            .with_closing_debit();
            (Vec::new(), capture)
        }
    };
    let built = build_transfer(&keys, journal, &draft, false);
    let created: Vec<_> = creations
        .iter()
        .map(|c| build_creation(&keys, journal, c))
        .collect();
    let events = vec![built.transfer.clone()];
    let touched: KeySet = discover(&created, &events);
    let pre = book.store().load(&touched).expect("load pre");
    let root_before = book.root().expect("root");

    // Apply for real: the root after and the post-state openings.
    let applied = book
        .apply(
            &Unit {
                steps: vec![Step {
                    creations: creations.clone(),
                    chain: vec![draft.clone()],
                }],
            },
            ts,
        )
        .expect("apply");
    assert_applied(&applied);
    let root_after = applied.root;
    let seq = applied.seq.expect("committed");
    let mut openings = built.openings.clone();
    openings.extend(net_openings(book, &built.transfer));
    let oracle = OpeningsOracle::new(openings);

    // The kernel's pre-state for the chain. For a new holding position it is
    // the post-creation state: CipherClerk's succinct witness covers
    // `apply_batch` only, so the creation is checked beside it.
    let mut chain_pre = pre.clone();
    let mut creation_bytes = 0;
    if !created.is_empty() {
        let mut vl = VecLedger::new();
        vl.set_journal(
            pre.journals
                .values()
                .next()
                .cloned()
                .flatten()
                .expect("journal proven"),
        );
        // Book::apply seeds a step's creations at seed_ts - chain length.
        let r = apply_account_creations(&mut vl, &created, &mut OpeningsOracle::default(), ts - 1);
        assert!(r.iter().all(|r| r.status == EventStatus::Created));
        for a in vl.accounts {
            chain_pre.accounts.insert(a.id, Some(a));
        }
        creation_bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&created)
            .expect("serialise")
            .len();
    }

    let (proofs, _) = WitnessParts::proofs(model, &touched, empty);
    let parts = WitnessParts {
        proofs,
        pre: chain_pre.clone(),
        oracle: oracle.clone(),
        events: events.clone(),
        seed: ts,
    };
    let bytes = parts.to_bytes();
    let size = parts.size(&touched);
    let kernel_proofs: [BatchProof; 6] = std::array::from_fn(|i| parts.proofs[i].to_kernel());

    // The proofs reconstruct the committed pre-state (the new holding
    // position proven absent), and the chain's pre-state root.
    assert_eq!(
        WitnessParts::root_from_proofs(&kernel_proofs, &leaf_hashes(&pre)),
        root_before,
        "{}: witness does not open root_before",
        case.name()
    );
    let chain_root_before =
        WitnessParts::root_from_proofs(&kernel_proofs, &leaf_hashes(&chain_pre));

    // CipherClerk's own verifier, on bytes decoded as its own type.
    decode(&bytes).verify_transition(chain_root_before, root_after);

    // Root after, from the proofs alone, equals FileStore's after commit.
    let post = book.store().load(&touched).expect("load post");
    let post_hashes = leaf_hashes(&post);
    assert_eq!(
        WitnessParts::root_from_proofs(&kernel_proofs, &post_hashes),
        root_after,
        "{}: root derived from the witness differs from FileStore's",
        case.name()
    );

    std::fs::create_dir_all(results_dir().join("witness")).expect("mkdir");
    let stem = format!("{}-{n}", case.name());
    std::fs::write(
        results_dir().join("witness").join(format!("{stem}.bin")),
        &bytes,
    )
    .expect("write witness");
    std::fs::write(
        results_dir().join("witness").join(format!("{stem}.roots")),
        format!(
            "{}\n{}\n",
            spikes_native::golden::hex(&chain_root_before),
            spikes_native::golden::hex(&root_after)
        ),
    )
    .expect("write roots");

    // Timings. Build: load the touched leaves, extract the frontier, serialise.
    let w = 20;
    let build = spikes_native::stats::time_n(w, samples, || {
        let pre = book.store().load(&touched).expect("load");
        let (proofs, _) = WitnessParts::proofs(model, &touched, empty);
        let p = WitnessParts {
            proofs,
            pre,
            oracle: oracle.clone(),
            events: events.clone(),
            seed: ts,
        };
        std::hint::black_box(p.to_bytes());
    });
    let registrar = keys.registrar_key();
    let pre_hashes = leaf_hashes(&pre);
    let chain_pre_hashes = leaf_hashes(&chain_pre);
    let verify = spikes_native::stats::time_n(w, samples, || {
        if let Some(c) = created.first() {
            assert!(cipher_clerk::crypto::verify_signature(
                &registrar,
                &c.account.signing_payload(),
                &c.signature
            ));
            assert_eq!(
                WitnessParts::root_from_proofs(&kernel_proofs, &pre_hashes),
                root_before
            );
            std::hint::black_box(WitnessParts::root_from_proofs(
                &kernel_proofs,
                &chain_pre_hashes,
            ));
        }
        decode(std::hint::black_box(&bytes)).verify_transition(chain_root_before, root_after);
    });
    let root_after_t = spikes_native::stats::time_n(w, samples, || {
        std::hint::black_box(WitnessParts::root_from_proofs(&kernel_proofs, &post_hashes));
    });

    // Keep the index at the book's state.
    let rec = book.store().log_record(seq).expect("log").expect("record");
    model.apply(&rec.delta);
    assert_eq!(model.root(empty), book.root().expect("root"));

    CaseResult {
        case,
        size,
        creation_bytes,
        build,
        verify,
        root_after: root_after_t,
    }
}

/// Point reads of one account leaf through `load`: Zipf-hot and uniform.
fn point_reads(
    n: u64,
    seed: u64,
    samples: usize,
    perm: &[u64],
    mut read: impl FnMut(AccountId),
    sink: &mut Sink,
    variant: &str,
) -> Vec<(&'static str, spikes_native::stats::Summary)> {
    let zipf = Zipf::new(n);
    let mut rng = ChaCha20Rng::seed_from_u64(seed ^ 0xBEEF);
    let mut out = Vec::new();
    for (label, hot) in [("zipf", true), ("uniform", false)] {
        let ids: Vec<AccountId> = (0..samples + 50)
            .map(|_| {
                let idx = if hot {
                    perm[zipf.sample(&mut rng)]
                } else {
                    rng.next_u64() % n
                };
                customer(idx, ASSET_A).id()
            })
            .collect();
        let mut it = ids.into_iter();
        let s = spikes_native::stats::time_n(50, samples, || read(it.next().expect("id")));
        sink.series(
            "s3",
            "point-read",
            &format!("{variant}/{label}"),
            n,
            "total",
            "ns",
            &s,
        )
        .expect("csv");
        out.push((label, summarize(&s)));
    }
    out
}

/// A leaf-only redb table of every account (no node table): the value is
/// the stored account and its leaf hash, read and checked as `FileStore`
/// checks a leaf against its node.
const LEAF_TABLE: redb::TableDefinition<&[u8], &[u8]> = redb::TableDefinition::new("accounts");

fn build_leaf_table(path: &Path, accounts: impl Iterator<Item = cipher_clerk::Account>) -> u64 {
    let _ = std::fs::remove_file(path);
    let db = redb::Database::create(path).expect("redb");
    let txn = db.begin_write().expect("txn");
    {
        let mut t = txn.open_table(LEAF_TABLE).expect("table");
        for a in accounts {
            let mut v = ledger_kernel::store::codec::encode_account(&a);
            v.extend_from_slice(&cipher_clerk::merkle::smt_leaf_hash(&account_leaf_content(
                &a,
            )));
            t.insert(a.id.0.as_slice(), v.as_slice()).expect("insert");
        }
    }
    txn.commit().expect("commit");
    drop(db);
    file_len(path)
}

fn read_leaf(db: &redb::Database, id: AccountId) {
    use redb::ReadableDatabase;
    let txn = db.begin_read().expect("txn");
    let t = txn.open_table(LEAF_TABLE).expect("table");
    let v = t.get(id.0.as_slice()).expect("get").expect("present");
    let bytes = v.value();
    let (acct, hash) = bytes.split_at(bytes.len() - 32);
    let a = ledger_kernel::store::codec::decode_account(acct).expect("decode");
    assert_eq!(
        cipher_clerk::merkle::smt_leaf_hash(&account_leaf_content(&a)).as_slice(),
        hash
    );
}

#[allow(clippy::too_many_arguments)]
fn measure<S: LeafStore, V: VaultBackend>(
    args: &Args,
    book: &mut Book<S, V>,
    mut model: Model,
    mut ts: u64,
    sink: &mut Sink,
    store_note: &str,
) -> u64 {
    let n = args.accounts;
    let empty = build_empty_chain();
    let root = model.root(&empty);
    assert_eq!(
        root,
        book.root().expect("root"),
        "index root differs from the store's"
    );
    let leaves: Vec<usize> = model.trees.iter().map(|t| t.len()).collect();
    let total_leaves: usize = leaves.iter().sum();
    sink.series(
        "s3",
        "leaves",
        "all",
        n,
        "count",
        "leaves",
        &[total_leaves as u64],
    )
    .expect("csv");
    let sample_acct = book
        .kernel_account(&customer(0, ASSET_A).id())
        .expect("load")
        .expect("exists");
    let leaf_content = account_leaf_content(&sample_acct).len();

    let perm = permutation(n, args.seed);
    let payer = customer(perm[0], ASSET_A);
    let payee = customer(n / 2, ASSET_A);

    let mut results = Vec::new();
    let cases: &[Case] = if args.capture_only {
        &[Case::CaptureLess]
    } else {
        &[Case::Transfer, Case::ReservePooled, Case::ReserveNew]
    };
    for (i, case) in cases.iter().copied().enumerate() {
        ts += TS_STEP;
        let op = OperationRef(subject(
            if args.capture_only { 0xC6 } else { 0xC5 },
            i as u64,
        ));
        let r = run_case(
            case,
            book,
            &mut model,
            &empty,
            payer,
            payee,
            ts,
            op,
            args.samples,
            n,
        );
        let name = r.case.name();
        sink.series("s3", name, "build", n, "total", "ns", &r.build)
            .expect("csv");
        sink.series("s3", name, "verify-and-apply", n, "total", "ns", &r.verify)
            .expect("csv");
        sink.series("s3", name, "root-after", n, "total", "ns", &r.root_after)
            .expect("csv");
        sink.series(
            "s3",
            name,
            "witness",
            n,
            "bytes",
            "bytes",
            &[(r.size.bytes + r.creation_bytes) as u64],
        )
        .expect("csv");
        results.push(r);
    }

    println!(
        "\n### S3 at {n} accounts ({store_note}; release; n = {} samples per timing)\n",
        args.samples
    );
    println!(
        "Book: {total_leaves} leaves {leaves:?} (accounts, transfers, journals, external ids, voided, pending). Account leaf content {leaf_content} B.\n"
    );
    println!(
        "| Case | Witness B | Frontier nodes (acc/xfer/jrnl/eid/void/pend) | Proofs B | Events B | Openings B | Creation B | Build p50 µs | Build p99 µs | Verify+apply p50 µs | Verify+apply p99 µs | Root-after p50 µs | Root-after p99 µs |"
    );
    println!("|---|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    for r in &results {
        let b = summarize(&r.build);
        let v = summarize(&r.verify);
        let ra = summarize(&r.root_after);
        let f = r.size.frontier_nodes;
        println!(
            "| {} | {} | {}/{}/{}/{}/{}/{} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            r.case.name(),
            r.size.bytes + r.creation_bytes,
            f[0],
            f[1],
            f[2],
            f[3],
            f[4],
            f[5],
            r.size.proof_bytes,
            r.size.event_bytes,
            r.size.oracle_bytes,
            r.creation_bytes,
            us(b.p50),
            us(b.p99),
            us(v.p50),
            us(v.p99),
            us(ra.p50),
            us(ra.p99)
        );
    }
    ts
}

fn print_reads(label: &str, reads: &[(&str, spikes_native::stats::Summary)]) {
    println!("\n| Point read of one account leaf ({label}) | p50 µs | p95 µs | p99 µs | max µs |");
    println!("|---|---:|---:|---:|---:|");
    for (l, s) in reads {
        println!(
            "| {l} | {} | {} | {} | {} |",
            us(s.p50),
            us(s.p95),
            us(s.p99),
            us(s.max)
        );
    }
}

fn main() {
    let args = args();
    let n = args.accounts;
    let dir = book_dir(&args);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let suffix = if args.capture_only { "-capture" } else { "" };
    let mut sink = Sink::create(&format!("s3-{}-{n}{suffix}", args.store)).expect("csv");
    let perm = permutation(n, args.seed);
    match args.store.as_str() {
        "file" => {
            let mut ts = if dir.join("root").exists() {
                println!("book {} exists; reusing it", dir.display());
                std::fs::read_to_string(dir.join("ts"))
                    .expect("ts")
                    .trim()
                    .parse()
                    .expect("ts")
            } else {
                let mut book: FileBook = Book::bootstrap(
                    FileStore::open(dir.join("book.redb")).expect("store"),
                    FileVault::open(dir.join("vault.redb")).expect("vault"),
                    KeyRing::from_root(ROOT),
                )
                .expect("bootstrap");
                let (ts, root, _) = generate(&args, &mut book, Some(&mut sink));
                std::fs::write(dir.join("ts"), ts.to_string()).expect("ts");
                std::fs::write(dir.join("root"), spikes_native::golden::hex(&root)).expect("root");
                ts
            };
            let book_bytes = file_len(&dir.join("book.redb"));
            let vault_bytes = file_len(&dir.join("vault.redb"));
            sink.series(
                "s3",
                "files",
                "book.redb",
                n,
                "size",
                "bytes",
                &[book_bytes],
            )
            .expect("csv");
            sink.series(
                "s3",
                "files",
                "vault.redb",
                n,
                "size",
                "bytes",
                &[vault_bytes],
            )
            .expect("csv");
            println!("book.redb {book_bytes} B, vault.redb {vault_bytes} B");
            if args.gen_only {
                sink.finish().expect("csv");
                return;
            }
            let mut book = open_book(&dir);
            let t = Instant::now();
            let last = book.last_committed().expect("last").expect("bootstrapped");
            let mut maps = spikes_native::witness::LeafMaps::default();
            for seq in 0..=last {
                let rec = book.store().log_record(seq).expect("log").expect("record");
                Model::fold_delta(&mut maps, &rec.delta);
            }
            let model = Model::from_maps(maps);
            println!("index from the delta log in {} s", t.elapsed().as_secs());
            ts = measure(&args, &mut book, model, ts, &mut sink, "FileStore on disk");
            std::fs::write(dir.join("ts"), ts.to_string()).expect("ts");
            let reads = point_reads(
                n,
                args.seed,
                args.samples,
                &perm,
                |id| {
                    let mut k = KeySet::default();
                    k.accounts.insert(id);
                    let l = book.store().load(&k).expect("load");
                    assert!(matches!(l.accounts.values().next(), Some(Some(_))));
                },
                &mut sink,
                "filestore",
            );
            print_reads("FileStore, leaf + node check", &reads);
            // The live size: redb compaction drops pages freed by copy-on-write.
            drop(book);
            let mut db = redb::Database::open(dir.join("book.redb")).expect("redb");
            while db.compact().expect("compact") {}
            drop(db);
            let compacted = file_len(&dir.join("book.redb"));
            sink.series(
                "s3",
                "files",
                "book.redb-compacted",
                n,
                "size",
                "bytes",
                &[compacted],
            )
            .expect("csv");
            println!("book.redb after compaction: {compacted} B");
        }
        "index" => {
            let mut book =
                Book::bootstrap(IndexStore::new(), MemVault::new(), KeyRing::from_root(ROOT))
                    .expect("bootstrap");
            let (ts, _, _) = generate(&args, &mut book, Some(&mut sink));
            let model = book.store().model();
            measure(
                &args,
                &mut book,
                model,
                ts,
                &mut sink,
                "IndexStore in memory",
            );
            if args.capture_only {
                sink.finish().expect("csv");
                return;
            }
            let path = dir.join("leaves.redb");
            let bytes = build_leaf_table(&path, book.store().accounts().cloned());
            drop(book);
            sink.series("s3", "files", "leaves.redb", n, "size", "bytes", &[bytes])
                .expect("csv");
            println!("leaf-only account table: {bytes} B");
            let db = redb::Database::open(&path).expect("redb");
            let reads = point_reads(
                n,
                args.seed,
                args.samples,
                &perm,
                |id| read_leaf(&db, id),
                &mut sink,
                "leaf-table",
            );
            print_reads("leaf-only redb table, leaf + hash check", &reads);
        }
        other => panic!("unknown store {other}"),
    }
    sink.finish().expect("csv");
}

#[cfg(test)]
mod tests {
    use super::*;
    use ledger_kernel::store::FileStore;
    use ledger_kernel::vault::FileVault;

    fn args(n: u64, store: &str) -> Args {
        Args {
            store: store.into(),
            accounts: n,
            seed: 7,
            dir: PathBuf::new(),
            samples: 0,
            gen_only: true,
            capture_only: false,
        }
    }

    /// T-002-04: books build reproducibly from a seed, and the spike's
    /// IndexStore roots a book exactly as FileStore does.
    #[test]
    fn books_build_reproducibly_from_a_seed_on_both_stores() {
        let n = 1_500; // two funding units and two history units
        let root_of = |store: &str| -> [u8; 32] {
            let keys = KeyRing::from_root(ROOT);
            if store == "file" {
                let mut b = Book::bootstrap(
                    FileStore::in_memory().expect("store"),
                    FileVault::in_memory().expect("vault"),
                    keys,
                )
                .expect("bootstrap");
                generate(&args(n, store), &mut b, None).1
            } else {
                let mut b =
                    Book::bootstrap(IndexStore::new(), MemVault::new(), keys).expect("bootstrap");
                generate(&args(n, store), &mut b, None).1
            }
        };
        let a = root_of("index");
        assert_eq!(a, root_of("index"), "same seed, same root");
        assert_eq!(a, root_of("file"), "IndexStore and FileStore agree");
    }
}
