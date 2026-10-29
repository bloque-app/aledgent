// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A whole deployment in process for this crate's tests and the conformance suite's `F-018`
//! cases: ingress over any received-log store, the in-process core, the settlement adapter over
//! any store and the fake settlement layer (`services/settle`'s), an audit store on disk, and an
//! operations principal. It is `settle::testkit::Kit` generalised over both stores, so the drill
//! runs the same way over memory and over PostgreSQL.
//!
//! Nothing in `opsctl` uses it. The keys in it are the test deployments' public constants.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use audit_store::{AuditStoreConfig, FileAuditStore, WriterCredential};
use axum::Router;
use ingress::auth::{CredentialKind, Token};
use ingress::clock::{Clock, SharedClock};
use ingress::inprocess::MemJournal;
use ingress::service::Replica;
use ingress::store::Store as IngressStore;
use ingress::testkit::{Deployment, Party, T0, TestCore, USD, genesis};
use ledger_core::id::{AccountId, CredentialId, IdentityId, OperationId, account_reference};
use ledger_core::ledger::Ctx;
use ledger_core::principal::Caller;
use ledger_kernel::keys::KeyRing;
use ledger_kernel::posting::OperationRef;
use ledger_kernel::store::MemStore as KernelMemStore;
use ledger_kernel::vault::MemVault;
use serde_json::{Value, json};
use settle::adapter::Adapter;
use settle::chain::AccountId32;
use settle::chain::fake::{FakeChain, FakeConfig};
use settle::link::RouterTransport;
use settle::signer::Signer;
use settle::store::Store as AdapterStore;
use settle::testkit::{
    ADAPTER_TOKEN, INSTANCE, PAYEE, TEST_ANCHOR_PASS_USER, TEST_ANCHOR_SECRET, TEST_LANE_SECRET,
    TEST_PASS_USER, adapter_config, chain_usd, pass_lane,
};

use crate::Error;
use crate::backup::Manifest;
use crate::backup::core::CoreBackupJob;
use crate::restore::{ImportBy, restore_core};
use crate::seal::SealingKey;

/// How long a test waits on a condition the deployment reaches by itself before it fails: generous,
/// so a slow or contended runner is slow rather than wrong (as `F-009`'s cases, issue #48).
pub const PATIENCE: Duration = Duration::from_secs(120);

/// The operations principal's token.
pub const OPS_TOKEN: &str = "ops-token";
/// The test audit store's writer credential: a constant, public, worth nothing.
pub const TEST_AUDIT_CREDENTIAL: &[u8] = b"test-audit-writer";
/// The test deployments' sealing key: a constant, public, worth nothing.
pub const TEST_SEALING_KEY: [u8; 32] = [0x5a; 32];

/// The operator kinds the operations principal holds: the export's privileged read, the import,
/// the signal queries, and the daily settlement comparison (`F-017`).
pub const OPS_KINDS: [&str; 4] = [
    "privileged_read",
    "import_book",
    "read_signals",
    "compare_settlement",
];

/// How much the kit deposits through the settlement layer at its start, so the instance's position
/// and the book's record of it agree from the first comparison (`F-017` decision 24).
pub const OPENING_DEPOSIT: u64 = 5_000_000;

/// The adapter every kit runs.
pub type KitAdapter<A> = Adapter<FakeChain, A, RouterTransport>;

/// A deployment with its adapter and an operations principal. See the module documentation.
pub struct OpsKit<I: IngressStore, A: AdapterStore> {
    /// Ingress and the core.
    pub d: Deployment<I>,
    /// The fake settlement layer: the truth for settlement, outside every backup.
    pub chain: FakeChain,
    /// The adapter's store.
    pub adapter_store: A,
    /// The adapter.
    pub adapter: KitAdapter<A>,
    /// The adapter's principal.
    pub adapter_party: Party,
    /// The lanes.
    pub lanes: Vec<Arc<dyn Signer>>,
    /// The anchor key (`F-017`): one of the instance's keys, used for anchors alone.
    pub anchor: Arc<dyn Signer>,
    /// The operations principal.
    pub ops: Party,
    /// The audit store.
    pub audit: FileAuditStore,
    /// Where it lives.
    pub audit_dir: PathBuf,
    /// The customers, funded.
    pub parties: Vec<Party>,
    config: settle::config::AdapterConfig,
    counter: u64,
}

