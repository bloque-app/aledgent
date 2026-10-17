// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The genesis code (`F-036`, with amendment 0023's manifest, PR #201): a chainspec's header names
//! the code its genesis is built and run under, and the core host refuses to build or found a book
//! under other binaries.
//!
//! - `code-manifest --actor <vos> --runtime <pvm> --profile <json> --out <file>` writes the code
//!   manifest (`ledger_core::code`, magic `aledgent/code/v1`) of this host, this package and this
//!   runtime: this host's platform ([`PLATFORM`] at [`PLATFORM_REVISION`], the VOS it links), the
//!   code generation the package carries compiled in (`ledger_core::code::generation_of`; a
//!   `generation` in `--profile` must be the same), and each artefact's unkeyed blake2b-256
//!   digest, length and identities on the platform (VOS's `ProgramId`, [`VOS_PROGRAM_ID`]), with
//!   the build profiles the release pipeline gives in `--profile`. It prints the code version
//!   *H₀*. `migrate chainspec build --code <file>` puts it in the header.
//! - The check ([`check`]): the manifest's platform must be this host's, every artefact the
//!   manifest names must be the bytes this process runs, and the package must carry the
//!   manifest's code generation compiled in (amendment 0033, D1, D13). The package is `--actor`,
//!   the runtime `--runtime`, and `native-decide` is this executable, which decides the genesis
//!   natively. The construction is this build's.
//!
//! Where it holds:
//!
//! | Command | Package and runtime | This executable |
//! |---|---|---|
//! | `build-spec` | checked | checked |
//! | `serve` and `restore`, founding (no replica yet) | checked | checked |
//! | `serve`, restart (a replica) | checked: no upgrade exists yet, so the Agent still runs *H₀* | not checked: its one decision, the genesis, is checked against the manifest's roots and the Agent's |
//! | `verify-genesis` | checked | not checked: the genesis is executed inside the PVM |
//! | `check-code` ([`check_code`]) | checked, against a chainspec or a manifest file | not checked: a core image on a deployment's genesis bytes carries a newer host (`DEC-27` (i)) |
//!
//! `check-code` is what builds a core image with the current host and a deployment's genesis package
//! and runtime, copied from its pinned image (`deploy/docker/core.Dockerfile`, target
//! `core-on-genesis`; `F-038` §12 step 0): the build fails unless the copied bytes are the ones the
//! genesis code names.
//!
//! A chainspec whose header names no code is refused unless `--unnamed-code` is given: tests and
//! stress books, never a deployment's.

use std::fs;
use std::path::Path;

use ledger_core::chainspec::ChainSpec;
use ledger_core::code::{
    Artefact, BuildProfile, CodeManifest, Identity, KernelIdentity, Platform, Role, WriterKind,
    artefact_digest, generation_of,
};
use vos::agent_sdk::ProgramId;

/// The platform this host drives a writer on (`DEC-26` (a), (i)): VOS's `saga/agents` line.
pub const PLATFORM: &str = "vos-saga";

/// The revision of [`PLATFORM`] this host links: the VOS commit `agents/Cargo.toml` pins (a test
/// keeps the two equal). A manifest naming another revision is another platform's code.
pub const PLATFORM_REVISION: &str = "c54fc1ff13fcd55b86f2cf4edc77b041b62e245f";

/// The identity kind of VOS's `ProgramId` of a program, on [`PLATFORM`].
pub const VOS_PROGRAM_ID: &str = "vos-program-id";

/// This host's platform, as a manifest names it.
pub fn platform() -> Platform {
    Platform {
        name: PLATFORM.into(),
        revision: PLATFORM_REVISION.into(),
        identities: Vec::new(),
    }
}

/// Refuse a manifest whose platform is not this host's.
pub fn check_platform(manifest: &CodeManifest) -> Result<(), String> {
    let p = &manifest.platform;
    if p.name != PLATFORM || p.revision != PLATFORM_REVISION {
        return Err(format!(
            "the code runs on {}@{}, this host drives {PLATFORM}@{PLATFORM_REVISION}: another \
             platform's code is an upgrade (REQ-RT-16, REQ-RT-19)",
            p.name, p.revision
        ));
    }
    Ok(())
}

