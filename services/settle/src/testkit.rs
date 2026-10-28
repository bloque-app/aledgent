// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A whole deployment in process — ingress and the in-process core ([`ingress::testkit`]), the
//! adapter, its in-memory store, and the fake settlement layer — for this crate's tests and the
//! conformance suite's `F-016` cases (`crates/conformance/src/cases/f016.rs`).
//!
//! The adapter talks to ingress through the real router, as the principal the settlement
//! configuration names. Nothing here is used by the binary. The signing key is a test constant:
//! public, and worth nothing anywhere.

use std::sync::Arc;

use ingress::auth::{CredentialKind, Token};
use ingress::clock::Clock;
use ingress::testkit::{Deployment, PATIENCE, Party, USD};
use serde_json::{Value, json};

use crate::adapter::{Adapter, StepResult, chain_ref};
use crate::chain::fake::{FakeChain, FakeConfig};
use crate::chain::{AccountId32, ChainAsset};
use crate::config::{AdapterConfig, AssetBinding};
use crate::link::RouterTransport;
use crate::signer::{DevEd25519, DevSigner, PassAccount, Signer};
use crate::store::mem::MemStore;

/// The deployment's instance on the fake layer.
pub const INSTANCE: u32 = 1;
/// The fake layer's asset for `USD`: Kreivo's `FungibleAssetLocation::Here(1)`, the location the test
/// genesis binds `USD` to (`DEC-20`).
pub const CHAIN_USD: &str = ingress::testkit::USD_LOCATION;
/// The hashed user id of the test lane's community PASS account (`F-027`).
pub const TEST_PASS_USER: [u8; 32] = [0x51; 32];
/// The second test lane's PASS account.
pub const TEST_PASS_USER_2: [u8; 32] = [0x52; 32];
/// The test anchor key's PASS account.
pub const TEST_ANCHOR_PASS_USER: [u8; 32] = [0x53; 32];
/// The adapter's ingress token in the test deployment.
pub const ADAPTER_TOKEN: &str = "adapter-token";
/// The test lane's secret: a constant, public, worth nothing.
pub const TEST_LANE_SECRET: [u8; 32] = [0x5e; 32];
/// A second test lane's secret.
pub const TEST_LANE_SECRET_2: [u8; 32] = [0x6f; 32];
/// The test anchor key's secret (`F-017`): a constant, public, worth nothing.
pub const TEST_ANCHOR_SECRET: [u8; 32] = [0x7a; 32];
/// The destination account withdrawals pay out to.
pub const PAYEE: AccountId32 = [0xAB; 32];

/// The fake layer's asset for `USD`.
pub fn chain_usd() -> ChainAsset {
    ChainAsset(CHAIN_USD.into())
}

/// A test community PASS account (`F-027` T-027-03): the PASS account of `user`, whose device key
/// is the test constant `secret`.
pub fn pass_lane(secret: &[u8; 32], user: [u8; 32]) -> Arc<dyn Signer> {
    let device: Arc<dyn Signer> =
        Arc::new(DevSigner::from_secret(secret).unwrap_or_else(|e| panic!("the test key: {e}")));
    Arc::new(PassAccount::new(device, user))
}

/// A test native account (`F-045`): the Ed25519 key of the test constant `secret`, acting as its own
/// account.
pub fn native_lane(secret: &[u8; 32]) -> Arc<dyn Signer> {
    Arc::new(DevEd25519::from_secret(secret))
}