async fn ok<I: IngressStore>(
    d: &Deployment<I>,
    method: &str,
    path: &str,
    token: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> Value {
    // A transfer or a reservation is acknowledged at `Received`; wait for its decision.
    let (s, v) = d
        .call_decided(method, path, Some(token), headers, body)
        .await;
    assert!(s.is_success(), "{method} {path}: {s} {v}");
    v
}

/// The audit store's configuration in `dir`, apart from `book_dir`.
pub fn audit_config(dir: &Path, book_dir: &Path) -> AuditStoreConfig {
    AuditStoreConfig {
        dir: dir.to_path_buf(),
        book_dir: book_dir.to_path_buf(),
        segment_records: 64,
    }
}

/// Open (initialising if new) the test audit store in `dir`.
pub fn open_audit(dir: &Path, book_dir: &Path) -> FileAuditStore {
    let c = audit_config(dir, book_dir);
    let cred = WriterCredential::new(TEST_AUDIT_CREDENTIAL.to_vec());
    if !dir.join("credential").exists() {
        FileAuditStore::init(&c, &cred).unwrap_or_else(|e| panic!("the audit store: {e:?}"));
    }
    FileAuditStore::open(&c, &cred).unwrap_or_else(|e| panic!("the audit store: {e:?}"))
}

/// The anchor key: a device of a test community PASS account (`F-027`).
fn anchor_signer() -> Arc<dyn Signer> {
    pass_lane(&TEST_ANCHOR_SECRET, TEST_ANCHOR_PASS_USER)
}

/// The lanes: a device of a test community PASS account (`F-027`).
fn lanes() -> Vec<Arc<dyn Signer>> {
    vec![pass_lane(&TEST_LANE_SECRET, TEST_PASS_USER)]
}

/// The core's host factory for a fresh in-memory book (the core's later starts).
pub fn mem_factory() -> impl Fn() -> Result<
    ledger_kernel::book::Book<KernelMemStore, MemVault>,
    ledger_kernel::book::BindingFault,
> + Send
+ 'static {
    let root = genesis(T0).root;
    move || {
        ledger_kernel::book::Book::bootstrap(
            KernelMemStore::new(),
            MemVault::new(),
            KeyRing::from_root(root),
        )
    }
}