/// The code generation `actor` carries compiled in (`ledger_core::code::generation_of`).
pub fn package_generation(actor: &[u8]) -> Result<u64, String> {
    generation_of(actor).map_err(|e| {
        format!(
            "the package's compiled code generation ({e:?}): its record `ALEDGENT_CODE_GENERATION` \
             in `.aledgent.generation` must be there, once (amendment 0033, D13)"
        )
    })
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.windows(2).find(|w| w[0] == name).map(|w| w[1].clone())
}

/// One artefact's build profile, as `--profile` gives it.
struct ProfileIn {
    target: String,
    toolchain: String,
    opt_level: String,
    features: Vec<String>,
    source_revision: String,
    /// The path of the `Cargo.lock` it was built from.
    cargo_lock: String,
}

type Json = serde_json::Value;

fn field<'a>(v: &'a Json, at: &str, name: &str) -> Result<&'a Json, String> {
    v.get(name)
        .ok_or_else(|| format!("--profile: {at}.{name} is required"))
}

fn text(v: &Json, at: &str, name: &str) -> Result<String, String> {
    field(v, at, name)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("--profile: {at}.{name}: a string"))
}

fn known(v: &Json, at: &str, names: &[&str]) -> Result<(), String> {
    let o = v
        .as_object()
        .ok_or_else(|| format!("--profile: {at}: an object"))?;
    match o.keys().find(|k| !names.contains(&k.as_str())) {
        Some(k) => Err(format!("--profile: {at}.{k}: unknown")),
        None => Ok(()),
    }
}

impl ProfileIn {
    fn of(v: &Json, at: &str) -> Result<ProfileIn, String> {
        known(
            v,
            at,
            &[
                "target",
                "toolchain",
                "opt_level",
                "features",
                "source_revision",
                "cargo_lock",
            ],
        )?;
        let features = match v.get("features") {
            None => Vec::new(),
            Some(f) => f
                .as_array()
                .and_then(|a| a.iter().map(|x| x.as_str().map(str::to_owned)).collect())
                .ok_or_else(|| format!("--profile: {at}.features: strings"))?,
        };
        Ok(ProfileIn {
            target: text(v, at, "target")?,
            toolchain: text(v, at, "toolchain")?,
            opt_level: text(v, at, "opt_level")?,
            features,
            source_revision: text(v, at, "source_revision")?,
            cargo_lock: text(v, at, "cargo_lock")?,
        })
    }
}

fn hex32(s: &str, what: &str) -> Result<[u8; 32], String> {
    let v = hex::decode(s).map_err(|e| format!("{what}: {e}"))?;
    v.try_into().map_err(|_| format!("{what}: 32 bytes"))
}

fn profile(p: ProfileIn, base: &Path) -> Result<BuildProfile, String> {
    let lock = base.join(&p.cargo_lock);
    let bytes = fs::read(&lock).map_err(|e| format!("{}: {e}", lock.display()))?;
    let mut features = p.features;
    features.sort();
    features.dedup();
    Ok(BuildProfile {
        target: p.target,
        toolchain: p.toolchain,
        opt_level: p.opt_level,
        features,
        source_revision: p.source_revision,
        cargo_lock: artefact_digest(&bytes),
    })
}

fn this_executable() -> Result<Vec<u8>, String> {
    let exe = std::env::current_exe().map_err(|e| format!("this executable: {e}"))?;
    fs::read(&exe).map_err(|e| format!("{}: {e}", exe.display()))
}

