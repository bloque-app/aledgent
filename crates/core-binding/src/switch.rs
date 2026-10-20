// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `switch(H, n)`: the one operation by which the core host changes the code a writer runs
//! (`T-037-14`, amendment 0033, `REQ-RT-16`, `REQ-RT-18`, `REQ-RT-20`).
//!
//! The writer's hot row names the code version that executes its next input (`code`) and, once an
//! upgrade is scheduled, the version *H* that executes input *n* and after (`upgrade`). The host
//! never picks a version: [`target_code`] derives the one the next input must run from those two
//! fields, and [`ensure`] makes the platform run it, between input *n* − 1 and input *n*:
//!
//! - **One function, two callers.** The host calls [`ensure`] at start and before every input it
//!   sends; the start call is the crash recovery (a crash between the switch and *n*, or between a
//!   way-out input's commit and the reinstall of generation *g*, resumes by the same check).
//! - **Idempotent by digests, not by a flag.** [`ensure`] compares the artefacts the platform runs
//!   (package and runtime, by digest) with the target's staged ones: equal is success ("already
//!   switched"); different is an install, compare-and-swapped on what was running; an install the
//!   platform cannot do is an error, and the host does not drive the writer (the caller must send
//!   nothing).
//! - **Symmetric.** The same call installs *H* from *n*, and generation *g*'s package again after a
//!   way-out input at *n* (the hot row's `code` is still *g*, `upgrade` is cleared).
//! - **No arbitrary version.** Nothing here takes a version from a caller: a host that wants a
//!   different version must first commit an upgrade through the writer (`T-037-04`).
//! - **Staged, not fetched.** [`StagedCode`] answers from what the upgrade staged before it was
//!   logged (`T-037-04`, [`crate::staging`]; the sources are `T-037-06`'s); a version it does not
//!   hold is [`SwitchError::NotStaged`].
//!
//! The platform's install is a [`CodeInstaller`]: on `saga` the registry's compare-and-swap on the
//! old program, signed with the core's package key through the signer (`agents/core-host`); the
//! `vos-core` form (an install into the same instance) and the cross-platform form (`T-037-17`)
//! are written against the same trait.

use alloc::format;
use alloc::string::String;

/// The digests of the artefacts the host runs for a writer: the package and the runtime handed
/// to the platform (`blake2b-256` of their bytes, as the code manifest records them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Artefacts {
    /// The writer's package (on `saga`, the actor package).
    pub package: [u8; 32],
    /// The runtime it runs in (on `saga`, the program given to `Runtime::create`).
    pub runtime: [u8; 32],
}

/// What the upgrade staged: for each code version, the digests of its artefacts.
pub trait StagedCode {
    /// The artefacts of `code`, if its bytes were staged and checked before the upgrade committed.
    fn artefacts(&self, code: &[u8; 32]) -> Option<Artefacts>;
}

/// Why a platform's install did not complete.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InstallError {
    /// The compare-and-swap failed: the platform runs `running`, not what the host read.
    Stale {
        /// What the platform runs now.
        running: Artefacts,
    },
    /// The platform cannot install this change (for example a runtime that differs, on a
    /// platform that installs only the package).
    Cannot(String),
    /// The platform or the signer failed.
    Platform(String),
}

/// The platform's install of a staged code version.
pub trait CodeInstaller {
    /// The artefacts the platform runs for the writer now.
    fn running(&mut self) -> Result<Artefacts, InstallError>;
    /// Install the staged `code`, compare-and-swapped on `from` (what [`CodeInstaller::running`]
    /// answered): a platform whose state is no longer `from` answers [`InstallError::Stale`] and
    /// changes nothing.
    fn install(&mut self, from: Artefacts, code: &[u8; 32]) -> Result<(), InstallError>;
}

/// What [`ensure`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The platform already ran the target ("already switched" is success).
    Unchanged,
    /// The platform was changed to run the target.
    Installed,
}