impl<I: IngressStore, A: AdapterStore> OpsKit<I, A> {
    /// Start: ingress over `ingress_store`, the adapter over `adapter_store`, the fake layer with
    /// the instance registered and funded, the audit store in `audit_dir` (apart from
    /// `book_dir`), the operations principal with [`OPS_KINDS`], and three funded customers. The
    /// adapter has declared and serves.
    pub async fn start(
        ingress_store: I,
        adapter_store: A,
        audit_dir: &Path,
        book_dir: &Path,
    ) -> OpsKit<I, A> {
        let d =
            Deployment::start_on(ingress_store, ingress::testkit::fast_config("ingress-0")).await;
        let op = d.operator.token.clone();
        let org = |verifier: &'static str| json!({ "kind": "organisation", "verifier": verifier });
        let v = ok(&d, "POST", "/v1/identities", &op, &[], Some(org("0b"))).await;
        let adapter_party = party_of(&v, ADAPTER_TOKEN, "mem://adapter");
        register(&d, &adapter_party).await;
        let v = ok(&d, "POST", "/v1/identities", &op, &[], Some(org("0c"))).await;
        let ops = party_of(&v, OPS_TOKEN, "mem://ops");
        register(&d, &ops).await;
        ok(
            &d,
            "POST",
            "/v1/ops/operator-grants",
            &op,
            &[],
            Some(json!({
                "principal": ops.identity,
                "kinds": OPS_KINDS,
                "justification": "F-018: backups, restores and signals",
            })),
        )
        .await;
        ok(
            &d,
            "POST",
            "/v1/ops/settlement",
            &op,
            &[],
            Some(json!({
                "adapter": adapter_party.identity,
                "instance": INSTANCE,
                "inbound_deadline": 600_000u64,
                "expiry_window": 40,
                "block_time": 1_000,
                "finality_margin": 60_000u64,
            })),
        )
        .await;
        let lanes = lanes();
        let anchor = anchor_signer();
        let chain = FakeChain::new(FakeConfig::default());
        let mut keys: Vec<AccountId32> = lanes.iter().map(|l| l.account()).collect();
        keys.push(anchor.account());
        chain.register_instance(INSTANCE, &keys);
        chain.set_attestation_key(INSTANCE, ingress::testkit::test_clerk().public_key());
        chain.set_decimals(&chain_usd(), ingress::testkit::USD, 2);
        for l in lanes.iter().chain([&anchor]) {
            chain.set_native(l.account(), 1_000_000);
        }
        chain.advance(1);
        let config = adapter_config(FakeConfig::default().genesis);
        let adapter = Adapter::new(
            config.clone(),
            chain.clone(),
            adapter_store.clone(),
            RouterTransport::new(d.router.clone(), ADAPTER_TOKEN),
            lanes.clone(),
            Arc::new(d.clock.clone()),
        )
        .with_anchor_signer(Arc::clone(&anchor));
        let audit = open_audit(audit_dir, book_dir);
        let mut kit = OpsKit {
            d,
            chain,
            adapter_store,
            adapter,
            adapter_party,
            lanes,
            anchor,
            ops,
            audit,
            audit_dir: audit_dir.to_path_buf(),
            parties: Vec::new(),
            config,
            counter: 0,
        };
        let r = kit
            .adapter
            .step()
            .await
            .unwrap_or_else(|e| panic!("step: {e}"));
        assert!(r.serving, "the adapter serves: {:?}", kit.adapter.status());
        for name in ["alice", "bob", "carol"] {
            let p = kit.d.party(name, 1_000_000).await;
            kit.parties.push(p);
        }
        // The instance's position is funded through the layer, so the book records it.
        let alice = kit.parties[0].clone();
        kit.deposit(&alice, OPENING_DEPOSIT);
        kit.settle(3).await;
        kit
    }

    /// Wait until the core has applied a tick at or after the ingress clock's current time, and
    /// every row is decided: deadlines, checkpoints and the ticker's age then stand at "now".
    /// A condition, within a generous deadline (`PATIENCE`): see
    /// [`ingress::testkit::Deployment::tick_to_now`].
    pub async fn tick_to_now(&self) {
        if let Err(why) = self.d.tick_to_now(PATIENCE).await {
            panic!("{why}");
        }
    }

    /// Let an anchoring interval pass: the clock moves past it, the core ticks and creates a
    /// checkpoint, and the adapter anchors it until it is final and compared (at most `n` rounds).
    pub async fn anchor_round(&mut self, n: usize) {
        self.d.clock.advance(ingress::testkit::ANCHOR_INTERVAL_MS);
        self.tick_to_now().await;
        for _ in 0..n {
            let settled = self
                .d
                .core
                .with_ledger(|l| {
                    let s = l.state().anchoring();
                    s.in_flight().is_none() && s.uncompared().is_empty()
                })
                .unwrap_or(false);
            if settled {
                return;
            }
            self.chain.advance(1);
            self.adapter
                .step()
                .await
                .unwrap_or_else(|e| panic!("step: {e}"));
            assert!(self.d.quiesce(PATIENCE).await, "rows undecided");
        }
    }

