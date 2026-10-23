// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The chainspec genesis on the core host (`F-036`, owner ruling `genesis-chainspec`).
//!
//! Like a Substrate chain's, the book's genesis is a **state**, defined by its chainspec and
//! computed, not reached by executing inputs:
//!
//! - `build-spec --config <file> --chainspec <file> --out <dir>` builds the genesis state natively
//!   (`ledger_core`'s genesis with the chainspec: every chunk through `apply_spec_chunk`, the same
//!   code the Agent runs), with the deployment's key custody (the signer), into a leaf store at
//!   `<dir>/replica/`, and writes `<dir>/genesis.json`: the chainspec's digest and the roots. The
//!   manifest is reviewed before the window, and given to `serve`.
//! - `serve --chainspec <file> [--genesis-manifest <file>]` (and `restore`): the core host rebuilds
//!   the same state in its replica before the Agent's first input, refuses to start unless its roots
//!   are the manifest's, and founds the Agent on them in one invocation (`StateGenesis`). The file is
//!   kept at `<state>/chainspec.bin` and, with `--archive`, in the archive by its digest, so a
//!   restore rebuilds genesis from it (`REQ-RT-3`).
//! - `verify-genesis --config <file> --chainspec <file> --manifest <file> --runtime … --actor …
//!   [--out <report>]`: the second execution (`REQ-RC-6`). A scratch Agent, in memory, founded on
//!   the commitment alone, takes every chunk as an input inside the PVM, and its roots must be the
//!   manifest's. With `--out` it writes its report (`core_binding::verified::Report`), passed or not.
//! - `install-verified --config <file> --state <dir> --report <file> [--genesis-manifest <file>]
//!   [--archive <spec>]`: the operator's step that opens the book (`T-036-25`, rulings `a0022-q2`
//!   and `a0022-q2b`). A chainspec book takes no command until a passing report for its chainspec is
//!   in its state directory: the serving core host reads it at the next command, no restart. The
//!   report is checked against the deployment's genesis (and the manifest's roots, when given)
//!   before it is written; with `--archive` it is also kept beside the chainspec in the archive, so
//!   a restore lays it out again.
//! - `spec-measure`: the chunk-by-chunk path with keys in this process, for its gas.
//!
//! **Memory.** The native genesis streams what it can: each chunk's bytes and path are made when
//! it is applied (the tree's nodes, 64 bytes a chunk, are kept), and the service tree is written in
//! commits of 65,536 leaves. What it holds whole is the book's service store, as the replica holds it
//! when serving: O(records), about 1.3 KB a record (`memory_estimate`). `--max-memory <bytes>` on
//! `build-spec` and `serve` refuses a chainspec above it before anything is built.
//!
//! Before anything is built, the file is checked against genesis: its digest and chunk count are the
//! deployment's `genesis.chainspec`, it is for this genesis's operation reference, and its opening
//! set passes `F-014`'s zero-divergence gate against the deployment's `opening_sources` (`REQ-RC-4`).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use core_binding::core::Founding;
use core_binding::envelope::Roots;
use ingress::boundary::{Core, CoreError, CoreInput, InputBody, InputKey, Outcome};
use ledger_core::chainspec::{ChainSpec, ChunkBody};

/// The progress file of a chunk-by-chunk run, in its directory.
pub const STATUS_FILE: &str = "chainspec.status.json";
/// Where the core host keeps the book's chainspec in its state directory.
pub const STATE_FILE: &str = "chainspec.bin";
/// The manifest `build-spec` writes.
pub const MANIFEST_FILE: &str = "genesis.json";

fn arg(args: &[String], name: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone())
}

/// Read a chainspec file and check it against the genesis `founding` commits (module
/// documentation).
pub fn load(path: &Path, founding: &Founding) -> Result<ChainSpec, String> {
    let shown = path.display();
    let bytes = fs::read(path).map_err(|e| format!("chainspec {shown}: {e}"))?;
    let spec = ChainSpec::from_bytes(&bytes).map_err(|e| format!("chainspec {shown}: {e:?}"))?;
    let committed = founding.genesis.chainspec.ok_or_else(|| {
        "the deployment's genesis commits no chainspec (genesis.chainspec): a book founded without \
         one never takes one (NFR-17)"
            .to_string()
    })?;
    if spec.commitment() != committed {
        return Err(format!(
            "chainspec {shown}: its digest or chunk count is not the one genesis commits"
        ));
    }
    if spec.header().genesis_op != founding.op {
        return Err(format!("chainspec {shown}: built for another genesis"));
    }
    spec.gate(&founding.genesis.lifecycle.opening_sources)
        .map_err(|e| format!("chainspec {shown}: the opening gate refuses it: {e:?}"))?;
    Ok(spec)
}

/// Attach the book's chainspec to `founding` when its genesis commits one: from `--chainspec`, or
/// from `<state>/chainspec.bin` (`state`), which it keeps a copy in; and the manifest's roots from
/// `--genesis-manifest`. Returns the file's bytes, for the archive.
pub fn attach(
    args: &[String],
    founding: &mut Founding,
    state: Option<&Path>,
) -> Result<Option<Vec<u8>>, String> {
    let given = arg(args, "--chainspec").map(PathBuf::from);
    let Some(commitment) = founding.genesis.chainspec else {
        if given.is_some() {
            return Err("--chainspec: the deployment's genesis commits no chainspec".into());
        }
        return Ok(None);
    };
    let kept = state.map(|d| d.join(STATE_FILE));
    let path = match (&given, &kept) {
        (Some(p), _) => p.clone(),
        (None, Some(k)) if k.exists() => k.clone(),
        _ => {
            return Err(format!(
                "this genesis commits chainspec {}: --chainspec <file> is required",
                hex::encode(commitment.digest)
            ));
        }
    };
    let spec = load(&path, founding)?;
    check_memory(args, &spec)?;
    let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if let (Some(k), Some(_)) = (&kept, &given)
        && fs::read(k).ok().as_deref() != Some(bytes.as_slice())
    {
        let tmp = k.with_extension("bin.tmp");
        fs::write(&tmp, &bytes)
            .and_then(|_| fs::rename(&tmp, k))
            .map_err(|e| format!("{}: {e}", k.display()))?;
    }
    if let Some(m) = arg(args, "--genesis-manifest") {
        founding.genesis_roots = Some(manifest_roots(Path::new(&m), &spec)?);
    }
    founding.genesis.spec = Some(Arc::new(spec));
    Ok(Some(bytes))
}

