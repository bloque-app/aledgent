// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The `saga` half of `switch(H, n)` (`T-037-14`, amendment 0033): the code versions the host holds
//! staged, and the install of one of them into the running Agent.
//!
//! [`core_binding::switch`] decides *whether* the platform must change (the target version, the
//! digests, the compare-and-swap); this module is *how* `saga` changes:
//!
//! - **Staged versions** ([`StagedVersions`]): for each code version *H*, its manifest and the bytes
//!   of its package and runtime. A version enters only through [`StagedVersions::insert`], which
//!   checks what staging checks: the manifest names a package and a runtime of the bytes' digests
//!   and lengths, and the package carries the manifest's code generation compiled in. Fetching the
//!   bytes (`T-037-06`) and the `Upgrade` input that stages them (`T-037-04`) come later; the
//!   switch installs only from this store.
//! - **The install** (`impl CodeInstaller for VosPort`): the runtime's in-place upgrade of the
//!   installed actor ([`crate::driver::Runtime::upgrade_actor`]), a compare-and-swap on the
//!   installed deployment, authorised by a receipt the Agent's authority key signs through the
//!   signer, over a package the package key signed (its producer): no key is in this process.
//!   It installs the actor package only. A version whose runtime differs from the running one is
//!   refused ([`InstallError::Cannot`]): `saga` runs its runtime in this process, created once with
//!   the runtime program; changing it is a new process, which is the deployment's restart.
//!
//! What the host runs is what it loaded: the digests of the package and runtime bytes it was
//! started with ([`VosPort::track_code`]), updated by each install, never what a registry says.

use std::collections::HashMap;

use core_binding::switch::{Artefacts, CodeInstaller, InstallError, StagedCode};
use ledger_core::code::{CodeManifest, Role, artefact_digest, generation_of};

/// One staged code version: its manifest and the bytes of its package and runtime.
#[derive(Clone, Debug)]
pub struct StagedVersion {
    /// The code version's manifest (`aledgent/code/v1`).
    pub manifest: CodeManifest,
    /// The actor package (`core.vos`), the signed VOS3 bytes.
    pub package: Vec<u8>,
    /// The runtime program (`agent_runtime.pvm`).
    pub runtime: Vec<u8>,
}

impl StagedVersion {
    /// The digests of the package and the runtime.
    #[must_use]
    pub fn artefacts(&self) -> Artefacts {
        Artefacts {
            package: artefact_digest(&self.package),
            runtime: artefact_digest(&self.runtime),
        }
    }
}

/// The code versions staged for this host, by version.
#[derive(Clone, Debug, Default)]
pub struct StagedVersions(HashMap<[u8; 32], StagedVersion>);

impl StagedVersions {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Stage a code version: its `manifest`, and the `package` and `runtime` bytes it names.
    /// Returns the version, `blake2b-256` of the manifest.
    ///
    /// # Errors
    ///
    /// The manifest names no package or no runtime, a digest or length differs from the bytes', or
    /// the package does not carry the manifest's code generation compiled in.
    pub fn insert(
        &mut self,
        manifest: CodeManifest,
        package: Vec<u8>,
        runtime: Vec<u8>,
    ) -> Result<[u8; 32], String> {
        for (role, bytes) in [(Role::Package, &package), (Role::Runtime, &runtime)] {
            let a = manifest
                .artefact(role)
                .ok_or_else(|| format!("the manifest names no {role:?}"))?;
            if a.digest != artefact_digest(bytes) || a.len != bytes.len() as u64 {
                return Err(format!("the {role:?} bytes are not the manifest's"));
            }
        }
        // The package is a VOS3 envelope; its program carries the generation record.
        let program = crate::package::admit_actor(&package)
            .map_err(|e| format!("the package is not admitted: {e:?}"))?;
        let compiled =
            generation_of(program.program_bytes()).map_err(|e| format!("generation: {e:?}"))?;
        if compiled != manifest.generation {
            return Err(format!(
                "the package carries code generation {compiled}, its manifest names {}",
                manifest.generation
            ));
        }
        let version = manifest.version();
        self.0.insert(
            version,
            StagedVersion {
                manifest,
                package,
                runtime,
            },
        );
        Ok(version)
    }

    /// The staged version `code`, if any.
    #[must_use]
    pub fn get(&self, code: &[u8; 32]) -> Option<&StagedVersion> {
        self.0.get(code)
    }
}

impl StagedCode for StagedVersions {
    fn artefacts(&self, code: &[u8; 32]) -> Option<Artefacts> {
        self.get(code).map(StagedVersion::artefacts)
    }
}

