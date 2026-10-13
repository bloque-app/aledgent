// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The genesis code (`F-036`, with amendment 0023's manifest, PR #201): a chainspec's header names
//! the code its genesis is built and run under, and the core host refuses to build or found a book
//! under other binaries.
//!
//! - `code-manifest --actor <vos> --runtime <pvm> --profile <json> --out <file>` writes the code
//!   manifest (`ledger_core::code`, magic `aledgent/code/v1`) of this host, this package and this
//!   runtime: each artefact's unkeyed blake2b-256 digest, length and VOS `ProgramId`, with the
//!   build profiles the release pipeline gives in `--profile`. It prints the code version *H₀*.
//!   `migrate chainspec build --code <file>` puts it in the header.
//! - The check ([`check`]): every artefact the manifest names must be the bytes this process runs.
//!   The package is `--actor`, the runtime `--runtime`, and `native-decide` is this executable,
//!   which decides the genesis natively. The construction is this build's.
//!
//! Where it holds:
//!
//! | Command | Package and runtime | This executable |
//! |---|---|---|
//! | `build-spec` | checked | checked |
//! | `serve` and `restore`, founding (no replica yet) | checked | checked |
//! | `serve`, restart (a replica) | checked: no upgrade exists yet, so the Agent still runs *H₀* | not checked: its one decision, the genesis, is checked against the manifest's roots and the Agent's |
//! | `verify-genesis` | checked | not checked: the genesis is executed inside the PVM |
//!
//! A chainspec whose header names no code is refused unless `--unnamed-code` is given: tests and
//! stress books, never a deployment's.

use std::fs;
use std::path::Path;

use ledger_core::chainspec::ChainSpec;
use ledger_core::code::{
    Artefact, BuildProfile, CodeManifest, KernelIdentity, Role, WriterKind, artefact_digest,
};
use vos::agent_sdk::ProgramId;

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
        ],
    )?;
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
        program_id: program,
        profile,
    };
    Ok(CodeManifest {
        writer: WriterKind::Core,
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
    serde_json::json!({
        "version": hex::encode(m.version()),
        "writer": format!("{:?}", m.writer).to_lowercase(),
        "artefacts": m.artefacts.iter().map(|a| serde_json::json!({
            "role": role(a.role),
            "digest": hex::encode(a.digest),
            "len": a.len,
            "program_id": a.program_id.map(hex::encode),
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
}
