// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The removal of an unused asset through the core Agent (amendment 0043, `REQ-AS-10`,
//! `PLAN.md` `DEC-28`; `REQ-RT-1`, `REQ-RT-16`, `REQ-RT-17`, `REQ-RT-20`, `INV-24`).
//!
//! The first design read every ledger account and grant in one input, and the witness gave the
//! Agent every leaf of both kinds: at the production book's size, about a hundred times its heap.
//! A toy book did not show it. These tests run on a book seeded with many accounts and grants,
//! upgraded into the rules version that takes the removal, and measure every input the removal
//! makes: its witness must hold no kind whole, at most a page of leaves and a fixed overhead, and
//! must not grow with the book. They also cross the paths a toy book cannot: a page the Agent must
//! prove complete, a page too large, a finding, the cursor through a replay, and the version gate.

mod common;

use common::*;

use core_binding::core::{Founding, claimed_code_map, roll_forward_agent};
use core_binding::envelope::{BlobKey, Envelope, decode};
use core_binding::host::{AgentPort, NativePort, PortError, ReplicaStores};
use core_binding::hot::{Hot, Limits, MAX_BUDGET, RingEntry};
use core_binding::service::ServiceWitness;
use ledger_core::account::AccountKind;
use ledger_core::asset::{
    ASSET_RETIREMENT, AssetId, AssetKind, AssetStatus, MAX_SWEEP_PAGE, Minor, NonZeroMinor,
    SWEEP_END, SWEEP_START,
};
use ledger_core::authority::{GrantKind, OperationKind, Subject};
use ledger_core::chainspec::{self, ChainSpec, ChunkLimits, Section, SpecRecord};
use ledger_core::ledger::operator::{RegisterAsset, ReviewRef, SweepNext, SweepPage};
use ledger_core::ledger::{AssetDecl, GrantRequest};
use ledger_core::operator::OperatorOpKind;
use ledger_core::principal::Verifier;
use ledger_core::refusal::Refusal;
use ledger_core::set::NonEmptySet;
use peak_alloc::PeakAlloc;

#[global_allocator]
static PEAK: PeakAlloc = PeakAlloc;

/// The allocator counts the whole process: one measured test at a time.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The guest's heap (`vos/src/abi/pvm/alloc.rs`, `HEAP_SIZE`).
const GUEST_HEAP: usize = 256 * 1024;

/// One pack's bound (`BatchLimits::DEFAULT.max_pack_bytes`): an input's three blobs together.
const PACK: usize = 46 * 1024;

/// Leaves a page's input is given besides the page's own: the registry, the retirement, the
/// operator's grant and credential, the client-key and attribution leaves, the audit's.
const OVERHEAD_LEAVES: usize = 24;

/// One invocation, measured.
#[derive(Clone, Copy, Debug, Default)]
struct Sample {
    /// Peak live bytes of the transition.
    peak: usize,
    /// The blobs: kernel witness, service witness, private inputs.
    blobs: usize,
    /// The service witness alone.
    service: usize,
    /// Service leaves witnessed.
    leaves: usize,
    /// Kinds witnessed whole.
    whole: usize,
}

/// The native port, measured around each input.
struct Measured {
    inner: NativePort,
    samples: Vec<Sample>,
}

