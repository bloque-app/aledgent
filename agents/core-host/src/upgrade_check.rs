// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `check-upgrade`: the real-package check of a core upgrade, before it is published (`T-037-12`,
//! `T-037-19`'s precondition; owner ruling of 2026-10-27, bloque-app/bloque-ledgent#286; D18).
//!
//! The core release job (`.github/workflows/core-release.yml`) builds the next generation's
//! package *N* claiming the state of the package in force *P* (`package --previous`). Before it
//! publishes *N*, this check runs the recorded book of the `code-history` gate
//! (`crates/core-binding/tests/fixtures/code-history`) through the real packages, under the
//! runtime the book runs on, in this process's VOS:
//!
//! 1. **The state *P* recorded, carried by *N*.** The book is founded under *P* and every input of
//!    the generation *P* is of is re-sent to it; at the input the recorded upgrade takes effect,
//!    *N* is installed in place over *P*'s state, by the same stateful upgrade the node takes
//!    (`Runtime::upgrade_actor`, the producer's claim), and every input of *N*'s generation is
//!    re-sent to *N*. Every input must reach its archived log hash (`roll_forward_agent` checks
//!    each), and the switch must have been taken exactly once.
//! 2. **D18 with both packages.** For every way-out input at an upgrade's input in *P*'s
//!    generation, the book is brought to the input before it under *P*, *N* is installed over that
//!    state as if the switch had already been made, and *N* decides the way out: it must reach
//!    the same archived log hash *P* reached.
//!
//! Where the recorded book needs a generation before *P*'s, the check refuses: the packages of
//! those generations are not given (a later release supplies the chain of predecessors). The
//! history is the gate's, not a deployment's archive: the release job holds no credential to read
//! one. The keys are test keys, as every check of this binary uses; the host that drives the
//! packages is this tree's.

use std::collections::BTreeMap;
use std::path::Path;

use core_binding::code_map::CodeMap;
use core_binding::core::{Founding, roll_forward_agent};
use core_binding::envelope::{BlobKey, decode};
use core_binding::fence::{WriterCode, way_out_input};
use core_binding::host::{AgentPort, DRIVER, HostKeys, PortError, ReplicaStores};
use core_binding::hot::{Hot, Limits, RingEntry};
use ledger_core::id::OperationId;
use ledger_core::ledger::Genesis;
use vos::agent::package_admission::AdmittedActorPackage;

use crate::driver::{AgentKeys, Runtime};
use crate::port::VosPort;
use crate::serve::ACTOR_NAME;

/// What the recorded book is founded on: the genesis, its operation reference and time, and the
/// key-ring root of the test keys it runs over (`tests/code_history.rs`'s `Head`).
type Head = (Genesis, OperationId, u64, [u8; 32]);

/// The recorded book of the `code-history` gate.
pub struct History {
    /// What it is founded on.
    pub founding: Founding,
    /// Its archived entries, in order.
    pub entries: Vec<RingEntry>,
    /// The code map they were recorded under.
    pub map: CodeMap,
}

/// Read the recorded book in `dir`.
///
/// # Errors
///
/// A file is missing or does not decode.
pub fn read_history(dir: &Path) -> Result<History, String> {
    let read = |name: &str| {
        std::fs::read(dir.join(name)).map_err(|e| format!("{}: {e}", dir.join(name).display()))
    };
    let (genesis, op, at, root) =
        decode::<Head>(&read("founding.bin")?).map_err(|e| format!("founding.bin: {e}"))?;
    let bytes = read("entries.bin")?;
    let mut entries = Vec::new();
    let mut at_byte = 0usize;
    while at_byte < bytes.len() {
        let head = bytes
            .get(at_byte..at_byte + 4)
            .ok_or("entries.bin: a frame's length is cut")?;
        let len = u32::from_le_bytes(head.try_into().map_err(|_| "entries.bin")?) as usize;
        let body = bytes
            .get(at_byte + 4..at_byte + 4 + len)
            .ok_or("entries.bin: a frame is cut")?;
        entries.push(decode::<RingEntry>(body).map_err(|e| format!("entries.bin: {e}"))?);
        at_byte += 4 + len;
    }
    let map: CodeMap = serde_json::from_slice(&read("code-map.json")?)
        .map_err(|e| format!("code-map.json: {e}"))?;
    Ok(History {
        founding: Founding {
            genesis,
            op,
            at,
            keys: HostKeys::local(root, None),
            limits: Limits::DEFAULT,
            genesis_roots: None,
        },
        entries,
        map,
    })
}

