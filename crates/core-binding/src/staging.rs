// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! Staging a code version before the `Upgrade` that names it is logged (`T-037-04`; `REQ-RT-16`,
//! `REQ-RT-17`, `REQ-RT-20`; amendment 0023 (b) steps 1 and 2, amendment 0033 (b) "Staging").
//!
//! The execution node that drives the writer fetches *H*'s manifest and the artefacts it names,
//! and checks all of it, **before** the upgrade is accepted at the door, so that an upgrade whose
//! code cannot be had is refused with nothing logged and nothing changed:
//!
//! 1. the manifest's bytes hash to *H* (`blake2b-256`), and decode;
//! 2. the manifest's `generation` is the writer's plus one (`REQ-RT-20`);
//! 3. the package, and the runtime if it names one, are fetched, each of the length and
//!    `blake2b-256` the manifest gives;
//! 4. the package's compiled `GENERATION` is the manifest's (`ledger_core::code::generation_of`, the
//!    one reader), which the platform's port reads out of its package ([`Checked::check_package`]).
//!
//! Retrieval is behind [`CodeFetch`], one method shaped like amendment 0023's `CodeSource` trait
//! (`fetch(digest, len)`): untrusted, **every byte it returns is checked here**, so a source can be
//! anything. The sources themselves, the cache and `aledgent_code_source_mismatch_total` are
//! `T-037-06`'s; this module checks and nothing more. The manifest's own length is not known to
//! the upgrade (it names *H* alone), so the manifest is fetched with no length and checked by its
//! digest.
//!
//! Bytes that do not match are never used. A refusal names why, for the operator's log; the door
//! answers `ERR-InvalidTransition` and keeps nothing.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use ledger_core::code::{CodeManifest, Role, artefact_digest};

/// One place code can come from (amendment 0023, "Retrieval"). Untrusted: whatever it returns is
/// checked by [`fetch_checked`].
pub trait CodeFetch: Send + Sync {
    /// A name for logs (no credential).
    fn name(&self) -> &str;
    /// The bytes whose `blake2b-256` is `digest`, `len` long when the caller knows it (an artefact:
    /// the manifest names it; the manifest itself: `None`); `None` if this source does not hold
    /// them.
    fn fetch(&self, digest: &[u8; 32], len: Option<u64>) -> Option<Vec<u8>>;
    /// How many answers each place of this source discarded because the bytes were not what their
    /// digest or length names (`aledgent_code_source_mismatch_total`, `T-037-06`): the place's
    /// name, and the count. Empty for a source that does not count.
    fn mismatches(&self) -> Vec<(String, u64)> {
        Vec::new()
    }
}

/// Why a code version was not staged. Nothing was kept.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StageRefusal {
    /// This node has no source of code: nothing can be staged.
    NoSource,
    /// No source returned the manifest, or an artefact it names (`AC-K7.2`).
    NotRetrievable(String),
    /// What a source returned is not the digest or the length that names it (`AC-K7.2`).
    Mismatch(String),
    /// The manifest does not decode, or names no package or runtime.
    Manifest(String),
    /// The manifest names another generation than the writer's plus one (`AC-K7.16`).
    Generation {
        /// The writer's generation in force.
        writer: u64,
        /// The manifest's.
        manifest: u64,
    },
    /// The package does not carry the manifest's generation in its bytes (`AC-K7.16`).
    Package(String),
}

impl core::fmt::Display for StageRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoSource => f.write_str("this node has no source of code"),
            Self::NotRetrievable(what) => write!(f, "{what} is retrievable from no source"),
            Self::Mismatch(what) => write!(f, "{what} is not the bytes its digest names"),
            Self::Manifest(why) => write!(f, "the manifest is refused: {why}"),
            Self::Generation { writer, manifest } => write!(
                f,
                "the manifest names code generation {manifest}, the writer's next is {}",
                writer.saturating_add(1)
            ),
            Self::Package(why) => write!(f, "the package is refused: {why}"),
        }
    }
}