/// Why [`ensure`] refused: the host must not drive the writer (send nothing).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SwitchError {
    /// The code version the next input must run was never staged.
    NotStaged([u8; 32]),
    /// A compare-and-swap on a stale hash: another writer of the platform changed it; nothing
    /// was installed.
    Stale {
        /// What the platform runs now.
        running: Artefacts,
    },
    /// The platform cannot install the target.
    Cannot(String),
    /// The platform or the signer failed.
    Platform(String),
    /// The install returned, and the platform does not run the target.
    NotInstalled {
        /// What the platform runs now.
        running: Artefacts,
    },
}

impl core::fmt::Display for SwitchError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotStaged(_) => f.write_str("the code version of the next input was not staged"),
            Self::Stale { .. } => f.write_str(
                "the platform's install changed under the switch (stale compare-and-swap)",
            ),
            Self::Cannot(why) => write!(f, "the platform cannot install the target: {why}"),
            Self::Platform(why) => write!(f, "the platform's install failed: {why}"),
            Self::NotInstalled { .. } => {
                f.write_str("the platform does not run the target after its install")
            }
        }
    }
}

/// The code version that executes input `next_input`: the scheduled `upgrade`'s *H* from its input
/// *n* on, otherwise the hot row's `code`. The only source of a version the host may install.
#[must_use]
pub fn target_code(code: [u8; 32], upgrade: Option<([u8; 32], u64)>, next_input: u64) -> [u8; 32] {
    match upgrade {
        Some((h, n)) if next_input >= n => h,
        _ => code,
    }
}

/// Make the platform run the code version that executes input `next_input` (module
/// documentation). Call it at start and before sending every input; on `Err` send nothing.
///
/// # Errors
///
/// [`SwitchError`]: the version was not staged, the compare-and-swap was stale, the platform cannot
/// install it, or it does not run it after the install.
pub fn ensure(
    code: [u8; 32],
    upgrade: Option<([u8; 32], u64)>,
    next_input: u64,
    staged: &dyn StagedCode,
    installer: &mut dyn CodeInstaller,
) -> Result<Outcome, SwitchError> {
    let target = target_code(code, upgrade, next_input);
    let wanted = staged
        .artefacts(&target)
        .ok_or(SwitchError::NotStaged(target))?;
    let have = installer.running().map_err(to_switch())?;
    if have == wanted {
        return Ok(Outcome::Unchanged);
    }
    match installer.install(have, &target) {
        Ok(()) => {}
        // Another attempt (a host that crashed after its install, before it logged it) already
        // ran the target: "already switched" is success, not a stale compare-and-swap.
        Err(InstallError::Stale { running }) if running == wanted => return Ok(Outcome::Unchanged),
        Err(e) => return Err(to_switch()(e)),
    }
    let now = installer.running().map_err(to_switch())?;
    if now == wanted {
        Ok(Outcome::Installed)
    } else {
        Err(SwitchError::NotInstalled { running: now })
    }
}

fn to_switch() -> impl Fn(InstallError) -> SwitchError {
    move |e| match e {
        InstallError::Stale { running } => SwitchError::Stale { running },
        InstallError::Cannot(why) => SwitchError::Cannot(why),
        InstallError::Platform(why) => SwitchError::Platform(why),
    }
}