    fn next(&mut self) -> u64 {
        self.counter += 1;
        self.counter
    }

    /// A context for the operations principal at the ingress clock's time, under a fresh
    /// operation reference.
    pub fn ops_ctx(&mut self) -> Ctx {
        let n = self.next();
        let mut r = [0x0F; 16];
        r[8..].copy_from_slice(&n.to_be_bytes());
        Ctx {
            caller: caller_of(&self.ops),
            op: OperationId::from_ref(OperationRef(r)),
            now: self.d.clock.now(),
        }
    }

    /// One round of traffic through ingress and the adapter: a transfer, a captured and a
    /// released reservation, a withdrawal the adapter executes on the layer, and a deposit the
    /// adapter observes; the layer finalizes and every row is decided.
    pub async fn round(&mut self) {
        let n = self.next();
        let (a, b, c) = (
            self.parties[0].clone(),
            self.parties[1].clone(),
            self.parties[2].clone(),
        );
        let d = &self.d;
        let (s, v) = d
            .call(
                "POST",
                "/v1/transfers",
                Some(&a.token),
                &[],
                Some(transfer(&a, &b, 100 + n)),
            )
            .await;
        assert_eq!(s.as_u16(), 202, "transfer: {v}");
        let reserve = json!({ "payer": b.account, "asset": USD, "amount": (50 + n).to_string() });
        let r1 = ok(
            d,
            "POST",
            "/v1/reservations",
            &b.token,
            &[("client-key", &format!("r1-{n}"))],
            Some(reserve.clone()),
        )
        .await;
        let r1 = reference(&r1);
        ok(
            d,
            "POST",
            &format!("/v1/reservations/{r1}/capture"),
            &b.token,
            &[],
            Some(json!({ "destination": c.account, "cleared": (50 + n).to_string() })),
        )
        .await;
        let r2 = ok(
            d,
            "POST",
            "/v1/reservations",
            &b.token,
            &[("client-key", &format!("r2-{n}"))],
            Some(reserve),
        )
        .await;
        let r2 = reference(&r2);
        ok(
            d,
            "POST",
            &format!("/v1/reservations/{r2}/release"),
            &b.token,
            &[],
            None,
        )
        .await;
        // A withdrawal, executed on the layer by the adapter.
        let w = ok(
            d,
            "POST",
            "/v1/withdrawals",
            &c.token,
            &[],
            Some(json!({ "payer": c.account, "asset": USD, "amount": (10 + n).to_string() })),
        )
        .await;
        let w = reference(&w);
        ok(
            d,
            "POST",
            &format!("/v1/withdrawals/{w}/submit"),
            &c.token,
            &[],
            Some(json!({ "destination": { "account": hex::encode(PAYEE) } })),
        )
        .await;
        // A deposit to alice's account reference.
        self.deposit(&a, 20 + n);
        self.settle(4).await;
    }

    /// Put a deposit of `amount` to `p`'s account reference on the layer (not yet produced).
    pub fn deposit(&self, p: &Party, amount: u64) {
        let account = AccountId::parse(p.account.as_deref().unwrap_or_default())
            .unwrap_or_else(|_| panic!("a customer account"));
        let from = [0x77; 32];
        self.chain.mint(from, &chain_usd(), u128::from(amount));
        self.chain.deposit(
            from,
            INSTANCE,
            account_reference(&account),
            &chain_usd(),
            u128::from(amount),
        );
    }

    /// `n` adapter rounds — a block produced and finalized, a step — then wait until every row
    /// is decided.
    pub async fn settle(&mut self, n: usize) {
        for _ in 0..n {
            self.chain.advance(1);
            self.adapter
                .step()
                .await
                .unwrap_or_else(|e| panic!("step: {e}"));
            self.d.clock.advance(1_000);
        }
        assert!(self.d.quiesce(PATIENCE).await, "rows undecided");
    }