/// The memory a native genesis of `records` chainspec records takes at its peak, in bytes:
/// measured on 2026-10-08 (`F-036`) for a synthetic book and for one twice its size, so a base and a
/// slope, rounded up. The slope is the book's service store,
/// which the replica holds in memory (`ledger_core::state`), at build and when serving alike.
pub fn memory_estimate(records: u64) -> u64 {
    const BASE: u64 = 256 << 20;
    const PER_RECORD: u64 = 1_400;
    BASE + records * PER_RECORD
}

/// `--max-memory <bytes>` (suffixes K, M, G): refuse a chainspec whose native genesis would need
/// more, before anything is built.
pub fn check_memory(args: &[String], spec: &ChainSpec) -> Result<(), String> {
    let Some(v) = arg(args, "--max-memory") else {
        return Ok(());
    };
    let (n, mult) = match v.chars().last() {
        Some('K' | 'k') => (&v[..v.len() - 1], 1u64 << 10),
        Some('M' | 'm') => (&v[..v.len() - 1], 1 << 20),
        Some('G' | 'g') => (&v[..v.len() - 1], 1 << 30),
        _ => (v.as_str(), 1),
    };
    let max = n
        .parse::<u64>()
        .map_err(|_| "--max-memory: bytes, or a number with K, M or G")?
        .saturating_mul(mult);
    let need = memory_estimate(spec.records().len() as u64);
    if need > max {
        return Err(format!(
            "--max-memory: this chainspec's genesis needs about {} MiB, above the {} MiB allowed",
            need >> 20,
            max >> 20
        ));
    }
    Ok(())
}

fn hex32(v: &serde_json::Value, what: &str) -> Result<[u8; 32], String> {
    v.as_str()
        .and_then(|h| hex::decode(h).ok())
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| format!("the manifest's {what}"))
}

/// The roots a manifest states, checked to be of `spec`.
pub fn manifest_roots(path: &Path, spec: &ChainSpec) -> Result<Roots, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let m: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if hex32(&m["chainspec"]["digest"], "chainspec digest")? != spec.digest() {
        return Err(format!(
            "{}: the manifest of another chainspec",
            path.display()
        ));
    }
    Ok(Roots {
        kernel: hex32(&m["roots"]["kernel"], "kernel root")?,
        service: hex32(&m["roots"]["service"], "service root")?,
        audit: hex32(&m["roots"]["audit"], "audit root")?,
    })
}

fn dir_bytes(p: &Path) -> u64 {
    let Ok(rd) = fs::read_dir(p) else {
        return fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    };
    rd.filter_map(Result::ok)
        .map(|e| {
            let p = e.path();
            if p.is_dir() {
                dir_bytes(&p)
            } else {
                fs::metadata(&p).map(|m| m.len()).unwrap_or(0)
            }
        })
        .sum()
}

/// The deployment, the founding and the Agent's keys, from `--config` (the signer for the keys,
/// `F-035`), with the chainspec attached.
fn deployment(args: &[String]) -> Result<(Founding, crate::driver::AgentKeys), String> {
    let path = arg(args, "--config").ok_or("missing --config")?;
    let text = fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
    let config: ingress::config::DeploymentConfig =
        serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
    crate::serve::refuse_keys_in_environment()?;
    let core_keys = config
        .core_keys
        .as_ref()
        .ok_or_else(|| format!("{path}: core_keys is required (F-035): the signer service"))?;
    let (host_keys, keys) = crate::serve::signer_keys(core_keys)?;
    let (genesis, op, at) = config.genesis.agent_genesis()?;
    let mut founding = Founding {
        genesis,
        op,
        at,
        keys: host_keys,
        limits: core_binding::hot::Limits::DEFAULT,
        genesis_roots: None,
    };
    attach(args, &mut founding, None)?;
    if founding.genesis.spec.is_none() {
        return Err("--chainspec <file> is required".into());
    }
    Ok((founding, keys))
}

