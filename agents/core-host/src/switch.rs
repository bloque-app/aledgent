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

/// One staged code version: the bytes of its package and runtime, which [`StagedVersions::insert`]
/// checked against its manifest.
#[derive(Clone, Debug)]
pub struct StagedVersion {
    /// The actor package (`core.vos`), the signed VOS3 bytes.
    pub package: Vec<u8>,
    /// The runtime program (`agent_runtime.pvm`). On `saga` the install changes the package only;
    /// a runtime change is a restart, which reads these bytes (`T-037-06`).
    #[cfg_attr(not(test), allow(dead_code))]
    pub runtime: Vec<u8>,
    /// Their digests, hashed once when the version is held (megabytes, on every switch otherwise).
    artefacts: Artefacts,
}

impl StagedVersion {
    /// A version held with the bytes of its package and runtime.
    #[must_use]
    pub fn new(package: Vec<u8>, runtime: Vec<u8>) -> Self {
        let artefacts = Artefacts {
            package: artefact_digest(&package),
            runtime: artefact_digest(&runtime),
        };
        Self {
            package,
            runtime,
            artefacts,
        }
    }

    /// The digests of the package and the runtime.
    #[must_use]
    pub fn artefacts(&self) -> Artefacts {
        self.artefacts
    }
}

/// The code versions staged for this host, by version, and the version it runs now.
#[derive(Clone, Debug, Default)]
pub struct StagedVersions {
    versions: HashMap<[u8; 32], StagedVersion>,
}

/// The digests of every version a store holds: what [`core_binding::switch::ensure`] reads while
/// the port itself is the installer.
#[derive(Clone, Debug, Default)]
pub struct StagedDigests(HashMap<[u8; 32], Artefacts>);

impl StagedCode for StagedDigests {
    fn artefacts(&self, code: &[u8; 32]) -> Option<Artefacts> {
        self.0.get(code).copied()
    }
}

impl StagedVersions {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The version this host runs, whose `package` and `runtime` bytes the start check verified
    /// against the version's manifest ([`crate::code::check`]). Its bytes are kept: after a way-out
    /// at *n* the host runs it again, so it must be able to reinstall it (D17).
    pub fn running(&mut self, version: [u8; 32], package: &[u8], runtime: &[u8]) {
        self.versions.insert(
            version,
            StagedVersion::new(package.to_vec(), runtime.to_vec()),
        );
    }

    /// The digests of every version held.
    #[must_use]
    pub fn digests(&self) -> StagedDigests {
        StagedDigests(
            self.versions
                .iter()
                .map(|(k, v)| (*k, v.artefacts()))
                .collect(),
        )
    }

    /// Stage a code version: its `manifest`, and the `package` and `runtime` bytes it names.
    /// Returns the version, `blake2b-256` of the manifest.
    ///
    /// # Errors
    ///
    /// The manifest names no package or no runtime, a digest or length differs from the bytes', or
    /// the package does not carry the manifest's code generation compiled in.
    #[cfg_attr(not(test), allow(dead_code))] // staging arrives with `T-037-04` and `T-037-06`
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
        self.versions
            .insert(version, StagedVersion::new(package, runtime));
        Ok(version)
    }

    /// The staged version `code`, if any.
    #[must_use]
    pub fn get(&self, code: &[u8; 32]) -> Option<&StagedVersion> {
        self.versions.get(code)
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
    /// The retry bound of a switch that fails ([`Park`]).
    pub park: Park,
}

/// The most failed attempts at one target before the host stops trying until it restarts.
pub const PARK_MAX_FAILURES: u32 = 8;
/// The wait after the first failure, doubled after each, to [`PARK_MAX_WAIT_MS`].
pub const PARK_FIRST_WAIT_MS: u64 = 30_000;
/// The longest wait between two attempts.
pub const PARK_MAX_WAIT_MS: u64 = 900_000;

/// The park of a failing switch (`T-037-14`, review of #44, F4): the host's driver asks the port to
/// run a version before every send, and a failing install is retried by the forwarder at once. On
/// `saga` each attempt at an upgrade the runtime refuses takes an authority slot of the runtime
/// that never compacts (256 in all), so the attempts are bounded: after a failure the same target
/// is not tried again for a while (30 s, doubling to 15 min), and after
/// [`PARK_MAX_FAILURES`] it is not tried until the host restarts (an operator looks at it).
#[derive(Clone, Debug, Default)]
pub struct Park {
    target: Option<[u8; 32]>,
    /// Failed attempts at the target, of any kind: they set the wait.
    failures: u32,
    /// Those a retry cannot settle (the platform refused the install, the version is not staged,
    /// the hash is stale): after [`PARK_MAX_FAILURES`] of them the host stops trying. A failure of
    /// the platform or the signer is transient and never stops it.
    refusals: u32,
    next_ms: u64,
    why: String,
}

impl Park {
    /// Why an attempt at `target` must not be made at `now_ms` (milliseconds on a monotonic clock),
    /// if it must not.
    #[must_use]
    pub fn blocked(&self, target: &[u8; 32], now_ms: u64) -> Option<String> {
        if self.target.as_ref() != Some(target) {
            return None;
        }
        if self.refusals >= PARK_MAX_FAILURES {
            return Some(format!(
                "parked after {} refused attempts, until the host restarts: {}",
                self.refusals, self.why
            ));
        }
        (now_ms < self.next_ms).then(|| {
            format!(
                "parked for another {} s after {} failed attempts: {}",
                (self.next_ms - now_ms).div_ceil(1000),
                self.failures,
                self.why
            )
        })
    }

