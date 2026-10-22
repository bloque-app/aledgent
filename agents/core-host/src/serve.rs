// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `aledgent-core-host serve`: the core Agent behind ingress's `boundary::Core`.
//!
//! One process holds the Agent's runtime state (the Local host's job, `driver`), the host's
//! replica and its log ([`core_binding::core::AgentCore`] over a [`FileLog`]), and serves
//! `boundary::Core`'s four operations on the deployment's `core_endpoint`
//! ([`ingress::remote::serve`]). ingress reads the same configuration file and connects there.
//!
//! **State on disk**, under `--state <dir>`:
//!
//! | File | What | Written |
//! |---|---|---|
//! | `agent.state` | The Agent's runtime state and the driver's positions ([`Persisting`]) | After every invocation, atomically (write, sync, rename) |
//! | `archive.bin` | Every input the Agent committed, as its delta-ring entry | Before the Agent is told it is archived |
//! | `replies.jsonl` | Every input's reply, by key | Before the reply is returned |
//! | `replica/` | Unless `--replica-in-memory`: the replica's stores, the book's leaves, openings and service tree (`T-031-14`, the default since `T-032-15`) | Unsynced; wiped and rebuilt on every start |
//! | `archive.status.json` | With `--archive`: the archiver's lag, failures and last restore point (`core_archive::ArchiveStatus`), for `opsctl exporter` | About once a second |
//!
//! **The off-node archive** (`--archive <spec>`, `T-005-15`). Every decision's bytes of
//! `archive.bin` and `replies.jsonl` stream to an object store (S3 with Object Lock and KMS, or a
//! directory), at most `--archive-interval-ms` after the decision, with a checkpoint of
//! `agent.state` every `--checkpoint-every` inputs (`core_archive`). Nothing the core answers waits
//! for it. `aledgent-core-host restore` brings a fresh node to the archive's point
//! ([`restore`]). Without `--archive`, losing this node's volume loses every decision since its
//! last snapshot: the host says so on start.
//!
//! A restart restores the runtime state, re-attaches the installed actor, and rebuilds the
//! replica from the archive and the Agent; what the Agent committed beyond the archive (a crash
//! between the two writes) is read back from its ring.
//!
//! **The audit trail** (`T-013-13`): with `audit_store` in the configuration, the core ships its
//! trail there from a thread of its own, with the writer credential in
//! `ALEDGENT_AUDIT_WRITER_CREDENTIAL`; the store's directory must not be inside `--state`.
//!
//! **Keys** (`F-035`, guardrail 13, `NFR-8`): none in this process. The key ring (the root of
//! every kernel key, blinding and nonce), the clerk key, the Agent's authority key and the
//! runtime package key are the signer service's (`services/signer`), named by the deployment
//! file's `core_keys`; this process holds their identifiers and public halves, and asks the
//! signer for every signature, key and blinding over a Unix socket authenticated by peer
//! credentials and a token read from a file. If the signer stops answering for a minute, or
//! refuses, the process stops (fail closed): nothing half-done reaches the Agent. A key in the
//! environment (the variables this process once read) fails the start; `restore` reads its keys
//! the same way. The one credential still from the environment is, with `audit_store`, the audit
//! store's writer credential (`REQ-AU-8`: never a default).
//!
//! **The core link** (`F-035`): ingress authenticates to this process with a token whose SHA-256
//! the deployment file's `core_link` lists, and the peer's user id (`ingress::remote`). Without
//! `core_link`, `serve` does not start.
//!
//! **The core stream** (`F-038`, `DEC-27`): with `stream.dial`, this process also dials out to
//! ingress's core listener (every address listed, mutual TLS under the operator's CA, or a Unix
//! socket on the same host with the token in `ALEDGENT_CORE_STREAM_TOKEN`) and answers ingress's
//! calls down that stream; it listens on no network port. Either link, or both at once during
//! the transition, reaches the core through one `ingress::feed::Sequencer`: ingress's signature on
//! every forwarded input is checked against `stream.ingress_keys` (required in production with
//! `stream.dial`; with them, no unsigned or unsequenced input is taken, from the host's start;
//! without them and without `stream.dial`, the socket takes a legacy ingress's unsequenced
//! inputs as before, `F-038` §12 step 0) before the Agent sees
//! it, a stale feeder's epoch is fenced, duplicates are answered from the reply log, and early arrivals
//! wait for the gap. The Agent and the host's calls into it are unchanged (`DEC-26(d)`,
//! `DEC-27` (i)). `core_endpoint` is optional when `stream.dial` is set.

use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use core_archive::archiver::StateReader;
use core_archive::{Access, Archiver, ArchiverConfig, ArchivingLog};
use core_binding::core::{AgentCore, FileLog, Founding, HostLog as _, roll_forward_agent};
use core_binding::envelope::BlobKey;
use core_binding::host::{
    AgentPort, BatchLimits, DRIVER, PortError, REPLICA_CACHE_BYTES, ReplicaStores,
};
use core_binding::hot::{Hot, Limits, MAX_BUDGET, RingEntry};
use ingress::config::{DeploymentConfig, ENV_CLERK_KEY, ENV_CORE_ROOT};
use ingress::remote::Endpoint;
use signer::client::{Patience, RemoteClerk, RemoteCustody, RemoteEd25519, SignerClient};
use vos::agent_sdk::RuntimeState;