/// `build-spec --config <file> --chainspec <file> --actor <vos> --runtime <pvm> --out <dir>
/// [--unnamed-code]` (module documentation): refused unless this executable, the package and the
/// runtime are the chainspec's genesis code (`crate::code`).
pub fn build_spec(args: &[String]) -> Result<(), String> {
    let (founding, _) = deployment(args)?;
    let read = |name: &str| -> Result<Option<Vec<u8>>, String> {
        arg(args, name)
            .map(|p| fs::read(&p).map_err(|e| format!("{p}: {e}")))
            .transpose()
    };
    let (actor, runtime) = (read("--actor")?, read("--runtime")?);
    let code = crate::code::check(
        args,
        founding.genesis.spec.as_deref().ok_or("no chainspec")?,
        actor.as_deref(),
        runtime.as_deref(),
        crate::code::Check::Founding,
    )?;
    let out = PathBuf::from(arg(args, "--out").ok_or("missing --out")?);
    let store = out.join("replica");
    if store.exists() {
        return Err(format!(
            "{}: exists; build-spec writes a new store",
            store.display()
        ));
    }
    fs::create_dir_all(&out).map_err(|e| format!("{}: {e}", out.display()))?;
    let spec = founding.genesis.spec.clone().ok_or("no chainspec")?;
    let started = Instant::now();
    let stores = core_binding::host::ReplicaStores::Disk {
        dir: store.clone(),
        cache_bytes: core_binding::host::REPLICA_CACHE_BYTES,
    };
    let built = core_binding::host::build_genesis(
        &stores,
        &founding.keys,
        founding.genesis.clone(),
        founding.op,
        founding.at,
    )
    .map_err(|e| format!("the genesis state: {e:?}"))?;
    let built_ms = started.elapsed().as_millis();
    let next_posting = built.state.next_posting;
    drop(built.replica);
    drop(built.service);
    let c = spec.commitment();
    let manifest = serde_json::json!({
        "chainspec": { "digest": hex::encode(c.digest), "chunks": c.chunks },
        "genesis_op": hex::encode(founding.op.as_bytes()),
        "code": code.map(hex::encode),
        "ids_sha256": spec.header().ids_sha256.map(hex::encode),
        "credentials": spec.header().counts.credentials,
        "records": spec.records().len(),
        "roots": {
            "kernel": hex::encode(built.roots.kernel),
            "service": hex::encode(built.roots.service),
            "audit": hex::encode(built.roots.audit),
        },
        "next_posting": next_posting,
        "built_ms": built_ms,
        "memory_estimate_bytes": memory_estimate(spec.records().len() as u64),
        "leaf_store": { "path": "replica", "bytes": dir_bytes(&store) },
    });
    let text = serde_json::to_string_pretty(&manifest).map_err(|e| e.to_string())?;
    fs::write(out.join(MANIFEST_FILE), &text).map_err(|e| format!("{MANIFEST_FILE}: {e}"))?;
    println!("{text}");
    Ok(())
}

/// `verify-genesis --config <file> --chainspec <file> --manifest <file> --runtime <pvm> --actor
/// <vos> [--batch N] [--segments K] [--engine interpreter|recompiler] [--dir <dir>] [--out <report>]`
/// (module documentation): exit 0 when the chunk-by-chunk Agent ends at the manifest's roots.
/// `--out` writes the report `install-verified` takes, whether it passed or not.
///
/// **The engine.** The PVM runs on the recompiler where VOS has one (x86-64 Linux) and on the
/// interpreter elsewhere, unless `--engine` (or else `F025_BACKEND`) says otherwise. Both are VOS's
/// PVM and run the same program to the same state (`F025_LOCKSTEP`); the recompiler is about seven
/// times faster (`F-036`). Before this the core host's default, the
/// interpreter, applied here too, and a first verify ran on it.
///
/// **Segments** (`--segments K`, default 1). The chunks are cut into `K` runs of about as many
/// records, and each runs in a scratch Agent of its own, in parallel, one child process each
/// (`verify-genesis --segment i/K`, which prints its [`Segment`] as one JSON line: a process keeps
/// each segment's memory its own, and the same command runs a segment on another host). Segment 0
/// is founded on the commitment alone, as the single run is; segment `i > 0` on the state of chunks
/// `0..start_i` applied natively, which is then the host's word, and is accepted only if it is
/// exactly the state segment `i - 1` reached inside the PVM (the hot row's genesis fields,
/// `Hot::genesis_state`). The chain is therefore the chunk-by-chunk execution from the commitment,
/// as strong as one Agent: each segment proves its step from the state the one before it proved.
/// Memory: each segment holds the book up to its last chunk, so about `(K + 1) / 2` times the
/// single run's.
pub fn verify_genesis(args: &[String]) -> Result<(), String> {
    let (mut founding, keys) = deployment(args)?;
    let spec = founding.genesis.spec.take().ok_or("no chainspec")?;
    let manifest = arg(args, "--manifest").ok_or("missing --manifest")?;
    let out = arg(args, "--out").map(PathBuf::from);
    let want = manifest_roots(Path::new(&manifest), &spec)?;
    founding.limits.budget = core_binding::hot::MAX_BUDGET;
    let batch = arg(args, "--batch")
        .and_then(|b| b.parse::<usize>().ok())
        .unwrap_or(ingress::service::DEFAULT_CORE_BATCH)
        .max(1);
    let segments = match arg(args, "--segments") {
        None => 1,
        Some(k) => k
            .parse::<u32>()
            .ok()
            .filter(|k| *k >= 1)
            .ok_or("--segments: a number, 1 or more")?,
    };
    let engine = match arg(args, "--engine") {
        Some(name) => crate::driver::engine_named(&name)?,
        None => match std::env::var("F025_BACKEND").as_deref() {
            Ok(name) => crate::driver::engine_named(name)?,
            Err(_) if crate::driver::RECOMPILER_AVAILABLE => vos_pvm::PvmBackend::ForceRecompiler,
            Err(_) => vos_pvm::PvmBackend::ForceInterpreter,
        },
    };
    crate::driver::set_engine(engine);
    let runtime = fs::read(arg(args, "--runtime").ok_or("missing --runtime")?)
        .map_err(|e| format!("runtime: {e}"))?;
    let actor = fs::read(arg(args, "--actor").ok_or("missing --actor")?)
        .map_err(|e| format!("actor: {e}"))?;
    crate::code::check(
        args,
        &spec,
        Some(&actor),
        Some(&runtime),
        crate::code::Check::Agent,
    )?;
    let dir = PathBuf::from(arg(args, "--dir").unwrap_or_else(|| ".".into()));
    eprintln!(
        "aledgent-core-host: verify-genesis: {} chunks, {} records, engine {}, {segments} \
         segment(s)",
        spec.chunk_count(),
        spec.records().len(),
        crate::driver::engine_name(engine)
    );
    let started = Instant::now();
    let scratch = Scratch {
        runtime: &runtime,
        actor: &actor,
        keys: &keys,
        founding: &founding,
        batch,
        dir: &dir,
    };
    if let Some(which) = arg(args, "--segment") {
        // A child of `--segments K` (or a run of one segment by hand): one segment, as JSON.
        let (i, k) = which
            .split_once('/')
            .and_then(|(i, k)| Some((i.parse::<u32>().ok()?, k.parse::<u32>().ok()?)))
            .filter(|(i, k)| i < k)
            .ok_or("--segment: i/K, with i below K")?;
        let range = cuts(&spec, k)
            .get(i as usize)
            .cloned()
            .ok_or("--segment: the chainspec has fewer chunks than segments")?;
        let seg = run_segment(
            &scratch,
            &spec,
            &Inputs::of(&spec),
            &chunk_records(&spec),
            range,
            None,
        )?;
        println!(
            "{}",
            serde_json::to_string(&seg).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    let run = verify_segments(&scratch, &spec, segments, args)?;
    let millis = started.elapsed().as_millis();
    if run.segments.len() > 1 {
        // The chain, for the operator's record: each boundary's state, as both sides hold it.
        for (i, seg) in run.segments.iter().enumerate() {
            let r = seg.reached.roots();
            eprintln!(
                "aledgent-core-host: segment {i} chunks {}..{} founded at service {} reached \
                 kernel {} service {} audit {}{}",
                seg.chunks.start,
                seg.chunks.end,
                hex::encode(seg.founded.service_root),
                hex::encode(r.kernel),
                hex::encode(r.service),
                hex::encode(r.audit),
                match run.segments.get(i + 1) {
                    Some(next) if next.founded == seg.reached => ": the next segment starts here",
                    Some(_) => ": NOT the next segment's founding state",
                    None => "",
                }
            );
        }
    }
    let ok = run.chained && run.sealed && run.roots == want;
    let verified = core_binding::verified::Report::new(
        &spec.commitment(),
        &run.roots,
        ok,
        u64::from(spec.chunk_count()),
        spec.records().len() as u64,
        u64::try_from(millis).unwrap_or(u64::MAX),
    );
    let text = verified.to_json();
    if let Some(out) = &out {
        fs::write(out, &text).map_err(|e| format!("{}: {e}", out.display()))?;
    }
    println!("{text}");
    if ok {
        Ok(())
    } else if !run.chained {
        Err(
            "verify-genesis: a segment was founded on a state the segment before it did not \
             reach inside the Agent"
                .into(),
        )
    } else {
        Err(
            "verify-genesis: the chainspec, executed inside the Agent, does not reach the \
             manifest's roots"
                .into(),
        )
    }
}

/// What every scratch Agent of a `verify-genesis` is made from.
pub struct Scratch<'a> {
    /// The runtime's PVM.
    pub runtime: &'a [u8],
    /// The core's package.
    pub actor: &'a [u8],
    /// The Agent's keys.
    pub keys: &'a crate::driver::AgentKeys,
    /// The founding, on the chainspec's commitment alone (`genesis.spec` is `None`).
    pub founding: &'a Founding,
    /// Inputs an invocation, at most.
    pub batch: usize,
    /// Where the progress file goes.
    pub dir: &'a Path,
}

/// What a segmented run came to ([`verify_segments`]).
#[derive(Clone, Debug)]
pub struct SegmentedRun {
    /// Every segment's founding state was the state the segment before it reached.
    pub chained: bool,
    /// The last chunk sealed the chainspec.
    pub sealed: bool,
    /// The roots the last segment's Agent reached.
    pub roots: Roots,
    /// Each segment: its chunks, and the state it was founded on and reached.
    pub segments: Vec<Segment>,
}

/// One segment of a `verify-genesis`: what a `--segment i/K` child prints.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Segment {
    /// Its chunks.
    pub chunks: std::ops::Range<u32>,
    /// The Agent's state when founded.
    pub founded: core_binding::envelope::GenesisState,
    /// The Agent's state after its last chunk.
    pub reached: core_binding::envelope::GenesisState,
    /// Its records.
    pub records: u64,
    /// Milliseconds, founding included.
    pub millis: u64,
}