    /// Back up the running core into `root/core/<id>/`, with the received-log's pass-through data
    /// read after its checkpoint (`REQ-PO-5`).
    pub async fn backup_core(
        &mut self,
        root: &Path,
        id: &str,
    ) -> Result<(PathBuf, Manifest), Error> {
        let ctx = self.ops_ctx();
        let key = SealingKey::from_bytes(TEST_SEALING_KEY);
        let job = CoreBackupJob {
            ctx,
            justification: b"F-018 scheduled core backup",
            sealing: &key,
            id,
        };
        let taken = crate::backup::core::take(&self.d.core, &job, &mut self.audit)?;
        let rows = self
            .d
            .store
            .rows()
            .await
            .map_err(|e| Error::Restore(format!("the received-log: {e}")))?;
        taken.write(root, &rows)
    }

    /// The running core's kernel composite root, book root and input count.
    pub fn roots(&self) -> Option<Roots> {
        self.d
            .core
            .with_ledger(|l| {
                let kernel = l.book().root().ok()?;
                let (_, book) = l.checkpoint().ok()?;
                Some((kernel, book))
            })
            .flatten()
            .zip(self.d.core.inputs())
            .map(|((kernel, book), inputs)| Roots {
                kernel,
                book,
                inputs,
            })
    }

    /// Stop the replica's forwarder, ticker and delivery worker, and forward every row still
    /// undecided, as the forwarder would: afterwards the core's inputs are exactly the log's
    /// decided rows, so its roots are the last committed ones.
    pub async fn quiet(&mut self) {
        use ingress::boundary::Core as _;
        self.d.replica.kill();
        let rows = self.d.store.rows().await.unwrap_or_default();
        for r in rows
            .into_iter()
            .filter(|r| r.status != ingress::store::Status::Decided)
        {
            let now = self.d.clock.now();
            let row = self
                .d
                .store
                .mark_forwarded(r.new.op_ref, now)
                .await
                .unwrap_or_else(|e| panic!("mark forwarded: {e}"));
            let input = ingress::service::input_of(&row).unwrap_or_else(|e| panic!("{e}"));
            let reply = self
                .d
                .core
                .apply(&input)
                .await
                .unwrap_or_else(|e| panic!("apply: {e:?}"));
            let bytes = serde_json::to_vec(&reply).unwrap_or_default();
            self.d
                .store
                .mark_decided(r.new.op_ref, reply.input_seq, bytes, now)
                .await
                .unwrap_or_else(|e| panic!("mark decided: {e}"));
        }
    }

    /// The failure: the core dies with its journal, and the replica with it.
    pub fn fail(&mut self) {
        self.d.replica.kill();
        self.d.core.stop();
    }

    /// The adapter over `store` against `router`: a restored adapter.
    pub fn adapter_on<S: AdapterStore>(&self, store: S, router: &Router) -> KitAdapter<S> {
        Adapter::new(
            self.config.clone(),
            self.chain.clone(),
            store,
            RouterTransport::new(router.clone(), ADAPTER_TOKEN),
            self.lanes.clone(),
            Arc::new(self.d.clock.clone()),
        )
        .with_anchor_signer(Arc::clone(&self.anchor))
    }
}

/// A kernel composite root, a book root and an input count.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Roots {
    /// The kernel's composite root.
    pub kernel: [u8; 32],
    /// The book root: kernel and service.
    pub book: [u8; 32],
    /// Inputs applied.
    pub inputs: u64,
}

/// A caller of a party.
pub fn caller_of(p: &Party) -> Caller {
    Caller {
        principal: IdentityId::parse(&p.identity).unwrap_or_else(|_| panic!("an identity")),
        credential: CredentialId::parse(&p.credential).unwrap_or_else(|_| panic!("a credential")),
        on_behalf_of: None,
    }
}

fn party_of(v: &Value, token: &str, endpoint: &str) -> Party {
    Party {
        identity: v["result"]["identity"].as_str().unwrap_or_default().into(),
        credential: v["result"]["credential"]
            .as_str()
            .unwrap_or_default()
            .into(),
        token: token.into(),
        account: None,
        endpoint: endpoint.into(),
    }
}