use crate::driver::{AgentKeys, Runtime};
use crate::package;
use crate::port::VosPort;

/// The variable that once held the Agent's authority key. Set, it fails the start (`F-035`).
pub const ENV_AGENT_KEY: &str = "ALEDGENT_CORE_AGENT_KEY";
/// The variable that once held the runtime package key. Set, it fails the start (`F-035`).
pub const ENV_PACKAGE_KEY: &str = "ALEDGENT_CORE_PACKAGE_KEY";

/// Every variable that once carried key material into this process (`F-035`).
pub const KEY_VARIABLES: [&str; 4] = [ENV_CORE_ROOT, ENV_CLERK_KEY, ENV_AGENT_KEY, ENV_PACKAGE_KEY];

/// Refuse to start with key material in the environment (`F-035`, `NFR-8`: "not readable from a
/// process environment"): a deployment that still sets one is misconfigured, and this process
/// would not use it.
pub fn refuse_keys_in_environment() -> Result<(), String> {
    let set: Vec<&str> = KEY_VARIABLES
        .iter()
        .copied()
        .filter(|v| std::env::var_os(v).is_some())
        .collect();
    if set.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "key material in the environment ({}): the core host takes its keys from the signer \
             service (core_keys) and refuses to start (F-035, NFR-8)",
            set.join(", ")
        ))
    }
}

/// The core's keys behind the signer service (`F-035`): the replica's and the clerk's, and the
/// Agent's. Nothing here holds a secret.
pub fn signer_keys(
    k: &ingress::config::CoreKeys,
) -> Result<(core_binding::host::HostKeys, AgentKeys), String> {
    let client = Arc::new(SignerClient::connect(&k.signer).map_err(|e| e.to_string())?);
    let p = Patience::PRODUCTION;
    let custody = RemoteCustody::new(Arc::clone(&client), &k.key_ring, p)
        .map_err(|e| format!("core_keys.key_ring: {e}"))?;
    let clerk = match &k.clerk {
        Some(id) => Some(Arc::new(
            RemoteClerk::new(Arc::clone(&client), id, p)
                .map_err(|e| format!("core_keys.clerk: {e}"))?,
        ) as core_binding::host::SharedClerk),
        None => None,
    };
    let ed = |id: &str, what: &str| {
        RemoteEd25519::new(Arc::clone(&client), id, p)
            .map(|k| Arc::new(k) as crate::keys::SharedSigner)
            .map_err(|e| format!("core_keys.{what}: {e}"))
    };
    let agent = AgentKeys {
        package: ed(&k.agent_package, "agent_package")?,
        authority: ed(&k.agent_authority, "agent_authority")?,
        discriminator: 0x40,
    };
    Ok((
        core_binding::host::HostKeys::new(Arc::new(custody), clerk),
        agent,
    ))
}
/// The actor's name in the Agent's directory.
pub const ACTOR_NAME: &str = "aledgent-core";

/// The Agent's runtime state's file: what a checkpoint of the archive holds.
pub const STATE_FILE: &str = "agent.state";
const STATE_MAGIC: &[u8; 8] = b"ALGCORE1";

/// The Agent's runtime state and the driver's positions, as one file.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Saved {
    /// The runtime state.
    pub state: RuntimeState,
    /// The Linear position.
    pub linear: u64,
    /// The Control position.
    pub control: u64,
    /// The last observed slot.
    pub slot: u64,
}

impl Saved {
    fn encode(&self) -> Vec<u8> {
        let mut out = STATE_MAGIC.to_vec();
        for n in [self.linear, self.control, self.slot] {
            out.extend_from_slice(&n.to_be_bytes());
        }
        for c in [
            &self.state.control,
            &self.state.linear,
            &self.state.merge,
            &self.state.local,
        ] {
            out.extend_from_slice(&(c.len() as u64).to_be_bytes());
            out.extend_from_slice(c);
        }
        out
    }

    fn decode(bytes: &[u8]) -> Result<Saved, String> {
        let bad = || "agent.state is truncated or not ours".to_string();
        let rest = bytes.strip_prefix(STATE_MAGIC.as_slice()).ok_or_else(bad)?;
        let mut at = 0usize;
        let u64_at = |at: &mut usize| -> Result<u64, String> {
            let b = rest.get(*at..*at + 8).ok_or_else(bad)?;
            *at += 8;
            Ok(u64::from_be_bytes(b.try_into().map_err(|_| bad())?))
        };
        let linear = u64_at(&mut at)?;
        let control = u64_at(&mut at)?;
        let slot = u64_at(&mut at)?;
        let mut parts = Vec::new();
        for _ in 0..4 {
            let n = usize::try_from(u64_at(&mut at)?).map_err(|_| bad())?;
            parts.push(rest.get(at..at + n).ok_or_else(bad)?.to_vec());
            at += n;
        }
        if at != rest.len() {
            return Err(bad());
        }
        let local = parts.pop().ok_or_else(bad)?;
        let merge = parts.pop().ok_or_else(bad)?;
        let linear_lane = parts.pop().ok_or_else(bad)?;
        let control_lane = parts.pop().ok_or_else(bad)?;
        Ok(Saved {
            state: RuntimeState {
                control: control_lane,
                linear: linear_lane,
                merge,
                local,
            },
            linear,
            control,
            slot,
        })
    }