impl AgentPort for Measured {
    fn invoke(&mut self, message: Vec<u8>, blobs: Vec<Vec<u8>>) -> Result<Vec<u8>, PortError> {
        let mut sample = Sample {
            blobs: blobs.iter().map(Vec::len).sum(),
            ..Sample::default()
        };
        if let Ok(Envelope::Input(i)) = decode::<Envelope>(&message) {
            let blob = blobs
                .iter()
                .find(|b| self.inner.blob_key(b) == i.service)
                .expect("the service witness");
            let w: ServiceWitness = decode(blob).unwrap();
            sample.service = blob.len();
            sample.leaves = w.leaves.len();
            sample.whole = w.whole.len();
        }
        let base = PEAK.current_usage();
        PEAK.reset_peak_usage();
        let r = self.inner.invoke(message, blobs);
        sample.peak = PEAK.peak_usage().saturating_sub(base);
        self.samples.push(sample);
        r
    }
    fn blob_key(&self, bytes: &[u8]) -> BlobKey {
        self.inner.blob_key(bytes)
    }
    fn hot(&mut self) -> Result<Option<Hot>, PortError> {
        self.inner.hot()
    }
    fn entry(&mut self, seq: u64) -> Result<Option<RingEntry>, PortError> {
        self.inner.entry(seq)
    }
    fn switch_to(&mut self, code: [u8; 32]) -> Result<(), String> {
        self.inner.switch_to(code)
    }
    fn stage_code(
        &mut self,
        checked: core_binding::staging::Checked,
    ) -> Result<(), core_binding::staging::StageRefusal> {
        self.inner.stage_code(checked)
    }
    fn holds_code(&self, code: &[u8; 32]) -> bool {
        self.inner.holds_code(code)
    }
    fn can_install(&self, checked: &core_binding::staging::Checked) -> Result<(), String> {
        self.inner.can_install(checked)
    }
}

fn tstx() -> AssetId {
    AssetId::parse("TSTX").unwrap()
}

/// How many accounts, and grants, the larger book holds. `ALEDGENT_RETIRE_GROW` sets another.
fn grow() -> u64 {
    std::env::var("ALEDGENT_RETIRE_GROW")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4_000)
}

/// The genesis: the fixture's USD, and `TSTX`, book-only at the wrong precision.
fn assets() -> Vec<AssetDecl> {
    let mut a = genesis().assets;
    a.push(AssetDecl {
        id: tstx(),
        location: None,
        precision: 8,
        kind: AssetKind::Stable,
        deprecated: false,
    });
    a
}

/// A seed of `n` identities, `n` delegated accounts and `n` grants, one over each account and
/// bounded to USD, except grant `bound`, if any, which also lists `TSTX`.
fn seed(n: u64, bound: Option<u64>) -> ChainSpec {
    let op = op(0);
    let mut records = Vec::new();
    for _ in 0..n {
        records.push(SpecRecord::Identity {
            kind: ledger_core::identity::IdentityKind::Person,
            verifier: Verifier::new(vec![0x5E; 32]),
        });
    }
    for i in 0..n {
        records.push(SpecRecord::Account {
            owner: chainspec::identity_of(&op, i),
            kind: AccountKind::Customer,
        });
    }
    for i in 0..n {
        let assets = if bound == Some(i) {
            NonEmptySet::new([usd(), tstx()].into()).unwrap()
        } else {
            NonEmptySet::one(usd())
        };
        records.push(SpecRecord::Grant(GrantRequest {
            kind: GrantKind::Operator,
            principal: chainspec::operator_of(&op),
            subject: Subject::Account(chainspec::account_of(&op, i)),
            ops: NonEmptySet::one(OperationKind::Transfer),
            per_operation: Some(NonZeroMinor::new(Minor::new(1_000)).unwrap()),
            per_window: None,
            assets: Some(assets),
            counterparties: None,
            parent: None,
            expires: None,
        }));
    }
    ChainSpec::build(op, 1, assets(), records, None, None, ChunkLimits::DEFAULT).unwrap()
}

/// A book founded on `spec`, its genesis built natively.
fn founded(spec: ChainSpec) -> (Founding, Fx<Measured>) {
    let mut g = genesis();
    g.assets = assets();
    g.chainspec = Some(spec.commitment());
    g.spec = Some(std::sync::Arc::new(spec));
    let limits = Limits {
        budget: MAX_BUDGET,
        ..Limits::DEFAULT
    };
    let fx = Fx::with_genesis(
        Measured {
            inner: NativePort::new(),
            samples: Vec::new(),
        },
        limits,
        g.clone(),
    );
    let f = Founding {
        genesis: g,
        op: op(0),
        at: 1,
        keys: keys(),
        limits,
        genesis_roots: None,
    };
    (f, fx)
}

