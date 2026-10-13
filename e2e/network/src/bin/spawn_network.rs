// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Spawn the network, wait until Kreivo finalizes blocks on both of its nodes, write the endpoints
//! to `ZOMBIENET_ENV_FILE` as `KEY=value` lines, and hold the network open until SIGINT or
//! SIGTERM.
//!
//! Bring-up is its own CI step, so a slow spawn reads as "Spawn network" and
//! not as a scenario that hangs, and the suite starts against a chain that already finalizes.
//!
//!   ZOMBIENET_ENV_FILE=/tmp/endpoints spawn_network
//!
//! Written to the file:
//!
//! - `KREIVO_WS`: the collator's RPC. The suite submits through it.
//! - `KREIVO_PRUNED_WS`: the pruned full node's RPC.
//! - `RELAY_WS`: the relay chain's RPC (alice).
//! - `ZOMBIENET_BASE_DIR`: zombienet's directory for this network. Each node's `--base-path` is
//!   under it, which is how the fault steps find a node's process.
//! - `KREIVO_CHAIN_SPEC`, `RELAY_CHAIN_SPEC` and their `LIGHT_*` copies with one local node per
//!   chain as boot node, for a light client; `KREIVO_PRUNED_P2P`, the pruned node's multiaddr.

use std::time::{Duration, Instant};

use aledgent_e2e_network::{network_config, COLLATOR, PRUNED_NODE};
use zombienet_sdk::subxt::{OnlineClient, PolkadotConfig};
use zombienet_sdk::NetworkConfigExt;

#[tokio::main]
async fn main() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "zombienet_orchestrator=info,zombienet_provider=info".into()),
        )
        .with_target(true)
        .without_time()
        .try_init();

    let started = Instant::now();
    println!("[network] spawning the relay chain and Kreivo (collator and pruned full node)");
    let heartbeat = tokio::spawn(async move {
        let mut waited = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            waited += 30;
            println!("[network] still spawning, {waited}s elapsed");
        }
    });
    let network = network_config()
        .spawn_native()
        .await
        .expect("network should spawn");
    heartbeat.abort();
    println!(
        "METER phase=network_spawned t={}s",
        started.elapsed().as_secs()
    );

    let uri = |name: &str| {
        network
            .get_node(name)
            .unwrap_or_else(|e| panic!("node {name}: {e}"))
            .ws_uri()
            .to_string()
    };
    let kreivo = uri(COLLATOR);
    let pruned = uri(PRUNED_NODE);
    let relay = uri("alice");

    // Finality, not only blocks: every step of the suite waits for it.
    wait_for_finalized(&kreivo, 2, Duration::from_secs(900)).await;
    println!(
        "METER phase=kreivo_finalizes t={}s",
        started.elapsed().as_secs()
    );
    wait_for_finalized(&pruned, 2, Duration::from_secs(300)).await;
    println!("METER phase=chain_ready t={}s", started.elapsed().as_secs());

    let base_dir = network.base_dir().unwrap_or_default().to_string();
    let mut contents = format!(
        "KREIVO_WS={kreivo}\nKREIVO_PRUNED_WS={pruned}\nRELAY_WS={relay}\nZOMBIENET_BASE_DIR={base_dir}\n"
    );
    // The raw chain-specs zombienet generated, for a light client (the aledgent driver's adapter):
    // as generated, and with this network's nodes as boot nodes (`LIGHT_*`). A light client
    // reaches nothing but its boot nodes and what they tell it, and every node listens over
    // WebSocket, which is the transport smoldot speaks.
    //
    // One boot node per chain: smoldot-light 0.17.2 panicked in its network service
    // (`network_service.rs:1277`, `event_pending_send.is_none()`) with two per chain on this
    // network, and connected with one (F-026 NOTES, "Against Kreivo main (0.17), and through
    // aledgent", finding 5).
    let multiaddr = |name: &str| {
        network
            .get_node(name)
            .map(|n| n.multiaddr().to_string())
            .unwrap_or_default()
    };
    // The pruned node's, for the aledgent driver's "prune node state": its adapter then reaches
    // Kreivo through that node alone.
    contents.push_str(&format!("KREIVO_PRUNED_P2P={}\n", multiaddr(PRUNED_NODE)));
    for (key, light, file, nodes) in [
        (
            "KREIVO_CHAIN_SPEC",
            "LIGHT_KREIVO_CHAIN_SPEC",
            "kreivo-local.json",
            vec![multiaddr(COLLATOR)],
        ),
        (
            "RELAY_CHAIN_SPEC",
            "LIGHT_RELAY_CHAIN_SPEC",
            "rococo-local.json",
            vec![multiaddr("alice")],
        ),
    ] {
        let path = std::path::Path::new(&base_dir).join(file);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        contents.push_str(&format!("{key}={}\n", path.display()));
        if let Ok(mut spec) = serde_json::from_str::<serde_json::Value>(&text) {
            let nodes: Vec<_> = nodes.into_iter().filter(|a| !a.is_empty()).collect();
            println!("[network] {light}: boot nodes {nodes:?}");
            spec["bootNodes"] = serde_json::json!(nodes);
            let out = path.with_file_name(format!("light-{file}"));
            std::fs::write(&out, spec.to_string())
                .unwrap_or_else(|e| panic!("writing {}: {e}", out.display()));
            contents.push_str(&format!("{light}={}\n", out.display()));
        }
    }
    match std::env::var("ZOMBIENET_ENV_FILE") {
        Ok(path) => {
            std::fs::write(&path, &contents)
                .unwrap_or_else(|e| panic!("writing the endpoints to {path}: {e}"));
            println!("[network] endpoints written to {path}");
        }
        Err(_) => println!("[network] ready:\n{contents}"),
    }

    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("a SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
    println!("[network] tearing down");
    if let Err(e) = network.destroy().await {
        eprintln!("[network] teardown: {e}");
    }
}

/// Connect to `ws_uri` and wait until its finalized block is at least `n`. Says what it last saw
/// if it times out: an unreachable node and a node that does not finalize are different failures.
async fn wait_for_finalized(ws_uri: &str, n: u32, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let mut attempts = 0u32;
    loop {
        let last = match OnlineClient::<PolkadotConfig>::from_url(ws_uri).await {
            // `at_latest` is the latest finalized block.
            Ok(client) => match client.blocks().at_latest().await {
                Ok(block) if block.number() >= n => {
                    println!("[network] {ws_uri} finalized block {}", block.number());
                    return;
                }
                Ok(block) => format!("connected, finalized block {}", block.number()),
                Err(e) => format!("connected, reading the finalized block failed: {e}"),
            },
            Err(e) => format!("could not connect: {e}"),
        };
        attempts += 1;
        if attempts % 10 == 0 {
            println!("[network] waiting for finalized block {n} at {ws_uri}: {last}");
        }
        if Instant::now() >= deadline {
            panic!("{ws_uri} did not finalize block {n} within {timeout:?}: {last}");
        }
        tokio::time::sleep(Duration::from_secs(3)).await;
    }
}
