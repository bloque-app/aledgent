// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The adapter against a **local** chain running the settlement pallet (`T-016-10`, the exit
//! criterion): ingress and the in-process core in this process, the adapter over `RpcChain` —
//! through the light client, as the binary connects by default, or over a node's RPC when asked.
//! Ignored by default: it needs a chain, which this repository does not build (`NOTES.md`,
//! "The end-to-end run"). `services/settle/README.md` says how to start one and run this.
//!
//! It refuses anything not on this machine: nothing here may touch a public network. Through the
//! light client, every boot node of every chain spec must be local; over RPC, the endpoint.
//!
//! Environment:
//!
//! - `ALEDGENT_SETTLE_CONNECTION`: `light-client` (the default) or `rpc`. Nothing falls back from
//!   one to the other.
//! - `ALEDGENT_SETTLE_CHAIN_SPEC`: with `light-client`, the settlement layer's raw chain spec.
//! - `ALEDGENT_SETTLE_RELAY_CHAIN_SPEC`: with `light-client`, its relay chain's raw chain spec,
//!   for a parachain (Kreivo on a local Kusama). Unset for a chain of its own.
//! - `ALEDGENT_SETTLE_NODE`: with `rpc`, the node's RPC endpoint, `ws://127.0.0.1:<port>`.
//! - `ALEDGENT_SETTLE_GENESIS`: the genesis hash the deployment declares, hex.
//! - `ALEDGENT_SETTLE_SPEC`: the allowed `spec_version`.
//! - `ALEDGENT_SETTLE_INSTANCE`: the registered instance whose keys include the lane.
//! - `ALEDGENT_SETTLE_KEY_FILE`: the lane's secp256k1 secret, hex, in a file (a development key).
//! - `ALEDGENT_SETTLE_ASSET`: the settlement asset, in the runtime value's textual form, with a
//!   withdrawal bound set and the position funded; its decimals are 2.
//! - `ALEDGENT_SETTLE_PALLET`, `ALEDGENT_SETTLE_ASSETS_PALLET`, `ALEDGENT_SETTLE_PALLET_ID`:
//!   default `AledgentSettlement`, `Assets`, `kv/aldst`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use ingress::testkit::USD;
use settle::adapter::{Adapter, Refusing};
use settle::chain::rpc::{Names, RpcChain};
use settle::chain::{Chain, ChainAsset};
use settle::config::{AssetBinding, Connection};
use settle::link::RouterTransport;
use settle::signer::{DevSigner, Signer};
use settle::store::mem::MemStore;
use settle::testkit::{ADAPTER_TOKEN, adapter_config, ingress_with_adapter};

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} is not set (see README.md)"))
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_owned())
}

fn is_local_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1")
}

/// Only an endpoint on this machine.
fn local(url: &str) -> bool {
    let rest = url
        .strip_prefix("ws://")
        .or_else(|| url.strip_prefix("wss://"))
        .unwrap_or("");
    let host = rest.split([':', '/']).next().unwrap_or("");
    is_local_host(host)
}

/// Only a chain spec whose boot nodes are all on this machine, and that has at least one: the
/// light client reaches nothing but what they name. A multiaddr's host is its second segment
/// (`/ip4/127.0.0.1/tcp/…`, `/ip6/::1/…`, `/dns/localhost/…`).
fn local_spec(spec: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(spec) else {
        return false;
    };
    let Some(nodes) = v["bootNodes"].as_array() else {
        return false;
    };
    !nodes.is_empty()
        && nodes.iter().all(|n| {
            let mut parts = n.as_str().unwrap_or("").split('/').skip(1);
            let (protocol, host) = (parts.next(), parts.next());
            matches!(protocol, Some("ip4" | "ip6" | "dns" | "dns4" | "dns6"))
                && host.is_some_and(is_local_host)
        })
}

fn read_local_spec(var: &str) -> String {
    let path = env(var);
    let spec = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    assert!(
        local_spec(&spec),
        "refusing {path}: a chain spec whose boot nodes are not all on this machine"
    );
    spec
}

/// The connection the environment names, exactly: never the other one.
fn connection() -> Connection {
    serde_json::from_value(env_or("ALEDGENT_SETTLE_CONNECTION", "light-client").into())
        .expect("ALEDGENT_SETTLE_CONNECTION is light-client or rpc")
}