/// A code version, fetched: its manifest and the bytes of the package and the runtime it names,
/// each checked against the manifest's digest and length. Its package's compiled generation is
/// checked apart ([`Checked::check_package`]), by whoever knows how the platform wraps a package.
#[derive(Clone, Debug)]
pub struct Checked {
    /// Its manifest.
    pub manifest: CodeManifest,
    /// The package's bytes (`core.vos`).
    pub package: Vec<u8>,
    /// The runtime's bytes, if the manifest names one: a platform that runs its own has none
    /// (`REQ-RT-19`), and the port that needs one refuses its absence.
    pub runtime: Option<Vec<u8>>,
}

impl Checked {
    /// Check that the package carries the manifest's generation compiled in. `compiled` reads the
    /// generation compiled into the package's bytes: the platform knows how its package wraps the
    /// program (`ledger_core::code::generation_of` over the program).
    ///
    /// # Errors
    ///
    /// [`StageRefusal::Package`]: no generation, or another one, in the package.
    pub fn check_package(
        &self,
        compiled: &dyn Fn(&[u8]) -> Result<u64, String>,
    ) -> Result<(), StageRefusal> {
        let compiled = compiled(&self.package).map_err(StageRefusal::Package)?;
        if compiled != self.manifest.generation {
            return Err(StageRefusal::Package(format!(
                "it carries code generation {compiled}, its manifest names {}",
                self.manifest.generation
            )));
        }
        Ok(())
    }
}

/// Fetch and check the code version `code` through `source` for a writer whose generation in force
/// is `writer_generation`, and check its package's compiled generation with `compiled`
/// ([`fetch`] then [`Checked::check_package`]).
///
/// # Errors
///
/// [`StageRefusal`]: anything the module documentation lists. Nothing is kept.
pub fn fetch_checked(
    source: &dyn CodeFetch,
    code: &[u8; 32],
    writer_generation: u64,
    compiled: &dyn Fn(&[u8]) -> Result<u64, String>,
) -> Result<Checked, StageRefusal> {
    let checked = fetch(source, code, writer_generation)?;
    checked.check_package(compiled)?;
    Ok(checked)
}

/// Fetch the code version `code` through `source`, and check steps 1 to 3 of the module
/// documentation: the manifest, its generation, and the bytes of the artefacts. Slow (it is
/// retrieval), and touches no state: the execution node does it outside its locks, and stages the
/// result ([`crate::host::AgentPort::stage_code`]).
///
/// # Errors
///
/// [`StageRefusal`]: the manifest or an artefact was not retrievable or did not match, or the
/// manifest names another generation than the writer's plus one. Nothing is kept.
pub fn fetch(
    source: &dyn CodeFetch,
    code: &[u8; 32],
    writer_generation: u64,
) -> Result<Checked, StageRefusal> {
    fetch_generation(source, code, writer_generation.checked_add(1)).map_err(|r| match r {
        StageRefusal::Generation { manifest, .. } => StageRefusal::Generation {
            writer: writer_generation,
            manifest,
        },
        other => other,
    })
}

