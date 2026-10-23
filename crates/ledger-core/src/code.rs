// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The code manifest (amendment 0023, `DEC-26` (a), as amendment 0033 shaped it before it was
//! built): what names the code that decides a writer's transitions, independently of the platform
//! that runs it (`REQ-RT-15`, `REQ-RT-16`, `REQ-RT-19`, `REQ-RT-20`).
//!
//! - An **artefact digest** is `blake2b-256(bytes)`, unkeyed, over the artefact's exact bytes: the
//!   hash JAM's `lookup` and Substrate's preimage pallet use.
//! - The **manifest** is a canonical encoding that starts with the magic `aledgent/code/v1` and
//!   names, for one writer kind:
//!   - its **platform** ([`Platform`]): the runtime and the execution node's software, with a
//!     revision (`vos-saga@bd8d1bb7…` today), and any identity the platform gives what it supplies
//!     itself (on `vos-core`, the node's default agent program);
//!   - its **code generation** (`generation`): 0 at genesis, one more after each upgrade. Every
//!     package carries it compiled in ([`generation`]), and the build fails when the two differ;
//!   - each artefact that decides a transition (`role`: `package`, the optional `runtime`,
//!     `native-decide`), with its `digest`, `len`, every **identity** the platform knows it by
//!     ([`Identity`]: VOS's `ProgramId` on `saga`; the ELF's hash, the derived program's hash and
//!     the transpiler's version on `vos-core`) and its **build profile** (target, toolchain,
//!     `opt-level`, Cargo features, source revision, the digest of `Cargo.lock`). The `runtime`
//!     role is optional: a platform that runs its own (`vos-core`) has none;
//!   - the kernel's identity (informative), and the constructions it reads and the message
//!     formats it accepts and emits.
//! - The **code version** *H* is `blake2b-256(manifest bytes)`.
//!
//! What only replays a range, and decides nothing (its replayer), is not in the manifest
//! (amendment 0033, D6): replacing it changes no code version.
//!
//! The native genesis build is a transition the host decides natively, so the core host's own
//! executable is the manifest's `native-decide` artefact for a chainspec book.
//!
//! The encoding is `F-014`'s canonical one, so this module's bytes are the manifest's bytes, the
//! same on every machine.

pub mod authority;
pub mod generation;

pub use generation::{
    GenerationError, RECORD_LEN, SECTION as GENERATION_SECTION, SYMBOL as GENERATION_SYMBOL,
    TAG as GENERATION_TAG, generation_of, generation_record, record_generation,
};

use alloc::string::String;
use alloc::vec::Vec;

use cipher_clerk::crypto::blake2b_256;

use crate::export::canon::{Canonical, Dec, DecodeError, Enc};

/// The manifest's magic: its first bytes.
pub const MAGIC: &[u8] = b"aledgent/code/v1";

/// An artefact's digest: `blake2b-256(bytes)`, unkeyed.
pub fn artefact_digest(bytes: &[u8]) -> [u8; 32] {
    blake2b_256(b"", &[bytes])
}

/// A way out of a scheduled upgrade, taken by the writer (`REQ-RT-20`, amendment 0033 D2, D10;
/// `T-037-15`): `reschedule_upgrade` or `cancel_upgrade`, accepted. Code of either generation
/// applies it the same: the upgrade does not take effect at its input, and the generation stays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WayOut {
    /// The upgrade is moved to input `from`, later than the way out's own and than the input the
    /// scheduled upgrade applied from (owner ruling w6: decided by the library, never here).
    Reschedule {
        /// The input number it now applies from.
        from: u64,
    },
    /// The upgrade is cancelled.
    Cancel,
}

impl WayOut {
    /// The scheduled upgrade (code version and input number) after this way out: cancelled, or
    /// moved. With nothing scheduled, nothing changes.
    pub fn apply(self, upgrade: Option<([u8; 32], u64)>) -> Option<([u8; 32], u64)> {
        match self {
            WayOut::Cancel => None,
            WayOut::Reschedule { from } => upgrade.map(|(code, _)| (code, from)),
        }
    }
}

/// The writer a manifest's code runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum WriterKind {
    /// The single core Agent.
    Core,
    /// A book shard.
    Shard,
    /// The principals Agent.
    Principals,
    /// The settlement Agent.
    Settlement,
    /// The anchor Agent.
    Anchor,
}