/// What a port tracks of the code it runs (`T-037-14`): the actor's name, the digests of the
/// package and runtime bytes the host loaded, and the staged versions it may switch to.
pub struct CodeState {
    /// The installed actor's name.
    pub name: String,
    /// The digests of what the host runs now.
    pub running: Artefacts,
    /// The versions staged for this host.
    pub staged: StagedVersions,
}

impl CodeState {
    /// Track the code `package` and `runtime` bytes the host loaded, under the actor `name`.
    #[must_use]
    pub fn new(name: &str, package: &[u8], runtime: &[u8], staged: StagedVersions) -> Self {
        Self {
            name: name.into(),
            running: Artefacts {
                package: artefact_digest(package),
                runtime: artefact_digest(runtime),
            },
            staged,
        }
    }
}

impl CodeInstaller for crate::port::VosPort {
    fn running(&mut self) -> Result<Artefacts, InstallError> {
        self.code_state()
            .map(|c| c.running)
            .ok_or_else(|| InstallError::Cannot("this port tracks no code".into()))
    }

    fn install(&mut self, from: Artefacts, code: &[u8; 32]) -> Result<(), InstallError> {
        let Some(state) = self.code_state() else {
            return Err(InstallError::Cannot("this port tracks no code".into()));
        };
        // The compare-and-swap: what the host read is not what it runs.
        if state.running != from {
            return Err(InstallError::Stale {
                running: state.running,
            });
        }
        let Some(version) = state.staged.get(code).cloned() else {
            return Err(InstallError::Cannot("the version was not staged".into()));
        };
        let want = version.artefacts();
        if want.runtime != state.running.runtime {
            return Err(InstallError::Cannot(
                "saga installs the actor package only: the version's runtime differs from the \
                 one this process runs (a runtime change is a restart)"
                    .into(),
            ));
        }
        let name = state.name.clone();
        let package = crate::package::admit_actor(&version.package)
            .map_err(|e| InstallError::Platform(format!("the package is not admitted: {e:?}")))?;
        self.upgrade_actor(&package, &name)
            .map_err(|e| InstallError::Platform(format!("{e:?}")))?;
        if let Some(c) = self.code_state_mut() {
            c.running.package = want.package;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ledger_core::code::{
        Artefact, BuildProfile, CodeManifest, KernelIdentity, Platform, WriterKind,
    };

    fn manifest(package: &[u8], runtime: &[u8]) -> CodeManifest {
        let profile = || BuildProfile {
            target: "riscv64em-vos".into(),
            toolchain: "nightly".into(),
            opt_level: "s".into(),
            features: Vec::new(),
            source_revision: "0123abcd".into(),
            cargo_lock: [0; 32],
        };
        CodeManifest {
            writer: WriterKind::Core,
            platform: Platform {
                name: "vos-saga".into(),
                revision: "c54fc1ff".into(),
                identities: Vec::new(),
            },
            generation: 0,
            artefacts: vec![
                Artefact {
                    role: Role::Package,
                    digest: artefact_digest(package),
                    len: package.len() as u64,
                    identities: Vec::new(),
                    profile: profile(),
                },
                Artefact {
                    role: Role::Runtime,
                    digest: artefact_digest(runtime),
                    len: runtime.len() as u64,
                    identities: Vec::new(),
                    profile: profile(),
                },
            ],
            kernel: KernelIdentity {
                revision: "e9a7b08".into(),
                checksum: None,
            },
            constructions: Vec::new(),
            messages: Vec::new(),
        }
    }

    #[test]
    fn t037_14_a_version_enters_only_with_the_bytes_its_manifest_names() {
        let mut s = StagedVersions::new();
        // Bytes that are not the manifest's digests.
        let m = manifest(b"package", b"runtime");
        let err = s.insert(m.clone(), b"other".to_vec(), b"runtime".to_vec());
        assert!(err.unwrap_err().contains("not the manifest's"));
        let err = s.insert(m.clone(), b"package".to_vec(), b"other".to_vec());
        assert!(err.unwrap_err().contains("not the manifest's"));
        // A manifest without a runtime.
        let mut no_runtime = m;
        no_runtime.artefacts.retain(|a| a.role != Role::Runtime);
        let err = s.insert(no_runtime, b"package".to_vec(), b"runtime".to_vec());
        assert!(err.unwrap_err().contains("names no"));
        // Nothing was staged.
        assert!(s.artefacts(&[0; 32]).is_none());
    }

    #[test]
    fn t037_14_the_digests_of_a_staged_version_are_its_bytes() {
        let v = StagedVersion {
            manifest: manifest(b"p", b"r"),
            package: b"p".to_vec(),
            runtime: b"r".to_vec(),
        };
        assert_eq!(
            v.artefacts(),
            Artefacts {
                package: artefact_digest(b"p"),
                runtime: artefact_digest(b"r"),
            }
        );
    }
}