/// The manifest of these artefacts with these profiles.
pub fn manifest_of(
    actor: &[u8],
    runtime: &[u8],
    host: &[u8],
    profiles: &str,
    base: &Path,
) -> Result<CodeManifest, String> {
    let p: Json = serde_json::from_str(profiles).map_err(|e| format!("--profile: {e}"))?;
    known(
        &p,
        "profile",
        &[
            "constructions",
            "messages",
            "kernel",
            "package",
            "runtime",
            "native_decide",
            "generation",
        ],
    )?;
    let generation = package_generation(actor)?;
    if let Some(g) = p.get("generation") {
        let g = g.as_u64().ok_or("--profile: generation: a number")?;
        if g != generation {
            return Err(format!(
                "--profile: generation {g}, but the package is built as generation {generation}"
            ));
        }
    }
    let kernel = field(&p, "profile", "kernel")?;
    known(kernel, "kernel", &["revision", "checksum"])?;
    let constructions = match p.get("constructions") {
        None => vec![ledger_core::anchor::construction()],
        Some(c) => c
            .as_array()
            .and_then(|a| {
                a.iter()
                    .map(|x| x.as_u64().and_then(|n| u8::try_from(n).ok()))
                    .collect()
            })
            .ok_or("--profile: constructions: numbers")?,
    };
    let messages = match p.get("messages") {
        None => Vec::new(),
        Some(m) => m
            .as_array()
            .and_then(|a| a.iter().map(|x| x.as_str().map(str::to_owned)).collect())
            .ok_or("--profile: messages: strings")?,
    };
    let pkg = crate::package::admit_actor(actor).map_err(|e| format!("actor package: {e:?}"))?;
    let artefact = |role, bytes: &[u8], program: Option<[u8; 32]>, profile| Artefact {
        role,
        digest: artefact_digest(bytes),
        len: bytes.len() as u64,
        identities: program
            .map(|id| Identity {
                kind: VOS_PROGRAM_ID.into(),
                value: id.to_vec(),
            })
            .into_iter()
            .collect(),
        profile,
    };
    Ok(CodeManifest {
        writer: WriterKind::Core,
        platform: platform(),
        generation,
        artefacts: vec![
            artefact(
                Role::Package,
                actor,
                Some(pkg.program().0),
                profile(
                    ProfileIn::of(field(&p, "profile", "package")?, "package")?,
                    base,
                )?,
            ),
            artefact(
                Role::Runtime,
                runtime,
                Some(ProgramId::of_pvm(runtime).0),
                profile(
                    ProfileIn::of(field(&p, "profile", "runtime")?, "runtime")?,
                    base,
                )?,
            ),
            artefact(
                Role::NativeDecide,
                host,
                None,
                profile(
                    ProfileIn::of(field(&p, "profile", "native_decide")?, "native_decide")?,
                    base,
                )?,
            ),
        ],
        kernel: KernelIdentity {
            revision: text(kernel, "kernel", "revision")?,
            checksum: match kernel.get("checksum") {
                None | Some(Json::Null) => None,
                Some(_) => Some(hex32(
                    &text(kernel, "kernel", "checksum")?,
                    "kernel.checksum",
                )?),
            },
        },
        constructions,
        messages,
    })
}

/// The manifest as JSON, for a reviewer.
pub fn describe(m: &CodeManifest) -> serde_json::Value {
    let role = |r: Role| match r {
        Role::Package => "package",
        Role::Runtime => "runtime",
        Role::NativeDecide => "native-decide",
    };
    let identities = |ids: &[Identity]| {
        ids.iter()
            .map(|i| serde_json::json!({ "kind": i.kind, "value": hex::encode(&i.value) }))
            .collect::<Vec<_>>()
    };
    serde_json::json!({
        "version": hex::encode(m.version()),
        "writer": format!("{:?}", m.writer).to_lowercase(),
        "platform": {
            "name": m.platform.name,
            "revision": m.platform.revision,
            "identities": identities(&m.platform.identities[..]),
        },
        "generation": m.generation,
        "artefacts": m.artefacts.iter().map(|a| serde_json::json!({
            "role": role(a.role),
            "digest": hex::encode(a.digest),
            "len": a.len,
            "identities": identities(&a.identities[..]),
            "profile": {
                "target": a.profile.target,
                "toolchain": a.profile.toolchain,
                "opt_level": a.profile.opt_level,
                "features": a.profile.features,
                "source_revision": a.profile.source_revision,
                "cargo_lock": hex::encode(a.profile.cargo_lock),
            },
        })).collect::<Vec<_>>(),
        "kernel": { "revision": m.kernel.revision, "checksum": m.kernel.checksum.map(hex::encode) },
        "constructions": m.constructions,
        "messages": m.messages,
    })
}