async fn register<I: IngressStore>(d: &Deployment<I>, p: &Party) {
    d.auth
        .add_token(
            &Token::new(p.token.as_bytes()),
            &p.identity,
            &p.credential,
            CredentialKind::Service,
        )
        .unwrap_or_else(|e| panic!("a token registers: {e}"));
}

fn transfer(from: &Party, to: &Party, amount: u64) -> Value {
    json!({ "legs": [{ "from": from.account, "to": to.account, "asset": USD, "amount": amount.to_string() }] })
}

fn reference(v: &Value) -> String {
    v["operation_reference"]
        .as_str()
        .unwrap_or_else(|| panic!("no operation reference in {v}"))
        .to_owned()
}

/// The fake settlement layer as a verifier reads it: anchors and the attestation key at the
/// finalized head (`verify::AnchorSource`).
#[derive(Clone)]
pub struct FakeAnchors(pub FakeChain);

impl FakeAnchors {
    /// The deployment whose contract `venue` is (`F-027`): the fake layer runs the settlement
    /// contract only, so a pallet venue holds nothing here.
    fn instance(venue: &verify::Venue) -> Option<u32> {
        match venue {
            verify::Venue::Contract { account }
                if *account == settle::chain::fake::position_account(INSTANCE) =>
            {
                Some(INSTANCE)
            }
            _ => None,
        }
    }
}

impl verify::AnchorSource for FakeAnchors {
    fn anchor(
        &self,
        venue: &verify::Venue,
        number: u64,
    ) -> Result<Option<verify::PublicAnchor>, String> {
        let read_at = self.0.finalized_number();
        let Some(instance) = Self::instance(venue) else {
            return Ok(None);
        };
        Ok(self
            .0
            .anchor_at_finalized(instance, number)
            .map(|(commitment, stored_at)| verify::PublicAnchor {
                commitment,
                stored_at,
                read_at,
            }))
    }

    fn attestation_key(&self, venue: &verify::Venue) -> Result<Option<[u8; 32]>, String> {
        Ok(Self::instance(venue).and_then(|i| self.0.attestation_key_at_finalized(i)))
    }
}

/// The parts of a deployment that are not restored from a backup: the ingress authenticator
/// (its tokens are configuration), the recipients' transport, the ingress clock, and the
/// settlement layer with the adapter's configuration and lanes (the layer is the truth for
/// settlement; the lanes' keys are the KMS's).
pub struct Outside<A: AdapterStore> {
    /// The authenticator.
    pub auth: ingress::auth::Authenticator,
    /// Where notifications go.
    pub transport: Arc<dyn ingress::delivery::Transport>,
    /// The ingress clock.
    pub clock: ingress::clock::ManualClock,
    /// The operations principal, which imports.
    pub ops: Party,
    /// The layer's public read of anchors, at its finalized head (`F-017`): the drill's `INV-19`.
    pub anchors: FakeAnchors,
    /// The layer, its adapter configuration and lanes, as a factory of adapters.
    #[allow(clippy::type_complexity)]
    pub adapter: Box<dyn Fn(A, &Router) -> KitAdapter<A> + Send + Sync>,
}

impl<I: IngressStore, A: AdapterStore> OpsKit<I, A> {
    /// What a restore on an isolated host takes from outside the backups.
    pub fn outside(&self) -> Outside<A> {
        let config = self.config.clone();
        let chain = self.chain.clone();
        let lanes = self.lanes.clone();
        let anchor = Arc::clone(&self.anchor);
        let clock = self.d.clock.clone();
        Outside {
            auth: self.d.auth.clone(),
            transport: Arc::clone(&self.d.recipients) as Arc<dyn ingress::delivery::Transport>,
            clock: self.d.clock.clone(),
            ops: self.ops.clone(),
            anchors: FakeAnchors(self.chain.clone()),
            adapter: Box::new(move |store, router| {
                Adapter::new(
                    config.clone(),
                    chain.clone(),
                    store,
                    RouterTransport::new(router.clone(), ADAPTER_TOKEN),
                    lanes.clone(),
                    Arc::new(clock.clone()),
                )
                .with_anchor_signer(Arc::clone(&anchor))
            }),
        }
    }
}

