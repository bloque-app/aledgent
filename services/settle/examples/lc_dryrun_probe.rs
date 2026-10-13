// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! A light-client dry-run probe (F-027 NOTES, decision 36): the same `ContractsApi_call` bytes of
//! the contract's `execute`, at one finalized block, through the adapter's own light client and
//! against the collator's RPC.
//!
//!   lc_dryrun_probe inputs  <rpc> <contract> <peer> <origin> <payee> <inputs.json>
//!   lc_dryrun_probe light   adapter|subxt <kreivo-spec> <relay-spec> <inputs.json> <dir>
//!   lc_dryrun_probe compare <rpc> <inputs.json> <dir> <summary.md>
//!
//! - `inputs` builds the cases with the adapter's own encoders (`kreivo::envelope`,
//!   `kreivo::execute_data`, `kreivo::contracts_api_call_args`), reading the anchor number the
//!   contract expects next over the RPC: a read of the head (the control: read messages are known
//!   to work over the light client), a withdrawal to an account, a withdrawal to another
//!   deployment (a call in a frame of its own: child-trie writes in two contracts, an asset
//!   moved), an anchor, and a batch of sixteen withdrawals to accounts.
//! - `light` is path (c), the adapter's exact path: `RpcChain::light`, then
//!   `RpcChain::runtime_call` (`state_call` through subxt 0.44.3's smoldot-light 0.17.2), which is
//!   what `Chain::dry_run` runs when no dry-run node is configured. It writes `ready-c` once it
//!   follows the chain, waits for `target.json` (the block every path calls at), and writes
//!   `results-c.json`.
//! - `compare` is path (a), the reference: `state_call` at the collator, at every block a path
//!   used, and a Markdown table of every path's result against it, byte for byte.
//!
//! Not part of the adapter; built only by the suite (`e2e/scripts/build.sh`, `BUILD_PROBE=1`).

use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use settle::chain::kreivo::{self, AssetId, Call, Destination, Envelope};
use settle::chain::rpc::{Names, RpcChain};
use settle::chain::{AccountId32, Chain};

/// One runtime call to make through every path.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Case {
    name: String,
    /// What it is, for the table.
    what: String,
    method: String,
    /// The SCALE-encoded arguments, hex with `0x`.
    args: String,
    /// `ContractsApi_call`'s parts, for a client that encodes them itself (PAPI).
    origin: String,
    dest: String,
    input: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Inputs {
    contract: String,
    peer: String,
    cases: Vec<Case>,
}

/// The block every path calls at.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Target {
    hash: String,
    number: u64,
}

/// One path's result for one case.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Outcome {
    case: String,
    /// The block it ran at, hex with `0x`.
    at: String,
    ok: bool,
    #[serde(default)]
    output: Option<String>,
    #[serde(default)]
    error: Option<String>,
    ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct PathResults {
    path: String,
    label: String,
    #[serde(default)]
    note: Option<String>,
    outcomes: Vec<Outcome>,
}

fn hex0(b: &[u8]) -> String {
    format!("0x{}", hex::encode(b))
}

fn unhex(s: &str) -> Result<Vec<u8>, String> {
    hex::decode(s.trim_start_matches("0x")).map_err(|e| format!("{s}: {e}"))
}

fn h32(s: &str) -> Result<[u8; 32], String> {
    unhex(s)?
        .try_into()
        .map_err(|_| format!("{s}: not 32 bytes"))
}

/// An account as SS58 (any prefix) or hex.
fn account(s: &str) -> Result<AccountId32, String> {
    if s.starts_with("0x") {
        return h32(s);
    }
    subxt::utils::AccountId32::from_str(s)
        .map(|a| a.0)
        .map_err(|e| format!("{s}: {e}"))
}

fn names(contract: AccountId32, peer: AccountId32) -> Names {
    Names {
        contracts: "Contracts".into(),
        assets: "Assets".into(),
        deployments: vec![(1, contract), (2, peer)],
    }
}

fn random32(salt: u64) -> [u8; 32] {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let mut seed = t.to_le_bytes().to_vec();
    seed.extend_from_slice(&salt.to_le_bytes());
    settle::chain::blake2_256(&seed)
}