/// An in-place upgrade to the next rules version, taken through the Agent.
fn upgrade(fx: &mut Fx<Measured>) -> u64 {
    let source = HeldCode::default();
    let g = fx.driver.writer().generation;
    let h = source.version(g + 1);
    let checked = core_binding::staging::fetch(&source, &h, g).unwrap();
    fx.driver.port_mut().stage_code(checked).unwrap();
    let n = fx.driver.seq() + 2;
    let c = fx.ctx(fx.operator());
    fx.replica
        .upgrade(&c, h, n, b"the removal's rules version")
        .unwrap();
    fx.synced();
    let _ = fx.person();
    assert_eq!(fx.driver.writer().generation, g + 1);
    g + 1
}

/// Up to the removal's rules version, and the operator's grant of its kind.
fn ready(fx: &mut Fx<Measured>) {
    while fx.driver.writer().generation < ASSET_RETIREMENT {
        upgrade(fx);
    }
    let ctx = fx.ctx(fx.operator());
    let op = fx.operator();
    fx.replica
        .grant_operator(
            &ctx,
            op.principal,
            NonEmptySet::new([OperatorOpKind::RegisterAsset, OperatorOpKind::RemoveAsset].into())
                .unwrap(),
            None,
            b"asset staff",
        )
        .unwrap();
    fx.synced();
}

/// The last input's measurement.
fn last(fx: &Fx<Measured>) -> Sample {
    *fx.driver.port().samples.last().unwrap()
}

fn retire(fx: &mut Fx<Measured>) -> Result<Option<SweepNext>, Refusal> {
    let ctx = fx.ctx(fx.operator());
    let r = fx
        .replica
        .retire_asset(&ctx, tstx(), ReviewRef([8; 32]), b"8 decimals, never used")
        .map(|a| a.value);
    fx.synced();
    r
}

fn page(fx: &mut Fx<Measured>, page: SweepPage) -> Result<Option<SweepNext>, Refusal> {
    let ctx = fx.ctx(fx.operator());
    let r = fx
        .replica
        .sweep_asset_uses(&ctx, tstx(), page, b"sweep")
        .map(|a| a.value);
    fx.synced();
    r
}

fn remove(fx: &mut Fx<Measured>) -> Result<(), Refusal> {
    let ctx = fx.ctx(fx.operator());
    let r = fx
        .replica
        .remove_asset(&ctx, tstx(), ReviewRef([9; 32]), b"wrong precision")
        .map(|a| a.value);
    fx.synced();
    r
}

/// The sweep, page by page at the level the host suggests, from `next`: each page's measurement,
/// or the first refusal with the page it refused.
fn sweep(
    fx: &mut Fx<Measured>,
    mut next: Option<SweepNext>,
) -> Result<Vec<Sample>, (Refusal, SweepPage)> {
    let mut samples = Vec::new();
    while let Some(n) = next {
        let p = SweepPage {
            start: n.start,
            level: n.level.expect("the replica suggests a level"),
        };
        next = page(fx, p).map_err(|e| (e, p))?;
        samples.push(last(fx));
    }
    Ok(samples)
}

/// Every measured input of the removal is bounded: no kind whole, at most a page of leaves and a
/// fixed overhead, the blobs within one pack, the heap within three quarters of the guest's.
fn bounded(what: &str, s: &Sample) {
    assert_eq!(s.whole, 0, "{what} reads a kind whole");
    assert!(
        s.leaves <= MAX_SWEEP_PAGE + 1 + OVERHEAD_LEAVES,
        "{what} witnesses {} leaves",
        s.leaves
    );
    assert!(s.blobs <= PACK, "{what} brings {} B of blobs", s.blobs);
    assert!(
        s.peak + s.blobs <= GUEST_HEAP * 3 / 4,
        "{what} reaches {} B",
        s.peak + s.blobs
    );
}

/// The worst of `samples`, field by field.
fn worst(samples: &[Sample]) -> Sample {
    samples.iter().fold(Sample::default(), |w, s| Sample {
        peak: w.peak.max(s.peak),
        blobs: w.blobs.max(s.blobs),
        service: w.service.max(s.service),
        leaves: w.leaves.max(s.leaves),
        whole: w.whole.max(s.whole),
    })
}