/// What an artefact is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "kebab-case"))]
pub enum Role {
    /// The writer's actor package (`core.vos`): the code that carries the code generation.
    Package,
    /// The runtime program that executes the package, where the platform takes one from the
    /// execution node (the PVM AgentRuntime on `saga`). Optional: a platform that runs its own has
    /// none (`vos-core`).
    Runtime,
    /// A host executable that decides a transition natively (the chainspec genesis's build).
    NativeDecide,
}

/// The platform a code version runs on (`SPEC.md` §2 **Platform**, `REQ-RT-19`): the runtime that
/// executes the writer's code and the execution node's software that drives it, named with their
/// revision.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Platform {
    /// Its name (`vos-saga`, `vos-core`).
    pub name: String,
    /// Its revision (a commit of the runtime).
    pub revision: String,
    /// The identities of what the platform supplies itself and the code depends on (on
    /// `vos-core`, the node's default agent program's hash), sorted by kind, each kind once.
    pub identities: Vec<Identity>,
}

/// One identity under which a platform knows an artefact (`REQ-RT-19`): a kind and its value.
/// The kinds are the platform's, not aledgent's; the value is opaque bytes.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Identity {
    /// What the identity is (`vos-program-id`; `elf-hash`, `program-hash`, `transpiler-version`).
    pub kind: String,
    /// Its value.
    pub value: Vec<u8>,
}

/// How an artefact was built.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BuildProfile {
    /// The target triple or target specification (`riscv64em-vos`, `x86_64-unknown-linux-gnu`).
    pub target: String,
    /// The toolchain (`nightly-2026-03-20`, `1.98.0`).
    pub toolchain: String,
    /// The `opt-level` (`3`, `s`).
    pub opt_level: String,
    /// The Cargo features, sorted.
    pub features: Vec<String>,
    /// The source revision (a commit).
    pub source_revision: String,
    /// The digest of the `Cargo.lock` it was built from.
    pub cargo_lock: [u8; 32],
}

/// One artefact.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Artefact {
    /// What it is.
    pub role: Role,
    /// [`artefact_digest`] of its bytes.
    pub digest: [u8; 32],
    /// Its length.
    pub len: u64,
    /// Every identity the platform knows it by, sorted by kind, each kind once.
    pub identities: Vec<Identity>,
    /// How it was built.
    pub profile: BuildProfile,
}

/// The kernel's identity: informative, since the kernel is compiled into the artefacts and their
/// digests bind it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct KernelIdentity {
    /// CipherClerk's revision.
    pub revision: String,
    /// Its crate checksum, if the build has one.
    pub checksum: Option<[u8; 32]>,
}

/// A code manifest (module documentation).
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CodeManifest {
    /// The writer it runs.
    pub writer: WriterKind,
    /// The platform it runs on.
    pub platform: Platform,
    /// The code generation it runs as (amendment 0033, D1): every package it names carries it
    /// compiled in ([`generation_of`]).
    pub generation: u64,
    /// Its artefacts, at most one per role, in role order.
    pub artefacts: Vec<Artefact>,
    /// The kernel's identity.
    pub kernel: KernelIdentity,
    /// The constructions of the state commitment it reads (`DEC-25`: 1 and 2).
    pub constructions: Vec<u8>,
    /// The message formats it accepts and emits (`REQ-RT-13`).
    pub messages: Vec<String>,
}

/// Why running code is not a manifest's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodeMismatch {
    /// The manifest names an artefact of this role and none was given.
    Missing(Role),
    /// The bytes given for this role are not the manifest's.
    Digest(Role),
    /// The package's compiled generation could not be read: folded away, or duplicated.
    Generation(GenerationError),
    /// The package's compiled generation is not the manifest's.
    GenerationDiffers {
        /// The manifest's `generation`.
        manifest: u64,
        /// The package's compiled one.
        package: u64,
    },
}