/// How a kit starts.
#[derive(Clone, Debug)]
pub struct KitOptions {
    /// The fake layer's configuration (its genesis, its runtime).
    pub fake: FakeConfig,
    /// The decimals the fake layer states for `USD`.
    pub decimals: u8,
    /// The instance's starting position in `USD`.
    pub position: u128,
    /// The lane's native balance.
    pub native: u128,
    /// Whether the instance is registered, with the lane's key.
    pub registered: bool,
    /// How many heights an instruction stays valid for (`expiry_window`).
    pub expiry_window: u64,
    /// The core's estimate of one height, in milliseconds.
    pub block_time: u64,
    /// How many lanes.
    pub lanes: usize,
    /// Whether the adapter anchors, with its own key, and reports positions (`F-017`); the
    /// core's clerk key is then published on the instance's record.
    pub anchoring: bool,
    /// A lower cap on the gas a batch is given than the runtime's ([`AdapterConfig::batch_gas_cap`]).
    pub batch_gas_cap: Option<crate::chain::kreivo::Weight>,
    /// A batch's gas margin over its dry run, in percent
    /// ([`AdapterConfig::batch_gas_margin_percent`]).
    pub batch_gas_margin_percent: u64,
    /// Whether the lanes and the anchor key are native accounts (`F-045`, Ed25519 keys signing v4
    /// transactions) rather than PASS accounts' devices.
    pub native_accounts: bool,
    /// The code generation the core decides at (amendment 0041 needs
    /// `ledger_core::settlement::SETTLEMENT_EPOCHS`); 0, as every kit before it.
    pub core_generation: u64,
    /// Whether the book is founded on a chainspec (an empty seed), so it has a genesis digest a
    /// settlement sequence can name ([`Kit::book`]); otherwise it has none.
    pub named_book: bool,
}

impl Default for KitOptions {
    fn default() -> Self {
        KitOptions {
            fake: FakeConfig::default(),
            decimals: 2,
            position: 1_000_000,
            native: 1_000_000,
            registered: true,
            expiry_window: 40,
            block_time: 1_000,
            lanes: 1,
            anchoring: false,
            batch_gas_cap: None,
            batch_gas_margin_percent: crate::config::default_batch_gas_margin_percent(),
            native_accounts: false,
            core_generation: 0,
            named_book: false,
        }
    }
}

/// The adapter every kit runs.
pub type KitAdapter = Adapter<FakeChain, MemStore, RouterTransport>;

/// A deployment with its adapter. See the module documentation.
pub struct Kit {
    /// Ingress and the core.
    pub d: Deployment,
    /// The fake settlement layer.
    pub chain: FakeChain,
    /// The adapter's store: it outlives the adapter across restarts.
    pub store: MemStore,
    /// The adapter's principal.
    pub adapter_party: Party,
    /// The adapter.
    pub adapter: KitAdapter,
    /// Its lanes.
    pub lanes: Vec<Arc<dyn Signer>>,
    /// Its anchor key, if it anchors (`F-017`).
    pub anchor_signer: Option<Arc<dyn Signer>>,
    config: AdapterConfig,
}

/// The adapter configuration of a kit.
pub fn adapter_config(genesis: [u8; 32]) -> AdapterConfig {
    AdapterConfig {
        holder: "settle-0".into(),
        genesis: hex::encode(genesis),
        spec_versions: Vec::new(),
        formats: Default::default(),
        instance: INSTANCE,
        assets: vec![AssetBinding {
            asset: USD.into(),
            chain: chain_usd(),
        }],
        keys: Vec::new(),
        accounts: Vec::new(),
        fee_floor: 100,
        lease_ttl: 30_000,
        status_lease: 120_000,
        page: 32,
        max_era: 64,
        challenge_window: crate::config::default_challenge_window(),
        rescan_every: None,
        rescan_window: None,
        anchor_key: None,
        anchor_account: None,
        batch_gas_cap: None,
        batch_gas_margin_percent: crate::config::default_batch_gas_margin_percent(),
        not_now_alert_after: crate::config::default_not_now_alert_after(),
    }
}

async fn ok(d: &Deployment, method: &str, path: &str, token: &str, body: Value) -> Value {
    let (s, v) = d.call(method, path, Some(token), &[], Some(body)).await;
    assert!(s.is_success(), "{method} {path}: {s} {v}");
    v
}

impl Kit {
    /// A kit with the default options.
    pub async fn start() -> Kit {
        Kit::start_with(KitOptions::default()).await
    }