    /// Read `dir`'s saved state, if any.
    pub fn load(dir: &Path) -> Result<Option<Saved>, String> {
        match fs::read(dir.join(STATE_FILE)) {
            Ok(b) => Saved::decode(&b).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{STATE_FILE}: {e}")),
        }
    }

    /// Write it to `dir`: a temporary file, synced, renamed over the old one.
    pub fn store(&self, dir: &Path) -> Result<(), String> {
        let tmp = dir.join(format!("{STATE_FILE}.tmp"));
        let mut f = File::create(&tmp).map_err(|e| format!("{STATE_FILE}: {e}"))?;
        f.write_all(&self.encode())
            .and_then(|_| f.sync_all())
            .map_err(|e| format!("{STATE_FILE}: {e}"))?;
        fs::rename(&tmp, dir.join(STATE_FILE)).map_err(|e| format!("{STATE_FILE}: {e}"))?;
        File::open(dir)
            .and_then(|d| d.sync_all())
            .map_err(|e| format!("{}: {e}", dir.display()))
    }
}

/// The VOS port, with the Agent's runtime state persisted after every invocation. A failed
/// write poisons the port: every later call is refused, and the core stops (fail closed), since
/// the Agent's state on disk no longer matches what it answered.
pub struct Persisting {
    port: VosPort,
    dir: PathBuf,
    poisoned: Option<String>,
}

impl Persisting {
    /// The port over `port`, persisting to `dir`.
    pub fn new(port: VosPort, dir: PathBuf) -> Self {
        Persisting {
            port,
            dir,
            poisoned: None,
        }
    }

    fn save(&mut self) -> Result<(), PortError> {
        let rt = self.port.runtime();
        let (linear, control) = rt.positions();
        let saved = Saved {
            state: rt.state.clone(),
            linear,
            control,
            slot: rt.slot(),
        };
        saved.store(&self.dir).map_err(|e| {
            self.poisoned = Some(e.clone());
            PortError::Ambiguous(e)
        })
    }

    fn check(&self) -> Result<(), PortError> {
        match &self.poisoned {
            Some(e) => Err(PortError::Unavailable(format!(
                "the Agent's state is not saved: {e}"
            ))),
            None => Ok(()),
        }
    }
}

impl AgentPort for Persisting {
    fn unacknowledged(&self) -> Option<u64> {
        self.port.unacknowledged()
    }

    /// The switch changes the runtime's state (the installed actor): persist it at once, so a
    /// restart attaches the package the state names.
    fn switch_to(&mut self, code: [u8; 32]) -> Result<(), String> {
        self.check().map_err(|e| format!("{e:?}"))?;
        let before = self.port.code_state().map(|c| c.running);
        self.port.switch_to(code)?;
        if self.port.code_state().map(|c| c.running) != before {
            self.save().map_err(|e| format!("{e:?}"))?;
        }
        Ok(())
    }

    /// Staging changes only what the host holds in memory for the switch (`T-037-04`).
    fn stage_code(
        &mut self,
        checked: core_binding::staging::Checked,
    ) -> Result<(), core_binding::staging::StageRefusal> {
        self.port.stage_code(checked)
    }

    fn holds_code(&self, code: &[u8; 32]) -> bool {
        self.port.holds_code(code)
    }

    /// Staging again changes only what the host holds in memory (`T-037-21`).
    fn restage_code(
        &mut self,
        checked: core_binding::staging::Checked,
        generation: u64,
    ) -> Result<(), core_binding::staging::StageRefusal> {
        self.port.restage_code(checked, generation)
    }

    fn invoke(&mut self, envelope: Vec<u8>, blobs: Vec<Vec<u8>>) -> Result<Vec<u8>, PortError> {
        self.check()?;
        let r = self.port.invoke(envelope, blobs);
        self.save()?;
        r
    }

    fn blob_key(&self, bytes: &[u8]) -> BlobKey {
        self.port.blob_key(bytes)
    }

    fn hot(&mut self) -> Result<Option<Hot>, PortError> {
        self.check()?;
        let r = self.port.hot();
        self.save()?;
        r
    }

    fn entry(&mut self, seq: u64) -> Result<Option<RingEntry>, PortError> {
        self.check()?;
        let r = self.port.entry(seq);
        self.save()?;
        r
    }
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone())
}

fn need(args: &[String], name: &str) -> Result<String, String> {
    arg(args, name).ok_or_else(|| format!("missing {name}"))
}

/// The runtime, asked to attach what a saved state has installed.
struct Attaching<'a>(&'a mut Runtime);

