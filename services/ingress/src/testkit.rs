// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! An in-memory deployment — ingress over the in-memory store, the in-process core over a
//! memory journal, and recipients that verify what they receive — for this crate's tests and
//! the conformance suite's `F-009` cases (`crates/conformance/src/cases/f009.rs`).
//!
//! Requests go through the real router, in process, without a socket. Nothing here is used by
//! the binary.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ledger_core::account::Time;
use ledger_core::asset::{AssetId, AssetKind};
use ledger_core::authority::OperationKind;
use ledger_core::custody::{RecoveryConfig, RecoveryKey};
use ledger_core::id::OperationId;
use ledger_core::ledger::{AssetDecl, Genesis};
use ledger_core::lifecycle::LifecycleConfig;
use ledger_core::principal::Verifier;
use ledger_kernel::posting::OperationRef;
use ledger_kernel::store::MemStore as KernelMemStore;
use ledger_kernel::vault::MemVault;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::auth::{Authenticator, CredentialKind, Token};
use crate::boundary::Core;
use crate::clock::{Clock as _, ManualClock, SharedClock};
use crate::delivery::{BoxFuture, ReferenceVerifier, Transport, Verdict};
use crate::inprocess::{CoreGenesis, InProcessCore, MemJournal};
use crate::service::{Config, Replica};
use crate::store::mem::MemStore;
use crate::store::{Recipient, Status, Store};
use crate::surface::router;
use crate::wire::NotificationBody;

/// The core every test deployment runs.
pub type TestCore = InProcessCore<KernelMemStore, MemVault>;

/// What one endpoint received.
#[derive(Default, Clone)]
pub struct Received {
    /// Every notification accepted as fresh, in arrival order.
    pub fresh: Vec<NotificationBody>,
    /// Every delivery's identity, duplicates included, in arrival order.
    pub deliveries: Vec<String>,
    /// Deliveries recognised as seen before.
    pub duplicates: u64,
    /// Deliveries that failed verification.
    pub rejected: u64,
}

struct Endpoint {
    up: bool,
    verifier: ReferenceVerifier,
    received: Received,
}

/// Recipients in memory: each endpoint verifies every delivery with the reference verifier, and
/// can be taken down.
pub struct Recipients {
    clock: SharedClock,
    endpoints: Mutex<HashMap<String, Endpoint>>,
}

