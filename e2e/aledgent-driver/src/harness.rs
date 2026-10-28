// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The adapter's process, as the driver runs it: the `aledgent-settle` binary in a container, in
//! a network namespace of its own that a long-lived container holds (with `iptables`, and
//! `NET_ADMIN` in that namespace only), on a Docker network of the run's own with the adapter's
//! PostgreSQL. See the crate documentation.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;
use settle::chain::ChainAsset;
use settle::config::{
    AssetBinding, Connection, DeploymentContract, KeySource, PassKey, ProcessConfig,
};
use settle::testkit::{
    ADAPTER_TOKEN, TEST_ANCHOR_PASS_USER, TEST_ANCHOR_SECRET, TEST_LANE_SECRET, TEST_PASS_USER,
    adapter_config,
};

use crate::{env, say, tail};

/// How the adapter's light client reaches Kreivo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Variant {
    /// Through the collator, as the chain specs say.
    Normal,
    /// Through the pruned full node only: its chain spec names it, and the collator's parachain
    /// port is blocked in the adapter's namespace.
    PrunedOnly,
    /// As [`Variant::PrunedOnly`], with the collator's RPC (an archive) as the adapter's history
    /// endpoint (`T-043-04`): what it cannot read from the pruned node it reads from there.
    PrunedWithHistory,
}

/// A transaction the adapter stored.
pub struct Stored {
    /// Its nonce.
    pub nonce: u64,
    /// Its hash, hex.
    pub hash: String,
}

/// The adapter's process and what it runs beside. See the module documentation.
pub struct Harness {
    tag: String,
    image: String,
    dir: PathBuf,
    bin_dir: PathBuf,
    net: String,
    gateway: String,
    pg: String,
    pg_ip: String,
    ns: String,
    ingress_port: u16,
    collator_port: Option<String>,
    runs: u32,
    current: Option<(String, Variant)>,
    finished: String,
}

async fn docker(args: &[&str]) -> Result<String, String> {
    let out = tokio::process::Command::new("docker")
        .args(args)
        .output()
        .await
        .map_err(|e| format!("docker: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
    } else {
        Err(format!(
            "docker {}: {}",
            args.join(" "),
            tail(&String::from_utf8_lossy(&out.stderr), 10)
        ))
    }
}

/// `docker logs`: both streams.
async fn logs(name: &str) -> String {
    match tokio::process::Command::new("docker")
        .args(["logs", name])
        .output()
        .await
    {
        Ok(o) => format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        ),
        Err(_) => String::new(),
    }
}

/// A chain spec with its boot nodes (one per chain) moved from the loopback to `gateway`, where
/// the adapter's namespace reaches the runner; with `boot`, that boot node instead.
fn rewrite_spec(path: &str, gateway: &str, boot: Option<&str>) -> Result<String, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    let mut spec: Value = serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
    let nodes: Vec<String> = match boot {
        Some(b) => vec![b.to_owned()],
        None => spec["bootNodes"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|n| n.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
    };
    let nodes: Vec<String> = nodes
        .into_iter()
        .map(|n| n.replace("/ip4/127.0.0.1/", &format!("/ip4/{gateway}/")))
        .collect();
    say(format!("{path}: boot nodes {nodes:?}"));
    spec["bootNodes"] = serde_json::json!(nodes);
    Ok(spec.to_string())
}

/// The port of a `/ip4/…/tcp/<port>/…` multiaddr.
fn port_of(multiaddr: &str) -> Option<String> {
    let parts: Vec<&str> = multiaddr.split('/').collect();
    let at = parts.iter().position(|p| *p == "tcp")?;
    parts.get(at + 1).map(|p| (*p).to_owned())
}

/// Forward `0.0.0.0:listen` to `127.0.0.1:to`, for as long as the driver runs: a node's RPC, which
/// listens on the runner's loopback only, for an adapter opted into RPC.
async fn forward(listen: u16, to: u16) -> Result<(), String> {
    let l = tokio::net::TcpListener::bind(("0.0.0.0", listen))
        .await
        .map_err(|e| format!("forwarding port {listen}: {e}"))?;
    tokio::spawn(async move {
        while let Ok((mut inbound, _)) = l.accept().await {
            tokio::spawn(async move {
                if let Ok(mut outbound) = tokio::net::TcpStream::connect(("127.0.0.1", to)).await {
                    let _ = tokio::io::copy_bidirectional(&mut inbound, &mut outbound).await;
                }
            });
        }
    });
    Ok(())
}