/// What the check runs: how many of the recorded inputs, and where the upgrade takes effect.
#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    /// The recorded inputs re-sent: those of the predecessor's generation and the next.
    pub inputs: usize,
    /// The first input of the next generation: where *N* is installed over *P*'s state.
    pub switch_at: u64,
}

/// Which of the recorded inputs the packages of generations `previous` and `previous + 1` can run.
///
/// # Errors
///
/// The book needs a generation before `previous` (its package is not given), or it records no
/// range of `previous` or of the next generation.
pub fn plan(map: &CodeMap, previous: u64) -> Result<Plan, String> {
    let next = previous + 1;
    if let Some(r) = map.ranges.iter().find(|r| r.generation < previous) {
        return Err(format!(
            "the recorded book's inputs {}..={} ran generation {}, before the code in force's {previous}: \
             the check needs the package of every earlier generation, which is not given",
            r.from, r.to, r.generation
        ));
    }
    if !map.ranges.iter().any(|r| r.generation == previous) {
        return Err(format!(
            "the recorded book has no input of generation {previous}: extend it (T-037-12)"
        ));
    }
    let switch_at = map
        .ranges
        .iter()
        .find(|r| r.generation == next)
        .map(|r| r.from)
        .ok_or_else(|| {
            format!("the recorded book has no input of generation {next}: extend it (T-037-12)")
        })?;
    let last = map
        .ranges
        .iter()
        .filter(|r| r.generation <= next)
        .map(|r| r.to)
        .max()
        .unwrap_or(0);
    Ok(Plan {
        inputs: usize::try_from(last).map_err(|e| e.to_string())?,
        switch_at,
    })
}

/// The packages by generation, and a port over the real runtime that installs, at each switch,
/// the package of the generation the recorded code map names for the input: in place, over the
/// state the one before wrote (`T-037-19`).
struct Upgrading {
    port: VosPort,
    generation_of: BTreeMap<[u8; 32], u64>,
    packages: BTreeMap<u64, AdmittedActorPackage>,
    installed: u64,
    /// Install nothing more: D18's case, where *N* decides an input the map gives *P*.
    pinned: bool,
    /// The generations installed by a switch, in order.
    installs: Vec<u64>,
}

impl Upgrading {
    fn install(&mut self, generation: u64) -> Result<(), String> {
        let package = self
            .packages
            .get(&generation)
            .ok_or_else(|| format!("no package of generation {generation} is given"))?;
        self.port
            .upgrade_actor(package, ACTOR_NAME)
            .map_err(|e| format!("installing generation {generation} in place: {e}"))?;
        self.installed = generation;
        self.installs.push(generation);
        Ok(())
    }
}

impl AgentPort for Upgrading {
    fn invoke(&mut self, message: Vec<u8>, blobs: Vec<Vec<u8>>) -> Result<Vec<u8>, PortError> {
        self.port.invoke(message, blobs)
    }
    fn blob_key(&self, bytes: &[u8]) -> BlobKey {
        self.port.blob_key(bytes)
    }
    fn hot(&mut self) -> Result<Option<Hot>, PortError> {
        self.port.hot()
    }
    fn entry(&mut self, seq: u64) -> Result<Option<RingEntry>, PortError> {
        self.port.entry(seq)
    }
    fn unacknowledged(&self) -> Option<u64> {
        self.port.unacknowledged()
    }
    fn holds_code(&self, code: &[u8; 32]) -> bool {
        self.generation_of
            .get(code)
            .is_some_and(|g| self.packages.contains_key(g))
    }
    fn switch_to(&mut self, code: [u8; 32]) -> Result<(), String> {
        if self.pinned {
            return Ok(());
        }
        let g = *self
            .generation_of
            .get(&code)
            .ok_or("a code version the recorded code map does not name")?;
        if g == self.installed {
            return Ok(());
        }
        self.install(g)
    }
}