/// `code-manifest --actor <vos> --runtime <pvm> --profile <json> --out <file>` (module
/// documentation). `cargo_lock` paths in the profile are relative to the profile's directory.
pub fn code_manifest(args: &[String]) -> Result<(), String> {
    let read = |name: &str| -> Result<Vec<u8>, String> {
        let p = arg(args, name).ok_or_else(|| format!("missing {name}"))?;
        fs::read(&p).map_err(|e| format!("{p}: {e}"))
    };
    let actor = read("--actor")?;
    let runtime = read("--runtime")?;
    let profile_path = arg(args, "--profile").ok_or("missing --profile")?;
    let profiles = fs::read_to_string(&profile_path).map_err(|e| format!("{profile_path}: {e}"))?;
    let base = Path::new(&profile_path)
        .parent()
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let m = manifest_of(&actor, &runtime, &this_executable()?, &profiles, &base)?;
    let out = arg(args, "--out").ok_or("missing --out")?;
    fs::write(&out, m.to_bytes()).map_err(|e| format!("{out}: {e}"))?;
    println!(
        "{}",
        serde_json::to_string_pretty(&describe(&m)).map_err(|e| e.to_string())?
    );
    Ok(())
}

/// Which artefacts a command checks (module documentation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Check {
    /// Package, runtime and this executable: a build or a founding.
    Founding,
    /// Package and runtime: a restart, or the genesis executed inside the PVM.
    Agent,
}

/// Refuse unless the chainspec's genesis code is what this process runs (module documentation).
/// Returns *H₀*, or `None` for a chainspec that names no code, allowed by `--unnamed-code`.
pub fn check(
    args: &[String],
    spec: &ChainSpec,
    actor: Option<&[u8]>,
    runtime: Option<&[u8]>,
    which: Check,
) -> Result<Option<[u8; 32]>, String> {
    let Some(code) = &spec.header().code else {
        if args.iter().any(|a| a == "--unnamed-code") {
            eprintln!(
                "aledgent-core-host: the chainspec names no genesis code (--unnamed-code): \
                 the binaries are not checked"
            );
            return Ok(None);
        }
        return Err(
            "the chainspec's header names no genesis code (amendment 0023): build it with \
             `migrate chainspec build --code <manifest>`, or give --unnamed-code for a test book"
                .into(),
        );
    };
    let ours = ledger_core::anchor::construction();
    if code.construction != ours {
        return Err(format!(
            "the chainspec's genesis is in construction {}, this build's is {ours}",
            code.construction
        ));
    }
    // H₀ is generation 0 (amendment 0033, D1); the chainspec refuses any other, and so does this.
    if code.manifest.generation != 0 {
        return Err(format!(
            "the chainspec's genesis code is generation {}: a genesis's is 0",
            code.manifest.generation
        ));
    }
    check_platform(&code.manifest)?;
    let host;
    let mut given: Vec<(Role, &[u8])> = Vec::new();
    match (actor, runtime) {
        (Some(a), Some(r)) => {
            given.push((Role::Package, a));
            given.push((Role::Runtime, r));
        }
        _ => return Err("--actor and --runtime are required: the genesis code names them".into()),
    }
    if which == Check::Founding {
        host = this_executable()?;
        given.push((Role::NativeDecide, &host));
    }
    let manifest = &code.manifest;
    let named: Vec<&Artefact> = manifest
        .artefacts
        .iter()
        .filter(|a| given.iter().any(|(r, _)| *r == a.role))
        .collect();
    let subset = CodeManifest {
        artefacts: named.into_iter().cloned().collect(),
        ..manifest.clone()
    };
    for (role, _) in &given {
        if manifest.artefact(*role).is_none() {
            return Err(format!(
                "the genesis code names no {role:?} artefact: this process runs one"
            ));
        }
    }
    subset.check(&given).map_err(|e| {
        format!(
            "the running binaries are not the chainspec's genesis code {} ({e:?}): build or \
             fetch the artefacts its manifest names",
            hex::encode(code.version())
        )
    })?;
    Ok(Some(code.version()))
}