impl Recipients {
    /// No endpoints yet.
    pub fn new(clock: SharedClock) -> Recipients {
        Recipients {
            clock,
            endpoints: Mutex::new(HashMap::new()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Endpoint>> {
        self.endpoints.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Add an endpoint verifying with `secret`, up.
    pub fn add(&self, endpoint: &str, secret: &[u8]) {
        self.lock().insert(
            endpoint.to_owned(),
            Endpoint {
                up: true,
                verifier: ReferenceVerifier::new(secret.to_vec(), 60_000),
                received: Received::default(),
            },
        );
    }

    /// Take one endpoint down or bring it up.
    pub fn set_up(&self, endpoint: &str, up: bool) {
        if let Some(e) = self.lock().get_mut(endpoint) {
            e.up = up;
        }
    }

    /// Take every endpoint down or bring every one up.
    pub fn set_all(&self, up: bool) {
        for e in self.lock().values_mut() {
            e.up = up;
        }
    }

    /// What `endpoint` received.
    pub fn received(&self, endpoint: &str) -> Received {
        self.lock()
            .get(endpoint)
            .map(|e| e.received.clone())
            .unwrap_or_default()
    }

    /// Every notification identity any endpoint accepted.
    pub fn delivered_ids(&self) -> BTreeSet<String> {
        self.lock()
            .values()
            .flat_map(|e| e.received.deliveries.iter().cloned())
            .collect()
    }
}

impl Transport for Recipients {
    fn post<'a>(
        &'a self,
        endpoint: &'a str,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> BoxFuture<'a, Result<u16, String>> {
        let now = self.clock.now();
        let status = {
            let mut all = self.lock();
            match all.get_mut(endpoint) {
                None => Err("connect".to_owned()),
                Some(e) if !e.up => Ok(503),
                Some(e) => match e.verifier.verify(&headers, &body, now) {
                    Ok(v) => {
                        let n: Option<NotificationBody> = serde_json::from_slice(&body).ok();
                        if let Some(n) = n {
                            e.received.deliveries.push(n.id.clone());
                            match v {
                                Verdict::Fresh => e.received.fresh.push(n),
                                Verdict::Duplicate => e.received.duplicates += 1,
                            }
                        }
                        Ok(200)
                    }
                    Err(_) => {
                        e.received.rejected += 1;
                        Ok(401)
                    }
                },
            }
        };
        Box::pin(async move { status })
    }
}

/// A principal of a test deployment.
#[derive(Clone, Debug)]
pub struct Party {
    /// Its identity.
    pub identity: String,
    /// Its credential.
    pub credential: String,
    /// Its bearer token.
    pub token: String,
    /// Its customer account, if it has one.
    pub account: Option<String>,
    /// Where its notifications go.
    pub endpoint: String,
}

/// The test asset, precision 2.
pub const USD: &str = "USD";

/// Where the test asset `USD` lives on the settlement layer (`DEC-20`): `Here(1)`, the asset the
/// zombienet suite creates with symbol `USD` and two decimals.
pub const USD_LOCATION: &str = "Here(1)";

/// Where the test asset `EUR` lives on the settlement layer: `Here(2)`.
pub const EUR_LOCATION: &str = "Here(2)";

/// The public key of every test deployment's recovery authority (`F-012`): the test key the
/// conformance suite gives `services/recovery` (`conformance::harness::service_recovery_authority`).
/// Public material; its secret is a test key of the suite's, never a default of this crate.
pub const TEST_RECOVERY_KEY: [u8; 32] = [
    0xe2, 0xf2, 0xce, 0x23, 0x24, 0x5b, 0x7d, 0xef, 0xe4, 0x27, 0x78, 0x49, 0x20, 0x33, 0x6f, 0x56,
    0x73, 0xef, 0x32, 0x62, 0x11, 0x10, 0xc7, 0xc4, 0x92, 0x88, 0xe8, 0xf1, 0x8c, 0x6c, 0x97, 0x59,
];

/// The test deployments' recovery delay: ten minutes (`F-012`).
pub const RECOVERY_DELAY_MS: Time = 600_000;

/// The test deployments' succession delay: one hour (`F-012`).
pub const SUCCESSION_DELAY_MS: Time = 3_600_000;

/// The test deployments' anchoring interval: one minute of the test clock (`F-017`).
pub const ANCHOR_INTERVAL_MS: Time = 60_000;

/// The seed of every test deployment's clerk key (`F-017`): public, worth nothing.
pub const TEST_CLERK_SEED: [u8; 32] = [0xC1; 32];

/// The clerk key every test deployment's core signs disclosures with (`F-017`).
pub fn test_clerk() -> crate::inprocess::SharedClerk {
    std::sync::Arc::new(ledger_core::anchor::clerk::InProcessClerk::from_seed(
        TEST_CLERK_SEED,
    ))
}

/// The assurance declaration every test deployment's disclosures carry (`F-017`).
pub fn test_assurance() -> ledger_core::anchor::AssuranceDeclaration {
    ledger_core::anchor::AssuranceDeclaration {
        digest: [0xA5; 32],
        summary: "Test deployment: no declaration generated.".into(),
    }
}

/// The genesis of every test deployment: `USD` and `EUR` at precision 2, a default deadline of
/// ten minutes for reservations, transfers and withdrawals, and a decision bound of one minute.
pub fn genesis(at: Time) -> CoreGenesis {
    let asset = |id: &str, location: &str| AssetDecl {
        id: AssetId::parse(id).unwrap_or_else(|_| unreachable!("a valid asset id")),
        // Bound from genesis: a test deployment's position starts empty with the book (F-029 NOTES).
        location: Some(
            ledger_core::asset::AssetLocation::parse(location)
                .unwrap_or_else(|_| unreachable!("a valid location")),
        ),
        precision: 2,
        kind: AssetKind::Fiat,
        deprecated: false,
    };
    let lifecycle = LifecycleConfig::new([0xD9; 16], 60_000, 3_600_000)
        .with_default_deadline(OperationKind::Reserve, 600_000)
        .with_default_deadline(OperationKind::Transfer, 600_000)
        .with_default_deadline(OperationKind::Redeem, 600_000)
        .with_recovery(
            RecoveryConfig::owned_custody(RecoveryKey(TEST_RECOVERY_KEY))
                .with_recovery_delay(RECOVERY_DELAY_MS)
                .with_succession_delay(Some(SUCCESSION_DELAY_MS))
                .with_holder_held(true),
        )
        .with_anchoring(ledger_core::anchor::AnchoringConfig {
            interval: ANCHOR_INTERVAL_MS,
            assurance: Some(test_assurance()),
        });
    CoreGenesis {
        genesis: Genesis {
            assets: vec![asset(USD, USD_LOCATION), asset("EUR", EUR_LOCATION)],
            operator_verifier: Verifier::new(vec![0x0F]),
            lifecycle,
            chainspec: None,
            spec: None,
        },
        op: OperationId::from_ref(OperationRef([0xE9; 16])),
        at,
        root: [0xC9; 32],
    }
}

/// Intervals short enough for tests.
pub fn fast_config(replica: &str) -> Config {
    Config {
        replica: replica.to_owned(),
        poll: Duration::from_millis(5),
        tick_interval: Duration::from_millis(50),
        // Every interval, as before `F-031`: tests count on a tick being logged.
        tick_heartbeat: Duration::ZERO,
        lease: Duration::from_millis(50),
        delivery_interval: Duration::from_millis(20),
        retry_base: Duration::from_millis(5),
        retry_max: Duration::from_millis(40),
        delivery_backoff_base: 20,
        delivery_backoff_max: 200,
        delivery_max_attempts: 24,
        decision_bound: 5_000,
        decision_wait: Duration::from_secs(30),
        core_batch: crate::service::DEFAULT_CORE_BATCH,
        genesis_gate: false,
        archived: Arc::default(),
        feed_signer: None,
        // The tests exercise `F-038`'s sequenced feed; the legacy link's own tests set
        // `ForwardLink::Legacy` (`F-038` §12).
        link: crate::service::ForwardLink::Sequenced,
    }
}

/// The time every test deployment starts at.
pub const T0: Time = 1_800_000_000_000;

/// How long a test waits on a condition the deployment reaches by itself before it fails:
/// generous, so a slow or contended runner is slow rather than wrong (issues #48, #85). Every
/// wait on it ends as soon as its condition holds.
pub const PATIENCE: Duration = Duration::from_secs(120);

/// The ticker's interval under [`Deployment::drive_ticks`]: longer than any test runs.
const NO_TICKS: Duration = Duration::from_secs(365 * 86_400);

/// An in-memory deployment. See the module documentation. The core is the in-process one unless
/// another is given ([`Deployment::assemble`]: `F-005`'s Agent core).
pub struct Deployment<S: Store = MemStore, C: Core + Clone = TestCore> {
    /// The ingress store.
    pub store: S,
    /// The core.
    pub core: C,
    /// The core's journal.
    pub journal: MemJournal,
    /// The ingress clock.
    pub clock: ManualClock,
    /// The authenticator.
    pub auth: Authenticator,
    /// The recipients.
    pub recipients: Arc<Recipients>,
    /// The first replica.
    pub replica: Replica<S, C>,
    /// Its surface.
    pub router: Router,
    /// The operator.
    pub operator: Party,
    names: Mutex<BTreeMap<String, Party>>,
}

/// A reply: its status and its JSON body (a string for a body that is not JSON).
pub type Reply = (StatusCode, Value);

impl Deployment<MemStore> {
    /// Start a deployment over the in-memory store: the core, one replica, the operator's token
    /// and recipient, and an issuance grant over `USD` held by the operator.
    pub async fn start() -> Deployment<MemStore> {
        Deployment::start_with(fast_config("ingress-0")).await
    }

    /// The same, with `config`.
    pub async fn start_with(config: Config) -> Deployment<MemStore> {
        let store = MemStore::new(config.lease);
        Deployment::start_on(store, config).await
    }

    /// Start another replica over the same store, core, authenticator and recipients, and its
    /// surface.
    pub fn another_replica(&self, config: Config) -> (Replica<MemStore, TestCore>, Router) {
        self.replica_on(self.store.clone(), config)
    }
}

impl<S: Store> Deployment<S> {
    /// Start a deployment over `store`.
    pub async fn start_on(store: S, config: Config) -> Deployment<S> {
        let journal = MemJournal::new();
        let core = InProcessCore::in_memory(genesis(T0), journal.clone()).with_clerk(test_clerk());
        core.start()
            .unwrap_or_else(|e| panic!("the core starts: {e}"));
        let founded = core
            .founded()
            .unwrap_or_else(|| unreachable!("a started core is founded"));
        Deployment::assemble(store, config, core, founded, journal).await
    }
}

impl<S: Store, C: Core + Clone> Deployment<S, C> {
    /// A deployment over `store` and a started `core` founded on [`genesis`] at [`T0`] with
    /// `founded` its roles: one replica, the operator's token and recipient, and an issuance
    /// grant over `USD` held by the operator. `journal` is the in-process core's, and empty for
    /// another core.
    pub async fn assemble(
        store: S,
        config: Config,
        core: C,
        founded: ledger_core::ledger::Founded,
        journal: MemJournal,
    ) -> Deployment<S, C> {
        let clock = ManualClock::at(T0);
        let shared_clock: SharedClock = Arc::new(clock.clone());
        let auth = Authenticator::new();
        auth.allow_holders(true);
        let recipients = Arc::new(Recipients::new(Arc::clone(&shared_clock)));
        let replica = Replica::start(
            store.clone(),
            core.clone(),
            Arc::clone(&shared_clock),
            auth.clone(),
            Arc::clone(&recipients) as Arc<dyn Transport>,
            config,
        );
        let router = router(Arc::clone(&replica.shared));
        let operator = Party {
            identity: founded.operator.to_did(),
            credential: founded.operator_credential.to_did(),
            token: "operator-token".into(),
            account: None,
            endpoint: "mem://operator".into(),
        };
        let d = Deployment {
            store,
            core,
            journal,
            clock,
            auth,
            recipients,
            replica,
            router,
            operator,
            names: Mutex::new(BTreeMap::new()),
        };
        d.register(&d.operator, CredentialKind::Service).await;
        let (s, v) = d
            .call(
                "POST",
                "/v1/grants",
                Some("operator-token"),
                &[],
                Some(json!({
                    "kind": "issuance",
                    "principal": d.operator.identity,
                    "subject": { "asset": USD },
                    "ops": ["issue", "redeem"],
                    "per_window": { "length": 86_400_000u64, "limit": "1000000000000" },
                    "assets": [USD],
                })),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "the operator's issuance grant: {v}");
        d
    }

    async fn register(&self, p: &Party, kind: CredentialKind) {
        self.auth
            .add_token(
                &Token::new(p.token.as_bytes()),
                &p.identity,
                &p.credential,
                kind,
            )
            .unwrap_or_else(|e| panic!("a token registers: {e}"));
        let secret = format!("secret-of-{}", p.token).into_bytes();
        self.recipients.add(&p.endpoint, &secret);
        self.store
            .put_recipient(Recipient {
                principal: p.identity.clone(),
                endpoint: p.endpoint.clone(),
                secret: Some(secret),
            })
            .await
            .unwrap_or_else(|e| panic!("a recipient registers: {e}"));
    }

    /// Send a request through the router.
    pub async fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> Reply {
        call(&self.router, method, path, token, headers, body).await
    }

    /// Send a request whose body is these bytes as they are, not a JSON value: for a body that is
    /// not JSON at all (amendment 0021, `ERR-MalformedCommand`).
    pub async fn call_raw(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> Reply {
        call_bytes(&self.router, method, path, token, headers, Some(body)).await
    }

    /// Send a command and wait for its decision (amendment 0007, `OQ-61` as ruled on 2026-09-29):
    /// a transfer or a reservation is acknowledged at `Received` (`202`), and a repeat under the
    /// same client key returns the decision once the core has made it (`REQ-OP-10`). The request
    /// is repeated until the reply is not an acknowledgement, for up to ten seconds; the last
    /// reply is returned either way. Any other command replies with its decision at once.
    pub async fn call_decided(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> Reply {
        decided(&self.router, method, path, token, headers, body).await
    }

    /// Start another replica over `store` (a store of its own for PostgreSQL, so it holds its own
    /// leader lock), the same core, authenticator and recipients, and its surface.
    pub fn replica_on(&self, store: S, config: Config) -> (Replica<S, C>, Router) {
        let clock: SharedClock = Arc::new(self.clock.clone());
        let r = Replica::start(
            store,
            self.core.clone(),
            clock,
            self.auth.clone(),
            Arc::clone(&self.recipients) as Arc<dyn Transport>,
            config,
        );
        let surface = router(Arc::clone(&r.shared));
        (r, surface)
    }

    /// Replace the replica with one whose ticker does not tick within a test, over the same
    /// store, core, authenticator and recipients: every tick is then the test's own
    /// ([`Deployment::pass`], [`Deployment::tick_to_now`]), so the test says when a checkpoint or
    /// a deadline is taken (issue #85). The new replica's surface replaces [`Deployment::router`]:
    /// whatever holds the old router must take the new one.
    ///
    /// The ticker ticks at the ingress clock's time, and a tick takes whatever is due at that
    /// time. With it running, a checkpoint is taken as soon as the first entry exists, or as soon
    /// as an entry follows a pass of the clock, a few milliseconds before or after the test's own
    /// tick: so how many checkpoints there are, and what each covers, depends on timing.
    pub fn drive_ticks(&mut self) {
        self.replica.kill();
        let mut config = fast_config("ingress-driven");
        config.tick_interval = NO_TICKS;
        let (replica, router) = self.replica_on(self.store.clone(), config);
        self.replica = replica;
        self.router = router;
    }

    /// Open a person with a funded customer account, a token and a recipient.
    pub async fn party(&self, name: &str, funds: u64) -> Party {
        let op = Some(self.operator.token.as_str());
        let (s, v) = self
            .call(
                "POST",
                "/v1/identities",
                op,
                &[("client-key", &format!("identity-{name}"))],
                Some(json!({ "kind": "person", "verifier": "0a" })),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "open_identity: {v}");
        let identity = v["result"]["identity"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let credential = v["result"]["credential"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let (s, v) = self
            .call(
                "POST",
                "/v1/accounts",
                op,
                &[("client-key", &format!("account-{name}"))],
                Some(json!({ "owner": identity, "kind": "customer", "custody": "delegated" })),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "open_account: {v}");
        let account = v["result"]["account"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let p = Party {
            identity,
            credential,
            token: format!("token-{name}"),
            account: Some(account.clone()),
            endpoint: format!("mem://{name}"),
        };
        self.register(&p, CredentialKind::Holder).await;
        if funds > 0 {
            self.fund(&account, funds).await;
        }
        self.names
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(name.to_owned(), p.clone());
        p
    }

    /// Issue `amount` minor units of `USD` into `account`.
    pub async fn fund(&self, account: &str, amount: u64) {
        let (s, v) = self
            .call(
                "POST",
                "/v1/issuances",
                Some(self.operator.token.as_str()),
                &[],
                Some(json!({ "asset": USD, "destination": account, "amount": amount.to_string() })),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "issue: {v}");
    }

    /// Wait until every logged row is decided, or `timeout` passes. Returns whether it did.
    pub async fn quiesce(&self, timeout: Duration) -> bool {
        let until = Instant::now() + timeout;
        loop {
            if self
                .store
                .oldest_undecided()
                .await
                .is_ok_and(|o| o.is_none())
            {
                return true;
            }
            if Instant::now() >= until {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Wait until the core has applied a tick at or after the ingress clock's current time and
    /// every row is decided, or `timeout` passes. Deadlines and checkpoints then stand at "now".
    /// The error says what was missing.
    ///
    /// A condition, not a single tick (issue #85): the replica's own ticker may hold a tick
    /// logged at an earlier time undecided, and then [`tick_once`](crate::service::tick_once)
    /// logs nothing (at most one tick waits), so one call can leave the core a tick behind the
    /// clock. This logs ticks until one at the current time is decided.
    pub async fn tick_to_now(&self, timeout: Duration) -> Result<(), String> {
        let now = self.clock.now();
        let until = Instant::now() + timeout;
        loop {
            crate::service::tick_once(&self.replica.shared).await;
            let decided = self
                .quiesce(until.saturating_duration_since(Instant::now()))
                .await;
            let ticked = self.core.last_tick().await.ok().flatten();
            if decided && ticked.is_some_and(|t| t >= now) {
                return Ok(());
            }
            if Instant::now() >= until {
                return Err(if decided {
                    format!("no tick at {now} within {timeout:?}: the last is at {ticked:?}")
                } else {
                    format!("rows undecided after {timeout:?}")
                });
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Move the ingress clock forward by `ms`, then [`Deployment::tick_to_now`].
    pub async fn pass(&self, ms: Time, timeout: Duration) -> Result<(), String> {
        self.clock.advance(ms);
        self.tick_to_now(timeout).await
    }

    /// Wait until the core's outbox holds no undelivered notification, or `timeout` passes. The
    /// ingress clock moves while it waits, so delivery backoffs run out.
    ///
    /// Every row is seen decided first, and the outbox read after: a row decided has committed
    /// its notifications, so none of them can be missed. The other order raced (issue #48): an
    /// empty outbox read just before the core decided a row, then every row decided, passed while
    /// that row's notifications were still owed.
    pub async fn all_delivered(&self, timeout: Duration) -> bool {
        let until = Instant::now() + timeout;
        while Instant::now() < until {
            self.clock.advance(50);
            if self.quiesce(Duration::ZERO).await && self.undelivered_now().await == Some(0) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        false
    }

    /// Undelivered notifications in the core's committed state (the first page), while it runs.
    pub async fn undelivered_now(&self) -> Option<usize> {
        self.core
            .outbox(None, true, 0)
            .await
            .ok()
            .map(|p| p.items.len())
    }

    /// Wait until the decided rows hold exactly the core's input numbers `1..=N`, each once, read
    /// at one consistent instant, and return `N`; or, past `timeout`, the last reading as an
    /// error.
    ///
    /// A live replica keeps appending rows of its own (ticks, delivery records), and the
    /// forwarder marks a row decided only after the core applied it. So "every row decided" and
    /// then two separate reads can see the core one input ahead of the rows (issue #48; the
    /// PostgreSQL case in run 36610739250 saw 20 inputs against 19 rows). This reads the core's
    /// count, then the rows, then the count again, and accepts only a reading in which the count
    /// did not move and the rows hold `1..=N`. A real gap, or a number held twice, never goes
    /// away, and fails.
    /// `inputs` reads the core's count of applied inputs.
    pub async fn gapless_inputs(
        &self,
        inputs: impl Fn() -> Option<u64>,
        timeout: Duration,
    ) -> Result<u64, (Vec<u64>, u64)> {
        let until = Instant::now() + timeout;
        loop {
            let before = inputs().unwrap_or(0);
            let numbers = self.input_numbers().await;
            let after = inputs().unwrap_or(0);
            if before == after && numbers == (1..=after).collect::<Vec<u64>>() {
                return Ok(after);
            }
            if Instant::now() >= until {
                return Err((numbers, after));
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Every decided row's core input number, in order.
    pub async fn input_numbers(&self) -> Vec<u64> {
        let mut v: Vec<u64> = self
            .store
            .rows()
            .await
            .unwrap_or_default()
            .iter()
            .filter(|r| r.status == Status::Decided)
            .filter_map(|r| r.core_input_seq)
            .collect();
        v.sort_unstable();
        v
    }
}

/// Send a command through `router` and wait for its decision: see
/// [`Deployment::call_decided`].
pub async fn decided(
    router: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> Reply {
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let (s, v) = call(router, method, path, token, headers, body.clone()).await;
        if s != StatusCode::ACCEPTED || Instant::now() >= until {
            return (s, v);
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Send a request through `router`.
pub async fn call(
    router: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> Reply {
    let bytes = body.map(|b| serde_json::to_vec(&b).unwrap_or_default());
    call_bytes(router, method, path, token, headers, bytes).await
}

/// Send a request with `body` as raw bytes, labelled JSON, through the router.
pub async fn call_bytes(
    router: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    headers: &[(&str, &str)],
    body: Option<Vec<u8>>,
) -> Reply {
    let mut req = Request::builder().method(method).uri(path);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            Body::from(b)
        }
        None => Body::empty(),
    };
    let req = req
        .body(body)
        .unwrap_or_else(|e| panic!("a well-formed request: {e}"));
    let resp = router
        .clone()
        .oneshot(req)
        .await
        .unwrap_or_else(|e| match e {});
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap_or_default();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    (status, value)
}
