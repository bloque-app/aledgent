// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: AGPL-3.0-or-later
//! A restart on the code the book names (`T-037-21`; `REQ-RT-16`, `REQ-RT-17`, `REQ-RT-20`;
//! amendment 0033, "at start and at every switch, the host checks every artefact it runs ... and
//! installs that version where any differs").
//!
//! The core host forgets everything but its own start-up files when it restarts. Two things the
//! book can name are therefore lost to it:
//!
//! - **A scheduled upgrade.** The version staged for the `Upgrade` was held in memory. A host
//!   restarted before input *n* must stage it again, or the switch at *n* finds it missing and the
//!   writer stays fenced until a cancel.
//! - **The code in force after a switch.** The runtime's saved state names the installed program,
//!   which is no longer the one `--actor` holds. The host must attach the package that is
//!   installed, and know which version it is.
//!
//! The book is read in this order, because the Agent cannot be asked before a package is attached:
//!
//! 1. **The installed package** ([`package_installed`]). The runtime's directory names the package
//!    it has installed by VOS's reference to its exact bytes. The host holds the genesis package
//!    (`--actor`, still checked against the chainspec's genesis code) and every package it ever
//!    fetched, in the code cache ([`crate::code_source::Fetcher`]); the one whose reference matches
//!    is attached. A copy that is not byte for byte the installed one is never attached.
//! 2. **The versions the hot row names** ([`core_binding::staging::restage`]). With the Agent
//!    attached, its hot row names the code in force and a scheduled upgrade's version
//!    ([`core_binding::switch::named`]); each is fetched again, from the cache first, and checked
//!    as staging checks it.
//! 3. **The start check** (`Driver::ensure_code`, run when the core opens) compares what is
//!    attached, by digest, with the version that executes the next input, and installs it only if
//!    they differ: a host that was stopped between the install and the input finds the platform
//!    running it already, and installs nothing more.
//!
//! Nothing is written for this: the cache is the only memory, and `agent.state` keeps its format.

use core_binding::host::AgentPort;
use core_binding::staging::{CodeFetch, Restaged};
use vos::agent_sdk::BlobRef;

use crate::code_source::Fetcher;
use crate::switch::{CodeState, StagedVersions};

/// The start of the error [`package_installed`] gives when it finds nothing; a caller adds what the
/// operator can do from where the host was started.
pub const NOT_FOUND: &str = "the Agent has installed a package";

/// The packages a host holds beside the genesis one, by the digest they were fetched under.
pub trait Held {
    /// The entries there are, with their lengths, in a fixed order.
    ///
    /// # Errors
    ///
    /// The cache cannot be listed.
    fn entries(&self) -> Result<Vec<([u8; 32], u64)>, String>;
    /// The bytes of the entry for `digest`, `len` long, if they hash to it.
    fn read(&self, digest: &[u8; 32], len: u64) -> Option<Vec<u8>>;
}

impl Held for Fetcher {
    fn entries(&self) -> Result<Vec<([u8; 32], u64)>, String> {
        self.held()
    }
    fn read(&self, digest: &[u8; 32], len: u64) -> Option<Vec<u8>> {
        self.read_held(digest, len)
    }
}