    /// A kit with `options`: the deployment, the adapter's identity and token, the settlement
    /// configuration naming it, the fake layer with the instance registered and funded, and an
    /// adapter that has not yet run a step.
    pub async fn start_with(options: KitOptions) -> Kit {
        let d = if options.named_book {
            named_deployment().await
        } else {
            Deployment::start().await
        };
        let (d, adapter_party) =
            ingress_with_adapter_on(d, INSTANCE, options.expiry_window, options.block_time).await;
        if options.core_generation > 0 {
            d.core
                .checkpoint(|l, _| l.set_generation(options.core_generation, false))
                .unwrap_or_else(|| panic!("the core runs"));
        }
        let lanes: Vec<Arc<dyn Signer>> = [
            (TEST_LANE_SECRET, TEST_PASS_USER),
            (TEST_LANE_SECRET_2, TEST_PASS_USER_2),
        ]
        .iter()
        .take(options.lanes.max(1))
        .map(|(s, u)| {
            if options.native_accounts {
                native_lane(s)
            } else {
                pass_lane(s, *u)
            }
        })
        .collect();
        let chain = FakeChain::new(options.fake.clone());
        let anchor_signer: Option<Arc<dyn Signer>> = options.anchoring.then(|| {
            if options.native_accounts {
                native_lane(&TEST_ANCHOR_SECRET)
            } else {
                pass_lane(&TEST_ANCHOR_SECRET, TEST_ANCHOR_PASS_USER)
            }
        });
        if options.registered {
            let mut keys: Vec<AccountId32> = lanes.iter().map(|l| l.account()).collect();
            keys.extend(anchor_signer.iter().map(|s| s.account()));
            chain.register_instance(INSTANCE, &keys);
            if options.anchoring {
                chain.set_attestation_key(INSTANCE, ingress::testkit::test_clerk().public_key());
            }
        }
        if let Some(s) = &anchor_signer {
            chain.set_native(s.account(), options.native);
        }
        chain.set_decimals(&chain_usd(), USD, options.decimals);
        for l in &lanes {
            chain.set_native(l.account(), options.native);
        }
        chain.fund_position(INSTANCE, &chain_usd(), options.position);
        chain.advance(1);
        let mut config = adapter_config(FakeConfig::default().genesis);
        config.batch_gas_cap = options.batch_gas_cap;
        config.batch_gas_margin_percent = options.batch_gas_margin_percent;
        let store = MemStore::new();
        let mut adapter = Adapter::new(
            config.clone(),
            chain.clone(),
            store.clone(),
            RouterTransport::new(d.router.clone(), ADAPTER_TOKEN),
            lanes.clone(),
            Arc::new(d.clock.clone()),
        );
        if let Some(s) = &anchor_signer {
            adapter = adapter.with_anchor_signer(Arc::clone(s));
        }
        Kit {
            d,
            chain,
            store,
            adapter_party,
            adapter,
            lanes,
            anchor_signer,
            config,
        }
    }
}

/// An in-memory deployment of ingress and the core ([`Deployment`]) with the settlement adapter's
/// identity opened, its token registered ([`ADAPTER_TOKEN`]), and the settlement configuration
/// naming it for `instance`. The adapter's principal is returned.
pub async fn ingress_with_adapter(
    instance: u32,
    expiry_window: u64,
    block_time: u64,
) -> (Deployment, Party) {
    ingress_with_adapter_on(
        Deployment::start().await,
        instance,
        expiry_window,
        block_time,
    )
    .await
}

/// A deployment whose book is founded on a chainspec: the test genesis with an empty seed, so the
/// book has a genesis digest ([`Kit::book`], amendment 0041).
pub async fn named_deployment() -> Deployment {
    use ingress::testkit::{T0, fast_config, genesis, test_clerk};
    let mut g = genesis(T0);
    let spec = ledger_core::chainspec::ChainSpec::build(
        g.op,
        g.at,
        g.genesis.assets.clone(),
        Vec::new(),
        None,
        None,
        ledger_core::chainspec::ChunkLimits::DEFAULT,
    )
    .unwrap_or_else(|e| panic!("an empty seed builds: {e:?}"));
    g.genesis.chainspec = Some(spec.commitment());
    g.genesis.spec = Some(Arc::new(spec));
    let journal = ingress::inprocess::MemJournal::new();
    let core =
        ingress::inprocess::InProcessCore::in_memory(g, journal.clone()).with_clerk(test_clerk());
    core.start()
        .unwrap_or_else(|e| panic!("the core starts: {e}"));
    let founded = core
        .founded()
        .unwrap_or_else(|| panic!("a started core is founded"));
    let config = fast_config("ingress-0");
    let store = ingress::store::mem::MemStore::new(config.lease);
    Deployment::assemble(store, config, core, founded, journal).await
}