impl CodeManifest {
    /// Its canonical bytes, the magic first.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut e = Enc::new();
        e.raw(MAGIC).put(self);
        e.into_bytes()
    }

    /// A manifest from its canonical bytes.
    pub fn from_bytes(b: &[u8]) -> Result<CodeManifest, DecodeError> {
        let rest = b
            .strip_prefix(MAGIC)
            .ok_or(DecodeError("not a code manifest"))?;
        crate::export::canon::from_bytes(rest)
    }

    /// The code version: `blake2b-256(manifest bytes)`.
    pub fn version(&self) -> [u8; 32] {
        artefact_digest(&self.to_bytes())
    }

    /// The artefact of `role`, if the manifest names one.
    pub fn artefact(&self, role: Role) -> Option<&Artefact> {
        self.artefacts.iter().find(|a| a.role == role)
    }

    /// Refuse unless the package's code carries this manifest's generation compiled in (D1, D13):
    /// the build-time check (`check-code`), staging and the execution node's checks run it. `code`
    /// is what [`generation_of`] reads: the package's ELF, or the standard program it carries (a
    /// host reads it out of a VOS3 package first).
    pub fn check_generation(&self, code: &[u8]) -> Result<(), CodeMismatch> {
        let compiled = generation_of(code).map_err(CodeMismatch::Generation)?;
        if compiled != self.generation {
            return Err(CodeMismatch::GenerationDiffers {
                manifest: self.generation,
                package: compiled,
            });
        }
        Ok(())
    }

    /// Check running code against the manifest: every artefact it names must be given, as bytes
    /// whose digest and length are the manifest's. Roles the manifest does not name are ignored.
    /// The package's compiled generation is [`CodeManifest::check_generation`]'s, on the program
    /// the package carries.
    pub fn check(&self, given: &[(Role, &[u8])]) -> Result<(), CodeMismatch> {
        for a in &self.artefacts {
            let bytes = given
                .iter()
                .find(|(r, _)| *r == a.role)
                .map(|(_, b)| *b)
                .ok_or(CodeMismatch::Missing(a.role))?;
            if bytes.len() as u64 != a.len || artefact_digest(bytes) != a.digest {
                return Err(CodeMismatch::Digest(a.role));
            }
        }
        Ok(())
    }
}

/// Amendment 0023's first manifest layout (before amendment 0033): one VOS `ProgramId` per
/// artefact, no platform and no code generation. Format-4 chainspecs carry it (the books founded
/// before amendment 0033), and this build still **reads** it, so that ingress still serves those books
/// and `opsctl` reads their chainspec. Nothing drives a writer under it: the core host founds and
/// drives only books whose chainspec names a [`CodeManifest`] (`chainspec::SPEC_VERSION`). Nothing
/// else of those books is read: their archives and exports are refused by their format, so they
/// are restored with the `opsctl` inside their own pinned core image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestV4 {
    /// The writer it runs.
    pub writer: WriterKind,
    /// Its artefacts, at most one per role, in role order.
    pub artefacts: Vec<ArtefactV4>,
    /// The kernel's identity.
    pub kernel: KernelIdentity,
    /// The constructions of the state commitment it reads.
    pub constructions: Vec<u8>,
    /// The message formats it accepts and emits.
    pub messages: Vec<String>,
}

/// One artefact of a [`ManifestV4`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArtefactV4 {
    /// What it is.
    pub role: Role,
    /// [`artefact_digest`] of its bytes.
    pub digest: [u8; 32],
    /// Its length.
    pub len: u64,
    /// VOS's `ProgramId` of the program, where it has one.
    pub program_id: Option<[u8; 32]>,
    /// How it was built.
    pub profile: BuildProfile,
}

impl ManifestV4 {
    /// Its code version: `blake2b-256` of the magic and its canonical bytes, as it was computed
    /// when it was written.
    pub fn version(&self) -> [u8; 32] {
        let mut e = Enc::new();
        e.raw(MAGIC).put(self);
        artefact_digest(&e.into_bytes())
    }
}

impl Canonical for ArtefactV4 {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.role)
            .put(&self.digest)
            .put(&self.len)
            .put(&self.program_id)
            .put(&self.profile);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(ArtefactV4 {
            role: d.get()?,
            digest: d.get()?,
            len: d.get()?,
            program_id: d.get()?,
            profile: d.get()?,
        })
    }
}

impl Canonical for ManifestV4 {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.writer)
            .put(&self.artefacts)
            .put(&self.kernel)
            .put(&self.constructions)
            .put(&self.messages);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        let m = ManifestV4 {
            writer: d.get()?,
            artefacts: d.get()?,
            kernel: d.get()?,
            constructions: d.get()?,
            messages: d.get()?,
        };
        if m.artefacts.windows(2).any(|w| w[0].role >= w[1].role) {
            return Err(DecodeError("artefacts not in role order, or a role twice"));
        }
        Ok(m)
    }
}