/// A short, log-safe name for a code version: its first eight bytes in hex.
#[must_use]
pub fn short(code: &[u8; 32]) -> String {
    code.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::collections::BTreeMap;
    use alloc::vec::Vec;

    fn h(b: u8) -> [u8; 32] {
        [b; 32]
    }
    fn art(p: u8, r: u8) -> Artefacts {
        Artefacts {
            package: h(p),
            runtime: h(r),
        }
    }

    /// Generation g is version 1 (package 1, runtime 9); H is version 2 (package 2, runtime 9);
    /// version 3 changes the runtime too (package 3, runtime 8).
    struct Staged(BTreeMap<[u8; 32], Artefacts>);
    impl Staged {
        fn new() -> Self {
            let mut m = BTreeMap::new();
            m.insert(h(1), art(1, 9));
            m.insert(h(2), art(2, 9));
            m.insert(h(3), art(3, 8));
            Self(m)
        }
    }
    impl StagedCode for Staged {
        fn artefacts(&self, code: &[u8; 32]) -> Option<Artefacts> {
            self.0.get(code).copied()
        }
    }

    /// A platform: what it runs, what it was asked, and how it fails.
    struct Platform {
        runs: Artefacts,
        by_code: BTreeMap<[u8; 32], Artefacts>,
        installs: Vec<[u8; 32]>,
        runtime_installable: bool,
        /// A concurrent change slipped in before the install: what it runs at the swap.
        slips_to: Option<Artefacts>,
        signed: Vec<[u8; 32]>,
    }
    impl Platform {
        fn running(code: u8) -> Self {
            let staged = Staged::new();
            Self {
                runs: staged.0[&h(code)],
                by_code: staged.0,
                installs: Vec::new(),
                runtime_installable: true,
                slips_to: None,
                signed: Vec::new(),
            }
        }
    }
    impl CodeInstaller for Platform {
        fn running(&mut self) -> Result<Artefacts, InstallError> {
            Ok(self.runs)
        }
        fn install(&mut self, from: Artefacts, code: &[u8; 32]) -> Result<(), InstallError> {
            if let Some(other) = self.slips_to.take() {
                self.runs = other;
            }
            if self.runs != from {
                return Err(InstallError::Stale { running: self.runs });
            }
            let to = self.by_code[code];
            if to.runtime != self.runs.runtime && !self.runtime_installable {
                return Err(InstallError::Cannot("the runtime differs".into()));
            }
            // The signed mutation: one per install, naming the code, never a key.
            self.signed.push(*code);
            self.installs.push(*code);
            self.runs = to;
            Ok(())
        }
    }

    #[test]
    fn t037_14_the_program_switches_before_n() {
        let staged = Staged::new();
        let mut p = Platform::running(1);
        let upgrade = Some((h(2), 5));
        // Before n the target is the hot row's code: nothing to do.
        assert_eq!(
            ensure(h(1), upgrade, 4, &staged, &mut p),
            Ok(Outcome::Unchanged)
        );
        assert!(p.installs.is_empty());
        // At n the program switches, before the input is sent.
        assert_eq!(
            ensure(h(1), upgrade, 5, &staged, &mut p),
            Ok(Outcome::Installed)
        );
        assert_eq!(p.runs, art(2, 9));
        assert_eq!(p.installs, [h(2)]);
    }

    #[test]
    fn t037_14_runtime_mismatch_installs_or_refuses() {
        let staged = Staged::new();
        // A host whose runtime differs from the target's installs it where it can ...
        let mut p = Platform::running(1);
        assert_eq!(
            ensure(h(1), Some((h(3), 5)), 5, &staged, &mut p),
            Ok(Outcome::Installed)
        );
        assert_eq!(p.runs, art(3, 8));
        // ... and refuses to drive the writer where it cannot, changing nothing.
        let mut p = Platform::running(1);
        p.runtime_installable = false;
        let err = ensure(h(1), Some((h(3), 5)), 5, &staged, &mut p).unwrap_err();
        assert!(matches!(err, SwitchError::Cannot(_)), "{err}");
        assert_eq!(p.runs, art(1, 9));
        assert!(p.installs.is_empty());
        // The same check at start: a restarted host whose runtime is not the code map's.
        let mut p = Platform::running(1);
        p.runtime_installable = false;
        let err = ensure(h(3), None, 9, &staged, &mut p).unwrap_err();
        assert!(matches!(err, SwitchError::Cannot(_)), "{err}");
    }

    #[test]
    fn t037_14_way_out_reinstalls_g_through_the_signer() {
        let staged = Staged::new();
        let mut p = Platform::running(1);
        // The switch to H at n = 5 ...
        ensure(h(1), Some((h(2), 5)), 5, &staged, &mut p).unwrap();
        // ... input 5 was a way-out: it committed with `code` still g and `upgrade` cleared, so the
        // host reinstalls g before 6, by the same call.
        assert_eq!(
            ensure(h(1), None, 6, &staged, &mut p),
            Ok(Outcome::Installed)
        );
        assert_eq!(p.runs, art(1, 9));
        assert_eq!(p.installs, [h(2), h(1)]);
        // Every install went through the signed mutation, none without it.
        assert_eq!(p.signed, p.installs);
    }

    #[test]
    fn t037_14_crash_after_way_out_installs_g_at_start() {
        let staged = Staged::new();
        // The platform still runs H (the previous process crashed between the way-out's commit
        // and the reinstall): the restarted host's start check installs g.
        let mut p = Platform::running(2);
        assert_eq!(
            ensure(h(1), None, 6, &staged, &mut p),
            Ok(Outcome::Installed)
        );
        assert_eq!(p.runs, art(1, 9));
        assert_eq!(p.installs, [h(1)]);
    }

    #[test]
    fn t037_14_stale_cas_changes_nothing_and_sends_nothing() {
        let staged = Staged::new();
        let mut p = Platform::running(1);
        // Another change slipped in between the read and the swap.
        p.slips_to = Some(art(3, 8));
        let err = ensure(h(1), Some((h(2), 5)), 5, &staged, &mut p).unwrap_err();
        assert_eq!(err, SwitchError::Stale { running: art(3, 8) });
        // The platform kept what it had: nothing was installed or signed, and the caller got an
        // error, so it sends nothing.
        assert_eq!(p.runs, art(3, 8));
        assert!(p.installs.is_empty() && p.signed.is_empty());
    }

    #[test]
    fn t037_14_a_stale_swap_that_already_runs_the_target_is_success() {
        let staged = Staged::new();
        let mut p = Platform::running(1);
        // A previous attempt's install landed meanwhile: "already switched to H".
        p.slips_to = Some(art(2, 9));
        assert_eq!(
            ensure(h(1), Some((h(2), 5)), 5, &staged, &mut p),
            Ok(Outcome::Unchanged)
        );
        assert!(p.installs.is_empty());
    }

    #[test]
    fn t037_14_crash_after_switch_resumes_once() {
        let staged = Staged::new();
        let mut p = Platform::running(1);
        let upgrade = Some((h(2), 5));
        ensure(h(1), upgrade, 5, &staged, &mut p).unwrap();
        // The process crashed after the switch and before input 5 committed: the restarted host
        // reads the same hot row (upgrade still scheduled) and does not switch twice.
        assert_eq!(
            ensure(h(1), upgrade, 5, &staged, &mut p),
            Ok(Outcome::Unchanged)
        );
        assert_eq!(p.installs, [h(2)]);
    }

    #[test]
    fn t037_14_no_switch_without_a_committed_upgrade() {
        // The target is a function of the hot row alone: with no scheduled upgrade it is the hot
        // row's code at every input number.
        for n in [0, 1, 5, u64::MAX] {
            assert_eq!(target_code(h(1), None, n), h(1));
        }
        // A scheduled upgrade moves it from its input, never before.
        assert_eq!(target_code(h(1), Some((h(2), 5)), 4), h(1));
        assert_eq!(target_code(h(1), Some((h(2), 5)), 5), h(2));
        // A version nobody staged is refused, whatever the hot row says.
        let staged = Staged::new();
        let mut p = Platform::running(1);
        assert_eq!(
            ensure(h(1), Some((h(7), 5)), 5, &staged, &mut p),
            Err(SwitchError::NotStaged(h(7)))
        );
        assert!(p.installs.is_empty());
    }

    #[test]
    fn t037_14_the_install_that_does_not_take_is_refused() {
        // A platform that answers Ok and still runs the old code: the host does not drive.
        struct Liar(Artefacts);
        impl CodeInstaller for Liar {
            fn running(&mut self) -> Result<Artefacts, InstallError> {
                Ok(self.0)
            }
            fn install(&mut self, _: Artefacts, _: &[u8; 32]) -> Result<(), InstallError> {
                Ok(())
            }
        }
        let staged = Staged::new();
        let mut l = Liar(art(1, 9));
        let err = ensure(h(1), Some((h(2), 5)), 5, &staged, &mut l).unwrap_err();
        assert!(matches!(err, SwitchError::NotInstalled { .. }), "{err}");
    }

    #[test]
    fn t037_14_short_names_a_version_by_its_first_eight_bytes() {
        assert_eq!(short(&h(0xab)), "abababababababab");
    }
}