/// Refuse unless `actor` and `runtime` are the package and runtime `manifest` names, for a core
/// whose genesis code it is and in this build's construction: the check a restart runs ([`check`]
/// with [`Check::Agent`]), from a manifest file instead of a chainspec's header. This executable is
/// not checked: a core image on a deployment's genesis bytes carries a newer host by design
/// (`DEC-27` (i)).
pub fn check_agent(manifest: &CodeManifest, actor: &[u8], runtime: &[u8]) -> Result<(), String> {
    if manifest.writer != WriterKind::Core {
        return Err(format!(
            "the manifest is a {:?} writer's code, not the core's",
            manifest.writer
        ));
    }
    check_platform(manifest)?;
    let ours = ledger_core::anchor::construction();
    if !manifest.constructions.contains(&ours) {
        return Err(format!(
            "the manifest's code reads constructions {:?}, this build's is {ours}: build the host \
             with the genesis's roots",
            manifest.constructions
        ));
    }
    for role in [Role::Package, Role::Runtime] {
        if manifest.artefact(role).is_none() {
            return Err(format!("the manifest names no {role:?} artefact"));
        }
    }
    let subset = CodeManifest {
        artefacts: manifest
            .artefacts
            .iter()
            .filter(|a| matches!(a.role, Role::Package | Role::Runtime))
            .cloned()
            .collect(),
        ..manifest.clone()
    };
    subset
        .check(&[(Role::Package, actor), (Role::Runtime, runtime)])
        .map_err(|e| {
            format!(
                "the package and runtime are not the genesis code {} ({e:?}): copy them from the \
                 deployment's pinned core image",
                hex::encode(manifest.version())
            )
        })
}

