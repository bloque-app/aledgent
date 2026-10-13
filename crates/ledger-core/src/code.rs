// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The code manifest (amendment 0023, draft, PR #201: "code history and upgrades", proposed
//! `DEC-26` (a)): what names the code that decides a writer's transitions. This module implements
//! the manifest that amendment describes, for the one use `F-036` needs now: **the chainspec names its
//! genesis code** (the genesis code version *H₀*), as Substrate's genesis carries `:code`, and the core
//! host refuses to build or install a genesis under other binaries.
//!
//! As amendment 0023 defines it:
//!
//! - an **artefact digest** is `blake2b-256(bytes)`, unkeyed, over the artefact's exact bytes: the
//!   hash JAM's `lookup` and Substrate's preimage pallet use. VOS's `ProgramId` is recorded beside it;
//! - the **manifest** is a canonical encoding that starts with the magic `aledgent/code/v1` and lists,
//!   for one writer kind, each artefact that decides a transition (`role`: `package`, `runtime`,
//!   `native-decide`; `digest`, `len`, `ProgramId`) with its **build profile** (target, toolchain,
//!   `opt-level`, Cargo features, source revision, the digest of `Cargo.lock`), the kernel's identity
//!   (informative), and the constructions it reads and the message formats it accepts and emits;
//! - the **code version** *H* is `blake2b-256(manifest bytes)`.
//!
//! The native genesis build is a transition the host decides natively, so the core host's own
//! executable is the manifest's `native-decide` artefact for a chainspec book.
//!
//! The encoding is `F-014`'s canonical one, so this module's bytes are the manifest's bytes; when
//! `F-037` lands the rest of amendment 0023 (the hot row's `:code`, upgrades, the code map), it uses
//! this type.

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
    /// The writer's actor package (`core.vos`).
    Package,
    /// The PVM runtime program.
    Runtime,
    /// A host executable that decides a transition natively (the chainspec genesis's build).
    NativeDecide,
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
    /// VOS's `ProgramId` of the program, where it has one.
    pub program_id: Option<[u8; 32]>,
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
    /// Its artefacts, at most one per role.
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

    /// Check running code against the manifest: every artefact it names must be given, as bytes
    /// whose digest and length are the manifest's. Roles the manifest does not name are ignored.
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

impl Canonical for Artefact {
    fn encode(&self, e: &mut Enc) {
        e.put(&self.role)
            .put(&self.digest)
            .put(&self.len)
            .put(&self.program_id)
            .put(&self.profile);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        Ok(Artefact {
            role: d.get()?,
            digest: d.get()?,
            len: d.get()?,
            program_id: d.get()?,
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
            .put(&self.artefacts)
            .put(&self.kernel)
            .put(&self.constructions)
            .put(&self.messages);
    }
    fn decode(d: &mut Dec<'_>) -> Result<Self, DecodeError> {
        let m = CodeManifest {
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

#[cfg(test)]
pub(crate) use tests::manifest as sample;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// A manifest naming these bytes as its artefacts.
    pub(crate) fn manifest(package: &[u8], runtime: &[u8], host: &[u8]) -> CodeManifest {
        let profile = |target: &str| BuildProfile {
            target: target.into(),
            toolchain: "nightly-2026-03-20".into(),
            opt_level: "3".into(),
            features: vec!["full".into()],
            source_revision: "0123abcd".into(),
            cargo_lock: [9; 32],
        };
        let art = |role, bytes: &[u8], target: &str| Artefact {
            role,
            digest: artefact_digest(bytes),
            len: bytes.len() as u64,
            program_id: None,
            profile: profile(target),
        };
        CodeManifest {
            writer: WriterKind::Core,
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
        // Another opt-level is another code version.
        let mut o = m.clone();
        o.artefacts[0].profile.opt_level = "s".into();
        assert_ne!(o.version(), m.version());
    }

    #[test]
    fn a0023_running_code_must_be_the_manifests() {
        let m = manifest(b"pkg", b"rt", b"host");
        let ok = [
            (Role::Package, &b"pkg"[..]),
            (Role::Runtime, &b"rt"[..]),
            (Role::NativeDecide, &b"host"[..]),
        ];
        assert_eq!(m.check(&ok), Ok(()));
        let wrong = [
            (Role::Package, &b"pkg"[..]),
            (Role::Runtime, &b"rt2"[..]),
            (Role::NativeDecide, &b"host"[..]),
        ];
        assert_eq!(m.check(&wrong), Err(CodeMismatch::Digest(Role::Runtime)));
        assert_eq!(
            m.check(&ok[..2]),
            Err(CodeMismatch::Missing(Role::NativeDecide))
        );
    }
}