/// One clean removal on a book of `n` accounts and `n` grants: the worst page, the retirement's
/// and the removal's measurements, and the number of pages.
fn clean_removal(n: u64) -> (Sample, Sample, Sample, usize) {
    let (_, mut fx) = founded(seed(n, None));
    ready(&mut fx);
    let next = retire(&mut fx).unwrap();
    let retired = last(&fx);
    assert_eq!(next.map(|n| n.start), Some(SWEEP_START));
    let pages = sweep(&mut fx, next).unwrap();
    assert_eq!(
        fx.replica.state().retirement(&tstx()).map(|r| r.cursor),
        Some(SWEEP_END)
    );
    remove(&mut fx).unwrap();
    let removed = last(&fx);
    assert!(fx.replica.state().registry().get(&tstx()).is_err());
    (worst(&pages), retired, removed, pages.len())
}

/// `REQ-RT-1`, `REQ-AS-10`, `DEC-28`: the witness-size test. For each input of the removal, the
/// witness does not grow with the number of accounts and grants: a book of `grow()` of each
/// (4 000 by default) gives the same bound as one of 200, page for page, whereas a whole read of
/// either kind would bring the Agent every one of their leaves.
#[test]
fn req_rt_1_every_input_of_a_removal_has_a_bounded_witness_however_large_the_book() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let small = clean_removal(200);
    let n = grow();
    let large = clean_removal(n);
    for (size, (page, retired, removed, pages)) in [(200, small), (n, large)] {
        println!(
            "{size} accounts and grants: {pages} pages; worst page {} leaves, {} B service \
             witness, {} B blobs, {} B peak; retire {} leaves, {} B; remove {} leaves, {} B",
            page.leaves,
            page.service,
            page.blobs,
            page.peak,
            retired.leaves,
            retired.blobs,
            removed.leaves,
            removed.blobs
        );
        bounded("a page", &page);
        bounded("the retirement", &retired);
        bounded("the removal", &removed);
        // Two kinds of `size` leaves, at most a page each: at least that many pages.
        assert!(pages as u64 >= 2 * size / MAX_SWEEP_PAGE as u64, "{pages}");
    }
    // The same bound at twenty times the book: only the proofs' depth grows, by the logarithm of
    // the tree (a few dozen bytes a level), never by the collection.
    let (s, l) = (small.0, large.0);
    assert!(l.leaves <= MAX_SWEEP_PAGE + 1 + OVERHEAD_LEAVES);
    assert!(
        l.service <= s.service + 8 * 1024,
        "the page's witness grew from {} B to {} B",
        s.service,
        l.service
    );
    for (a, b) in [(small.1, large.1), (small.2, large.2)] {
        assert!(
            b.leaves <= a.leaves + 2,
            "{} to {} leaves",
            a.leaves,
            b.leaves
        );
        assert!(
            b.blobs <= a.blobs + 4 * 1024,
            "{} B to {} B",
            a.blobs,
            b.blobs
        );
    }
}