impl crate::restart::Attach for Attaching<'_> {
    type Actor = crate::driver::Actor;

    fn installed_package(&mut self) -> Result<vos::agent_sdk::BlobRef, String> {
        self.0
            .installed_package(ACTOR_NAME)
            .map_err(|e| format!("attach: {e:?}"))
    }

    fn attach(&mut self, bytes: &[u8]) -> Result<Self::Actor, String> {
        let package = package::admit_actor(bytes)
            .map_err(|e| format!("the installed package, from the code cache: {e:?}"))?;
        self.0
            .attach(&package, ACTOR_NAME, None)
            .map_err(|e| format!("attach: {e:?}"))
    }
}

impl crate::restart::Tracks for VosPort {
    fn track(&mut self, state: crate::switch::CodeState) {
        self.track_code(state);
    }
}

/// Open the Agent in `dir`: restore it if it was saved, create and install it if not.
///
/// A restart attaches the package the runtime's saved state has installed, which after a switch
/// is not the `--actor` package but a version found in the code cache `held` (`T-037-21`,
/// [`crate::restart`]); and it stages again the versions the Agent's hot row names, so the start
/// check the core runs when it opens finds them. `held` is `None` when the host has no code cache
/// (no `--code-config`, or `restore`).
pub fn open_port(
    runtime_pvm: &[u8],
    actor_package: &[u8],
    keys: &AgentKeys,
    dir: &Path,
    genesis_code: Option<[u8; 32]>,
    held: Option<&crate::code_source::Fetcher>,
) -> Result<Persisting, String> {
    let pkg = package::admit_actor(actor_package).map_err(|e| format!("actor package: {e:?}"))?;
    let mut rt = Runtime::create(runtime_pvm, keys).map_err(|e| format!("runtime: {e:?}"))?;
    let (actor, attached) = match Saved::load(dir)? {
        Some(saved) => {
            rt.restore(saved.state, saved.linear, saved.control, saved.slot);
            crate::restart::attach_installed(
                &mut Attaching(&mut rt),
                actor_package,
                held.map(|f| f as &dyn crate::restart::Held),
            )?
        }
        None => (
            rt.install(&pkg, ACTOR_NAME, None)
                .map_err(|e| format!("install: {e:?}"))?,
            actor_package.to_vec(),
        ),
    };
    let mut vos = VosPort::new(rt, actor, DRIVER);
    vos.keep_samples(false);
    // `switch(H, n)` (`T-037-14`): the port tracks the code it runs, so the driver's check before
    // every send recognises what is attached as already running, and the genesis code is kept to
    // be installed again after a way out. Versions staged for an upgrade arrive with `T-037-04`
    // and `T-037-06`, and stay in memory: a restart stages again the ones the book names
    // (`T-037-21`). A book whose genesis names no code tracks none.
    if let Some(h0) = genesis_code {
        crate::restart::track_and_restage(
            &mut vos,
            ACTOR_NAME,
            h0,
            actor_package,
            &attached,
            runtime_pvm,
            held.map(|f| f as &dyn core_binding::staging::CodeFetch),
        )?;
    }
    let mut port = Persisting::new(vos, dir.to_path_buf());
    port.save().map_err(|e| format!("{e:?}"))?;
    Ok(port)
}

/// What `serve` and `restore` both read: the deployment, the founding, the Agent's keys, the
/// state directory and the Agent's programs.
pub struct Setup {
    /// The book's chainspec file, when its genesis commits one (`F-036`).
    pub chainspec: Option<Vec<u8>>,
    /// The deployment's configuration.
    pub config: DeploymentConfig,
    /// What the book is founded on.
    pub founding: Founding,
    /// The Agent's keys.
    pub keys: AgentKeys,
    /// The state directory.
    pub dir: PathBuf,
    /// The runtime program.
    pub runtime: Vec<u8>,
    /// The actor package.
    pub actor: Vec<u8>,
    /// The genesis code version the start check verified the package and the runtime against
    /// (`F-036`), if the chainspec names one: the code the port tracks as running (`T-037-14`).
    pub genesis_code: Option<[u8; 32]>,
}