impl Harness {
    /// The network, PostgreSQL, the namespace and the adapter's configuration. Nothing runs the
    /// adapter yet.
    pub async fn up(
        dir: &Path,
        ingress_port: u16,
        genesis: &str,
        spec: u32,
        contract: [u8; 32],
    ) -> Result<Harness, String> {
        let tag = std::env::var("ALEDGENT_TAG")
            .or_else(|_| std::env::var("GITHUB_RUN_ID"))
            .unwrap_or_else(|_| std::process::id().to_string());
        let image = std::env::var("ALEDGENT_IMAGE").unwrap_or_else(|_| "rust:1-bookworm".into());
        let pg_image =
            std::env::var("ALEDGENT_PG_IMAGE").unwrap_or_else(|_| "postgres:16-alpine".into());
        let bin = PathBuf::from(env("SETTLE_BIN"));
        let bin_dir = bin
            .parent()
            .map(Path::to_path_buf)
            .ok_or("SETTLE_BIN has no directory")?;
        let label = format!("aledgent-e2e={tag}");
        let net = format!("aledgent-{tag}");
        docker(&["network", "create", "--label", &label, &net]).await?;
        let gateway = docker(&[
            "network",
            "inspect",
            "-f",
            "{{(index .IPAM.Config 0).Gateway}}",
            &net,
        ])
        .await?;
        let pg = format!("aledgent-pg-{tag}");
        docker(&[
            "run",
            "-d",
            "--name",
            &pg,
            "--label",
            &label,
            "--network",
            &net,
            "-e",
            "POSTGRES_USER=settle",
            "-e",
            "POSTGRES_PASSWORD=settle",
            "-e",
            "POSTGRES_DB=settle",
            &pg_image,
        ])
        .await?;
        let ns = format!("aledgent-ns-{tag}");
        docker(&[
            "run",
            "-d",
            "--name",
            &ns,
            "--label",
            &label,
            "--network",
            &net,
            "--cap-add",
            "NET_ADMIN",
            &image,
            "sleep",
            "infinity",
        ])
        .await?;
        // iptables in the namespace, while it still reaches Debian's mirrors.
        docker(&[
            "exec", &ns, "bash", "-c",
            "apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends iptables >/dev/null \
             && iptables -N STARVE && iptables -N PRUNE && iptables -A OUTPUT -j PRUNE && iptables -A OUTPUT -j STARVE",
        ])
        .await?;
        let pg_ip = docker(&[
            "inspect",
            "-f",
            "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}",
            &pg,
        ])
        .await?;
        // PostgreSQL takes its connections once its first start is over.
        let start = Instant::now();
        loop {
            let ready = docker(&[
                "exec",
                &pg,
                "psql",
                "-U",
                "settle",
                "-d",
                "settle",
                "-h",
                "127.0.0.1",
                "-c",
                "select 1",
            ])
            .await;
            if ready.is_ok() {
                break;
            }
            if start.elapsed() > Duration::from_secs(120) {
                return Err(format!("PostgreSQL did not start: {ready:?}"));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }

        // What the adapter reads: its keys (test constants), its token, the chain specs.
        let write = |name: &str, text: &str| {
            std::fs::write(dir.join(name), text).map_err(|e| format!("{name}: {e}"))
        };
        write("lane.key", &hex::encode(TEST_LANE_SECRET))?;
        write("anchor.key", &hex::encode(TEST_ANCHOR_SECRET))?;
        write("token", ADAPTER_TOKEN)?;
        let kreivo_spec = env("LIGHT_KREIVO_CHAIN_SPEC");
        write("kreivo.json", &rewrite_spec(&kreivo_spec, &gateway, None)?)?;
        write(
            "relay.json",
            &rewrite_spec(&env("LIGHT_RELAY_CHAIN_SPEC"), &gateway, None)?,
        )?;
        let spec_text = std::fs::read_to_string(&kreivo_spec).unwrap_or_default();
        let collator_port = serde_json::from_str::<Value>(&spec_text)
            .ok()
            .and_then(|s| s["bootNodes"][0].as_str().and_then(port_of));
        let pruned = std::env::var("KREIVO_PRUNED_P2P").ok();
        if let Some(p) = &pruned {
            write(
                "kreivo-pruned.json",
                &rewrite_spec(&kreivo_spec, &gateway, Some(p))?,
            )?;
        }

        let connection =
            std::env::var("ALEDGENT_CONNECTION").unwrap_or_else(|_| "light-client".into());
        let mut adapter = adapter_config([0; 32]);
        adapter.holder = "settle-e2e".into();
        adapter.genesis = genesis.to_owned();
        adapter.spec_versions = vec![spec];
        adapter.instance = crate::INSTANCE;
        adapter.assets = vec![
            AssetBinding {
                asset: ingress::testkit::USD.into(),
                chain: ChainAsset(ingress::testkit::USD_LOCATION.into()),
            },
            // The book-only asset, bound during the run (F-029, amendment 0011).
            AssetBinding {
                asset: crate::BOOK_ONLY.into(),
                chain: ChainAsset(format!("Here({})", crate::BOOK_ONLY_INDEX)),
            },
        ];
        adapter.keys = vec![PassKey {
            user: hex::encode(TEST_PASS_USER),
            device: KeySource::File("/cfg/lane.key".into()),
        }];
        adapter.anchor_key = Some(PassKey {
            user: hex::encode(TEST_ANCHOR_PASS_USER),
            device: KeySource::File("/cfg/anchor.key".into()),
        });
        let mut config = ProcessConfig {
            // The suite's PostgreSQL has no TLS.
            profile: ingress::config::Profile::Development,
            adapter,
            engines: Default::default(),
            keys: Default::default(),
            connection: Connection::LightClient,
            chain_spec: Some("/cfg/kreivo.json".into()),
            relay_chain_spec: Some("/cfg/relay.json".into()),
            endpoint: None,
            deployments: vec![DeploymentContract {
                instance: crate::INSTANCE,
                contract: hex::encode(contract),
            }],
            contracts_pallet: "Contracts".into(),
            assets_pallet: "Assets".into(),
            ingress: format!("http://{gateway}:{ingress_port}"),
            ingress_token_file: Some("/cfg/token".into()),
            ingress_token_env: None,
            database: Some(format!("postgres://settle:settle@{pg}:5432/settle")),
            database_env: None,
            interval: 2_000,
            // `NFR-15` (T-032-12): `ALEDGENT_SETTLE_WAKE_POLL=0` measures the adapter without it.
            wake_poll: std::env::var("ALEDGENT_SETTLE_WAKE_POLL")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or_else(settle::config::default_wake_poll),
            signals_file: None,
            dry_run_endpoint: None,
            history_endpoint: None,
            // A zombienet run's lane keys are test values in files (F-035: development keys,
            // declared as such; production's are the signer service's).
            signer: None,
            development_keys: true,
        };
        // Over the light client the adapter dry-runs its batches there too, with no node named
        // (the patched smoldot, F-027 NOTES decision 43). With `rpc`, everything goes through the
        // collator's RPC, forwarded from the loopback, where the node listens, to where the
        // adapter's namespace reaches the runner.
        if connection == "rpc" {
            let ws = env("KREIVO_WS");
            let to: u16 = port_of(&ws.replace("ws://", "/ip4/").replace(':', "/tcp/"))
                .and_then(|p| p.parse().ok())
                .ok_or_else(|| format!("KREIVO_WS {ws}: no port"))?;
            let listen = ingress_port + 1;
            forward(listen, to).await?;
            config.connection = Connection::Rpc;
            config.chain_spec = None;
            config.relay_chain_spec = None;
            config.endpoint = Some(format!("ws://{gateway}:{listen}"));
        }
        let text = serde_json::to_string_pretty(&config).map_err(|e| e.to_string())?;
        write("settle.json", &text)?;
        if pruned.is_some() && config.connection == Connection::LightClient {
            config.chain_spec = Some("/cfg/kreivo-pruned.json".into());
            write(
                "settle-pruned.json",
                &serde_json::to_string_pretty(&config).map_err(|e| e.to_string())?,
            )?;
            // The history endpoint: the collator's RPC (archive-canonical), forwarded from the
            // loopback, where it listens, to where the adapter's namespace reaches the runner.
            let ws = env("KREIVO_WS");
            let to: u16 = port_of(&ws.replace("ws://", "/ip4/").replace(':', "/tcp/"))
                .and_then(|p| p.parse().ok())
                .ok_or_else(|| format!("KREIVO_WS {ws}: no port"))?;
            let listen = ingress_port + 2;
            forward(listen, to).await?;
            config.history_endpoint = Some(format!("ws://{gateway}:{listen}"));
            write(
                "settle-pruned-history.json",
                &serde_json::to_string_pretty(&config).map_err(|e| e.to_string())?,
            )?;
        }
        say(format!(
            "the adapter's harness is up: network {net} (runner at {gateway}), PostgreSQL at \
             {pg_ip}, {connection}"
        ));
        Ok(Harness {
            tag,
            image,
            dir: dir.to_path_buf(),
            bin_dir,
            net,
            gateway,
            pg,
            pg_ip,
            ns,
            ingress_port,
            collator_port,
            runs: 0,
            current: None,
            finished: String::new(),
        })
    }

    fn iptables(&self, rule: &str) -> Vec<String> {
        let mut args = vec!["exec".to_owned(), self.ns.clone(), "iptables".to_owned()];
        args.extend(rule.split_whitespace().map(str::to_owned));
        args
    }

    async fn rule(&self, rule: &str) -> Result<(), String> {
        let args = self.iptables(rule);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        docker(&refs).await.map(|_| ())
    }

    /// Start a new adapter process over the same store; with `crash`, it dies at that point
    /// (`ALEDGENT_SETTLE_CRASH_AT`).
    pub async fn start(&mut self, variant: Variant, crash: Option<&str>) -> Result<(), String> {
        self.stop().await;
        match variant {
            Variant::Normal => self.rule("-F PRUNE").await?,
            Variant::PrunedOnly | Variant::PrunedWithHistory => {
                let port = self
                    .collator_port
                    .clone()
                    .ok_or("the collator's parachain port is not known")?;
                self.rule("-F PRUNE").await?;
                self.rule(&format!(
                    "-A PRUNE -p tcp --dport {port} -j REJECT --reject-with tcp-reset"
                ))
                .await?;
            }
        }
        let config = match variant {
            Variant::Normal => "/cfg/settle.json",
            Variant::PrunedOnly => "/cfg/settle-pruned.json",
            Variant::PrunedWithHistory => "/cfg/settle-pruned-history.json",
        };
        if !self.dir.join(config.trim_start_matches("/cfg/")).exists() {
            return Err(format!(
                "{config} was not written (KREIVO_PRUNED_P2P, light client)"
            ));
        }
        self.runs += 1;
        let name = format!("aledgent-settle-{}-{}", self.tag, self.runs);
        let label = format!("aledgent-e2e={}", self.tag);
        let netns = format!("container:{}", self.ns);
        let dist = format!("{}:/dist:ro", self.bin_dir.display());
        let cfg = format!("{}:/cfg:ro", self.dir.display());
        let crash_env = format!("ALEDGENT_SETTLE_CRASH_AT={}", crash.unwrap_or_default());
        let mut args = vec![
            "run",
            "-d",
            "--name",
            &name,
            "--label",
            &label,
            "--network",
            &netns,
            "-v",
            &dist,
            "-v",
            &cfg,
            "-e",
            "RUST_LOG=info,smoldot=info",
            "-e",
            "RUST_BACKTRACE=1",
        ];
        match crash {
            Some(_) => args.extend(["-e", crash_env.as_str()]),
            // As a supervisor would: a process that dies (the binary aborts on any panic, and
            // smoldot panics) is started again over the same store.
            None => args.extend(["--restart", "on-failure"]),
        }
        args.extend([self.image.as_str(), "/dist/aledgent-settle", config]);
        docker(&args).await?;
        say(format!(
            "adapter process {} started ({variant:?}{})",
            self.runs,
            crash.map(|c| format!(", dies at {c}")).unwrap_or_default()
        ));
        self.current = Some((name, variant));
        Ok(())
    }

    /// Stop the current process (it is killed), keeping its log.
    pub async fn stop(&mut self) {
        if let Some((name, _)) = self.current.take() {
            let text = logs(&name).await;
            let _ = std::fs::write(self.dir.join(format!("adapter-{}.log", self.runs)), &text);
            self.finished.push_str(&text);
            let _ = docker(&["rm", "-f", &name]).await;
        }
    }

    /// A serving adapter of `variant`: the current one, or a new one.
    pub async fn ensure(&mut self, variant: Variant) -> Result<(), String> {
        let running = match &self.current {
            Some((name, v)) if *v == variant => {
                docker(&["inspect", "-f", "{{.State.Running}}", name]).await == Ok("true".into())
            }
            _ => false,
        };
        if !running {
            self.start(variant, None).await?;
        }
        self.serving(Duration::from_secs(300)).await
    }

    async fn current_log(&self) -> String {
        match &self.current {
            Some((name, _)) => logs(name).await,
            None => String::new(),
        }
    }

    /// Until the current process says it serves.
    pub async fn serving(&self, limit: Duration) -> Result<(), String> {
        let start = Instant::now();
        loop {
            let log = self.current_log().await;
            if log.contains("serving=true") {
                say(format!(
                    "the adapter serves ({}s)",
                    start.elapsed().as_secs()
                ));
                return Ok(());
            }
            if start.elapsed() > limit {
                return Err(format!(
                    "the adapter did not serve within {}s; its log ends:\n{}",
                    limit.as_secs(),
                    tail(&log, 30)
                ));
            }
            tokio::time::sleep(Duration::from_secs(3)).await;
        }
    }

    /// How many lines of every process's log so far contain `pattern`.
    pub async fn count_all(&self, pattern: &str) -> usize {
        self.finished.matches(pattern).count() + self.current_log().await.matches(pattern).count()
    }

    /// How many lines of the current process's log contain `pattern`.
    pub async fn count_current(&self, pattern: &str) -> usize {
        self.current_log().await.matches(pattern).count()
    }

    /// The last `n` lines of the current process's log.
    pub async fn log_tail(&self, n: usize) -> String {
        tail(&self.current_log().await, n)
    }

    /// Whether the current process's log contains `pattern`.
    pub async fn log_contains(&self, pattern: &str) -> bool {
        self.current_log().await.contains(pattern)
    }

    /// Until the logs hold `n` lines with `pattern`.
    pub async fn wait_log(&self, pattern: &str, n: usize, limit: Duration) -> Result<(), String> {
        let start = Instant::now();
        while self.count_all(pattern).await < n {
            if start.elapsed() > limit {
                return Err(format!(
                    "'{pattern}' not logged within {}s; the log ends:\n{}",
                    limit.as_secs(),
                    tail(&self.current_log().await, 30)
                ));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Ok(())
    }

    /// Until the current process exits: its exit code.
    pub async fn wait_exit(&self, limit: Duration) -> Result<String, String> {
        let name = self
            .current
            .as_ref()
            .map(|(n, _)| n.clone())
            .ok_or("no process")?;
        let start = Instant::now();
        loop {
            let s = docker(&[
                "inspect",
                "-f",
                "{{.State.Running}} {{.State.ExitCode}}",
                &name,
            ])
            .await?;
            if let Some(code) = s.strip_prefix("false ") {
                return Ok(code.to_owned());
            }
            if start.elapsed() > limit {
                return Err(format!(
                    "the adapter did not exit within {}s",
                    limit.as_secs()
                ));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// Cut every peer: nothing leaves the namespace but to PostgreSQL and to ingress.
    pub async fn starve(&self) -> Result<(), String> {
        for rule in [
            "-F STARVE".to_owned(),
            "-A STARVE -o lo -j ACCEPT".to_owned(),
            format!("-A STARVE -d {} -j ACCEPT", self.pg_ip),
            format!(
                "-A STARVE -d {} -p tcp --dport {} -j ACCEPT",
                self.gateway, self.ingress_port
            ),
            "-A STARVE -p tcp -j REJECT --reject-with tcp-reset".to_owned(),
            "-A STARVE -j DROP".to_owned(),
        ] {
            self.rule(&rule).await?;
        }
        say("the adapter's light client is cut from every peer");
        Ok(())
    }

    /// Let the peers through again.
    pub async fn feed(&self) -> Result<(), String> {
        self.rule("-F STARVE").await
    }

    /// The adapter store's `settle_reports` bodies of `kind`.
    pub async fn reports(&self, kind: &str) -> Vec<Value> {
        let rows = docker(&[
            "exec",
            &self.pg,
            "psql",
            "-At",
            "-U",
            "settle",
            "-d",
            "settle",
            "-c",
            "select body from settle_reports",
        ])
        .await
        .unwrap_or_default();
        rows.lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|r| r["kind"] == kind)
            .map(|r| r["body"].clone())
            .collect()
    }

    /// The transaction the store holds for `op`.
    pub async fn extrinsic_of(&self, op: &str) -> Option<Stored> {
        let sql = format!("select nonce, hash from settle_extrinsics where operation = '{op}'");
        let row = docker(&[
            "exec", &self.pg, "psql", "-At", "-U", "settle", "-d", "settle", "-c", &sql,
        ])
        .await
        .ok()?;
        let (nonce, hash) = row.lines().next()?.split_once('|')?;
        Some(Stored {
            nonce: nonce.parse().ok()?,
            hash: hash.to_owned(),
        })
    }

    /// Keep the current process's log so far.
    pub async fn save_logs(&self) {
        let _ = std::fs::write(
            self.dir.join(format!("adapter-{}.log", self.runs)),
            self.current_log().await,
        );
    }

    /// Stop the adapter and remove everything the harness started.
    pub async fn down(&mut self) {
        self.stop().await;
        let _ = docker(&["rm", "-f", &self.ns, &self.pg]).await;
        let _ = docker(&["network", "rm", &self.net]).await;
        say("the adapter's harness is down");
    }
}