/// `check-code --actor <vos> --runtime <pvm> (--chainspec <file> | --manifest <file>) [--code <hex>]
/// [--write-manifest <file>] [--out <file>]`: refuse unless the package and the runtime are the
/// genesis code's, as a restart checks them ([`Check::Agent`]), without starting anything. It is
/// the build-time check of the compiled code generation (`T-037-01`, `T-038-25`): a package whose
/// record is missing, folded away or duplicated, or names another generation than the manifest's,
/// fails it. The
/// genesis code is the chainspec header's or an `aledgent/code/v1` manifest file's; `--code` also
/// requires its version to be *H₀*. `--write-manifest` writes the manifest it checked against (a
/// chainspec's, extracted); `--out` writes the report it prints. A core image on a deployment's
/// genesis bytes is built with it (`deploy/docker/core.Dockerfile`, target `core-on-genesis`;
/// `F-038` §12 step 0).
pub fn check_code(args: &[String]) -> Result<(), String> {
    let read = |name: &str| -> Result<Vec<u8>, String> {
        let p = arg(args, name).ok_or_else(|| format!("missing {name}"))?;
        fs::read(&p).map_err(|e| format!("{p}: {e}"))
    };
    let actor = read("--actor")?;
    let runtime = read("--runtime")?;
    let (manifest, from) = match (arg(args, "--chainspec"), arg(args, "--manifest")) {
        (Some(p), None) => {
            let bytes = fs::read(&p).map_err(|e| format!("{p}: {e}"))?;
            let spec =
                ChainSpec::from_bytes(&bytes).map_err(|e| format!("chainspec {p}: {e:?}"))?;
            // No `--unnamed-code` here: a chainspec that names no code has nothing to check.
            check(&[], &spec, Some(&actor), Some(&runtime), Check::Agent)?;
            let code = spec
                .header()
                .code
                .as_ref()
                .ok_or("the chainspec names no genesis code")?;
            (code.manifest.clone(), format!("chainspec {p}"))
        }
        (None, Some(p)) => {
            let bytes = fs::read(&p).map_err(|e| format!("{p}: {e}"))?;
            let m = CodeManifest::from_bytes(&bytes).map_err(|e| {
                format!(
                    "{p}: not a code manifest of amendment 0033's format (platform, generation, \
                     identities): {e:?}"
                )
            })?;
            check_agent(&m, &actor, &runtime)?;
            (m, format!("manifest {p}"))
        }
        _ => return Err("exactly one of --chainspec <file> or --manifest <file>".into()),
    };
    let version = manifest.version();
    if let Some(want) = arg(args, "--code")
        && hex32(&want, "--code")? != version
    {
        return Err(format!(
            "the genesis code is {}, not --code {want}",
            hex::encode(version)
        ));
    }
    if let Some(out) = arg(args, "--write-manifest") {
        fs::write(&out, manifest.to_bytes()).map_err(|e| format!("{out}: {e}"))?;
    }
    let report = serde_json::json!({
        "code": hex::encode(version),
        "from": from,
        "checked": ["platform", "package", "runtime", "generation"],
        "generation": manifest.generation,
        "native_decide": "not checked: this host is not the genesis's (DEC-27 (i)); on a restart \
                          the genesis is checked against the manifest's roots and the Agent's",
        "construction": ledger_core::anchor::construction(),
        "manifest": describe(&manifest),
    });
    let text = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    if let Some(out) = arg(args, "--out") {
        fs::write(&out, format!("{text}\n")).map_err(|e| format!("{out}: {e}"))?;
    }
    println!("{text}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a0023_the_profile_reads_and_the_manifest_names_the_bytes() {
        let dir = std::env::temp_dir().join(format!("code-manifest-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Cargo.lock"), b"lock").unwrap();
        let p = ProfileIn {
            target: "riscv64em-vos".into(),
            toolchain: "nightly-2026-03-20".into(),
            opt_level: "3".into(),
            features: vec!["b".into(), "a".into(), "a".into()],
            source_revision: "abc".into(),
            cargo_lock: "Cargo.lock".into(),
        };
        let b = profile(p, &dir).unwrap();
        assert_eq!(b.features, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(b.cargo_lock, artefact_digest(b"lock"));
        fs::remove_dir_all(&dir).ok();
    }

    /// A package of these bytes, built as generation `g`.
    fn package(bytes: &[u8], g: u64) -> Vec<u8> {
        let mut p = bytes.to_vec();
        p.extend_from_slice(&ledger_core::code::generation_record(g));
        p
    }

    fn manifest(package: &[u8], runtime: &[u8], constructions: Vec<u8>) -> CodeManifest {
        let profile = BuildProfile {
            target: "riscv64em-vos".into(),
            toolchain: "nightly-2026-03-20".into(),
            opt_level: "s".into(),
            features: Vec::new(),
            source_revision: "abc".into(),
            cargo_lock: [0; 32],
        };
        let artefact = |role, bytes: &[u8]| Artefact {
            role,
            digest: artefact_digest(bytes),
            len: bytes.len() as u64,
            identities: Vec::new(),
            profile: profile.clone(),
        };
        CodeManifest {
            writer: WriterKind::Core,
            platform: platform(),
            generation: 0,
            artefacts: vec![
                artefact(Role::Package, package),
                artefact(Role::Runtime, runtime),
                artefact(Role::NativeDecide, b"the genesis host"),
            ],
            kernel: KernelIdentity {
                revision: "e9a7b08".into(),
                checksum: None,
            },
            constructions,
            messages: Vec::new(),
        }
    }

    /// `F-038` §12 step 0, `DEC-27` (i): the genesis package and runtime pass under a newer host
    /// (the manifest's native-decide artefact is the genesis host's, and is not checked); a changed
    /// byte, swapped roles, a missing role, another writer or another construction is refused.
    #[test]
    fn dec_27_i_the_genesis_bytes_pass_and_a_tampered_copy_is_refused() {
        let ours = ledger_core::anchor::construction();
        let core = package(b"core.vos", 0);
        let m = manifest(&core, b"agent_runtime.pvm", vec![ours]);
        // As a manifest file gives it.
        let m = CodeManifest::from_bytes(&m.to_bytes()).unwrap();
        check_agent(&m, &core, b"agent_runtime.pvm").unwrap();
        let mut tampered = core.clone();
        tampered[3] ^= 1;
        assert!(check_agent(&m, &tampered, b"agent_runtime.pvm").is_err());
        assert!(check_agent(&m, &core, b"agent_runtime.pvm\0").is_err());
        assert!(check_agent(&m, b"agent_runtime.pvm", &core).is_err());
        let mut other = m.clone();
        other.writer = WriterKind::Shard;
        assert!(check_agent(&other, &core, b"agent_runtime.pvm").is_err());
        let mut no_runtime = m.clone();
        no_runtime.artefacts.retain(|a| a.role != Role::Runtime);
        assert!(check_agent(&no_runtime, &core, b"agent_runtime.pvm").is_err());
        let elsewhere = manifest(&core, b"agent_runtime.pvm", vec![ours.wrapping_add(1)]);
        assert!(check_agent(&elsewhere, &core, b"agent_runtime.pvm").is_err());
    }

    /// T-037-01: the build check refuses a package whose compiled generation is not the
    /// manifest's, or whose record is folded away or duplicated; and a manifest of another
    /// platform.
    #[test]
    fn t037_01_check_code_refuses_another_generation_platform_or_record() {
        let ours = ledger_core::anchor::construction();
        let one = package(b"core.vos", 1);
        // The manifest says 0, the package was built as 1.
        let m = manifest(&one, b"agent_runtime.pvm", vec![ours]);
        let e = check_agent(&m, &one, b"agent_runtime.pvm").unwrap_err();
        assert!(e.contains("GenerationDiffers"), "{e}");
        // Folded away: no record at all.
        let m = manifest(b"core.vos", b"agent_runtime.pvm", vec![ours]);
        let e = check_agent(&m, b"core.vos", b"agent_runtime.pvm").unwrap_err();
        assert!(e.contains("Missing"), "{e}");
        // Duplicated.
        let twice = package(&package(b"core.vos", 0), 0);
        let m = manifest(&twice, b"agent_runtime.pvm", vec![ours]);
        let e = check_agent(&m, &twice, b"agent_runtime.pvm").unwrap_err();
        assert!(e.contains("Duplicated"), "{e}");
        // Another platform, or another revision of this one.
        let zero = package(b"core.vos", 0);
        let mut m = manifest(&zero, b"agent_runtime.pvm", vec![ours]);
        check_agent(&m, &zero, b"agent_runtime.pvm").unwrap();
        m.platform.revision = "b74eeef8".into();
        assert!(check_agent(&m, &zero, b"agent_runtime.pvm").is_err());
        m.platform = Platform {
            name: "vos-core".into(),
            revision: PLATFORM_REVISION.into(),
            identities: Vec::new(),
        };
        assert!(check_agent(&m, &zero, b"agent_runtime.pvm").is_err());
        assert_eq!(package_generation(&zero), Ok(0));
    }

    /// The platform this host names is the VOS it links.
    #[test]
    fn the_platform_revision_is_the_vos_pin() {
        let manifest = include_str!("../../Cargo.toml");
        assert!(
            manifest.contains(&format!("rev = \"{PLATFORM_REVISION}\"")),
            "agents/Cargo.toml no longer pins VOS at {PLATFORM_REVISION}: update PLATFORM_REVISION"
        );
    }
}