macro_rules! tag_enum {
    ($t:ident { $($v:ident => $n:expr),+ $(,)? }) => {
        impl Canonical for $t {
            fn encode(&self, e: &mut Enc) {
                e.tag(match self { $($t::$v => $n),+ });
            }
            fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
                match d.tag()? {
                    $($n => Ok($t::$v),)+
                    _ => Err(DecodeError(stringify!($t))),
                }
            }
        }
    };
}

tag_enum!(WriterKind { Core => 1, Shard => 2, Principals => 3, Settlement => 4, Anchor => 5 });
tag_enum!(Role { Package => 1, Runtime => 2, NativeDecide => 3 });

impl Canonical for BuildProfile {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.target)
            .put(&self.toolchain)
            .put(&self.opt_level)
            .put(&self.features)
            .put(&self.source_revision)
            .put(&self.cargo_lock);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        let p = BuildProfile {
            target: d.get()?,
            toolchain: d.get()?,
            opt_level: d.get()?,
            features: d.get()?,
            source_revision: d.get()?,
            cargo_lock: d.get()?,
        };
        if p.features.windows(2).any(|w| w[0] >= w[1]) {
            return Err(DecodeError("features not sorted"));
        }
        Ok(p)
    }
}

/// Identities in canonical order: sorted by kind, each kind once.
fn identities(d: &mut Dec<'_>) -> Result<Vec<Identity>, DecodeError> {
    let ids: Vec<Identity> = d.get()?;
    if ids.windows(2).any(|w| w[0].kind >= w[1].kind) {
        return Err(DecodeError(
            "identities not sorted by kind, or a kind twice",
        ));
    }
    Ok(ids)
}

impl Canonical for Identity {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.kind).put(&self.value);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Identity {
            kind: d.get()?,
            value: d.get()?,
        })
    }
}

impl Canonical for Platform {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.name).put(&self.revision).put(&self.identities);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Platform {
            name: d.get()?,
            revision: d.get()?,
            identities: identities(d)?,
        })
    }
}

impl Canonical for Artefact {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.role)
            .put(&self.digest)
            .put(&self.len)
            .put(&self.identities)
            .put(&self.profile);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Artefact {
            role: d.get()?,
            digest: d.get()?,
            len: d.get()?,
            identities: identities(d)?,
            profile: d.get()?,
        })
    }
}

impl Canonical for KernelIdentity {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.revision).put(&self.checksum);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(KernelIdentity {
            revision: d.get()?,
            checksum: d.get()?,
        })
    }
}

impl Canonical for CodeManifest {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.writer)
            .put(&self.platform)
            .put(&self.generation)
            .put(&self.artefacts)
            .put(&self.kernel)
            .put(&self.constructions)
            .put(&self.messages);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        let m = CodeManifest {
            writer: d.get()?,
            platform: d.get()?,
            generation: d.get()?,
            artefacts: d.get()?,
            kernel: d.get()?,
            constructions: d.get()?,
            messages: d.get()?,
        };
        if m.artefacts.windows(2).any(|w| w[0].role >= w[1].role) {
            return Err(DecodeError("artefacts not in role order, or a role twice"));
        }
        if m.artefact(Role::Package).is_none() {
            return Err(DecodeError(
                "no package: the code that carries the generation",
            ));
        }
        Ok(m)
    }
}