fn write_json(path: &Path, v: &impl Serialize) -> Result<(), String> {
    let text = serde_json::to_string_pretty(v).map_err(|e| e.to_string())?;
    std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

async fn inputs(args: &[String]) -> Result<(), String> {
    let [rpc, contract, peer, origin, payee, out] = args else {
        return Err("inputs <rpc> <contract> <peer> <origin> <payee> <inputs.json>".into());
    };
    let (contract, peer, origin, payee) = (
        account(contract)?,
        account(peer)?,
        account(origin)?,
        account(payee)?,
    );
    let chain = RpcChain::connect(rpc, names(contract, peer), &[])
        .await
        .map_err(|e| e.to_string())?;
    let at = chain.finalized().await.map_err(|e| e.to_string())?;
    let view = chain
        .instance(&at, 1)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("the contract is not instantiated")?;
    if !view.keys.contains(&origin) {
        return Err(format!(
            "the origin {} is not one of the contract's keys",
            hex0(&origin)
        ));
    }
    let asset = AssetId::Here(1);
    let to_account = |i: u64| Envelope {
        operation_ref: random32(i),
        call: Call::Withdraw {
            to: Destination::Account(payee),
            asset,
            amount: 1_000,
        },
    };
    let mut cases = Vec::new();
    let mut case = |name: &str, what: &str, input: Vec<u8>| {
        cases.push(Case {
            name: name.into(),
            what: what.into(),
            method: "ContractsApi_call".into(),
            args: hex0(&kreivo::contracts_api_call_args(&origin, &contract, &input)),
            origin: hex0(&origin),
            dest: hex0(&contract),
            input: hex0(&input),
        });
    };
    case(
        "head",
        "control: the read message `head()`",
        kreivo::selector::HEAD.to_vec(),
    );
    case(
        "withdraw-account",
        "`execute`: one withdrawal to an account",
        kreivo::execute_data(&[to_account(1)]),
    );
    case(
        "withdraw-deployment",
        "`execute`: one withdrawal to another deployment (a framed call)",
        kreivo::execute_data(&[Envelope {
            operation_ref: random32(2),
            call: Call::Withdraw {
                to: Destination::Deployment {
                    contract: peer,
                    account_ref: random32(3),
                },
                asset,
                amount: 1_000,
            },
        }]),
    );
    case(
        "anchor",
        "`execute`: one anchor",
        kreivo::execute_data(&[Envelope {
            operation_ref: random32(4),
            call: Call::Anchor {
                number: view.last_anchor + 1,
                commitment: random32(5),
            },
        }]),
    );
    case(
        "batch-2",
        "`execute`: two withdrawals to accounts",
        kreivo::execute_data(&(300..302).map(to_account).collect::<Vec<_>>()),
    );
    case(
        "batch-8",
        "`execute`: eight withdrawals to accounts",
        kreivo::execute_data(&(200..208).map(to_account).collect::<Vec<_>>()),
    );
    case(
        "batch-16",
        "`execute`: sixteen withdrawals to accounts",
        kreivo::execute_data(&(100..116).map(to_account).collect::<Vec<_>>()),
    );
    write_json(
        Path::new(out),
        &Inputs {
            contract: hex0(&contract),
            peer: hex0(&peer),
            cases,
        },
    )?;
    println!(
        "[probe] inputs at #{}: next anchor {}, inbox {}",
        at.number,
        view.last_anchor + 1,
        view.next_seq
    );
    Ok(())
}

/// Wait for `path` to exist, for at most `timeout`.
async fn wait_file(path: &Path, timeout: Duration) -> Result<(), String> {
    let end = Instant::now() + timeout;
    while !path.exists() {
        if Instant::now() > end {
            return Err(format!("{} did not appear", path.display()));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Ok(())
}

/// The light client a `light` run goes through.
enum Light {
    /// The adapter's: `RpcChain::light` (smoldot-light 2.x, `chain/light.rs`), then
    /// `RpcChain::runtime_call`, which is what `Chain::dry_run` runs with no dry-run node.
    Adapter(Box<RpcChain>),
    /// subxt 0.44.3's own (smoldot-light 0.17.2, smoldot 0.19.4), which the adapter used until
    /// F-027 NOTES decision 42, with the same `state_call` it sent.
    #[cfg(feature = "probe-subxt-lightclient")]
    Subxt(subxt::backend::legacy::LegacyRpcMethods<subxt::PolkadotConfig>),
}

impl Light {
    async fn finalized(&self) -> Result<u64, String> {
        match self {
            Light::Adapter(chain) => chain
                .finalized()
                .await
                .map(|f| f.number)
                .map_err(|e| e.to_string()),
            #[cfg(feature = "probe-subxt-lightclient")]
            Light::Subxt(rpc) => {
                let hash = rpc
                    .chain_get_finalized_head()
                    .await
                    .map_err(|e| e.to_string())?;
                let header = rpc
                    .chain_get_header(Some(hash))
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or("no header")?;
                Ok(header.number.into())
            }
        }
    }

    async fn call(&self, method: &str, args: &[u8], at: [u8; 32]) -> Result<Vec<u8>, String> {
        match self {
            Light::Adapter(chain) => chain
                .runtime_call(method, args, at)
                .await
                .map_err(|e| e.to_string()),
            #[cfg(feature = "probe-subxt-lightclient")]
            Light::Subxt(rpc) => rpc
                .state_call(method, Some(args), Some(subxt::utils::H256(at)))
                .await
                .map_err(|e| e.to_string()),
        }
    }
}

async fn light(args: &[String]) -> Result<(), String> {
    let [which, kreivo_spec, relay_spec, inputs, dir] = args else {
        return Err("light adapter|subxt <kreivo-spec> <relay-spec> <inputs.json> <dir>".into());
    };
    let dir = PathBuf::from(dir);
    let inputs: Inputs = read_json(Path::new(inputs))?;
    let read = |p: &str| std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}"));
    let (kreivo_spec, relay_spec) = (read(kreivo_spec)?, read(relay_spec)?);
    let (client, path, label) = match which.as_str() {
        "adapter" => (
            Light::Adapter(Box::new(
                RpcChain::light(
                    &kreivo_spec,
                    Some(&relay_spec),
                    names(h32(&inputs.contract)?, h32(&inputs.peer)?),
                    &[],
                )
                .await
                .map_err(|e| e.to_string())?,
            )),
            "c2",
            "the adapter: `RpcChain` over smoldot-light 2.1.0 (smoldot 4.0.0) as the workspace builds them, patched to skip the end-of-call child-trie roots, `state_call`",
        ),
        #[cfg(feature = "probe-subxt-lightclient")]
        "subxt" => {
            use subxt::lightclient::LightClient;
            let (relay, _) =
                LightClient::relay_chain(relay_spec.as_str()).map_err(|e| e.to_string())?;
            let rpc = relay
                .parachain(kreivo_spec.as_str())
                .map_err(|e| e.to_string())?;
            (
                Light::Subxt(subxt::backend::legacy::LegacyRpcMethods::new(
                    subxt::backend::rpc::RpcClient::new(rpc),
                )),
                "c",
                "the adapter before the fix: subxt 0.44.3 + smoldot-light 0.17.2 (smoldot 0.19.4), `state_call`",
            )
        }
        other => return Err(format!("no light client {other} in this build")),
    };
    let started = Instant::now();
    loop {
        match client.finalized().await {
            Ok(n) if n > 0 => break,
            _ if started.elapsed() > Duration::from_secs(900) => {
                return Err("the light client did not follow the chain in 900 s".into());
            }
            _ => tokio::time::sleep(Duration::from_secs(2)).await,
        }
    }
    println!("[probe {path}] following after {:?}", started.elapsed());
    std::fs::write(dir.join(format!("ready-{path}")), b"").map_err(|e| e.to_string())?;
    let target_path = dir.join("target.json");
    wait_file(&target_path, Duration::from_secs(900)).await?;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let target: Target = read_json(&target_path)?;
    let hash = h32(&target.hash)?;
    // The light client knows the block once it has finalized it.
    let end = Instant::now() + Duration::from_secs(300);
    loop {
        match client.finalized().await {
            Ok(n) if n >= target.number => break,
            _ if Instant::now() > end => {
                return Err(format!("never finalized #{}", target.number));
            }
            _ => tokio::time::sleep(Duration::from_secs(1)).await,
        }
    }
    let mut outcomes = Vec::new();
    for case in &inputs.cases {
        let args = unhex(&case.args)?;
        let t = Instant::now();
        let r = client.call(&case.method, &args, hash).await;
        let ms = u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX);
        let failed = r.is_err();
        println!(
            "[probe {path}] {}: {} ({ms} ms)",
            case.name,
            r.as_ref().err().map(String::as_str).unwrap_or("ok")
        );
        outcomes.push(Outcome {
            case: case.name.clone(),
            at: target.hash.clone(),
            ok: !failed,
            output: r.as_ref().ok().map(|o| hex0(o)),
            error: r.err(),
            ms,
        });
        if failed {
            // smoldot bans for 40 s a peer whose call proof it could not use, and this network
            // has one: wait it out, so that the next case is not refused for want of a peer.
            tokio::time::sleep(Duration::from_secs(45)).await;
        }
    }
    write_json(
        &dir.join(format!("results-{path}.json")),
        &PathResults {
            path: path.into(),
            label: label.into(),
            note: None,
            outcomes,
        },
    )
}

/// What a `ContractsApi_call` result says, for the table.
fn describe(case: &str, bytes: &[u8]) -> String {
    if case == "head" {
        return match kreivo::contract_exec_data(bytes)
            .and_then(|d| kreivo::message_result::<kreivo::Head>(&d))
        {
            Ok(h) => format!(
                "head: next_seq {}, last_anchor {}",
                h.next_seq, h.last_anchor
            ),
            Err(e) => e,
        };
    }
    match kreivo::execute_dry_run(bytes) {
        Ok(d) => format!(
            "{}; gas {} ps / {} B; deposit {}",
            match &d.failure {
                None => "applies".to_owned(),
                Some(f) => format!("{f:?}"),
            },
            d.gas_required.ref_time,
            d.gas_required.proof_size,
            d.storage_deposit
        ),
        Err(e) => e,
    }
}

async fn compare(args: &[String]) -> Result<(), String> {
    let [rpc, inputs, dir, summary] = args else {
        return Err("compare <rpc> <inputs.json> <dir> <summary.md>".into());
    };
    let inputs: Inputs = read_json(Path::new(inputs))?;
    let chain = RpcChain::connect(rpc, names(h32(&inputs.contract)?, h32(&inputs.peer)?), &[])
        .await
        .map_err(|e| e.to_string())?;
    let runtime = chain.runtime().await.map_err(|e| e.to_string())?;
    let mut paths: Vec<PathResults> = Vec::new();
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("results-") && n.ends_with(".json"))
        })
        .collect();
    entries.sort();
    for p in entries {
        match read_json::<PathResults>(&p) {
            Ok(r) => paths.push(r),
            Err(e) => println!("[probe] {e}"),
        }
    }
    let mut reference: std::collections::BTreeMap<(String, String), Result<Vec<u8>, String>> =
        Default::default();
    let mut md = format!(
        "### Light-client dry runs of `execute` (spec {})\n\n",
        runtime.spec_version
    );
    md.push_str("| Path | Case | Result | Byte-identical to (a) | What (a) says at that block |\n|---|---|---|---|---|\n");
    let mut lines = Vec::new();
    for path in &paths {
        for o in &path.outcomes {
            let Some(case) = inputs.cases.iter().find(|c| c.name == o.case) else {
                continue;
            };
            let key = (o.case.clone(), o.at.clone());
            if !reference.contains_key(&key) {
                let r = chain
                    .runtime_call(&case.method, &unhex(&case.args)?, h32(&o.at)?)
                    .await
                    .map_err(|e| e.to_string());
                reference.insert(key.clone(), r);
            }
            let reference = &reference[&key];
            let same = match (reference, &o.output) {
                (Ok(r), Some(out)) => {
                    if unhex(out)? == *r {
                        "yes".to_owned()
                    } else {
                        "**no**".to_owned()
                    }
                }
                (Err(e), _) => format!("(a) failed: {e}"),
                (Ok(_), None) => "—".to_owned(),
            };
            let said = match reference {
                Ok(r) => describe(&o.case, r),
                Err(e) => e.clone(),
            };
            let result = if o.ok {
                format!("ok ({} ms)", o.ms)
            } else {
                format!(
                    "**error** ({} ms): {}",
                    o.ms,
                    o.error.as_deref().unwrap_or("?").replace('|', "/")
                )
            };
            lines.push(format!(
                "| {} {} | `{}` | {} | {} | {} at `{}…` |",
                path.path,
                path.label,
                o.case,
                result,
                same,
                said,
                &o.at[..12.min(o.at.len())]
            ));
        }
    }
    md.push_str(&lines.join("\n"));
    md.push('\n');
    for path in &paths {
        if let Some(n) = &path.note {
            md.push_str(&format!("\n- {}: {n}", path.path));
        }
    }
    md.push_str("\n\nCases:\n");
    for c in &inputs.cases {
        md.push_str(&format!("- `{}`: {}\n", c.name, c.what));
    }
    std::fs::write(summary, &md).map_err(|e| e.to_string())?;
    println!("{md}");
    Ok(())
}