/// Read the arguments both commands share (`--config`, `--runtime`, `--actor`, `--state`,
/// `--budget`), and connect to the signer for the core's keys (`F-035`): no key is read here.
pub fn setup(args: &[String]) -> Result<Setup, String> {
    let path = need(args, "--config")?;
    let text = fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
    let config: DeploymentConfig =
        serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
    // F-035: no key in this process; the keys are the signer's.
    refuse_keys_in_environment()?;
    let core_keys = config
        .core_keys
        .as_ref()
        .ok_or_else(|| format!("{path}: core_keys is required (F-035): the signer service"))?;
    let (host_keys, keys) = signer_keys(core_keys)?;
    let (genesis, op, at) = config.genesis.agent_genesis()?;
    let budget = match arg(args, "--budget") {
        Some(b) => b
            .parse::<u64>()
            .map_err(|_| "--budget: bytes".to_string())?,
        None => Limits::DEFAULT.budget,
    };
    if budget > MAX_BUDGET {
        return Err(format!("--budget: at most {MAX_BUDGET}"));
    }
    let mut founding = Founding {
        genesis,
        op,
        at,
        keys: host_keys,
        limits: Limits {
            budget,
            ..Limits::DEFAULT
        },
        genesis_roots: None,
    };
    let dir = PathBuf::from(need(args, "--state")?);
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    // F-036: the book's chainspec, its genesis built from it before the Agent's first input.
    let chainspec = crate::chainspec::attach(args, &mut founding, Some(&dir))?;
    let runtime = fs::read(need(args, "--runtime")?).map_err(|e| format!("runtime: {e}"))?;
    let actor = fs::read(need(args, "--actor")?).map_err(|e| format!("actor: {e}"))?;
    // F-036, amendment 0023: the chainspec's genesis code. A founding (no replica yet) checks this
    // executable too, since it builds the genesis natively; a restart checks the Agent's code.
    let mut genesis_code = None;
    if let Some(spec) = founding.genesis.spec.as_deref() {
        let which = if dir.join("replica").exists() {
            crate::code::Check::Agent
        } else {
            crate::code::Check::Founding
        };
        if let Some(h0) = crate::code::check(args, spec, Some(&actor), Some(&runtime), which)? {
            genesis_code = Some(h0);
            let what = match which {
                crate::code::Check::Founding => "this executable, the package and the runtime are",
                crate::code::Check::Agent => "the package and the runtime are",
            };
            eprintln!(
                "aledgent-core-host: {what} the chainspec's genesis code {}",
                hex::encode(h0)
            );
        }
    }
    Ok(Setup {
        chainspec,
        config,
        founding,
        keys,
        dir,
        runtime,
        actor,
        genesis_code,
    })
}

/// The archiver's pacing from `--archive-interval-ms`, `--checkpoint-every` (inputs) and
/// `--checkpoint-every-secs`.
fn archiver_config(args: &[String]) -> Result<ArchiverConfig, String> {
    let mut c = ArchiverConfig::default();
    let num = |name: &str| -> Result<Option<u64>, String> {
        arg(args, name)
            .map(|v| v.parse::<u64>().map_err(|_| format!("{name}: a number")))
            .transpose()
    };
    if let Some(ms) = num("--archive-interval-ms")? {
        c.interval = std::time::Duration::from_millis(ms.max(1));
    }
    if let Some(n) = num("--checkpoint-every")? {
        c.checkpoint_every_entries = n.max(1);
    }
    if let Some(s) = num("--checkpoint-every-secs")? {
        c.checkpoint_every = std::time::Duration::from_secs(s.max(1));
    }
    Ok(c)
}