#[cfg(test)]
pub(crate) use tests::manifest as sample;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn profile(target: &str, opt_level: &str) -> BuildProfile {
        BuildProfile {
            target: target.into(),
            toolchain: "nightly-2026-03-20".into(),
            opt_level: opt_level.into(),
            features: vec!["full".into()],
            source_revision: "0123abcd".into(),
            cargo_lock: [9; 32],
        }
    }

    fn id(kind: &str, value: &[u8]) -> Identity {
        Identity {
            kind: kind.into(),
            value: value.to_vec(),
        }
    }

    /// A program of these bytes with generation `g` compiled in.
    pub(crate) fn package(bytes: &[u8], g: u64) -> Vec<u8> {
        let mut ro = bytes.to_vec();
        ro.extend_from_slice(&generation_record(g));
        generation::tests::program(&ro, b"", b"code")
    }

    /// A manifest naming these bytes as its artefacts, on `vos-saga`, generation 0.
    pub(crate) fn manifest(package: &[u8], runtime: &[u8], host: &[u8]) -> CodeManifest {
        let art = |role, bytes: &[u8], target: &str| Artefact {
            role,
            digest: artefact_digest(bytes),
            len: bytes.len() as u64,
            identities: Vec::new(),
            profile: profile(target, "3"),
        };
        CodeManifest {
            writer: WriterKind::Core,
            platform: Platform {
                name: "vos-saga".into(),
                revision: "c54fc1ff13fcd55b86f2cf4edc77b041b62e245f".into(),
                identities: Vec::new(),
            },
            generation: 0,
            artefacts: vec![
                art(Role::Package, package, "riscv64em-vos"),
                art(Role::Runtime, runtime, "riscv64em-vos"),
                art(Role::NativeDecide, host, "aarch64-apple-darwin"),
            ],
            kernel: KernelIdentity {
                revision: "e9a7b08".into(),
                checksum: None,
            },
            constructions: vec![2],
            messages: Vec::new(),
        }
    }

    #[test]
    fn a0023_the_manifest_round_trips_and_its_version_is_its_digest() {
        let m = manifest(b"pkg", b"rt", b"host");
        let b = m.to_bytes();
        assert!(b.starts_with(MAGIC));
        assert_eq!(CodeManifest::from_bytes(&b).unwrap(), m);
        assert_eq!(m.version(), artefact_digest(&b));
        // The unkeyed digest: the empty domain adds nothing.
        assert_ne!(artefact_digest(b"pkg"), artefact_digest(b"pkh"));
        // Every field is in the version: the platform, its revision, the generation.
        let mut o = m.clone();
        o.platform.revision = "b74eeef8".into();
        assert_ne!(o.version(), m.version());
        let mut o = m.clone();
        o.generation = 1;
        assert_ne!(o.version(), m.version());
    }

    /// T-037-01, `AC-K7.6` (its first half): two builds of one source at different `opt-level`s
    /// are two code versions. Their bytes differ, and so do their profiles; either alone is
    /// enough.
    #[test]
    fn t037_01_two_opt_levels_are_two_code_versions() {
        let at_3 = manifest(b"core.vos built at opt-level 3", b"rt", b"host");
        let at_s = manifest(b"core.vos built at opt-level s", b"rt", b"host");
        assert_ne!(at_3.version(), at_s.version());
        // Even were the bytes the same, the profile is in the version.
        let mut same_bytes = at_3.clone();
        same_bytes.artefacts[0].profile.opt_level = "s".into();
        assert_ne!(same_bytes.version(), at_3.version());
    }

    #[test]
    fn a0023_running_code_must_be_the_manifests() {
        let pkg = package(b"pkg", 0);
        let m = manifest(&pkg, b"rt", b"host");
        let ok = [
            (Role::Package, &pkg[..]),
            (Role::Runtime, &b"rt"[..]),
            (Role::NativeDecide, &b"host"[..]),
        ];
        assert_eq!(m.check(&ok), Ok(()));
        let wrong = [
            (Role::Package, &pkg[..]),
            (Role::Runtime, &b"rt2"[..]),
            (Role::NativeDecide, &b"host"[..]),
        ];
        assert_eq!(m.check(&wrong), Err(CodeMismatch::Digest(Role::Runtime)));
        assert_eq!(
            m.check(&ok[..2]),
            Err(CodeMismatch::Missing(Role::NativeDecide))
        );
    }

    /// T-037-01: a package whose compiled `GENERATION` differs from its manifest's fails the
    /// check, and so does one with the record folded away or duplicated.
    #[test]
    fn t037_01_the_compiled_generation_must_be_the_manifests() {
        let one = package(b"pkg", 1);
        let mut m = manifest(&one, b"rt", b"host");
        assert_eq!(
            m.check_generation(&one),
            Err(CodeMismatch::GenerationDiffers {
                manifest: 0,
                package: 1
            })
        );
        m.generation = 1;
        assert_eq!(m.check_generation(&one), Ok(()));
        assert_eq!(
            m.check_generation(&generation::tests::program(b"pkg", b"", b"")),
            Err(CodeMismatch::Generation(GenerationError::Missing))
        );
        let mut two = generation_record(1).to_vec();
        two.extend_from_slice(&generation_record(1));
        let twice = generation::tests::program(&two, b"", b"");
        assert_eq!(
            m.check_generation(&twice),
            Err(CodeMismatch::Generation(GenerationError::Duplicated(2)))
        );
    }

    /// The two platforms' manifests, as `T-037-01` fixes them: `vos-saga` with a runtime artefact
    /// and the `ProgramId`s, `vos-core` with none, the default agent program named under the
    /// platform and the ELF's, derived program's and transpiler's identities on the package.
    fn saga() -> CodeManifest {
        CodeManifest {
            writer: WriterKind::Core,
            platform: Platform {
                name: "vos-saga".into(),
                revision: "c54fc1ff13fcd55b86f2cf4edc77b041b62e245f".into(),
                identities: Vec::new(),
            },
            generation: 0,
            artefacts: vec![
                Artefact {
                    role: Role::Package,
                    digest: [0x11; 32],
                    len: 2_880_661,
                    identities: vec![id("vos-program-id", &[0x21; 32])],
                    profile: profile("riscv64em-vos", "s"),
                },
                Artefact {
                    role: Role::Runtime,
                    digest: [0x12; 32],
                    len: 1_048_576,
                    identities: vec![id("vos-program-id", &[0x22; 32])],
                    profile: profile("riscv64em-vos", "3"),
                },
                Artefact {
                    role: Role::NativeDecide,
                    digest: [0x13; 32],
                    len: 61_000_000,
                    identities: Vec::new(),
                    profile: profile("x86_64-unknown-linux-gnu", "3"),
                },
            ],
            kernel: KernelIdentity {
                revision: "e9a7b08".into(),
                checksum: None,
            },
            constructions: vec![2],
            messages: vec!["aledgent/core/v1".into()],
        }
    }

    fn vos_core() -> CodeManifest {
        CodeManifest {
            writer: WriterKind::Core,
            platform: Platform {
                name: "vos-core".into(),
                revision: "b74eeef8".into(),
                identities: vec![id("default-agent-program-hash", &[0x31; 32])],
            },
            generation: 1,
            artefacts: vec![Artefact {
                role: Role::Package,
                digest: [0x14; 32],
                len: 2_500_000,
                identities: vec![
                    id("elf-hash", &[0x41; 32]),
                    id("program-hash", &[0x42; 32]),
                    id("transpiler-version", b"0.4.0"),
                ],
                profile: profile("riscv64em-vos", "s"),
            }],
            kernel: KernelIdentity {
                revision: "e9a7b08".into(),
                checksum: None,
            },
            constructions: vec![2],
            messages: vec!["aledgent/core/v1".into()],
        }
    }

    fn hex(b: &[u8]) -> alloc::string::String {
        use core::fmt::Write;
        let mut s = alloc::string::String::new();
        for x in b {
            write!(s, "{x:02x}").unwrap();
        }
        s
    }

    /// T-037-01: a manifest naming each runtime, one of them with no `runtime` artefact, encodes
    /// and digests the same on every machine: these digests are fixed here, and every machine
    /// that runs the suite must reach them.
    #[test]
    fn t037_01_each_platforms_manifest_digests_the_same_everywhere() {
        let saga = saga();
        let core = vos_core();
        assert!(core.artefact(Role::Runtime).is_none());
        for m in [&saga, &core] {
            assert_eq!(&CodeManifest::from_bytes(&m.to_bytes()).unwrap(), m);
        }
        assert_eq!(hex(&saga.version()), SAGA_H);
        assert_eq!(hex(&core.version()), VOS_CORE_H);
    }

    const SAGA_H: &str = "85f8fa758a643cbf83a0db2a775db9e0af206e6a4942b560ea1be9cdb6ae1a94";
    const VOS_CORE_H: &str = "b07b75b31b7248a1cb7657686acc70215543e2e65213c51a618047f8c6ddf622";

    /// The canonical order is part of the format: identities sorted by kind, each kind once; a
    /// manifest with no package is not one.
    #[test]
    fn t037_01_a_manifest_out_of_canonical_order_does_not_decode() {
        let mut m = vos_core();
        m.artefacts[0].identities.swap(0, 1);
        assert!(CodeManifest::from_bytes(&m.to_bytes()).is_err());
        let mut m = vos_core();
        m.platform
            .identities
            .push(id("default-agent-program-hash", &[0; 32]));
        assert!(CodeManifest::from_bytes(&m.to_bytes()).is_err());
        let mut m = saga();
        m.artefacts.remove(0);
        assert!(CodeManifest::from_bytes(&m.to_bytes()).is_err());
    }
}