/// [`ingress_with_adapter`] on the deployment `d`.
pub async fn ingress_with_adapter_on(
    d: Deployment,
    instance: u32,
    expiry_window: u64,
    block_time: u64,
) -> (Deployment, Party) {
    {
        let op = d.operator.token.clone();
        let v = ok(
            &d,
            "POST",
            "/v1/identities",
            &op,
            json!({ "kind": "organisation", "verifier": "0b" }),
        )
        .await;
        let adapter_party = Party {
            identity: v["result"]["identity"].as_str().unwrap_or_default().into(),
            credential: v["result"]["credential"]
                .as_str()
                .unwrap_or_default()
                .into(),
            token: ADAPTER_TOKEN.into(),
            account: None,
            endpoint: "mem://adapter".into(),
        };
        d.auth
            .add_token(
                &Token::new(ADAPTER_TOKEN.as_bytes()),
                &adapter_party.identity,
                &adapter_party.credential,
                CredentialKind::Service,
            )
            .unwrap_or_else(|e| panic!("the adapter's token registers: {e}"));
        ok(
            &d,
            "POST",
            "/v1/ops/settlement",
            &op,
            json!({
                "adapter": adapter_party.identity,
                "instance": instance,
                "inbound_deadline": 600_000u64,
                "expiry_window": expiry_window,
                "block_time": block_time,
                "finality_margin": 60_000u64,
            }),
        )
        .await;
        (d, adapter_party)
    }
}

impl Kit {
    /// The book's genesis digest (its chainspec's), what a reset for it names (amendment 0041);
    /// zero for a book founded without one.
    pub fn book(&self) -> [u8; 32] {
        self.d
            .core
            .with_ledger(|l| l.code_binding().genesis)
            .unwrap_or_default()
    }

    /// The finalised head the core holds.
    pub fn core_head(&self) -> Option<ledger_core::settlement::FinalisedHead> {
        self.core_settlement(|s| s.head()).flatten()
    }

    /// Replace the adapter with a fresh process over the same store, chain and ingress: a
    /// restart after a crash.
    pub fn restart(&mut self) {
        let mut adapter = Adapter::new(
            self.config.clone(),
            self.chain.clone(),
            self.store.clone(),
            RouterTransport::new(self.d.router.clone(), ADAPTER_TOKEN),
            self.lanes.clone(),
            Arc::new(self.d.clock.clone()),
        );
        if let Some(s) = &self.anchor_signer {
            adapter = adapter.with_anchor_signer(Arc::clone(s));
        }
        self.adapter = adapter;
    }

    /// Replace the adapter with one configured by `f` (a second instance, another config).
    pub fn reconfigure(&mut self, f: impl FnOnce(&mut AdapterConfig)) {
        f(&mut self.config);
        self.restart();
    }

    /// One adapter step.
    pub async fn step(&mut self) -> StepResult<crate::StepReport> {
        self.adapter.step().await
    }

    /// Produce a block, finalize it, and run a step; `n` times. Stops at the first error.
    pub async fn rounds(&mut self, n: usize) -> StepResult<()> {
        for _ in 0..n {
            self.chain.advance(1);
            self.adapter.step().await?;
        }
        Ok(())
    }

    /// The ingress clock's time.
    pub fn now(&self) -> u64 {
        self.d.clock.now()
    }

    /// `withdraw` of `amount` minor units of `USD` by `party`, under `client_key`: the
    /// reservation's reply.
    pub async fn withdraw(&self, party: &Party, amount: u64, client_key: &str) -> (u16, Value) {
        let (s, v) = self
            .d
            .call_decided(
                "POST",
                "/v1/withdrawals",
                Some(&party.token),
                &[("client-key", client_key)],
                Some(json!({ "payer": party.account, "asset": USD, "amount": amount.to_string() })),
            )
            .await;
        (s.as_u16(), v)
    }

    /// `submit_outbound` of `reference` to [`PAYEE`] by `party`.
    pub async fn submit(&self, party: &Party, reference: &str) -> (u16, Value) {
        let (s, v) = self
            .d
            .call(
                "POST",
                &format!("/v1/withdrawals/{reference}/submit"),
                Some(&party.token),
                &[],
                Some(json!({ "destination": { "account": hex::encode(PAYEE) } })),
            )
            .await;
        (s.as_u16(), v)
    }

