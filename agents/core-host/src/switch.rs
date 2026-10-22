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
//!   and lengths, and the package carries the manifest's code generation compiled in. The `Upgrade`
//!   input stages them before it is logged (`T-037-04`, [`core_binding::staging`]); the sources
//!   that fetch them (`T-037-06`) come later; the switch installs only from this store.
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

    /// An attempt at `target` was refused for good (`T-037-19`: the runtime will not take the
    /// package as a revert, and it is no claim): the host does not try it again until it restarts.
    pub fn close(&mut self, target: [u8; 32], why: String) {
        self.target = Some(target);
        self.failures = self.failures.max(1);
        self.refusals = PARK_MAX_FAILURES;
        self.next_ms = u64::MAX;
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
        if let Err(e) = self.upgrade_actor(&package, &name) {
            if let (crate::driver::DriverError::RevertClosed(why), Some(c)) =
                (&e, self.code_state_mut())
            {
                // Final, not transient: a retry would take another authority slot of the runtime
                // (256 in all, never compacted) for the same answer.
                eprintln!("aledgent-core-host: switch to {}: {why}", hex::encode(code));
                c.park.close(*code, why.clone());
            }
            return Err(install_error(e));
        }
        if let Some(c) = self.code_state_mut() {
            c.running.package = want.package;
        }
        Ok(())
    }
}

/// What the door needs to know of a code version to say whether the switch could install it
/// (`T-037-22`): the digest of the runtime it names and the key its package is signed with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// The digest of the runtime the version carries; `None` if it carries none.
    pub runtime: Option<[u8; 32]>,
    /// The producer of its package (the package key's identity).
    pub producer: vos::agent_sdk::ProducerId,
    /// Whether its package may replace the program running: it is that very program, or it carries
    /// the producer's signed claim over it ([`claims_running`]).
    pub claims_running_program: bool,
}

/// Whether `candidate`'s package may replace the program `running`'s package holds: the same
/// program (nothing changes), or a claim, signed with the package, to read the state of the program
/// running (`T-037-19`: `state_compatible_with`). The runtime refuses a program change without it.
///
/// # Errors
///
/// A package that is not admitted.
pub fn claims_running(running: &[u8], candidate: &[u8]) -> Result<bool, String> {
    let admit = |bytes: &[u8]| {
        crate::package::admit_actor(bytes).map_err(|e| format!("the package is not admitted: {e}"))
    };
    let running = admit(running)?.program();
    let candidate = admit(candidate)?;
    Ok(candidate.program() == running
        || candidate
            .manifest()
            .state_compatible_with
            .contains(&running))
}

/// Whether `install` (above) would accept `candidate` while the host runs `running` from a package
/// signed by `running_producer` (`T-037-22`, owner ruling w16, proposal 4). The same conditions the
/// switch checks at its input, asked at the door, from what the host already knows, so a version
/// the switch would refuse never gets recorded:
///
/// 1. the runtime is the one this process runs (`saga` installs the package only: a runtime
///    change is a restart);
/// 2. the package is signed by the producer of the package running (the runtime refuses an upgrade
///    that changes the producer).
///
/// 3. the package changes the program only with the producer's signed claim over the program
///    running (`T-037-19`, the patched VOS's `state_compatible_with`): a version built without
///    `--previous` would be refused by the runtime at the switch. (The line `T-037-22` left as a
///    hook for PR #49, filled on the rehearsal integration branch.)
///
/// # Errors
///
/// Why the switch could not install `candidate`, for the operator's log.
pub fn installable(
    running: Artefacts,
    running_producer: vos::agent_sdk::ProducerId,
    candidate: Candidate,
) -> Result<(), String> {
    match candidate.runtime {
        None => return Err("the manifest names no runtime".into()),
        Some(r) if r != running.runtime => {
            return Err(
                "it names another runtime than the one this host runs (a runtime change is a \
                 restart of the host, not a switch)"
                    .into(),
            );
        }
        Some(_) => {}
    }
    if candidate.producer != running_producer {
        return Err(
            "its package is signed by another key than the package running (the runtime refuses \
             an upgrade that changes the producer)"
                .into(),
        );
    }
    if !candidate.claims_running_program {
        return Err(
            "its package carries no claim over the program running (build it with \
             `package --previous <the package running>`): the runtime refuses to change the \
             program of a stateful actor without it"
                .into(),
        );
    }
    Ok(())
}

impl CodeState {
    /// Whether the switch could install `checked` as this host runs now ([`installable`]): reads
    /// the producer of the package running from the bytes the host holds for it (`T-037-22`).
    ///
    /// # Errors
    ///
    /// Why not, for the operator's log.
    pub fn check_install(&self, checked: &core_binding::staging::Checked) -> Result<(), String> {
        let running = self
            .staged
            .versions
            .values()
            .find(|v| v.artefacts().package == self.running.package)
            .ok_or("the package this host runs is not held, so its key cannot be compared")?;
        let producer = |bytes: &[u8]| {
            crate::package::admit_actor(bytes)
                .map(|p| p.producer())
                .map_err(|e| format!("the package is not admitted: {e}"))
        };
        installable(
            self.running,
            producer(&running.package)?,
            Candidate {
                runtime: checked.runtime.as_deref().map(artefact_digest),
                producer: producer(&checked.package)?,
                claims_running_program: claims_running(&running.package, &checked.package)?,
            },
        )
    }
}