/// Where a drill runs and what it restores.
pub struct DrillPlan<'a> {
    /// The drill's identity.
    pub id: &'a str,
    /// The isolated host's name, for the record.
    pub host: &'a str,
    /// A directory of the isolated host's own: the restored audit store and leaf store go here.
    pub workdir: &'a Path,
    /// The core backup to restore.
    pub core_backup: &'a Path,
    /// The audit backup to restore.
    pub audit_backup: &'a Path,
    /// The source's last committed roots, when known.
    pub expected: Option<crate::drill::Expected>,
    /// The WAL window of the received-log's restore, if it was restored from an archive.
    pub wal_window_s: Option<u32>,
}

/// A drill's restored deployment, running.
pub struct Drilled<I: IngressStore, A: AdapterStore> {
    /// The restored core.
    pub core: TestCore,
    /// A replica over the restored received-log.
    pub replica: Replica<I, TestCore>,
    /// Its surface.
    pub router: Router,
    /// The restored adapter.
    pub adapter: KitAdapter<A>,
    /// The restored audit store.
    pub audit: FileAuditStore,
    /// The drill's record.
    pub report: crate::drill::DrillReport,
}

/// Run the restore drill (`T-018-03`): restore the audit store, the core from its backup against
/// the received-log in `ingress_store`, check the restored book before anything else reaches it,
/// rebuild the leaf store, start a replica, let it decide what the log holds undecided, check that
/// every acknowledged operation exists, and resume the adapter over `adapter_store` from the head
/// it accepted until it accepts the layer's. `ingress_store` and `adapter_store` are restored to their latest point.
pub async fn run_drill<I: IngressStore, A: AdapterStore>(
    plan: &DrillPlan<'_>,
    outside: &Outside<A>,
    ingress_store: I,
    adapter_store: A,
) -> Result<Drilled<I, A>, Error> {
    use crate::drill::{Check, DataLoss, DrillReport, Verdict, acknowledgements, checks, size_of};
    let started = std::time::Instant::now();
    let started_at = outside.clock.now();
    // The audit store, to the head the core backup's export names.
    let audit_dir = plan.workdir.join("audit");
    crate::backup::audit::restore(plan.audit_backup, &audit_dir)?;
    let mut audit = FileAuditStore::open(
        &audit_config(&audit_dir, &plan.workdir.join("book")),
        &WriterCredential::new(TEST_AUDIT_CREDENTIAL.to_vec()),
    )
    .map_err(|e| Error::Audit(format!("{e:?}")))?;
    // The core, and the ordered replay.
    let key = SealingKey::from_bytes(TEST_SEALING_KEY);
    let bundle = crate::backup::core::read(plan.core_backup, &key)?;
    let backup = bundle.manifest.id.clone();
    let export_bytes = bundle
        .manifest
        .artifact(crate::backup::core::EXPORT)
        .map_or(0, |a| a.bytes);
    let rows = ingress_store
        .rows()
        .await
        .map_err(|e| Error::Restore(format!("the received-log: {e}")))?;
    let carried = bundle.pass_through.clone();
    let by = ImportBy {
        ctx: Ctx {
            caller: caller_of(&outside.ops),
            op: OperationId::from_ref(OperationRef([0x1E; 16])),
            now: started_at,
        },
        justification: b"F-018 restore drill",
    };
    let (core, replay) = restore_core(
        bundle,
        genesis(T0),
        KernelMemStore::new(),
        MemVault::new(),
        mem_factory(),
        MemJournal::new(),
        Some(ingress::testkit::test_clerk()),
        &rows,
        &by,
        &mut audit,
    )
    .await?;
    // The checks, on the book exactly as restored.
    let (acks, pending) = acknowledgements(&rows)?;
    let leaf_dir = plan.workdir.join("leaf-store");
    let checked = core
        .with_ledger(|l| -> Result<_, Error> {
            let rebuild = crate::restore::rebuild_leaf_store(l.book().store(), Some(&leaf_dir))?;
            let c = checks(
                l,
                &acks,
                plan.expected.as_ref(),
                &rebuild,
                Some((
                    &outside.anchors as &dyn verify::AnchorSource,
                    replay.import_commits,
                )),
            );
            Ok((rebuild, c, size_of(l, rows.len(), export_bytes)))
        })
        .ok_or_else(|| Error::Core("the restored core is not running".into()))?;
    let (rebuild, mut found, size) = checked?;
    // REQ-PO-5 (ruling pq8-export (a)): the pass-through data the export carried, back on the
    // restored log; a log of another history fails the restore.
    let restored = crate::restore::restore_pass_through(&ingress_store, &carried).await;
    found.push(Check {
        name: "REQ-PO-5".into(),
        verdict: if restored.is_ok() {
            Verdict::Passed
        } else {
            Verdict::Failed
        },
        detail: match &restored {
            Ok(r) => format!("{r:?}"),
            Err(e) => e.to_string(),
        },
    });
    // Ingress over the restored log and core: what it holds undecided is decided now.
    let clock: SharedClock = Arc::new(outside.clock.clone());
    let replica = Replica::start(
        ingress_store.clone(),
        core.clone(),
        clock,
        outside.auth.clone(),
        Arc::clone(&outside.transport),
        ingress::testkit::fast_config("ingress-restored"),
    );
    let router = ingress::surface::router(Arc::clone(&replica.shared));
    let recovery_time_ms = started.elapsed().as_millis() as u64;
    let until = tokio::time::Instant::now() + PATIENCE;
    while !ingress_store
        .oldest_undecided()
        .await
        .is_ok_and(|o| o.is_none())
    {
        if tokio::time::Instant::now() >= until {
            return Err(Error::Restore("the restored log did not drain".into()));
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let lost: Vec<String> = core
        .with_ledger(|l| {
            pending
                .iter()
                .filter(|op| l.operation(op).is_none() && l.state().record(op).is_none())
                .map(|op| format!("Missing({})", op.to_did()))
                .collect()
        })
        .unwrap_or_default();
    if let Some(c) = found.iter_mut().find(|c| c.name == "NFR-4")
        && !lost.is_empty()
    {
        c.verdict = Verdict::Failed;
        c.detail = format!("{}; after the forwarder: {}", c.detail, lost.join("; "));
    }
    // The adapter, resuming from the head it accepted (amendment 0040).
    let mut adapter = (outside.adapter)(adapter_store, &router);
    let resume = crate::restore::resume_adapter(&mut adapter, 64).await?;
    found.push(Check {
        name: "REQ-IN-5".into(),
        verdict: if resume.is_clean() {
            Verdict::Passed
        } else {
            Verdict::Failed
        },
        detail: format!("{resume:?}"),
    });
    let inputs_restored = replay.last_input;
    let inputs_before = plan.expected.map(|e| e.inputs);
    let report = DrillReport {
        id: plan.id.to_owned(),
        host: plan.host.to_owned(),
        backup,
        started_at,
        recovery_time_ms,
        data_loss: DataLoss {
            inputs_before,
            inputs_restored,
            inputs_lost: inputs_before.map_or(0, |b| b.saturating_sub(inputs_restored)),
            wal_window_s: plan.wal_window_s,
        },
        replay,
        rebuild,
        resume: Some(resume),
        size,
        checks: found,
    };
    Ok(Drilled {
        core,
        replica,
        router,
        adapter,
        audit,
        report,
    })
}