/// `REQ-AS-10` step 2, through the Agent on a large book: a page over the maximum is
/// `ERR-BatchTooLarge` from a bounded witness (the Agent sees 65 leaves of it, not all), a grant
/// bounding the asset refuses its page in the replica and the Agent alike and leaves the cursor,
/// and once revoked the same page goes on. The whole history then replays from genesis under its
/// code map, the cursor and every page with it; and the asset registers again at 6 decimals.
#[test]
fn req_as_10_a_large_book_refuses_a_page_too_large_and_a_finding_then_removes() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let n = 600;
    let flagged = 417;
    let (founding, mut fx) = founded(seed(n, Some(flagged)));
    ready(&mut fx);
    let next = retire(&mut fx).unwrap();
    let whole = SweepPage {
        start: SWEEP_START,
        level: 8,
    };
    assert_eq!(page(&mut fx, whole), Err(Refusal::BatchTooLarge));
    bounded("a page too large", &last(&fx));
    assert!(last(&fx).leaves <= MAX_SWEEP_PAGE + 1 + OVERHEAD_LEAVES);
    let (refusal, at) = sweep(&mut fx, next).unwrap_err();
    assert_eq!(refusal, Refusal::NotAuthorised);
    bounded("a page with a finding", &last(&fx));
    assert_eq!(at.start[0], ledger_core::leaves::kind::GRANT);
    assert_eq!(
        fx.replica.state().retirement(&tstx()).map(|r| r.cursor),
        Some(at.start)
    );
    assert_eq!(remove(&mut fx), Err(Refusal::NotAuthorised));
    // The operator revokes the grant while the asset is retiring, and sends the same page.
    let grant = chainspec::grant_of(&op(0), Section::Grants, flagged);
    let ctx = fx.ctx(fx.operator());
    fx.replica.revoke_authority(&ctx, grant).unwrap();
    fx.synced();
    let next = page(&mut fx, at).unwrap();
    sweep(&mut fx, next).unwrap();
    remove(&mut fx).unwrap();
    let ctx = fx.ctx(fx.operator());
    fx.replica
        .register_asset(
            &ctx,
            RegisterAsset {
                id: tstx(),
                precision: 6,
                kind: AssetKind::Stable,
                review: ReviewRef([7; 32]),
            },
            b"relisting at 6 decimals",
        )
        .unwrap();
    fx.synced();
    let again = *fx.replica.state().registry().get(&tstx()).unwrap();
    assert_eq!(
        (again.precision(), again.status()),
        (6, AssetStatus::Active)
    );

    // Replay: every input, the cursor's moves included, reaches its archived roots.
    let map = claimed_code_map(&mut NativePort::new(), &founding, &fx.archive).unwrap();
    assert_eq!(
        map.ranges.last().map(|r| r.generation),
        Some(ASSET_RETIREMENT)
    );
    let mut port = NativePort::new();
    port.code_map = map.versions();
    let (_, rolled) = roll_forward_agent(port, &founding, &fx.archive, &ReplicaStores::Memory)
        .unwrap_or_else(|e| panic!("the history replays: {e}"));
    assert_eq!(rolled.log_head, fx.archive.last().unwrap().log_hash());
}

/// `REQ-RT-16`, `REQ-RT-17`, `REQ-RT-20`: before the removal's rules version, on a book an older
/// Agent runs, every step is `ERR-MalformedCommand` in the replica, and no input is made of it: the
/// driver has nothing to send, so an Agent that cannot decode the call never sees it. A grant of
/// the removal's kind likewise. After the upgrade the same steps are taken.
#[test]
fn req_rt_20_the_removal_makes_no_input_before_its_rules_version() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let (_, mut fx) = founded(seed(8, None));
    upgrade(&mut fx);
    assert!(fx.driver.writer().generation < ASSET_RETIREMENT);
    let before = fx.driver.seq();
    let op = fx.operator();
    let ctx = fx.ctx(op);
    assert_eq!(
        fx.replica
            .grant_operator(
                &ctx,
                op.principal,
                NonEmptySet::one(OperatorOpKind::RemoveAsset),
                None,
                b"x",
            )
            .map(|a| a.value),
        Err(Refusal::MalformedCommand)
    );
    let ctx = fx.ctx(op);
    assert_eq!(
        fx.replica
            .retire_asset(&ctx, tstx(), ReviewRef([8; 32]), b"x")
            .map(|a| a.value),
        Err(Refusal::MalformedCommand)
    );
    let ctx = fx.ctx(op);
    assert_eq!(
        fx.replica
            .remove_asset(&ctx, tstx(), ReviewRef([9; 32]), b"x")
            .map(|a| a.value),
        Err(Refusal::MalformedCommand)
    );
    assert!(fx.sync().unwrap().is_none(), "nothing to send");
    assert_eq!(fx.driver.seq(), before);
    // Taken at the rules version.
    ready(&mut fx);
    assert!(retire(&mut fx).unwrap().is_some());
    assert_eq!(
        fx.replica.state().registry().get(&tstx()).unwrap().status(),
        AssetStatus::Retiring
    );
}