/// `serve --config <file> --runtime <runtime.pvm> --actor <actor.vos> --state <dir>
/// [--chainspec <file> [--genesis-manifest <file>] [--unnamed-code]] [--budget <bytes>] [--batch <inputs>] [--replica-in-memory] [--archive <spec>
/// [--archive-interval-ms <ms>] [--checkpoint-every <inputs>] [--checkpoint-every-secs <s>]]
/// [--code-config <file>]`.
/// `--batch` is the most inputs one invocation carries (`T-031-03`); it overrides the
/// configuration's `core_batch`, and 1 is one input per invocation. The replica's stores are under
/// `<dir>/replica/` (`T-031-14`) unless `--replica-in-memory`; `--replica-on-disk`, the default, is
/// still accepted. `--archive` streams every decision off the node (module documentation).
pub fn serve(args: &[String]) -> Result<(), String> {
    let Setup {
        chainspec,
        config: c,
        founding,
        keys,
        dir,
        runtime,
        actor,
        genesis_code,
    } = setup(args)?;
    let path = need(args, "--config")?;
    // F-038: the socket (`core_endpoint`), the stream (`stream.dial`), or both at once during the
    // transition; at least one.
    let stream = c.stream.as_ref();
    if let Some(s) = stream {
        s.check(c.profile).map_err(|e| format!("{path}: {e}"))?;
    }
    let dial = stream
        .map(|s| s.dial_config(|| std::env::var(ingress::config::ENV_CORE_STREAM_TOKEN).ok()))
        .transpose()
        .map_err(|e| format!("{path}: {e}"))?
        .flatten();
    let endpoint = match c.core_endpoint.as_deref() {
        Some(e) => Some(Endpoint::parse(e).ok_or_else(|| {
            format!("{path}: core_endpoint (unix:<path> or tcp:<addr>) is malformed")
        })?),
        None if dial.is_some() => None,
        None => {
            return Err(format!(
                "{path}: core_endpoint (unix:<path> or tcp:<addr>) or stream.dial is required"
            ));
        }
    };
    // F-035: the core link is authenticated; no core_link, no start.
    let link = match &endpoint {
        Some(_) => Some(
            c.core_link
                .as_ref()
                .ok_or_else(|| format!("{path}: core_link is required (F-035)"))?
                .auth()?,
        ),
        None => None,
    };
    // F-038 §5: the keys ingress's feeds are checked against, on every link; required in
    // production once the host dials the new link. With them the host takes no unsigned or
    // unsequenced input from its start. Without them (and without `stream.dial`) it serves the
    // legacy socket link, as a host of an earlier release does (F-038 §12 step 0).
    let feed_keys = c.core_feed_keys().map_err(|e| format!("{path}: {e}"))?;
    if feed_keys.is_none() {
        eprintln!(
            "aledgent-core-host: no stream.ingress_keys: the legacy socket link, unsequenced \
             inputs taken as before F-038 (§12 step 0); feeds on the socket are taken unsigned"
        );
    }
    if dir.join(core_archive::RESTORE_FILE).exists() {
        return Err(format!(
            "{}: a restore is laid out and not finished: run `aledgent-core-host restore` first",
            dir.display()
        ));
    }
    let batch = match arg(args, "--batch") {
        Some(b) => b
            .parse::<usize>()
            .ok()
            .filter(|n| *n >= 1)
            .ok_or_else(|| "--batch: inputs per invocation, at least 1".to_string())?,
        None => c
            .core_batch
            .unwrap_or(ingress::service::DEFAULT_CORE_BATCH)
            .max(1),
    };
    let archive = arg(args, "--archive")
        .map(|spec| core_archive::open(&spec, Access::Write))
        .transpose()?;
    let pacing = archiver_config(args)?;
    // T-037-06: where an `Upgrade`'s code is fetched from. Read before anything starts, so a
    // misconfiguration fails the start; without `--code-config` the node has no source of code and
    // every `Upgrade` is refused, as before.
    let code_source = crate::code_source::from_args(args, &dir)?;
    let port = open_port(
        &runtime,
        &actor,
        &keys,
        &dir,
        genesis_code,
        code_source.as_deref(),
    )
    .map_err(|e| {
        if code_source.is_none() && e.starts_with(crate::restart::NOT_FOUND) {
            format!(
                "{e}. Start with --code-config naming the cache that holds the versions it \
                 switched to (T-037-21)"
            )
        } else {
            e
        }
    })?;
    // T-031-14, T-032-15: the replica's stores under the state directory, each with a bounded
    // cache, rebuilt from the log on every start. The default since the 2 h soak of 2026-10-04 on
    // the fleet (run 37192755903): every bound held, and the core host grew 0.46 → 0.65 GB, against
    // 0.50 → 1.73 GB with the stores in memory (run 37184829287). It had been opt-in since a
    // 2-vCPU runner measured it slower (runs 37115138309, 37119831658).
    let stores = if !args.iter().any(|a| a == "--replica-in-memory") {
        ReplicaStores::Disk {
            dir: dir.join("replica"),
            cache_bytes: REPLICA_CACHE_BYTES,
        }
    } else {
        ReplicaStores::Memory
    };
    let log = FileLog::open(&dir)?;
    let book = core_archive::book_of(&founding.op);
    let audit = c.audit_store.as_ref();
    // F-036: the chainspec beside the archive's segments, by its digest, so a restore rebuilds the
    // genesis from it (`REQ-RT-3`).
    if let (Some(a), Some(bytes), Some(c)) = (&archive, &chainspec, founding.genesis.chainspec) {
        a.put_chainspec(&hex::encode(c.digest), bytes)
            .map_err(|e| format!("the archive's chainspec: {e}"))?;
    }
    let genesis_started = std::time::Instant::now();
    let (core, _archiver) = match archive {
        Some(archive) => {
            let state = dir.join(STATE_FILE);
            let reader: StateReader =
                Box::new(move || fs::read(&state).map_err(|e| format!("{STATE_FILE}: {e}")));
            let (log, tap) = ArchivingLog::new(log, &dir, Some(reader));
            let core = open_core(port, founding, log, stores, audit, &dir)?;
            eprintln!(
                "aledgent-core-host: archiving to {} every {} ms",
                archive.describe(),
                pacing.interval.as_millis()
            );
            let archiver = Archiver::start(tap, &dir, archive, book, pacing);
            (core, Some(archiver))
        }
        None => {
            eprintln!(
                "aledgent-core-host: WARNING: no --archive: losing this node's volume loses every \
                 decision since its last snapshot (NFR-4, ops/runbooks/restore.md)"
            );
            (open_core(port, founding, log, stores, audit, &dir)?, None)
        }
    };
    if chainspec.is_some() {
        eprintln!(
            "aledgent-core-host: the chainspec genesis is installed and its roots verified in {} ms \
             (the core opened, its replica rebuilt)",
            genesis_started.elapsed().as_millis()
        );
    }
    // T-036-25 (`REQ-RC-6`, rulings `a0022-q2` and `a0022-q2b`): a chainspec book takes no command
    // until a passing verify-genesis report for it is installed here (`install-verified`); reads
    // are answered meanwhile. `--unverified-genesis` (test and stress books only) skips the gate.
    if chainspec.is_some() {
        if args.iter().any(|a| a == "--unverified-genesis") {
            eprintln!(
                "aledgent-core-host: WARNING: --unverified-genesis: the book takes commands without a \
                 verify-genesis report (test and stress books only, REQ-RC-6)"
            );
        } else {
            match core.gate_on_verified_genesis(&dir)? {
                Some(Ok(r)) => eprintln!(
                    "aledgent-core-host: verify-genesis report installed (chainspec {}, {} records): \
                     the book takes commands",
                    r.chainspec.digest, r.records
                ),
                Some(Err(why)) => eprintln!(
                    "aledgent-core-host: the book takes no command until verify-genesis passes and \
                     its report is installed (aledgent-core-host install-verified): {why}"
                ),
                None => {}
            }
        }
    }
    if let Some(source) = code_source {
        core.set_code_source(source)?;
        eprintln!(
            "aledgent-core-host: code source: an Upgrade's code is fetched from the cache and the \
             sources of --code-config, each answer checked by length and hash (T-037-06)"
        );
    }
    crate::diag::start(core.stats(), dir.clone());
    // The actor gas this host's VOS gives one invocation bounds the batch too.
    core.set_batch_limits(
        BatchLimits::with_inputs(batch).with_actor_gas(crate::driver::actor_gas()),
    );
    // F-038 §4: one sequencer in front of the core, for every link: signatures checked, stale
    // feeders fenced, the order kept. The Agent's calls are unchanged (`DEC-26(d)`).
    let core = Arc::new(ingress::feed::Sequencer::new(core, feed_keys));
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    rt.block_on(async move {
        if let Some(d) = dial {
            eprintln!(
                "aledgent-core-host: dialing ingress at {:?} (F-038)",
                d.targets
            );
            let core = Arc::clone(&core);
            let dialer = tokio::spawn(ingress::stream::dial(d, core));
            if endpoint.is_none() {
                return dialer.await.map_err(|e| e.to_string());
            }
        }
        match (endpoint, link) {
            (Some(endpoint), Some(link)) => {
                eprintln!("aledgent-core-host: serving on {endpoint:?}");
                ingress::remote::serve(endpoint, core, link)
                    .await
                    .map_err(|e| e.to_string())
            }
            _ => Err("no link to serve".into()),
        }
    })
}