/// The Agent's test keys: those of every check of this binary (`main.rs`).
fn keys() -> AgentKeys {
    AgentKeys {
        package: crate::keys::test_key(0x71),
        authority: crate::keys::test_key(0x72),
        discriminator: 0x40,
    }
}

/// A fresh Agent with the package of generation `first` installed, not founded.
fn fresh(
    runtime: &[u8],
    packages: &BTreeMap<u64, AdmittedActorPackage>,
    map: &CodeMap,
    first: u64,
) -> Result<Upgrading, String> {
    let mut rt = Runtime::create(runtime, &keys()).map_err(|e| format!("runtime: {e:?}"))?;
    let package = packages
        .get(&first)
        .ok_or_else(|| format!("no package of generation {first}"))?;
    let actor = rt
        .install(package, ACTOR_NAME, None)
        .map_err(|e| format!("install: {e:?}"))?;
    let mut port = VosPort::new(rt, actor, DRIVER);
    port.keep_samples(false);
    Ok(Upgrading {
        port,
        generation_of: map.versions(),
        packages: packages.clone(),
        installed: first,
        pinned: false,
        installs: Vec::new(),
    })
}

/// `check-upgrade --runtime <pvm> --previous <vos> --package <vos> --history <dir> [--out <file>]`
/// (module documentation). Prints its report as JSON, and writes it to `--out`.
///
/// # Errors
///
/// Why the upgrade must not be published.
pub fn check_upgrade(args: &[String]) -> Result<(), String> {
    let need = |name: &str| -> Result<String, String> {
        let i = args
            .iter()
            .position(|a| a == name)
            .ok_or_else(|| format!("{name} is required"))?;
        args.get(i + 1)
            .filter(|v| !v.starts_with("--"))
            .cloned()
            .ok_or_else(|| format!("{name}: a value is required"))
    };
    let read = |name: &str| -> Result<Vec<u8>, String> {
        let p = need(name)?;
        std::fs::read(&p).map_err(|e| format!("{name} {p}: {e}"))
    };
    let runtime = read("--runtime")?;
    let (previous, package) = (read("--previous")?, read("--package")?);
    let history = read_history(Path::new(&need("--history")?))?;
    let gp = crate::code::package_generation(&previous)?;
    let gn = crate::code::package_generation(&package)?;
    if gp.checked_add(1) != Some(gn) {
        return Err(format!(
            "the package is generation {gn} and the code in force {gp}: an upgrade is the next generation"
        ));
    }
    let admit =
        |b: &[u8], what: &str| crate::package::admit_actor(b).map_err(|e| format!("{what}: {e:?}"));
    let packages: BTreeMap<u64, AdmittedActorPackage> = [
        (gp, admit(&previous, "--previous")?),
        (gn, admit(&package, "--package")?),
    ]
    .into_iter()
    .collect();
    let plan = plan(&history.map, gp)?;
    let entries = &history.entries[..plan.inputs.min(history.entries.len())];
    let started = std::time::Instant::now();

    // 1. P's state, carried by N: the whole of both generations' recorded inputs.
    let port = fresh(&runtime, &packages, &history.map, gp)?;
    let (port, rolled) =
        roll_forward_agent(port, &history.founding, entries, &ReplicaStores::Memory)
            .map_err(|e| format!("the recorded book does not replay through the packages: {e}"))?;
    if port.installs != [gn] {
        return Err(format!(
            "the switches taken were to generations {:?}, not one to generation {gn}",
            port.installs
        ));
    }
    eprintln!(
        "aledgent-core-host: check-upgrade: {} recorded inputs replayed, generation {gp} then {gn} \
         installed in place at input {}",
        rolled.to, plan.switch_at
    );

    // 2. D18: every way out at an upgrade's input in P's generation, decided by N too.
    let mut way_outs = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        if way_out_input(&e.calls, e.deliveries.len()) != Ok(true) {
            continue;
        }
        if history.map.range_of(e.seq).map(|r| r.generation) != Some(gp) {
            continue;
        }
        let port = fresh(&runtime, &packages, &history.map, gp)?;
        let (mut port, _) = roll_forward_agent(
            port,
            &history.founding,
            &entries[..i],
            &ReplicaStores::Memory,
        )
        .map_err(|e| format!("D18, before input {}: {e}", i + 1))?;
        let hot = port
            .hot()
            .map_err(|e| format!("{e:?}"))?
            .ok_or("D18: the Agent is not founded")?;
        if !WriterCode::of(&hot).due(e.seq) {
            continue;
        }
        port.install(gn)?;
        port.pinned = true;
        roll_forward_agent(
            port,
            &history.founding,
            &entries[..=i],
            &ReplicaStores::Memory,
        )
        .map_err(|why| {
            format!(
                "D18: input {} (a way out at an upgrade's input) is not decided by generation {gn} \
                 as generation {gp} decided it: {why}",
                e.seq
            )
        })?;
        way_outs.push(e.seq);
    }
    if way_outs.is_empty() {
        return Err(format!(
            "the recorded book holds no way out at an upgrade's input in generation {gp}: D18 \
             checked nothing"
        ));
    }
    let report = serde_json::json!({
        "previous_generation": gp,
        "generation": gn,
        "inputs": rolled.to,
        "switch_at": plan.switch_at,
        "log_head": hex::encode(rolled.log_head),
        "way_outs_decided_by_both": way_outs,
        "elapsed_ms": started.elapsed().as_millis() as u64,
    });
    let text = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    if let Some(i) = args.iter().position(|a| a == "--out") {
        let p = args.get(i + 1).ok_or("--out: a file is required")?;
        std::fs::write(p, &text).map_err(|e| format!("--out {p}: {e}"))?;
    }
    println!("{text}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_binding::code_map::CodeRange;

    fn map(gens: &[(u64, u64, u64)]) -> CodeMap {
        CodeMap {
            ranges: gens
                .iter()
                .map(|&(from, to, generation)| CodeRange {
                    from,
                    to,
                    code: [generation as u8; 32],
                    generation,
                })
                .collect(),
        }
    }

    fn history() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../crates/core-binding/tests/fixtures/code-history")
    }

    /// `T-037-12`: the check re-sends the inputs of the code in force's generation and the next,
    /// and installs the package at the next one's first input.
    #[test]
    fn t037_12_the_check_runs_the_two_generations_it_has_packages_for() {
        let m = map(&[(1, 14, 0), (15, 34, 1), (35, 40, 2)]);
        assert_eq!(
            plan(&m, 0),
            Ok(Plan {
                inputs: 34,
                switch_at: 15
            })
        );
        // Generation 1's package alone cannot run the inputs of generation 0.
        assert!(
            plan(&m, 1)
                .unwrap_err()
                .contains("before the code in force's 1")
        );
        // Nothing recorded of the next generation: extend the book first.
        assert!(plan(&m, 2).is_err());
    }

    /// `T-037-12`: the recorded book reads back, and holds what the check needs for the first
    /// upgrade (generation 0 to 1).
    #[test]
    fn t037_12_the_recorded_book_reads_back() {
        let h = read_history(&history()).unwrap();
        assert_eq!(h.entries.len() as u64, h.map.ranges.last().unwrap().to);
        let p = plan(&h.map, 0).unwrap();
        assert!(p.switch_at > 1 && p.inputs >= p.switch_as_usize());
    }

    impl Plan {
        fn switch_as_usize(&self) -> usize {
            usize::try_from(self.switch_at).unwrap()
        }
    }
}