/// How a failed `UpgradeActor` is told to the switch: the runtime's own refusal (it decided, and
/// would decide again: `UnsupportedLane` on a stateful actor) is a refusal the park counts toward
/// its stop (so does the revert of a claimed switch after state was written under the new program:
/// the switch took effect and stays, `T-037-19`); the runtime program failing is the platform's, which a retry may settle.
fn install_error(e: crate::driver::DriverError) -> InstallError {
    use crate::driver::DriverError;
    match e {
        DriverError::Refused(why) | DriverError::RevertClosed(why) => {
            InstallError::Cannot(format!("the runtime refused it: {why}"))
        }
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
                revision: "bd8d1bb7".into(),
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

    /// `T-037-22`: the door asks what the switch would ask. The same runtime and the same package
    /// key pass; another runtime, no runtime, or another key do not, each with its own reason.
    #[test]
    fn t037_22_the_door_refuses_what_the_switch_could_not_install() {
        use vos::agent_sdk::ProducerId;
        let running = Artefacts {
            package: [1; 32],
            runtime: [9; 32],
        };
        let key = ProducerId::of_public_key(&[0x71; 32]);
        let other_key = ProducerId::of_public_key(&[0x72; 32]);
        let same = Candidate {
            runtime: Some([9; 32]),
            producer: key,
            claims_running_program: true,
        };
        assert_eq!(installable(running, key, same), Ok(()));
        let why = installable(
            running,
            key,
            Candidate {
                runtime: Some([8; 32]),
                ..same
            },
        )
        .unwrap_err();
        assert!(why.contains("another runtime"), "{why}");
        let why = installable(
            running,
            key,
            Candidate {
                runtime: None,
                ..same
            },
        )
        .unwrap_err();
        assert!(why.contains("no runtime"), "{why}");
        let why = installable(
            running,
            key,
            Candidate {
                producer: other_key,
                ..same
            },
        )
        .unwrap_err();
        assert!(why.contains("another key"), "{why}");
        let why = installable(
            running,
            key,
            Candidate {
                claims_running_program: false,
                ..same
            },
        )
        .unwrap_err();
        assert!(why.contains("no claim"), "{why}");
    }

    /// `T-037-22` over `T-037-19`: the claim is read from the packages themselves. A package built
    /// with its predecessor claims it; one built without does not; the same program needs none.
    #[test]
    fn t037_22_the_claim_over_the_running_program_is_read_from_the_packages() {
        use crate::stateful_fixture as fixture;
        let g0 = fixture::package(1, &[]);
        let claim = crate::package::state_claim(Some(&g0)).unwrap();
        let claimed = fixture::package(2, &claim);
        let unclaimed = fixture::package(2, &[]);
        assert_eq!(claims_running(&g0, &claimed), Ok(true));
        assert_eq!(claims_running(&g0, &unclaimed), Ok(false));
        assert_eq!(claims_running(&g0, &g0), Ok(true));
        assert!(claims_running(&g0, b"not a package").is_err());
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

    /// `T-037-19`: a revert the runtime refuses (a write since the claimed switch) is final at the
    /// first answer, not after eight, and the park says why.
    #[test]
    fn t037_19_a_refused_revert_parks_for_good_at_once() {
        use crate::driver::DriverError;
        let t = [9u8; 32];
        let mut p = Park::default();
        p.close(t, "upgrade (Revert): UnsupportedLane".into());
        let why = p.blocked(&t, 0).expect("parked");
        assert!(
            why.contains("until the host restarts") && why.contains("UnsupportedLane"),
            "{why}"
        );
        assert!(p.blocked(&t, u64::MAX).is_some(), "no wait ends it");
        // The port's own `failed` after the install does not reopen it.
        p.failed(t, "again".into(), false, 5);
        assert!(p.blocked(&t, u64::MAX).is_some());
        // Another target is free; a success for the target clears it (a restart does too).
        assert!(p.blocked(&[1; 32], 0).is_none());
        p.succeeded(&t);
        assert!(p.blocked(&t, 0).is_none());
        // The switch is told it is a refusal, not a platform failure.
        assert!(matches!(
            install_error(DriverError::RevertClosed("UnsupportedLane".into())),
            InstallError::Cannot(_)
        ));
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

    /// `T-037-19`: what the patched runtime answers to a program change it refuses is its own
    /// refusal, as `driver::upgrade_actor` words it, and counts toward the park's stop. The
    /// released runtime does not refuse a claimed request: its PVM panics, which the driver
    /// reports as a failure of the runtime (`Platform`: transient, so the park only lengthens its
    /// wait and never stops the host).
    #[test]
    fn t037_19_a_refusal_of_the_runtime_to_change_a_stateful_program_is_a_refusal() {
        use crate::driver::DriverError;
        use vos::agent_sdk::{ManagementError, RuntimeOutcome};
        for refusal in [
            ManagementError::UnsupportedLane,
            ManagementError::StaleDeployment,
        ] {
            let outcome: RuntimeOutcome = RuntimeOutcome::Management(Err(refusal));
            let e = install_error(DriverError::Refused(format!("upgrade: {outcome:?}")));
            assert!(
                matches!(&e, InstallError::Cannot(why) if why.contains(&format!("{refusal:?}"))),
                "{e:?}"
            );
        }
        // The released runtime panics on the claim: not a refusal.
        let panicked = install_error(DriverError::Runtime("the runtime PVM panicked".into()));
        assert!(
            matches!(panicked, InstallError::Platform(_)),
            "{panicked:?}"
        );
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