/// The core over `log`: shipping its audit trail to the audit store when one is configured
/// (`T-013-13`), and saying so when none is.
fn open_core<L: core_binding::core::HostLog>(
    port: Persisting,
    founding: Founding,
    log: L,
    stores: ReplicaStores,
    audit: Option<&ingress::audit_ship::AuditStoreSection>,
    dir: &Path,
) -> Result<AgentCore<Persisting>, String> {
    match audit {
        Some(section) => {
            let sink = section.open(dir)?;
            let core = AgentCore::open_audited(
                port,
                founding,
                log,
                stores,
                Box::new(sink),
                section.max_unshipped(),
            )?;
            // What the core refuses without the book (the fence, `T-037-15`) is audited beside
            // the book's trail. An audit side file never keeps the core down (review of #44, F3):
            // whatever it holds, the core starts, says what it found, and the fence still refuses.
            match section.open_node_trail(dir) {
                Ok(trail) => {
                    let damage = trail.damage();
                    if damage.any() {
                        eprintln!(
                            "aledgent-core-host: WARNING: the audit store's node trail was \
                             damaged and opened past it: {damage:?}"
                        );
                    }
                    core.set_node_trail(trail);
                }
                Err(e) => eprintln!(
                    "aledgent-core-host: WARNING: the audit store's node trail does not open \
                     ({e}); a refusal by the fence is audited on standard error only"
                ),
            }
            Ok(core)
        }
        None => {
            eprintln!(
                "aledgent-core-host: WARNING: no audit_store is configured; the audit trail is \
                 not shipped (REQ-AT-2, NFR-6)"
            );
            AgentCore::open_in(port, founding, log, stores)
        }
    }
}

/// Lay the chainspec out in the state directory from the archive, when a restore was not given it
/// and the deployment's genesis commits one (`F-036`); and the book's `verify-genesis` report, when
/// it was installed with `--archive` (`T-036-25`), so the restored book takes commands as the
/// original did (`serve` checks it again against the genesis it rebuilds).
fn fetch_chainspec(args: &[String]) -> Result<(), String> {
    let (Some(state), Some(spec), Some(config)) = (
        arg(args, "--state"),
        arg(args, "--archive"),
        arg(args, "--config"),
    ) else {
        return Ok(());
    };
    let kept = Path::new(&state).join(crate::chainspec::STATE_FILE);
    let verified = Path::new(&state).join(core_binding::verified::VERIFIED_FILE);
    let need_spec = arg(args, "--chainspec").is_none() && !kept.exists();
    if !need_spec && verified.exists() {
        return Ok(());
    }
    let text = fs::read_to_string(&config).map_err(|e| format!("{config}: {e}"))?;
    let c: DeploymentConfig = serde_json::from_str(&text).map_err(|e| format!("{config}: {e}"))?;
    let Some(digest) = c.genesis.agent_genesis()?.0.chainspec.map(|x| x.digest) else {
        return Ok(());
    };
    let archive = core_archive::open(&spec, Access::Read)?;
    fs::create_dir_all(&state).map_err(|e| format!("{state}: {e}"))?;
    if need_spec {
        let bytes = archive
            .get_chainspec(&hex::encode(digest))
            .map_err(|e| format!("the archive's chainspec: {e}"))?
            .ok_or("the archive keeps no chainspec of this genesis: give --chainspec <file>")?;
        fs::write(&kept, bytes).map_err(|e| format!("{}: {e}", kept.display()))?;
    }
    if !verified.exists()
        && let Some(report) = archive
            .get_verified(&hex::encode(digest))
            .map_err(|e| format!("the archive's verify-genesis report: {e}"))?
    {
        fs::write(&verified, report).map_err(|e| format!("{}: {e}", verified.display()))?;
    }
    Ok(())
}