/// The bytes of the package the runtime has installed, which `installed` refers to: the genesis
/// package (`--actor`) if it is that, else the entry of the code cache whose bytes are the ones
/// the reference names (hash and length). Every candidate is compared byte for byte by VOS's own
/// reference; nothing is guessed from a name. An entry whose length is not the installed
/// package's is skipped without being read.
///
/// # Errors
///
/// The installed package is neither the genesis one nor held: the host cannot attach it. The
/// message gives VOS's reference, which is VOS's own blob hash and not the digest the cache's
/// files are named by, and says how many entries were looked at, or that the cache could not be
/// listed.
pub fn package_installed(
    installed: &BlobRef,
    genesis: &[u8],
    held: Option<&dyn Held>,
) -> Result<Vec<u8>, String> {
    if installed.matches(genesis) {
        return Ok(genesis.to_vec());
    }
    let reference = format!(
        "VOS's own reference to it (blob hash {} over {} bytes; this is not the blake2b-256 \
         digest the code cache's files are named by)",
        hex::encode(installed.hash.0),
        installed.len
    );
    let Some(held) = held else {
        return Err(format!(
            "{NOT_FOUND} that is not the --actor package, and this host has no code cache to look \
             for it in. {reference}"
        ));
    };
    let entries = held.entries().map_err(|e| {
        format!("{NOT_FOUND} that is not the --actor package, and {e}. {reference}")
    })?;
    let mut read = 0usize;
    for (digest, len) in &entries {
        if *len != installed.len {
            continue;
        }
        read += 1;
        if let Some(bytes) = held.read(digest, *len)
            && installed.matches(&bytes)
        {
            return Ok(bytes);
        }
    }
    Err(format!(
        "{NOT_FOUND} that is not the --actor package, and the code cache holds no copy of it \
         ({} entries, {read} of that length read). {reference}. The cache must be on the same \
         persistent volume as the state: restore the cache that holds the versions this host \
         switched to",
        entries.len()
    ))
}

/// What a restarted host asks of the runtime to attach what is installed.
pub trait Attach {
    /// What attaching gives back (the runtime's handle on the actor).
    type Actor;
    /// VOS's reference to the package installed under the actor's name.
    ///
    /// # Errors
    ///
    /// The runtime could not be asked.
    fn installed_package(&mut self) -> Result<BlobRef, String>;
    /// Attach the installed actor, given the bytes of its package.
    ///
    /// # Errors
    ///
    /// The package is not admitted, or is not the installed program.
    fn attach(&mut self, package: &[u8]) -> Result<Self::Actor, String>;
}

/// Attach the actor a saved runtime state has installed (`T-037-21`): the genesis package if that
/// is what is installed, else the package of the code cache that is. Returns the actor and the
/// bytes of the package attached, which the host must track as running.
///
/// # Errors
///
/// [`package_installed`], or the runtime refused to attach.
pub fn attach_installed<A: Attach>(
    runtime: &mut A,
    genesis: &[u8],
    held: Option<&dyn Held>,
) -> Result<(A::Actor, Vec<u8>), String> {
    let installed = runtime.installed_package()?;
    let bytes = package_installed(&installed, genesis, held)?;
    if bytes != genesis {
        eprintln!(
            "aledgent-core-host: the Agent runs a package other than --actor ({} bytes, found in \
             the code cache): the host switched to it before it stopped",
            bytes.len()
        );
    }
    let actor = runtime.attach(&bytes)?;
    Ok((actor, bytes))
}

/// A port that tracks the code it runs.
pub trait Tracks: AgentPort {
    /// Track `state` (`T-037-14`).
    fn track(&mut self, state: CodeState);
}

/// The source of a host with no code cache: it holds nothing.
struct Nothing;

impl CodeFetch for Nothing {
    fn name(&self) -> &str {
        "none"
    }
    fn fetch(&self, _: &[u8; 32], _: Option<u64>) -> Option<Vec<u8>> {
        None
    }
}

/// Track as running the package that was attached (not `--actor`, which after a switch is not what
/// runs: a start check that took it for the running version would install the version a second
/// time), keep the genesis version for a way out, and stage again the versions the hot row names
/// that this host does not hold. Prints what became of each ([`describe`]).
///
/// # Errors
///
/// The hot row could not be read.
pub fn track_and_restage<P: Tracks>(
    port: &mut P,
    name: &str,
    genesis_code: [u8; 32],
    genesis_package: &[u8],
    attached: &[u8],
    runtime: &[u8],
    source: Option<&dyn CodeFetch>,
) -> Result<(), String> {
    let mut staged = StagedVersions::new();
    staged.running(genesis_code, genesis_package, runtime);
    port.track(CodeState::new(name, attached, runtime, staged));
    let has_cache = source.is_some();
    let source: &dyn CodeFetch = source.unwrap_or(&Nothing);
    // A source has no time limit yet (`T-037-20`), so a source that hangs holds the start here.
    eprintln!(
        "aledgent-core-host: staging again the code versions the book names (from the code cache, \
         then the sources; a source that does not answer holds the start up until T-037-20)"
    );
    let report = core_binding::staging::restage(port, source)
        .map_err(|e| format!("the Agent's hot row: {e:?}"))?;
    for line in describe(&report, has_cache) {
        eprintln!("aledgent-core-host: {line}");
    }
    Ok(())
}