/// [`fetch`] for a code version whose generation the caller already knows: the manifest must name
/// `generation` (`None`: no generation can be named, so none matches). Staging an `Upgrade` expects
/// the writer's next; staging a version again at start ([`restage`]) expects the one the book
/// gives it, which for the code in force is the writer's own.
///
/// # Errors
///
/// As [`fetch`]; [`StageRefusal::Generation`] carries the generation expected, less one, as
/// `writer`.
pub fn fetch_generation(
    source: &dyn CodeFetch,
    code: &[u8; 32],
    generation: Option<u64>,
) -> Result<Checked, StageRefusal> {
    let bytes = source.fetch(code, None).ok_or_else(|| {
        StageRefusal::NotRetrievable(format!("the manifest of {}", crate::switch::short(code)))
    })?;
    if &artefact_digest(&bytes) != code {
        return Err(StageRefusal::Mismatch(format!(
            "the manifest of {}",
            crate::switch::short(code)
        )));
    }
    let manifest =
        CodeManifest::from_bytes(&bytes).map_err(|e| StageRefusal::Manifest(String::from(e.0)))?;
    // `from_bytes` accepts a manifest only in its canonical form, so its digest is `code`'s.
    if &manifest.version() != code {
        return Err(StageRefusal::Manifest("not in its canonical form".into()));
    }
    if generation != Some(manifest.generation) {
        return Err(StageRefusal::Generation {
            writer: generation.map_or(u64::MAX, |g| g.saturating_sub(1)),
            manifest: manifest.generation,
        });
    }
    let get = |role: Role, a: &ledger_core::code::Artefact| -> Result<Vec<u8>, StageRefusal> {
        let name = || format!("the {role:?} of {}", crate::switch::short(code));
        let bytes = source
            .fetch(&a.digest, Some(a.len))
            .ok_or_else(|| StageRefusal::NotRetrievable(name()))?;
        if bytes.len() as u64 != a.len || artefact_digest(&bytes) != a.digest {
            return Err(StageRefusal::Mismatch(name()));
        }
        Ok(bytes)
    };
    let package = manifest
        .artefact(Role::Package)
        .ok_or_else(|| StageRefusal::Manifest("it names no Package".into()))
        .and_then(|a| get(Role::Package, a))?;
    let runtime = manifest
        .artefact(Role::Runtime)
        .map(|a| get(Role::Runtime, a))
        .transpose()?;
    Ok(Checked {
        manifest,
        package,
        runtime,
    })
}

/// What staging one named version again did ([`restage`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Restaged {
    /// The port held it already.
    Held([u8; 32]),
    /// It was fetched, checked and staged again.
    Staged([u8; 32]),
    /// It could not be, and nothing was kept: the host says so, and the switch that needs it will
    /// refuse (`SwitchError::NotStaged`) with a way out still open.
    Refused([u8; 32], StageRefusal),
}