/// The chunk ranges of `segments` runs of about as many records each, in order, none empty.
pub fn cuts(spec: &ChainSpec, segments: u32) -> Vec<std::ops::Range<u32>> {
    cuts_of(&chunk_records(spec), segments)
}

/// [`cuts`], over each chunk's record count: a run starts at the first chunk at which the records
/// before it reach its share; never more runs than chunks, the header chunk always in the first.
pub fn cuts_of(sizes: &[u64], segments: u32) -> Vec<std::ops::Range<u32>> {
    let n = sizes.len() as u32;
    let total: u64 = sizes.iter().sum::<u64>().max(1);
    let k = u64::from(segments.clamp(1, n.max(1)));
    let mut starts = vec![0u32];
    let mut acc = 0u64;
    for (i, size) in sizes.iter().enumerate() {
        let next = starts.len() as u64;
        if next < k && i > 0 && acc >= total * next / k {
            starts.push(i as u32);
        }
        acc += size;
    }
    starts
        .iter()
        .zip(starts.iter().skip(1).chain(core::iter::once(&n)))
        .map(|(a, b)| *a..*b)
        .collect()
}

fn chunk_records(spec: &ChainSpec) -> Vec<u64> {
    (0..spec.chunk_count())
        .map(|i| match spec.chunk(i).map(|c| c.body) {
            Some(ChunkBody::Records(r)) => r.len() as u64,
            _ => 0,
        })
        .collect()
}