/// What to tell the operator about a restage (`report` is in the order of
/// [`core_binding::switch::named`]: the code in force first, then a scheduled upgrade's version).
/// The two failures are not alike. A scheduled version that cannot be staged again refuses the
/// input it starts at, and the way out of the upgrade still goes through. The code in force cannot
/// be staged again: every input is refused, a way out included, because a way out is decided by the
/// code in force.
#[must_use]
pub fn describe(report: &[Restaged], has_cache: bool) -> Vec<String> {
    let no_cache = if has_cache {
        ""
    } else {
        " and this host has no --code-config"
    };
    report
        .iter()
        .enumerate()
        .filter_map(|(i, r)| match r {
            Restaged::Held(_) => None,
            Restaged::Staged(code) => Some(format!(
                "code version {} is staged again from the code cache",
                core_binding::switch::short(code)
            )),
            Restaged::Refused(code, why) if i == 0 => Some(format!(
                "WARNING: code version {}, the code IN FORCE, cannot be staged again ({why}){no_cache}. \
                 No input will run until it can: every input is refused, a cancel or a reschedule \
                 of an upgrade included, because a way out is decided by the code in force. Put its \
                 manifest and files back in the code cache (or in a source of --code-config) and \
                 restart the core host (T-037-21)",
                core_binding::switch::short(code)
            )),
            Restaged::Refused(code, why) => Some(format!(
                "WARNING: code version {}, the scheduled upgrade's, cannot be staged again \
                 ({why}){no_cache}. Inputs before the upgrade's input run; at that input the switch \
                 is refused. A cancel or a reschedule still goes through. Put its manifest and \
                 files back in the code cache (or a source of --code-config) and restart the core \
                 host before then (T-037-21)",
                core_binding::switch::short(code)
            )),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_binding::envelope::BlobKey;
    use core_binding::host::PortError;
    use core_binding::hot::{Hot, Limits, RingEntry};
    use core_binding::staging::{Checked, StageRefusal};
    use core_binding::switch::{
        Artefacts, CodeInstaller, InstallError, Outcome, StagedCode, ensure,
    };
    use ledger_core::code::{
        Artefact, BuildProfile, CodeManifest, KernelIdentity, Platform, Role, WriterKind,
        artefact_digest,
    };
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    /// A cache of fixed entries; it remembers which it was asked to read.
    struct Cache {
        files: BTreeMap<[u8; 32], Vec<u8>>,
        read: RefCell<Vec<[u8; 32]>>,
    }

    impl Cache {
        fn of(entries: &[&[u8]]) -> Self {
            Cache {
                files: entries
                    .iter()
                    .map(|b| (artefact_digest(b), b.to_vec()))
                    .collect(),
                read: RefCell::new(Vec::new()),
            }
        }
    }

    impl Held for Cache {
        fn entries(&self) -> Result<Vec<([u8; 32], u64)>, String> {
            Ok(self
                .files
                .iter()
                .map(|(d, b)| (*d, b.len() as u64))
                .collect())
        }
        fn read(&self, digest: &[u8; 32], _: u64) -> Option<Vec<u8>> {
            self.read.borrow_mut().push(*digest);
            self.files.get(digest).cloned()
        }
    }

    /// A cache that cannot be listed.
    struct Unreadable;

    impl Held for Unreadable {
        fn entries(&self) -> Result<Vec<([u8; 32], u64)>, String> {
            Err("the code cache /x cannot be listed: Permission denied".into())
        }
        fn read(&self, _: &[u8; 32], _: u64) -> Option<Vec<u8>> {
            None
        }
    }

    #[test]
    fn t037_21_the_genesis_package_is_attached_when_it_is_the_installed_one() {
        let g = b"package of g";
        // The usual start, and the start of a host that has switched nothing: no cache needed.
        let got = package_installed(&BlobRef::of_bytes(g), g, None).unwrap();
        assert_eq!(got, g);
        // The cache is not asked when the genesis package is the installed one.
        let cache = Cache::of(&[b"package of H"]);
        let got = package_installed(&BlobRef::of_bytes(g), g, Some(&cache)).unwrap();
        assert_eq!(got, g);
        assert!(cache.read.borrow().is_empty());
    }

    #[test]
    fn t037_21_after_a_switch_the_installed_package_is_found_in_the_cache() {
        let (g, h) = (b"package of g".as_slice(), b"package of H".as_slice());
        // The cache holds what was fetched: manifests, runtimes, packages.
        let cache = Cache::of(&[b"manifest of H", h, b"the runtime", b"another package"]);
        let got = package_installed(&BlobRef::of_bytes(h), g, Some(&cache)).unwrap();
        assert_eq!(got, h);
    }

    #[test]
    fn t037_21_entries_of_another_length_are_not_read() {
        let (g, h) = (b"package of g".as_slice(), b"package of H".as_slice());
        let cache = Cache::of(&[
            b"short",
            b"a much longer file than the package, a runtime",
            h,
        ]);
        package_installed(&BlobRef::of_bytes(h), g, Some(&cache)).unwrap();
        // Only the entry of the installed package's length was read, and hashed.
        assert_eq!(*cache.read.borrow(), [artefact_digest(h)]);
    }

    #[test]
    fn t037_21_a_copy_that_is_not_the_installed_one_is_never_attached() {
        let g = b"package of g";
        // Same length as the installed package, other bytes.
        let cache = Cache::of(&[b"package of X"]);
        let err = package_installed(&BlobRef::of_bytes(b"package of H"), g, Some(&cache));
        assert!(err.unwrap_err().contains("holds no copy"));
    }

    #[test]
    fn t037_21_the_error_names_vos_own_reference_and_what_to_do() {
        let g = b"package of g";
        let installed = BlobRef::of_bytes(b"package of H");
        let none = package_installed(&installed, g, None).unwrap_err();
        assert!(none.starts_with(NOT_FOUND), "{none}");
        assert!(none.contains("no code cache"), "{none}");
        assert!(
            !none.contains("--code-config"),
            "restore has no such flag: {none}"
        );
        let reference = hex::encode(installed.hash.0);
        let cache = Cache::of(&[b"package of X", b"other"]);
        let held = package_installed(&installed, g, Some(&cache)).unwrap_err();
        assert!(held.contains(&reference), "{held}");
        assert!(held.contains("not the blake2b-256 digest"), "{held}");
        assert!(held.contains("2 entries, 1 of that length read"), "{held}");
        assert!(held.contains("same persistent volume"), "{held}");
        // A cache that cannot be listed is not an empty cache.
        let unreadable = package_installed(&installed, g, Some(&Unreadable)).unwrap_err();
        assert!(unreadable.contains("cannot be listed"), "{unreadable}");
        assert!(!unreadable.contains("holds no copy"), "{unreadable}");
    }

    /// A runtime stand-in: it has installed `installed`, and says what it was asked to attach.
    struct Runtime {
        installed: Vec<u8>,
        attached: Option<Vec<u8>>,
    }

    impl Attach for Runtime {
        type Actor = &'static str;
        fn installed_package(&mut self) -> Result<BlobRef, String> {
            Ok(BlobRef::of_bytes(&self.installed))
        }
        fn attach(&mut self, package: &[u8]) -> Result<Self::Actor, String> {
            // The real runtime refuses a package that is not the installed program.
            if package != self.installed {
                return Err("the installed actor runs another program than this package".into());
            }
            self.attached = Some(package.to_vec());
            Ok("actor")
        }
    }

    /// The restart after a switch attaches the package the runtime has installed, and hands its
    /// bytes back to be tracked. A host that always attached `--actor` would be refused here, as
    /// the real runtime refuses it.
    #[test]
    fn t037_21_a_restart_attaches_the_installed_package_not_the_actor_file() {
        let (g, h) = (b"package of g".to_vec(), b"package of H".to_vec());
        let cache = Cache::of(&[&h]);
        let mut rt = Runtime {
            installed: h.clone(),
            attached: None,
        };
        let (_, bytes) = attach_installed(&mut rt, &g, Some(&cache)).unwrap();
        assert_eq!(bytes, h);
        assert_eq!(rt.attached, Some(h.clone()));
        // Nothing switched: the genesis package is attached.
        let mut rt = Runtime {
            installed: g.clone(),
            attached: None,
        };
        let (_, bytes) = attach_installed(&mut rt, &g, None).unwrap();
        assert_eq!((bytes, rt.attached), (g.clone(), Some(g.clone())));
        // The installed package is nowhere: the host does not start.
        let mut rt = Runtime {
            installed: h.clone(),
            attached: None,
        };
        assert!(attach_installed(&mut rt, &g, Some(&Cache::of(&[]))).is_err());
        assert_eq!(rt.attached, None);
    }

    // ----- the hot row's versions, staged again and tracked ------------------------------------

    fn manifest(generation: u64, package: &[u8], runtime: &[u8]) -> CodeManifest {
        let profile = || BuildProfile {
            target: "riscv64em-vos".into(),
            toolchain: "nightly".into(),
            opt_level: "s".into(),
            features: Vec::new(),
            source_revision: "0123abcd".into(),
            cargo_lock: [0; 32],
        };
        let artefact = |role, bytes: &[u8]| Artefact {
            role,
            digest: artefact_digest(bytes),
            len: bytes.len() as u64,
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
            artefacts: vec![
                artefact(Role::Package, package),
                artefact(Role::Runtime, runtime),
            ],
            kernel: KernelIdentity {
                revision: "e9a7b08".into(),
                checksum: None,
            },
            constructions: Vec::new(),
            messages: Vec::new(),
        }
    }

    /// Everything a node's code cache holds, by digest.
    #[derive(Default)]
    struct Files(BTreeMap<[u8; 32], Vec<u8>>);

    impl Files {
        fn version(&mut self, generation: u64, package: &[u8], runtime: &[u8]) -> [u8; 32] {
            let m = manifest(generation, package, runtime);
            for bytes in [package, runtime, &m.to_bytes()] {
                self.0.insert(artefact_digest(bytes), bytes.to_vec());
            }
            m.version()
        }
    }

    impl CodeFetch for Files {
        fn name(&self) -> &str {
            "files"
        }
        fn fetch(&self, digest: &[u8; 32], _: Option<u64>) -> Option<Vec<u8>> {
            self.0.get(digest).cloned()
        }
    }

    /// The core host's port, without a runtime: a hot row, the code it tracks, and the versions it
    /// was asked to stage again. What a real runtime would add is in the pull request.
    struct Port {
        hot: Hot,
        tracked: Option<CodeState>,
        restaged: BTreeMap<[u8; 32], Artefacts>,
    }

    impl Port {
        fn new(code: [u8; 32], generation: u64, upgrade: Option<([u8; 32], u64)>) -> Self {
            Port {
                hot: Hot {
                    layout: core_binding::hot::LAYOUT,
                    driver: [0; 32],
                    subroots: [[0; 32]; 6],
                    last_commit: None,
                    next_posting: 0,
                    service_root: [0; 32],
                    audit_emitted: 0,
                    audit_head: [0; 32],
                    audit_shipped: None,
                    inputs: 9,
                    log_head: [0; 32],
                    archived: 9,
                    ring_sizes: Vec::new(),
                    delivered: 0,
                    notified: 0,
                    limits: Limits::DEFAULT,
                    config_len: 0,
                    observed_slot: 0,
                    code,
                    generation,
                    construction: 0,
                    upgrade,
                    authority: [0; 32],
                    authority_next: None,
                    continued_at: None,
                },
                tracked: None,
                restaged: BTreeMap::new(),
            }
        }

        fn state(&self) -> &CodeState {
            self.tracked.as_ref().expect("the port tracks code")
        }

        /// What the start check sees as staged: the running genesis version and what was staged
        /// again.
        fn staged(&self) -> impl StagedCode + '_ {
            struct Both<'a>(&'a Port);
            impl StagedCode for Both<'_> {
                fn artefacts(&self, code: &[u8; 32]) -> Option<Artefacts> {
                    self.0
                        .restaged
                        .get(code)
                        .copied()
                        .or_else(|| self.0.state().staged.digests().artefacts(code))
                }
            }
            Both(self)
        }
    }

    impl AgentPort for Port {
        fn invoke(&mut self, _: Vec<u8>, _: Vec<Vec<u8>>) -> Result<Vec<u8>, PortError> {
            Err(PortError::Unavailable("no runtime".into()))
        }
        fn blob_key(&self, _: &[u8]) -> BlobKey {
            BlobKey {
                hash: [0; 32],
                len: 0,
            }
        }
        fn hot(&mut self) -> Result<Option<Hot>, PortError> {
            Ok(Some(self.hot.clone()))
        }
        fn entry(&mut self, _: u64) -> Result<Option<RingEntry>, PortError> {
            Ok(None)
        }
        fn holds_code(&self, code: &[u8; 32]) -> bool {
            self.tracked.as_ref().is_some_and(|s| s.holds(code)) || self.restaged.contains_key(code)
        }
        fn restage_code(&mut self, checked: Checked, generation: u64) -> Result<(), StageRefusal> {
            if checked.manifest.generation != generation {
                return Err(StageRefusal::Generation {
                    writer: generation.saturating_sub(1),
                    manifest: checked.manifest.generation,
                });
            }
            let package = artefact_digest(&checked.package);
            let runtime = artefact_digest(&checked.runtime.expect("a runtime"));
            self.restaged
                .insert(checked.manifest.version(), Artefacts { package, runtime });
            Ok(())
        }
    }

    impl Tracks for Port {
        fn track(&mut self, state: CodeState) {
            self.tracked = Some(state);
        }
    }

    /// A platform that runs `runs` and counts installs.
    struct Platform_ {
        runs: Artefacts,
        installs: u32,
    }

    impl CodeInstaller for Platform_ {
        fn running(&mut self) -> Result<Artefacts, InstallError> {
            Ok(self.runs)
        }
        fn install(&mut self, _: Artefacts, _: &[u8; 32]) -> Result<(), InstallError> {
            self.installs += 1;
            Err(InstallError::Cannot("not under test".into()))
        }
    }

    fn start(
        port: &mut Port,
        g: &(Vec<u8>, [u8; 32]),
        attached: &[u8],
        files: &Files,
    ) -> Result<(), String> {
        track_and_restage(
            port,
            "core",
            g.1,
            &g.0,
            attached,
            b"the runtime",
            Some(files),
        )
    }

    /// Restart before *n*: the scheduled version is fetched again from the cache and held, so the
    /// start check can install it at *n*. Without the re-fetch (`open_port` not calling it), *H*
    /// would be missing here.
    #[test]
    fn t037_21_the_host_stages_again_the_version_the_hot_row_schedules() {
        let mut files = Files::default();
        let g = (
            b"package of g".to_vec(),
            files.version(0, b"package of g", b"the runtime"),
        );
        let h = files.version(1, b"package of H", b"the runtime");
        let mut port = Port::new(g.1, 0, Some((h, 20)));
        start(&mut port, &g, &g.0, &files).unwrap();
        assert!(port.holds_code(&h), "H is staged again");
        assert!(port.holds_code(&g.1), "g is kept for a way out");
        // The switch at n finds it.
        let mut platform = Platform_ {
            runs: port.state().running,
            installs: 0,
        };
        let staged = port.staged();
        let r = ensure(g.1, Some((h, 20)), 19, &staged, &mut platform);
        assert_eq!(
            r,
            Ok(Outcome::Unchanged),
            "before n, g runs and nothing changes"
        );
    }

    /// Restart in the middle of a switch: the runtime has installed *H*, so the host attached *H*'s
    /// package and tracks *that* as running. The start check then finds the platform already runs
    /// the target and installs nothing. A host that tracked `--actor` instead would see *g* running,
    /// and install *H* over *H*.
    #[test]
    fn t037_21_a_restart_in_the_middle_of_a_switch_installs_nothing_a_second_time() {
        let mut files = Files::default();
        let g = (
            b"package of g".to_vec(),
            files.version(0, b"package of g", b"the runtime"),
        );
        let h_package = b"package of H".to_vec();
        let h = files.version(1, &h_package, b"the runtime");
        let mut port = Port::new(g.1, 0, Some((h, 20)));
        // The runtime has installed H; the host attached it.
        start(&mut port, &g, &h_package, &files).unwrap();
        assert_eq!(port.state().running.package, artefact_digest(&h_package));
        let mut platform = Platform_ {
            runs: port.state().running,
            installs: 0,
        };
        let staged = port.staged();
        let r = ensure(g.1, Some((h, 20)), 20, &staged, &mut platform);
        assert_eq!(r, Ok(Outcome::Unchanged));
        assert_eq!(platform.installs, 0, "no second switch");
    }

    /// Restart after the switch: the hot row's code in force is *H*, which only the cache holds.
    #[test]
    fn t037_21_a_restart_after_the_switch_stages_the_code_in_force() {
        let mut files = Files::default();
        let g = (
            b"package of g".to_vec(),
            files.version(0, b"package of g", b"the runtime"),
        );
        let h_package = b"package of H".to_vec();
        let h = files.version(1, &h_package, b"the runtime");
        let mut port = Port::new(h, 1, None);
        start(&mut port, &g, &h_package, &files).unwrap();
        assert!(port.holds_code(&h));
        let mut platform = Platform_ {
            runs: port.state().running,
            installs: 0,
        };
        let staged = port.staged();
        let r = ensure(h, None, 21, &staged, &mut platform);
        assert_eq!(r, Ok(Outcome::Unchanged));
        assert_eq!(platform.installs, 0);
    }

    /// The warning does not promise a way out when the code in force is the one that is missing.
    #[test]
    fn t037_21_the_warning_tells_the_two_failures_apart() {
        let (g, h) = ([1u8; 32], [2u8; 32]);
        let why = StageRefusal::NotRetrievable("the manifest of 0202".into());
        // The scheduled version is missing: a way out still goes through.
        let scheduled = describe(
            &[Restaged::Held(g), Restaged::Refused(h, why.clone())],
            true,
        );
        assert_eq!(scheduled.len(), 1);
        assert!(
            scheduled[0].contains("scheduled upgrade's"),
            "{scheduled:?}"
        );
        assert!(scheduled[0].contains("A cancel or a reschedule still goes through"));
        // The code in force is missing: nothing runs, a way out included.
        let in_force = describe(&[Restaged::Refused(h, why.clone())], true);
        assert!(in_force[0].contains("IN FORCE"), "{in_force:?}");
        assert!(in_force[0].contains("every input is refused, a cancel or a reschedule"));
        assert!(!in_force[0].contains("stays open"));
        assert!(in_force[0].contains("restart the core host"));
        // No log line has a lost run of spaces.
        for line in scheduled.iter().chain(&in_force) {
            assert!(!line.contains("  "), "{line}");
        }
        // Without a cache the line says so; a version staged again is reported plainly.
        let none = describe(&[Restaged::Refused(g, why)], false);
        assert!(none[0].contains("no --code-config"));
        let staged = describe(&[Restaged::Staged(h), Restaged::Held(g)], true);
        assert_eq!(staged.len(), 1);
    }
}
