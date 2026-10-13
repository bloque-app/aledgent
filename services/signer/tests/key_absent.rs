// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `F-035`, guardrail 13, `NFR-8`: the book runs with its keys in another process.
//!
//! The signer runs as its own process (the `aledgent-signer` binary). This test process is the
//! book's: a deployment over the core Agent (`AgentCore` over the native Agent, the core host's
//! shape), whose key custody and clerk are the signer's clients. Parties open, a reservation is
//! taken and captured, so the Agent creates accounts, signs transfers and blinds entries, all
//! through the signer. Then nothing in this process has loaded a root or a clerk key: the
//! process-wide count of roots loaded (`ledger_kernel::keys::local_roots_loaded`) is zero, and
//! the environment names no key.
//!
//! One test in this file, and nothing else in its process: the count is the whole process's.

mod common;

use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use core_binding::core::{AgentCore, Founding, MemLog};
use core_binding::host::{HostKeys, NativePort, SharedClerk};
use core_binding::hot::{Limits, MAX_BUDGET};
use ingress::inprocess::MemJournal;
use ingress::store::mem::MemStore;
use ingress::testkit::{Deployment, T0, USD, fast_config, genesis};
use serde_json::json;
use signer::client::{Patience, RemoteClerk, RemoteCustody, SignerClient, Token};

struct Signer(Child);

impl Drop for Signer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_signer(dir: &std::path::Path) -> (Signer, std::path::PathBuf) {
    let uid = my_uid(dir);
    let (core_token, core_hash) = token(dir, "core", 1);
    let (_, settle_hash) = token(dir, "settle", 2);
    let config = dir.join("signer.json");
    std::fs::write(
        &config,
        serde_json::to_vec(&dev_config(dir, uid, &core_hash, &settle_hash)).unwrap(),
    )
    .unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_aledgent-signer"))
        .arg("serve")
        .arg(&config)
        .env_remove("ALEDGENT_CORE_ROOT")
        .env_remove("ALEDGENT_CLERK_KEY")
        .spawn()
        .expect("the signer starts");
    let socket = dir.join("signer.sock");
    let started = Instant::now();
    while !socket.exists() {
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "the signer listens"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    (Signer(child), core_token)
}

#[tokio::test(flavor = "multi_thread")]
async fn nfr_8_the_book_runs_with_no_key_in_its_process() {
    for var in [
        "ALEDGENT_CORE_ROOT",
        "ALEDGENT_CLERK_KEY",
        "ALEDGENT_CORE_AGENT_KEY",
        "ALEDGENT_CORE_PACKAGE_KEY",
    ] {
        assert!(
            std::env::var_os(var).is_none(),
            "{var} is in the environment"
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let (_signer, core_token) = spawn_signer(dir.path());
    let socket = dir.path().join("signer.sock");
    let keys = tokio::task::spawn_blocking(move || {
        let client = Arc::new(
            SignerClient::with_token(
                &socket,
                "core",
                Token::from_file(&core_token).unwrap(),
                Duration::from_secs(5),
            )
            .unwrap(),
        );
        HostKeys::new(
            Arc::new(RemoteCustody::new(Arc::clone(&client), "keyring", Patience::TEST).unwrap()),
            Some(
                Arc::new(RemoteClerk::new(Arc::clone(&client), "clerk", Patience::TEST).unwrap())
                    as SharedClerk,
            ),
        )
    })
    .await
    .unwrap();
    let g = genesis(T0);
    let founding = Founding {
        genesis: g.genesis,
        op: g.op,
        at: g.at,
        keys,
        limits: Limits {
            budget: MAX_BUDGET,
            ..Limits::DEFAULT
        },
        genesis_roots: None,
    };
    let core = AgentCore::open(NativePort::new(), founding, MemLog::default())
        .unwrap_or_else(|e| panic!("the Agent core opens: {e}"));
    let d: Deployment<MemStore, AgentCore<NativePort>> = Deployment::assemble(
        MemStore::new(Duration::from_millis(50)),
        fast_config("ingress-0"),
        core.clone(),
        core.founded(),
        MemJournal::new(),
    )
    .await;
    let alice = d.party("alice", 1_000).await;
    let bob = d.party("bob", 0).await;
    let a = Some(alice.token.as_str());
    let (s, r) = d
        .call_decided(
            "POST",
            "/v1/reservations",
            a,
            &[("client-key", "r1")],
            Some(json!({ "payer": alice.account, "asset": USD, "amount": "7" })),
        )
        .await;
    assert_eq!(s.as_u16(), 200, "reserve: {r}");
    let reference = r["operation_reference"].as_str().unwrap().to_owned();
    let (s, c) = d
        .call(
            "POST",
            &format!("/v1/reservations/{reference}/capture"),
            a,
            &[],
            Some(json!({ "destination": bob.account, "cleared": "7" })),
        )
        .await;
    assert!(
        s.as_u16() == 200 && c["state"] == "Settled",
        "capture: {s} {c}"
    );
    assert_eq!(core.stopped(), None, "the replica and the Agent agree");
    // The whole process: no root was ever loaded here.
    assert_eq!(
        ledger_kernel::keys::local_roots_loaded(),
        0,
        "a key-ring root was loaded in the book's process"
    );
}