/// Run `spec` chunk by chunk in `segments` scratch Agents in parallel ([`verify_genesis`]'s
/// documentation), and check that they chain. One segment runs here; more run as children of
/// this executable, each `verify-genesis` with `args` and `--segment i/K`.
pub fn verify_segments(
    scratch: &Scratch<'_>,
    spec: &Arc<ChainSpec>,
    segments: u32,
    args: &[String],
) -> Result<SegmentedRun, String> {
    let ranges = cuts(spec, segments);
    let segments = if ranges.len() == 1 {
        let inputs = Inputs::of(spec);
        let sizes = chunk_records(spec);
        vec![run_segment(
            scratch,
            spec,
            &inputs,
            &sizes,
            ranges[0].clone(),
            Some(()),
        )?]
    } else {
        run_children(args, segments, ranges.len() as u32)?
    };
    for (seg, range) in segments.iter().zip(&ranges) {
        if seg.chunks != *range {
            return Err(format!(
                "a segment ran chunks {:?}, not {range:?}",
                seg.chunks
            ));
        }
    }
    let last = segments.last().ok_or("no segment")?;
    Ok(SegmentedRun {
        chained: chained(&segments),
        sealed: last.chunks.end == spec.chunk_count(),
        roots: last.reached.roots(),
        segments,
    })
}

/// Whether `segments` chain: each one's chunks start where the one before it ended, and it was
/// founded on exactly the state that one reached inside the PVM. The first starts at chunk 0, on
/// the commitment alone.
pub fn chained(segments: &[Segment]) -> bool {
    segments.first().is_some_and(|s| s.chunks.start == 0)
        && segments
            .windows(2)
            .all(|w| w[0].chunks.end == w[1].chunks.start && w[0].reached == w[1].founded)
}