async fn connect(names: Names, asset: &ChainAsset) -> RpcChain {
    let assets = std::slice::from_ref(asset);
    match connection() {
        Connection::LightClient => {
            let chain = read_local_spec("ALEDGENT_SETTLE_CHAIN_SPEC");
            let relay = std::env::var_os("ALEDGENT_SETTLE_RELAY_CHAIN_SPEC")
                .map(|_| read_local_spec("ALEDGENT_SETTLE_RELAY_CHAIN_SPEC"));
            RpcChain::light(&chain, relay.as_deref(), names, assets)
                .await
                .unwrap()
        }
        Connection::Rpc => {
            let url = env("ALEDGENT_SETTLE_NODE");
            assert!(local(&url), "refusing a non-local endpoint: {url}");
            RpcChain::connect(&url, names, assets).await.unwrap()
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a local Kreivo running the settlement contract: see services/settle/README.md"]
async fn exit_criterion_a_withdrawal_through_a_local_node_and_a_wrong_genesis_refused() {
    let instance: u32 = env("ALEDGENT_SETTLE_INSTANCE").parse().unwrap();
    let asset = ChainAsset(env("ALEDGENT_SETTLE_ASSET"));
    let hex32 = |var: &str| -> [u8; 32] {
        hex::decode(env(var).trim_start_matches("0x"))
            .ok()
            .and_then(|v| v.try_into().ok())
            .unwrap_or_else(|| panic!("{var}: 32 bytes of hex"))
    };
    let contract = hex32("ALEDGENT_SETTLE_CONTRACT");
    let names = || Names {
        contracts: env_or("ALEDGENT_SETTLE_CONTRACTS_PALLET", "Contracts"),
        assets: env_or("ALEDGENT_SETTLE_ASSETS_PALLET", "Assets"),
        deployments: vec![(instance, contract)],
    };
    let chain = connect(names(), &asset).await;
    let device: Arc<dyn Signer> = Arc::new(
        DevSigner::from_file(std::path::Path::new(&env("ALEDGENT_SETTLE_KEY_FILE"))).unwrap(),
    );
    let lane: Arc<dyn Signer> = Arc::new(settle::signer::PassAccount::new(
        device,
        hex32("ALEDGENT_SETTLE_PASS_USER"),
    ));
    let runtime = chain.runtime().await.unwrap();
    assert!(runtime.pallet, "pallet-contracts is in the runtime");

    let (d, _) = ingress_with_adapter(instance, 20, 6_000).await;
    let mut config = adapter_config([0; 32]);
    config.genesis = env("ALEDGENT_SETTLE_GENESIS");
    config.spec_versions = vec![env("ALEDGENT_SETTLE_SPEC").parse().unwrap()];
    config.instance = instance;
    config.assets = vec![AssetBinding {
        asset: USD.into(),
        chain: asset.clone(),
    }];
    let mut adapter = Adapter::new(
        config.clone(),
        chain,
        MemStore::new(),
        RouterTransport::new(d.router.clone(), ADAPTER_TOKEN),
        vec![Arc::clone(&lane)],
        Arc::new(d.clock.clone()),
    );
    let r = adapter.step().await.unwrap();
    assert!(r.serving, "{:?}", adapter.status());

    // A withdrawal, end to end: reserved, submitted, observed, final, settled.
    let alice = d.party("alice", 10_000).await;
    let (s, v) = d
        .call_decided(
            "POST",
            "/v1/withdrawals",
            Some(&alice.token),
            &[],
            Some(serde_json::json!({ "payer": alice.account, "asset": USD, "amount": "25" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    let op = v["operation_reference"].as_str().unwrap().to_owned();
    let (s, v) = d
        .call(
            "POST",
            &format!("/v1/withdrawals/{op}/submit"),
            Some(&alice.token),
            &[],
            Some(serde_json::json!({ "destination": { "account": hex::encode([0xAB; 32]) } })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "{v}");
    let until = Instant::now() + Duration::from_secs(600);
    let mut state = String::new();
    while Instant::now() < until {
        adapter.step().await.unwrap();
        let (_, v) = d
            .call(
                "GET",
                &format!("/v1/operations/{op}"),
                Some(&alice.token),
                &[],
                None,
            )
            .await;
        state = v["state"].as_str().unwrap_or_default().to_owned();
        if state == "Settled" {
            break;
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
    assert_eq!(state, "Settled");

    // AC-E4.2: the same node, declared as another network, is refused.
    let chain = connect(names(), &asset).await;
    let mut wrong = config;
    wrong.genesis = hex::encode([0x01; 32]);
    wrong.holder = "settle-wrong".into();
    let adapter = Adapter::new(
        wrong,
        chain,
        MemStore::new(),
        RouterTransport::new(d.router.clone(), ADAPTER_TOKEN),
        vec![lane],
        Arc::new(d.clock.clone()),
    );
    let refusing = adapter.assertions().await;
    assert!(
        matches!(refusing[..], [Refusing::Genesis { .. }]),
        "{refusing:?}"
    );
}

#[test]
fn only_a_local_endpoint_is_accepted() {
    assert!(local("ws://127.0.0.1:9944"));
    assert!(local("ws://localhost:9944/"));
    assert!(!local("wss://kreivo.kippu.rocks"));
    assert!(!local("wss://kreivo.io:443"));
}

#[test]
fn only_a_chain_spec_with_local_boot_nodes_is_accepted() {
    const PEER: &str = "12D3KooWEyoppNCUx8Yx66oV9fJnriXwCcXwDDUA2kj6vnc6iDEp";
    let spec =
        |nodes: &[String]| serde_json::json!({ "name": "Local", "bootNodes": nodes }).to_string();
    let node = |addr: &str| format!("{addr}/p2p/{PEER}");
    assert!(local_spec(&spec(&[
        node("/ip4/127.0.0.1/tcp/30333"),
        node("/ip6/::1/tcp/30334/ws"),
        node("/dns/localhost/tcp/30335"),
    ])));
    // One boot node elsewhere is enough to refuse it.
    assert!(!local_spec(&spec(&[
        node("/ip4/127.0.0.1/tcp/30333"),
        node("/dns/bootnode.example/tcp/30333"),
    ])));
    assert!(!local_spec(&spec(&[node("/ip4/10.0.0.7/tcp/30333")])));
    // No boot node, or no chain spec at all.
    assert!(!local_spec(&spec(&[])));
    assert!(!local_spec(r#"{ "name": "Local" }"#));
    assert!(!local_spec("not json"));
}