/// smoldot's own log (it logs through `log`), on stderr: everything at info and above, and at debug
/// only what says why a runtime call failed (a missing proof entry, a network error, a ban).
struct SmoldotLog;

impl log::Log for SmoldotLog {
    fn enabled(&self, m: &log::Metadata) -> bool {
        m.level() <= log::Level::Debug
    }
    fn log(&self, r: &log::Record) {
        let text = r.args().to_string();
        let wanted = r.level() <= log::Level::Info
            || [
                "runtime-call",
                "call-proof",
                "ban",
                "Inaccessible",
                "MissingProof",
            ]
            .iter()
            .any(|w| text.contains(w));
        if wanted {
            eprintln!("[{} {}] {text}", r.level(), r.target());
        }
    }
    fn flush(&self) {}
}

#[tokio::main]
async fn main() {
    let _ = log::set_logger(&SmoldotLog).map(|()| log::set_max_level(log::LevelFilter::Debug));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .try_init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(String::as_str) {
        Some("inputs") => inputs(&args[1..]).await,
        Some("light") => light(&args[1..]).await,
        Some("compare") => compare(&args[1..]).await,
        _ => Err("usage: lc_dryrun_probe inputs|light|compare …".into()),
    };
    if let Err(e) = r {
        eprintln!("[probe] {e}");
        std::process::exit(1);
    }
}