    /// `withdraw` then `submit_outbound`: the operation reference. Panics if either is refused.
    pub async fn withdrawal(&self, party: &Party, amount: u64, client_key: &str) -> String {
        let (s, v) = self.withdraw(party, amount, client_key).await;
        assert_eq!(s, 200, "withdraw: {v}");
        let reference = v["operation_reference"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let (s, v) = self.submit(party, &reference).await;
        assert_eq!(s, 200, "submit_outbound: {v}");
        reference
    }

    /// The state `party` sees for `reference`.
    pub async fn state(&self, party: &Party, reference: &str) -> String {
        let (_, v) = self
            .d
            .call(
                "GET",
                &format!("/v1/operations/{reference}"),
                Some(&party.token),
                &[],
                None,
            )
            .await;
        v["state"].as_str().unwrap_or_default().to_owned()
    }

    /// `party`'s balances of `USD`: (available, reserved).
    pub async fn balances(&self, party: &Party) -> (u64, u64) {
        let account = party.account.clone().unwrap_or_default();
        let (_, v) = self
            .d
            .call(
                "GET",
                &format!("/v1/balances?account={account}&asset={USD}"),
                Some(&party.token),
                &[],
                None,
            )
            .await;
        let find = |kind: &str| -> u64 { find_amount(&v, kind).unwrap_or(0) };
        (find("available"), find("reserved"))
    }

    /// Move the ingress clock forward by `ms` and wait until the core has applied a tick at the
    /// new time, every row decided (deadlines, `DEC-6`): [`Deployment::pass`], within
    /// [`PATIENCE`]. One tick logged is not enough: the replica's ticker may hold an older one
    /// undecided, and then the deadline would be taken after the caller looked (#85). Panics if no
    /// tick at the new time is decided within [`PATIENCE`].
    pub async fn pass(&self, ms: u64) {
        if let Err(why) = self.d.pass(ms, PATIENCE).await {
            panic!("the core did not tick at the new time: {why}");
        }
    }

    /// Stop the replica's own ticker for the rest of the test: every tick is then the test's own
    /// ([`Kit::pass`]), so a deadline is taken exactly when the test passes time
    /// ([`Deployment::drive_ticks`]). The adapter restarts over the new replica's router.
    pub fn drive_ticks(&mut self) {
        self.d.drive_ticks();
        self.restart();
    }

    /// An adapter over another store, with the kit's chain, ingress, lanes and configuration
    /// (the PostgreSQL store's test).
    pub fn adapter_on<S: crate::store::Store>(
        &self,
        store: S,
        holder: &str,
    ) -> Adapter<FakeChain, S, RouterTransport> {
        let mut config = self.config.clone();
        config.holder = holder.to_owned();
        Adapter::new(
            config,
            self.chain.clone(),
            store,
            RouterTransport::new(self.d.router.clone(), ADAPTER_TOKEN),
            self.lanes.clone(),
            Arc::new(self.d.clock.clone()),
        )
    }

    /// The core's settlement store, read.
    pub fn core_settlement<T>(
        &self,
        f: impl FnOnce(&ledger_core::settlement::store::SettlementStore) -> T,
    ) -> Option<T> {
        self.d.core.with_ledger(|l| f(l.state().settlement()))
    }

    /// The chain's operation reference of `reference`.
    pub fn chain_ref(reference: &str) -> [u8; 32] {
        ledger_core::id::OperationId::parse(reference)
            .map(|op| chain_ref(&op))
            .unwrap_or_default()
    }
}

/// The amount of balance kind `kind` in a balances reply, wherever the reply nests it.
fn find_amount(v: &Value, kind: &str) -> Option<u64> {
    match v {
        Value::Object(m) => {
            if let Some(x) = m.get(kind) {
                if let Some(s) = x.as_str() {
                    return s.parse().ok();
                }
                if let Some(n) = x.as_u64() {
                    return Some(n);
                }
            }
            m.values().find_map(|x| find_amount(x, kind))
        }
        Value::Array(a) => a.iter().find_map(|x| find_amount(x, kind)),
        _ => None,
    }
}