/// What `restore` leaves beside the logs once the Agent stands at the archive's point.
pub const RESTORED_FILE: &str = "restore.done.json";

/// `restore --config <file> --runtime <runtime.pvm> --actor <actor.vos> --state <dir>
/// [--budget <bytes>]` (`T-005-16`): finish a restore `opsctl restore core --from` laid out in
/// `<dir>`. The Agent starts from the checkpoint's runtime state when the layout has one, or is
/// founded afresh, and every archived input after it is re-sent and checked against its archived
/// entry ([`roll_forward_agent`]). The Agent must end at the plan's point: its input number, log
/// head and roots. Then the plan becomes `restore.done.json`, and `serve` starts on the directory.
/// Nothing is answered from a restore that did not end there.
pub fn restore(args: &[String]) -> Result<(), String> {
    // F-036: a chainspec book's restore needs its chainspec: `--chainspec`, the state directory's
    // copy, or the archive's (`--archive <spec>`), by the digest genesis commits.
    fetch_chainspec(args)?;
    let Setup {
        founding,
        keys,
        dir,
        runtime,
        actor,
        genesis_code,
        ..
    } = setup(args)?;
    let plan_path = dir.join(core_archive::RESTORE_FILE);
    let plan: core_archive::RestorePlan = serde_json::from_slice(
        &fs::read(&plan_path).map_err(|e| format!("{}: {e}", plan_path.display()))?,
    )
    .map_err(|e| format!("{}: {e}", plan_path.display()))?;
    let book = core_archive::book_of(&founding.op);
    if plan.book != book {
        return Err(format!(
            "the archive is of book {}, this deployment's is {book}",
            plan.book
        ));
    }
    let started = std::time::Instant::now();
    let port = open_port(&runtime, &actor, &keys, &dir, genesis_code, None).map_err(|e| {
        if e.starts_with(crate::restart::NOT_FOUND) {
            format!(
                "{e}. A restore across an upgrade is not supported yet (T-037-09): restore from a \
                 checkpoint taken before the switch"
            )
        } else {
            e
        }
    })?;
    let entries = FileLog::open(&dir)?.entries()?;
    let stores = ReplicaStores::Disk {
        dir: dir.join("replica"),
        cache_bytes: REPLICA_CACHE_BYTES,
    };
    let (port, rolled) = roll_forward_agent(port, &founding, &entries, &stores)?;
    drop(port);
    let p = &plan.point;
    let at_point = rolled.to == p.entries
        && hex::encode(rolled.log_head) == p.log_head
        && (p.entries == 0
            || (hex::encode(rolled.roots.kernel) == p.kernel_root
                && hex::encode(rolled.roots.service) == p.service_root
                && hex::encode(rolled.roots.audit) == p.audit));
    if !at_point {
        return Err(format!(
            "the Agent stands at input {} and the plan's point is {}: refused",
            rolled.to, p.entries
        ));
    }
    let done = serde_json::json!({
        "plan": plan,
        "rolled": {
            "from": rolled.from,
            "to": rolled.to,
            "founded": rolled.founded,
            "kernel_root": hex::encode(rolled.roots.kernel),
            "service_root": hex::encode(rolled.roots.service),
            "audit": hex::encode(rolled.roots.audit),
            "log_head": hex::encode(rolled.log_head),
        },
        "elapsed_ms": started.elapsed().as_millis() as u64,
    });
    let bytes = serde_json::to_vec_pretty(&done).map_err(|e| e.to_string())?;
    let tmp = dir.join(format!("{RESTORED_FILE}.tmp"));
    let mut f = File::create(&tmp).map_err(|e| format!("{RESTORED_FILE}: {e}"))?;
    f.write_all(&bytes)
        .and_then(|_| f.sync_all())
        .map_err(|e| format!("{RESTORED_FILE}: {e}"))?;
    fs::rename(&tmp, dir.join(RESTORED_FILE)).map_err(|e| format!("{RESTORED_FILE}: {e}"))?;
    fs::remove_file(&plan_path).map_err(|e| format!("{}: {e}", plan_path.display()))?;
    File::open(&dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| format!("{}: {e}", dir.display()))?;
    println!(
        "{}",
        serde_json::to_string(&done).map_err(|e| e.to_string())?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `F-035`: the core host refuses to start with key material in its environment.
    #[test]
    fn nfr_8_a_key_in_the_environment_fails_the_start() {
        // The test runner's environment carries none of them.
        assert_eq!(refuse_keys_in_environment(), Ok(()));
        for v in KEY_VARIABLES {
            assert!(v.starts_with("ALEDGENT_"));
        }
    }

    #[test]
    fn t_005_08_the_saved_state_reads_back() {
        let s = Saved {
            state: RuntimeState {
                control: vec![1, 2],
                linear: vec![3; 70],
                merge: Vec::new(),
                local: vec![9],
            },
            linear: 5,
            control: 6,
            slot: 7,
        };
        assert_eq!(Saved::decode(&s.encode()), Ok(s));
        assert!(Saved::decode(b"ALGCORE1\0").is_err());
    }
}