/// `runs` children `verify-genesis … --segment i/K`, in parallel, `K` the segments asked for (which
/// [`cuts`] makes into `runs`); each one's [`Segment`], in order.
fn run_children(args: &[String], k: u32, runs: u32) -> Result<Vec<Segment>, String> {
    let exe = std::env::current_exe().map_err(|e| format!("this executable: {e}"))?;
    // The parent's own arguments, without what only the parent does.
    let mut passed = Vec::new();
    let mut it = args.iter().skip(1);
    while let Some(a) = it.next() {
        if matches!(a.as_str(), "--segments" | "--out" | "--segment") {
            it.next();
            continue;
        }
        passed.push(a.clone());
    }
    let children = (0..runs)
        .map(|i| {
            std::process::Command::new(&exe)
                .args(&passed)
                .arg("--segment")
                .arg(format!("{i}/{k}"))
                .stdout(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| format!("segment {i}: {e}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut out = Vec::new();
    let mut failed = None;
    for (i, child) in children.into_iter().enumerate() {
        let done = child
            .wait_with_output()
            .map_err(|e| format!("segment {i}: {e}"))?;
        let line = String::from_utf8_lossy(&done.stdout);
        match serde_json::from_str::<Segment>(line.trim()) {
            Ok(seg) if done.status.success() => {
                eprintln!(
                    "aledgent-core-host: segment {i}: chunks {}..{}, {} records, {} ms",
                    seg.chunks.start, seg.chunks.end, seg.records, seg.millis
                );
                out.push(seg);
            }
            _ => {
                failed.get_or_insert(format!("segment {i} failed: {}", done.status));
            }
        }
    }
    match failed {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

/// One segment: a scratch Agent founded at `range.start` (on the commitment alone at 0), taking
/// chunks `range` as inputs. `status` writes the progress file (the single run's).
fn run_segment(
    scratch: &Scratch<'_>,
    spec: &Arc<ChainSpec>,
    inputs: &Inputs,
    sizes: &[u64],
    range: std::ops::Range<u32>,
    status: Option<()>,
) -> Result<Segment, String> {
    use core_binding::core::{AgentCore, MemLog};
    use core_binding::host::{BatchLimits, ReplicaStores};
    let started = Instant::now();
    let pkg =
        crate::package::admit_actor(scratch.actor).map_err(|e| format!("actor package: {e:?}"))?;
    let mut rt = crate::driver::Runtime::create(scratch.runtime, scratch.keys)
        .map_err(|e| format!("runtime: {e:?}"))?;
    let installed = rt
        .install(&pkg, crate::serve::ACTOR_NAME, None)
        .map_err(|e| format!("install: {e:?}"))?;
    let mut port = crate::port::VosPort::new(rt, installed, core_binding::host::DRIVER);
    port.keep_samples(false);
    let founding = scratch.founding.clone();
    let core = if range.start == 0 {
        AgentCore::open_in(port, founding, MemLog::default(), ReplicaStores::Memory)?
    } else {
        AgentCore::open_segment(
            port,
            founding,
            MemLog::default(),
            ReplicaStores::Memory,
            spec,
            range.start,
        )?
    };
    core.set_batch_limits(
        BatchLimits::with_inputs(scratch.batch).with_actor_gas(crate::driver::actor_gas()),
    );
    let hot = |core: &AgentCore<crate::port::VosPort>| {
        core.with_port(core_binding::host::AgentPort::hot)
            .and_then(Result::ok)
            .flatten()
            .ok_or_else(|| "the scratch Agent holds no state".to_string())
    };
    let founded = hot(&core)?.genesis_state();
    let report = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(apply_range(
            &core,
            inputs,
            sizes,
            range.clone(),
            scratch.batch,
            status.map(|()| scratch.dir),
            &|| core.stopped(),
        ))?;
    if range.end == spec.chunk_count() && !report.sealed {
        return Err("the last chunk did not seal the chainspec".into());
    }
    Ok(Segment {
        chunks: range,
        founded,
        reached: hot(&core)?.genesis_state(),
        records: report.records,
        millis: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

/// `install-verified --config <file> --state <dir> --report <file> [--genesis-manifest <file>]
/// [--archive <spec>]` (module documentation, `T-036-25`).
pub fn install_verified(args: &[String]) -> Result<(), String> {
    let path = arg(args, "--config").ok_or("missing --config")?;
    let text = fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
    let config: ingress::config::DeploymentConfig =
        serde_json::from_str(&text).map_err(|e| format!("{path}: {e}"))?;
    let commitment = config.genesis.agent_genesis()?.0.chainspec.ok_or_else(|| {
        format!("{path}: the deployment's genesis commits no chainspec: there is nothing to verify")
    })?;
    let state = PathBuf::from(arg(args, "--state").ok_or("missing --state")?);
    let report_path = arg(args, "--report").ok_or("missing --report")?;
    let report = fs::read(&report_path).map_err(|e| format!("{report_path}: {e}"))?;
    let roots = match arg(args, "--genesis-manifest") {
        Some(m) => Some(core_binding::verified::manifest_roots(
            &fs::read(&m).map_err(|e| format!("{m}: {e}"))?,
            &commitment,
        )?),
        None => None,
    };
    let installed = core_binding::verified::install(&report, &state, &commitment, roots.as_ref())?;
    if let Some(spec) = arg(args, "--archive") {
        let archive = core_archive::open(&spec, core_archive::Access::Write)?;
        let digest = hex::encode(commitment.digest);
        let bytes = installed.to_json().into_bytes();
        let kept = archive
            .get_verified(&digest)
            .map_err(|e| format!("the archive's verify-genesis report: {e}"))?;
        match kept {
            // Write-once: an earlier passing report of this chainspec is kept; restores lay it out.
            Some(old) if old != bytes => eprintln!(
                "aledgent-core-host: the archive keeps an earlier verify-genesis report of this \
                 chainspec; it is left as it is"
            ),
            Some(_) => {}
            None => {
                archive
                    .put_verified(&digest, &bytes)
                    .map_err(|e| format!("the archive's verify-genesis report: {e}"))?;
            }
        }
    }
    eprintln!(
        "aledgent-core-host: verify-genesis report for chainspec {} installed at {}: the book \
         takes commands from its next one",
        installed.chainspec.digest,
        state.join(core_binding::verified::VERIFIED_FILE).display()
    );
    Ok(())
}

/// Chunk `index`'s input key: the chainspec digest's first twelve bytes, then the index.
pub fn key(digest: &[u8; 32], index: u32) -> InputKey {
    let mut k = [0u8; 16];
    k[..12].copy_from_slice(&digest[..12]);
    k[12..].copy_from_slice(&index.to_be_bytes());
    InputKey(k)
}

/// The chainspec's chunks as inputs, built one at a time: the chunks' bytes and paths are computed once,
/// and an input's hex only when it is sent.
pub struct Inputs {
    digest: [u8; 32],
    bytes: Vec<Vec<u8>>,
    paths: Vec<Vec<[u8; 32]>>,
}

impl Inputs {
    /// The inputs of `spec`.
    pub fn of(spec: &ChainSpec) -> Inputs {
        Inputs {
            digest: spec.digest(),
            bytes: spec.chunk_bytes(),
            paths: spec.paths(),
        }
    }

    /// Chunk `i`'s input.
    pub fn get(&self, i: usize) -> CoreInput {
        CoreInput {
            key: key(&self.digest, i as u32),
            body: InputBody::SpecChunk {
                chunk: hex::encode(&self.bytes[i]),
                path: self.paths[i].iter().map(hex::encode).collect(),
            },
        }
    }
}

/// What a chunk-by-chunk run came to.
#[derive(Clone, Copy, Debug, Default)]
pub struct Report {
    /// Chunks the chainspec has.
    pub chunks: u32,
    /// Records it has.
    pub records: u64,
    /// Whether the last chunk sealed it.
    pub sealed: bool,
    /// Milliseconds from the first chunk sent to the seal.
    pub millis: u128,
}

fn status(dir: &Path, applied: u32, chunks: u32, records_per_s: u128, sealed: bool) {
    let body = serde_json::json!({
        "applied": applied,
        "chunks": chunks,
        "records_per_s": records_per_s,
        "sealed": sealed,
    });
    let tmp = dir.join(format!("{STATUS_FILE}.tmp"));
    if fs::write(&tmp, body.to_string()).is_ok() {
        let _ = fs::rename(&tmp, dir.join(STATUS_FILE));
    }
}

/// Apply `spec` chunk by chunk through `core` (a book founded on its commitment alone), `batch`
/// inputs a call, retrying an unavailable core, and failing once `stopped` says it stopped
/// ([`apply_range`]).
pub async fn apply<C: Core>(
    core: &C,
    spec: &ChainSpec,
    batch: usize,
    dir: &Path,
    stopped: &dyn Fn() -> Option<String>,
) -> Result<Report, String> {
    let all = Inputs::of(spec);
    let sizes = chunk_records(spec);
    apply_range(
        core,
        &all,
        &sizes,
        0..spec.chunk_count(),
        batch,
        Some(dir),
        stopped,
    )
    .await
}

/// Apply chunks `range` of `all` through `core`, which stands at `range.start`, `batch` inputs a
/// call, retrying an unavailable core; with `dir`, keeping the progress file there. `stopped`
/// says whether the core stopped for good (`AgentCore::stopped`): then the run fails with why,
/// instead of retrying an input that will never be taken (the segment that spun on 2026-10-11).
pub async fn apply_range<C: Core>(
    core: &C,
    all: &Inputs,
    sizes: &[u64],
    range: std::ops::Range<u32>,
    batch: usize,
    dir: Option<&Path>,
    stopped: &dyn Fn() -> Option<String>,
) -> Result<Report, String> {
    let chunks = range.end - range.start;
    let records: u64 = sizes[range.start as usize..range.end as usize].iter().sum();
    let started = Instant::now();
    let mut at = range.start as usize;
    let end_at = range.end as usize;
    let mut done_records = 0u64;
    let mut sealed = false;
    let mut last_print = Instant::now();
    while at < end_at {
        let end = (at + batch.max(1)).min(end_at);
        let batch_inputs: Vec<CoreInput> = (at..end).map(|i| all.get(i)).collect();
        let replies = core.apply_batch(&batch_inputs).await;
        let mut advanced = 0usize;
        for (i, r) in replies.into_iter().enumerate() {
            match r {
                Ok(reply) => match reply.outcome {
                    Outcome::SpecApplied { sealed: s, .. } => {
                        sealed = s;
                        done_records += sizes[at + i];
                        advanced += 1;
                    }
                    Outcome::Refused { error, .. } => {
                        return Err(format!(
                            "chainspec chunk {} refused: {} (see the core's log)",
                            at + i,
                            error.code()
                        ));
                    }
                    other => return Err(format!("chainspec chunk {}: {other:?}", at + i)),
                },
                Err(CoreError::Unavailable | CoreError::Ambiguous | CoreError::NotRun) => break,
            }
        }
        if advanced == 0 {
            if let Some(why) = stopped() {
                return Err(format!(
                    "chainspec chunk {at}: the core stopped and takes nothing more: {why}"
                ));
            }
            // The core could not take the chunk now: wait, then re-send under the same key.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            continue;
        }
        at += advanced;
        let us = started.elapsed().as_micros().max(1);
        let rate = u128::from(done_records) * 1_000_000 / us;
        if let Some(dir) = dir {
            status(dir, at as u32, range.end, rate, sealed);
        }
        if last_print.elapsed().as_secs() >= 10 || at == end_at {
            eprintln!(
                "aledgent-core-host: chainspec chunks {}..{at} of {}..{}, {done_records}/{records} \
                 records, {rate} records/s",
                range.start, range.start, range.end
            );
            last_print = Instant::now();
        }
    }
    Ok(Report {
        chunks,
        records,
        sealed,
        millis: started.elapsed().as_millis(),
    })
}

/// `spec-measure <runtime.pvm> <actor.vos> <chainspec.bin> <out.csv> [--batch N]`: apply a
/// chainspec chunk by chunk through the core Agent in VOS, as `verify-genesis` does, with keys held
/// in this process instead of the signer's, and record every invocation's gas and time (`F-036`
/// T-036-10). Gas does not depend on the machine; time does: on a host
/// without VOS's recompiler (anything but x86-64 Linux) the PVM is interpreted.
pub fn measure(args: &[String]) -> Result<(), String> {
    use core_binding::core::{AgentCore, MemLog};
    use core_binding::host::{BatchLimits, HostKeys, ReplicaStores};
    use core_binding::hot::{Limits, MAX_BUDGET};
    use ledger_core::ledger::Genesis;
    use ledger_core::lifecycle::LifecycleConfig;
    use ledger_core::principal::Verifier;
    use std::io::Write as _;

    let (runtime, actor, spec_path, out) = (&args[2], &args[3], &args[4], &args[5]);
    let batch = args
        .windows(2)
        .find(|w| w[0] == "--batch")
        .and_then(|w| w[1].parse::<usize>().ok())
        .unwrap_or(ingress::service::DEFAULT_CORE_BATCH)
        .max(1);
    let spec =
        ChainSpec::from_bytes(&fs::read(spec_path).map_err(|e| format!("{spec_path}: {e}"))?)
            .map_err(|e| format!("{spec_path}: {e:?}"))?;
    let h = spec.header();
    let founding = Founding {
        genesis: Genesis {
            assets: h.assets.clone(),
            operator_verifier: Verifier::new(vec![0x0F; 32]),
            lifecycle: LifecycleConfig::new([0x36; 16], 60_000, 3_600_000),
            chainspec: Some(spec.commitment()),
            spec: None,
        },
        op: h.genesis_op,
        at: h.at,
        keys: HostKeys::local([0xC5; 32], None),
        limits: Limits {
            budget: MAX_BUDGET,
            ..Limits::DEFAULT
        },
        genesis_roots: None,
    };
    let runtime = fs::read(runtime).map_err(|e| format!("runtime: {e}"))?;
    let pkg = crate::package::admit_actor(&fs::read(actor).map_err(|e| format!("actor: {e}"))?)
        .map_err(|e| format!("actor package: {e:?}"))?;
    let keys = crate::driver::AgentKeys {
        package: crate::keys::test_key(0x71),
        authority: crate::keys::test_key(0x72),
        discriminator: 0x40,
    };
    let mut rt =
        crate::driver::Runtime::create(&runtime, &keys).map_err(|e| format!("runtime: {e:?}"))?;
    let installed = rt
        .install(&pkg, crate::serve::ACTOR_NAME, None)
        .map_err(|e| format!("install: {e:?}"))?;
    let port = crate::port::VosPort::new(rt, installed, core_binding::host::DRIVER);
    let core = AgentCore::open_in(port, founding, MemLog::default(), ReplicaStores::Memory)?;
    core.set_batch_limits(
        BatchLimits::with_inputs(batch).with_actor_gas(crate::driver::actor_gas()),
    );
    let dir = Path::new(out).parent().unwrap_or(Path::new("."));
    let report = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(apply(&core, &spec, batch, dir, &|| core.stopped()))?;
    let samples = core
        .with_port(|p| std::mem::take(&mut p.samples))
        .unwrap_or_default();
    let mut f = fs::File::create(out).map_err(|e| format!("{out}: {e}"))?;
    writeln!(
        f,
        "invocation,actor_gas,invoke_gas,execution_us,caller_us,message_bytes,blob_bytes,\
         encode_us,preparation_us,decode_us,ack_gas,ack_us"
    )
    .map_err(|e| e.to_string())?;
    let (mut actor_gas, mut invoke_gas, mut exec_us) = (0u128, 0u128, 0u128);
    for (i, s) in samples.iter().enumerate() {
        writeln!(
            f,
            "{i},{},{},{},{},{},{},{},{},{},{},{}",
            s.actor_gas,
            s.invoke.gas_used,
            s.invoke.execution_us,
            s.caller_us,
            s.message_bytes,
            s.blob_bytes,
            s.invoke.encode_us,
            s.invoke.preparation_us,
            s.invoke.decode_us,
            s.ack.gas_used,
            s.ack.elapsed_us
        )
        .map_err(|e| e.to_string())?;
        actor_gas += u128::from(s.actor_gas);
        invoke_gas += u128::from(s.invoke.gas_used);
        exec_us += u128::from(s.invoke.execution_us);
    }
    let records = report.records.max(1) as u128;
    println!(
        "{}",
        serde_json::json!({
            "chunks": report.chunks,
            "records": report.records,
            "sealed": report.sealed,
            "millis": report.millis,
            "records_per_s": u128::from(report.records) * 1000 / report.millis.max(1),
            "invocations": samples.len(),
            "actor_gas": actor_gas.to_string(),
            "invoke_gas": invoke_gas.to_string(),
            "actor_gas_per_record": (actor_gas / records).to_string(),
            "invoke_gas_per_record": (invoke_gas / records).to_string(),
            "execution_ms": exec_us / 1000,
            "batch": batch,
        })
    );
    Ok(())
}

/// The grown state the stress benches found their books on (`shards --chainspec`, `fleet
/// --chainspec`; owner's request of 2026-10-08): every ledger account of a chainspec, as the shard
/// Agents name accounts (the account identifier's sixteen bytes), with the sum of its opening
/// balances, in minor units. The shard a ledger account lives in is the router's own rule:
/// `Directory::shard_of`, the partition of the account (`ledger_kernel::partition::Partition::of`,
/// `REQ-RT-9`), then the shard holding that partition's range. `max` caps how many are taken, in
/// chainspec order.
///
/// Trade-off, stated plainly: a shard Agent keeps its book in its own rows (layout 3, `F-031`), so
/// a grown book cannot be installed in it from roots as the core Agent's is (`StateGenesis`): its
/// rows are its state. The benches therefore open and fund these accounts through the principals
/// Agent and the shards, as bulk set-up before the timed phases, and VOS's Local Agent image limit
/// bounds how many each shard can hold until external state (`DEP-11`).
pub fn ballast(path: &str, max: Option<u64>) -> Result<Vec<([u8; 16], u64)>, String> {
    use ledger_core::chainspec::SpecRecord;
    let spec = ChainSpec::from_bytes(&fs::read(path).map_err(|e| format!("{path}: {e}"))?)
        .map_err(|e| format!("{path}: {e:?}"))?;
    let g = spec.header().genesis_op;
    let mut amounts: std::collections::BTreeMap<[u8; 16], u64> = std::collections::BTreeMap::new();
    for r in spec.records() {
        if let SpecRecord::Opening(row) = r {
            *amounts.entry(*row.account.as_bytes()).or_default() += row.amount.get();
        }
    }
    let n = spec.header().counts.accounts;
    let take = max.map_or(n, |m| m.min(n));
    Ok((0..take)
        .map(|i| {
            let id = *ledger_core::chainspec::account_of(&g, i).as_bytes();
            (id, amounts.get(&id).copied().unwrap_or(0))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_binding::envelope::GenesisState;

    #[test]
    fn f_036_segments_cut_by_records_and_cover_every_chunk() {
        // The header chunk (no records), then chunks of six and some of one.
        let mut sizes = vec![0u64];
        sizes.extend(std::iter::repeat_n(6, 30));
        sizes.extend(std::iter::repeat_n(1, 60));
        for k in [1u32, 2, 3, 4, 7, 91, 500] {
            let r = cuts_of(&sizes, k);
            assert!(!r.is_empty() && r.len() as u32 <= k, "k = {k}");
            if k <= 7 {
                assert_eq!(r.len() as u32, k);
            }
            assert_eq!(r.first().unwrap().start, 0);
            assert_eq!(r.last().unwrap().end, sizes.len() as u32);
            assert!(
                r.windows(2)
                    .all(|w| w[0].end == w[1].start && w[0].start < w[0].end)
            );
        }
        // Two runs of about 120 records each: the first ends inside the chunks of six.
        let two = cuts_of(&sizes, 2);
        let first: u64 = sizes[two[0].start as usize..two[0].end as usize]
            .iter()
            .sum();
        assert!((120..=126).contains(&first), "{first}");
        assert_eq!(cuts_of(&[0], 4), vec![0..1]);
    }

    fn state(n: u8) -> GenesisState {
        GenesisState {
            subroots: [[n; 32]; 6],
            last_commit: Some(u64::from(n)),
            next_posting: u64::from(n),
            service_root: [n; 32],
            audit_emitted: u64::from(n),
            audit_head: [n; 32],
            audit_shipped: None,
            notified: 0,
        }
    }

    fn seg(chunks: std::ops::Range<u32>, founded: u8, reached: u8) -> Segment {
        Segment {
            chunks,
            founded: state(founded),
            reached: state(reached),
            records: 1,
            millis: 1,
        }
    }

    /// `REQ-RC-6`: the segments chain only if each is founded on the state the one before it
    /// reached inside the PVM, over consecutive chunks from chunk 0.
    #[test]
    fn req_rc_6_segments_chain_only_on_the_state_reached_before() {
        assert!(chained(&[
            seg(0..5, 0, 1),
            seg(5..9, 1, 2),
            seg(9..12, 2, 3)
        ]));
        assert!(chained(&[seg(0..12, 0, 3)]));
        // A founding state the segment before did not reach.
        assert!(!chained(&[seg(0..5, 0, 1), seg(5..9, 7, 2)]));
        // A next-posting counter alone differs: not the same state.
        let mut off = seg(5..9, 1, 2);
        off.founded.next_posting += 1;
        assert!(!chained(&[seg(0..5, 0, 1), off]));
        // A gap, an overlap, or a chain that does not start at chunk 0.
        assert!(!chained(&[seg(0..5, 0, 1), seg(6..9, 1, 2)]));
        assert!(!chained(&[seg(0..5, 0, 1), seg(4..9, 1, 2)]));
        assert!(!chained(&[seg(1..5, 0, 1)]));
        assert!(!chained(&[]));
    }
}