/// At start, stage again the code versions the writer's hot row names that the port does not hold
/// ([`crate::switch::named`]): the code in force and the version of a scheduled upgrade
/// (`T-037-21`). A restarted host holds the code it started with and nothing it fetched before;
/// without this, an upgrade scheduled before the restart is lost to it (the switch at *n* finds
/// the version not staged), and a host restarted after the switch cannot name the version it runs.
///
/// Each version is fetched and checked as staging checks it, held to the generation the book gives
/// it ([`fetch_generation`]); the port checks it again ([`crate::host::AgentPort::restage_code`]). The sources
/// are untrusted as ever: the node's cache answers first. Call it before the start check
/// ([`crate::host::Driver::ensure_code`]).
///
/// # Errors
///
/// The port could not read the writer's hot row. A version that cannot be staged is an entry of
/// the result, not an error.
pub fn restage(
    port: &mut dyn crate::host::AgentPort,
    source: &dyn CodeFetch,
) -> Result<Vec<Restaged>, crate::host::PortError> {
    let Some(hot) = port.hot()? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for (code, generation) in crate::switch::named(hot.code, hot.generation, hot.upgrade) {
        if port.holds_code(&code) {
            out.push(Restaged::Held(code));
            continue;
        }
        let staged = fetch_generation(source, &code, Some(generation))
            .and_then(|checked| port.restage_code(checked, generation));
        out.push(match staged {
            Ok(()) => Restaged::Staged(code),
            Err(refusal) => Restaged::Refused(code, refusal),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use alloc::vec;
    use ledger_core::code::{
        Artefact, BuildProfile, KernelIdentity, Platform, WriterKind, generation_record,
    };
    use std::sync::Mutex;

    /// A source that holds what it was given, by digest, and counts its fetches.
    struct Held(Mutex<BTreeMap<[u8; 32], Vec<u8>>>);
    impl Held {
        fn new() -> Self {
            Held(Mutex::new(BTreeMap::new()))
        }
        fn put(&self, bytes: &[u8]) {
            self.0
                .lock()
                .unwrap()
                .insert(artefact_digest(bytes), bytes.to_vec());
        }
        /// Hold `bytes` under a digest that is not theirs: a source that lies.
        fn lie(&self, digest: [u8; 32], bytes: &[u8]) {
            self.0.lock().unwrap().insert(digest, bytes.to_vec());
        }
    }
    impl CodeFetch for Held {
        fn name(&self) -> &str {
            "held"
        }
        fn fetch(&self, digest: &[u8; 32], _: Option<u64>) -> Option<Vec<u8>> {
            self.0.lock().unwrap().get(digest).cloned()
        }
    }

    fn compiled(b: &[u8]) -> Result<u64, String> {
        ledger_core::code::generation_of(b).map_err(|e| format!("{e:?}"))
    }

    fn package(generation: u64) -> Vec<u8> {
        // A standard PVM program with the record in its read-only data is what `generation_of`
        // reads; the unit tests only need the bytes to be the record's carrier.
        let mut p = Vec::new();
        let ro = generation_record(generation);
        // E3(|o|) ‖ E3(|w|) ‖ E2(z) ‖ E3(s) ‖ o ‖ w ‖ E4(|c|) ‖ c
        p.extend_from_slice(&(ro.len() as u32).to_le_bytes()[..3]);
        p.extend_from_slice(&[0, 0, 0]);
        p.extend_from_slice(&[0, 0]);
        p.extend_from_slice(&[0, 0, 0]);
        p.extend_from_slice(&ro);
        p.extend_from_slice(&1u32.to_le_bytes());
        p.push(0);
        p
    }

    fn manifest(generation: u64, package: &[u8], runtime: &[u8]) -> CodeManifest {
        let profile = || BuildProfile {
            target: "riscv64em-vos".into(),
            toolchain: "nightly".into(),
            opt_level: "s".into(),
            features: Vec::new(),
            source_revision: "0123abcd".into(),
            cargo_lock: [0; 32],
        };
        let art = |role, b: &[u8]| Artefact {
            role,
            digest: artefact_digest(b),
            len: b.len() as u64,
            identities: Vec::new(),
            profile: profile(),
        };
        CodeManifest {
            writer: WriterKind::Core,
            platform: Platform {
                name: "vos-saga".into(),
                revision: "c54fc1ff".into(),
                identities: Vec::new(),
            },
            generation,
            artefacts: vec![art(Role::Package, package), art(Role::Runtime, runtime)],
            kernel: KernelIdentity {
                revision: "e9a7b08".into(),
                checksum: None,
            },
            constructions: Vec::new(),
            messages: Vec::new(),
        }
    }

    /// A source holding a whole code version of `generation`, and its version.
    fn held(generation: u64) -> (Held, [u8; 32]) {
        let (p, r) = (package(generation), b"runtime".to_vec());
        let m = manifest(generation, &p, &r);
        let s = Held::new();
        s.put(&m.to_bytes());
        s.put(&p);
        s.put(&r);
        (s, m.version())
    }

    #[test]
    fn t037_04_a_version_that_is_all_there_is_staged() {
        let (s, h) = held(1);
        let c = fetch_checked(&s, &h, 0, &compiled).expect("staged");
        assert_eq!(c.manifest.version(), h);
        assert_eq!(c.package, package(1));
    }

    /// `AC-K7.2`: artefacts no source returns, or returns with another digest or length.
    #[test]
    fn ac_k7_2_a_version_whose_bytes_are_missing_or_wrong_is_refused() {
        let (s, h) = held(1);
        // Unknown manifest.
        let none = Held::new();
        assert!(matches!(
            fetch_checked(&none, &h, 0, &compiled),
            Err(StageRefusal::NotRetrievable(_))
        ));
        // A manifest under a digest that is not its own.
        let lie = Held::new();
        let m = s.0.lock().unwrap().get(&h).cloned().unwrap();
        lie.lie(h, &[m.as_slice(), b"x"].concat());
        assert!(matches!(
            fetch_checked(&lie, &h, 0, &compiled),
            Err(StageRefusal::Mismatch(_))
        ));
        // A missing artefact, then each one under another digest.
        for role in [Role::Package, Role::Runtime] {
            let (s, h) = held(1);
            let m = CodeManifest::from_bytes(&s.0.lock().unwrap()[&h]).unwrap();
            let d = m.artefact(role).unwrap().digest;
            let bytes = s.0.lock().unwrap().remove(&d).unwrap();
            assert!(
                matches!(
                    fetch_checked(&s, &h, 0, &compiled),
                    Err(StageRefusal::NotRetrievable(_))
                ),
                "{role:?} missing"
            );
            let mut wrong = bytes.clone();
            wrong[0] ^= 1;
            s.lie(d, &wrong);
            assert!(
                matches!(
                    fetch_checked(&s, &h, 0, &compiled),
                    Err(StageRefusal::Mismatch(_))
                ),
                "{role:?} with another digest"
            );
            // Another length under the right digest cannot exist; a short read is a mismatch too.
            s.lie(d, &bytes[..bytes.len() - 1]);
            assert!(matches!(
                fetch_checked(&s, &h, 0, &compiled),
                Err(StageRefusal::Mismatch(_))
            ));
        }
    }

    /// `AC-K7.16`, second part: a manifest of another generation than the writer's plus one, and a
    /// package that carries another constant than its manifest names.
    #[test]
    fn ac_k7_16_another_generation_or_another_constant_is_refused() {
        // The writer is at 0: its next is 1. A version of generation 0, 2 or 7 is refused.
        for g in [0, 2, 7] {
            let (s, h) = held(g);
            assert_eq!(
                fetch_checked(&s, &h, 0, &compiled).unwrap_err(),
                StageRefusal::Generation {
                    writer: 0,
                    manifest: g
                }
            );
        }
        // The writer at 4: the version of generation 5 is the one.
        let (s, h) = held(5);
        assert!(fetch_checked(&s, &h, 4, &compiled).is_ok());
        assert!(fetch_checked(&s, &h, 5, &compiled).is_err());
        // A manifest that names generation 1 over a package that carries 2.
        let (p, r) = (package(2), b"runtime".to_vec());
        let m = manifest(1, &p, &r);
        let s = Held::new();
        s.put(&m.to_bytes());
        s.put(&p);
        s.put(&r);
        assert!(matches!(
            fetch_checked(&s, &m.version(), 0, &compiled),
            Err(StageRefusal::Package(why)) if why.contains("carries code generation 2")
        ));
        // A package with no constant at all.
        let (p, r) = (b"no constant here".to_vec(), b"runtime".to_vec());
        let m = manifest(1, &p, &r);
        let s = Held::new();
        s.put(&m.to_bytes());
        s.put(&p);
        s.put(&r);
        assert!(matches!(
            fetch_checked(&s, &m.version(), 0, &compiled),
            Err(StageRefusal::Package(_))
        ));
    }

    /// `REQ-RT-19`: a platform that runs its own runtime names none, and the version stages with the
    /// package alone; whether that platform can run it is the port's to say.
    #[test]
    fn t037_04_a_manifest_with_no_runtime_stages_the_package_alone() {
        let p = package(1);
        let mut m = manifest(1, &p, b"runtime");
        m.artefacts.retain(|a| a.role != Role::Runtime);
        let s = Held::new();
        s.put(&m.to_bytes());
        s.put(&p);
        let c = fetch_checked(&s, &m.version(), 0, &compiled).expect("staged");
        assert!(c.runtime.is_none());
        assert_eq!(c.package, p);
    }

    /// The writer's generation is a number that can end: plus one never wraps to a match.
    #[test]
    fn t037_04_the_last_generation_has_no_next() {
        let (s, h) = held(0);
        assert!(fetch_checked(&s, &h, u64::MAX, &compiled).is_err());
    }
}