    /// An attempt at `target` failed at `now_ms`; `transient`: a retry may settle it.
    pub fn failed(&mut self, target: [u8; 32], why: String, transient: bool, now_ms: u64) {
        if self.target != Some(target) {
            self.failures = 0;
            self.refusals = 0;
        }
        self.target = Some(target);
        self.failures = self.failures.saturating_add(1);
        if !transient {
            self.refusals = self.refusals.saturating_add(1);
        }
        let wait = PARK_FIRST_WAIT_MS
            .saturating_mul(1u64 << (self.failures - 1).min(20))
            .min(PARK_MAX_WAIT_MS);
        self.next_ms = now_ms.saturating_add(wait);
        self.why = why;
    }

    /// The platform runs `target`, as asked: what was parked for it is cleared. A park held for
    /// another target stays (a way-out's reinstall of *g* must not wipe the park of *H*).
    pub fn succeeded(&mut self, target: &[u8; 32]) {
        if self.target.as_ref() == Some(target) {
            *self = Self::default();
        }
    }
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
            park: Park::default(),
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
        self.upgrade_actor(&package, &name).map_err(install_error)?;
        if let Some(c) = self.code_state_mut() {
            c.running.package = want.package;
        }
        Ok(())
    }
}

/// How a failed `UpgradeActor` is told to the switch: the runtime's own refusal (it decided, and
/// would decide again: `UnsupportedLane` on a stateful actor) is a refusal the park counts toward
/// its stop; the runtime program failing is the platform's, which a retry may settle.
fn install_error(e: crate::driver::DriverError) -> InstallError {
    use crate::driver::DriverError;
    match e {
        DriverError::Refused(why) => InstallError::Cannot(format!("the runtime refused it: {why}")),
        other => InstallError::Platform(other.to_string()),
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
    fn t037_14_a_failing_switch_is_parked_with_a_growing_wait_and_a_limit() {
        let t = [7u8; 32];
        let mut p = Park::default();
        assert!(p.blocked(&t, 0).is_none(), "nothing failed yet");
        p.failed(t, "UnsupportedLane".into(), false, 1_000);
        // Parked for 30 s: no attempt is made, the reason is the failure's.
        let why = p.blocked(&t, 1_001).expect("parked");
        assert!(
            why.contains("UnsupportedLane") && why.contains("30"),
            "{why}"
        );
        assert!(
            p.blocked(&t, 1_000 + PARK_FIRST_WAIT_MS).is_none(),
            "the wait is over"
        );
        // Another target is not parked.
        assert!(p.blocked(&[8; 32], 1_001).is_none());
        // The wait doubles and stops at the cap.
        for i in 2..=PARK_MAX_FAILURES {
            p.failed(t, "UnsupportedLane".into(), false, 0);
            let wait = PARK_FIRST_WAIT_MS
                .saturating_mul(1u64 << (i - 1))
                .min(PARK_MAX_WAIT_MS);
            if i < PARK_MAX_FAILURES {
                assert!(p.blocked(&t, wait - 1).is_some(), "attempt {i}");
                assert!(p.blocked(&t, wait).is_none(), "attempt {i}");
            }
        }
        // After the limit nothing is tried until the host restarts, whatever the time.
        assert!(
            p.blocked(&t, u64::MAX)
                .unwrap()
                .contains("until the host restarts")
        );
        // A failure of the platform or the signer never stops the host: only the wait grows.
        let mut q = Park::default();
        for _ in 0..(PARK_MAX_FAILURES * 3) {
            q.failed(t, "signer down".into(), true, 0);
        }
        assert!(q.blocked(&t, u64::MAX).is_none());
        assert!(q.blocked(&t, PARK_MAX_WAIT_MS - 1).is_some());
        // A success clears it.
        p.succeeded(&t);
        assert!(p.blocked(&t, 0).is_none());
    }

    #[test]
    fn t037_14_a_success_for_another_target_keeps_the_park_of_h() {
        let (h, g) = ([1u8; 32], [2u8; 32]);
        let mut p = Park::default();
        p.failed(h, "refused".into(), false, 0);
        p.succeeded(&g);
        assert!(
            p.blocked(&h, 0).is_some(),
            "g's reinstall does not wipe H's park"
        );
        p.succeeded(&h);
        assert!(p.blocked(&h, 0).is_none());
    }

    #[test]
    fn t037_14_the_runtimes_refusal_counts_as_a_refusal_and_its_failure_does_not() {
        use crate::driver::DriverError;
        assert!(matches!(
            install_error(DriverError::Refused("UnsupportedLane".into())),
            InstallError::Cannot(_)
        ));
        assert!(matches!(
            install_error(DriverError::Runtime("signer down".into())),
            InstallError::Platform(_)
        ));
    }

    #[test]
    fn t037_14_the_running_version_keeps_its_bytes_so_g_can_be_reinstalled() {
        let mut s = StagedVersions::new();
        s.running([1; 32], b"package", b"runtime");
        let v = s.get(&[1; 32]).expect("kept");
        assert_eq!(
            (v.package.as_slice(), v.runtime.as_slice()),
            (&b"package"[..], &b"runtime"[..])
        );
        assert_eq!(s.digests().artefacts(&[1; 32]), Some(v.artefacts()));
    }

    #[test]
    fn t037_14_the_digests_of_a_staged_version_are_its_bytes() {
        let v = StagedVersion::new(b"p".to_vec(), b"r".to_vec());
        assert_eq!(
            v.artefacts(),
            Artefacts {
                package: artefact_digest(b"p"),
                runtime: artefact_digest(b"r"),
            }
        );
    }
}
