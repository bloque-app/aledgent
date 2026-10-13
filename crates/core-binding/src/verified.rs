// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The `verify-genesis` gate (`F-036` `T-036-25`, `REQ-RC-6`, owner rulings `a0022-q2` and
//! `a0022-q2b`): a chainspec-founded book takes no command until a passing `verify-genesis`
//! report for its chainspec is installed.
//!
//! The core host builds a chainspec book's genesis natively and founds the Agent on its roots
//! (`StateGenesis`): the Agent takes them on its founder's word. `REQ-RC-6` asks for a second
//! derivation inside the book's own execution before any command is taken; that is
//! `aledgent-core-host verify-genesis`, which replays the chainspec chunk by chunk inside the PVM
//! and must end at the manifest's roots. This module is what ties its pass to the book:
//!
//! - [`Report`]: what `verify-genesis --out <file>` writes. Format [`FORMAT`], JSON: whether it
//!   passed, the chainspec's commitment (digest and chunk count), the roots it reached, the records
//!   and the time. Public material: no key, no personal data (`INV-17`).
//! - [`install`]: the operator's step (`aledgent-core-host install-verified`). The report is
//!   checked against the deployment's genesis (the chainspec's digest and chunk count) and, when a
//!   manifest is given, its roots; then it is written into the core host's state directory as
//!   [`VERIFIED_FILE`], atomically. A report for another chainspec, other roots, or a failed run
//!   is refused and nothing is written.
//! - [`GenesisGate`]: what the serving core host holds. Closed until a report in the state
//!   directory names the book's own chainspec and the genesis roots the host rebuilt; then open
//!   for good. Each command asked while it is closed reads the file again, so an installed report
//!   opens the book without a restart; a restart finds the report kept with the state.
//!
//! **What the gate does not stop.** Reads (`a0022-q2b`: the operator's V1–V4 run while
//! `verify-genesis` does), the core's own ticks and delivery records, and nothing in the core:
//! the Agent and its program are unchanged (`m-jam-only`). Ingress refuses each command with
//! `ERR-InitialStateNotSealed` before it is logged ([`ingress::boundary::Core::genesis_open`]), so
//! nothing refused is ever taken in or replayed.

use std::format;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::string::{String, ToString};
use std::sync::atomic::{AtomicBool, Ordering};

use ledger_core::chainspec::SpecCommitment;
use serde::{Deserialize, Serialize};

use crate::envelope::Roots;

/// The report's format.
pub const FORMAT: &str = "aledgent/verify-genesis/v1";

/// The installed report's file in the core host's state directory.
pub const VERIFIED_FILE: &str = "genesis.verified.json";

/// A chainspec's commitment, as a report names it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainspecRef {
    /// The chainspec's digest, hex.
    pub digest: String,
    /// Its chunk count, the header included.
    pub chunks: u32,
}

/// Roots, as a report names them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootsRef {
    /// The kernel's composite root, hex.
    pub kernel: String,
    /// The service tree's root, hex.
    pub service: String,
    /// The audit chain's head, hex.
    pub audit: String,
}

/// What `verify-genesis` came to (module documentation).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    /// [`FORMAT`].
    pub format: String,
    /// Whether the chunk-by-chunk Agent sealed and ended at the manifest's roots.
    pub verified: bool,
    /// The chainspec it ran.
    pub chainspec: ChainspecRef,
    /// The roots the Agent reached.
    pub roots: RootsRef,
    /// Chunks applied.
    pub chunks: u64,
    /// Records applied.
    pub records: u64,
    /// How long it took, in milliseconds.
    pub millis: u64,
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[usize::from(b >> 4)] as char);
        s.push(DIGITS[usize::from(b & 0x0f)] as char);
    }
    s
}

fn unhex32(s: &str) -> Option<[u8; 32]> {
    let b = s.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let nibble = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = (nibble(b[2 * i])? << 4) | nibble(b[2 * i + 1])?;
    }
    Some(out)
}

impl RootsRef {
    /// `roots`, as a report names them.
    pub fn of(roots: &Roots) -> RootsRef {
        RootsRef {
            kernel: hex(&roots.kernel),
            service: hex(&roots.service),
            audit: hex(&roots.audit),
        }
    }

    /// The roots it names, if each is 32 bytes of hex.
    pub fn roots(&self) -> Option<Roots> {
        Some(Roots {
            kernel: unhex32(&self.kernel)?,
            service: unhex32(&self.service)?,
            audit: unhex32(&self.audit)?,
        })
    }
}

impl Report {
    /// A report of a run over the chainspec `commitment` names.
    pub fn new(
        commitment: &SpecCommitment,
        roots: &Roots,
        verified: bool,
        chunks: u64,
        records: u64,
        millis: u64,
    ) -> Report {
        Report {
            format: FORMAT.to_string(),
            verified,
            chainspec: ChainspecRef {
                digest: hex(&commitment.digest),
                chunks: commitment.chunks,
            },
            roots: RootsRef::of(roots),
            chunks,
            records,
            millis,
        }
    }

    /// Read a report. Any other format is refused.
    pub fn parse(bytes: &[u8]) -> Result<Report, String> {
        let r: Report = serde_json::from_slice(bytes)
            .map_err(|e| format!("not a verify-genesis report: {e}"))?;
        if r.format != FORMAT {
            return Err(format!(
                "not a verify-genesis report: format {:?}, not {FORMAT}",
                r.format
            ));
        }
        Ok(r)
    }

    /// The report's JSON, as written.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_default()
    }

    /// Whether the report opens the book whose genesis commits `commitment`: it passed, it ran
    /// that chainspec, and, when `roots` are given (the manifest's, or the genesis the core host
    /// rebuilt), it reached them.
    pub fn check(&self, commitment: &SpecCommitment, roots: Option<&Roots>) -> Result<(), String> {
        if !self.verified {
            return Err(
                "the report is of a failed run: verify-genesis did not reach the manifest's roots \
                 (a no-go for the window)"
                    .into(),
            );
        }
        if unhex32(&self.chainspec.digest) != Some(commitment.digest)
            || self.chainspec.chunks != commitment.chunks
        {
            return Err(format!(
                "the report is of chainspec {} ({} chunks), not of this book's {} ({} chunks)",
                self.chainspec.digest,
                self.chainspec.chunks,
                hex(&commitment.digest),
                commitment.chunks
            ));
        }
        if let Some(want) = roots {
            let got = self
                .roots
                .roots()
                .ok_or("the report's roots are not 32 bytes of hex each")?;
            if got != *want {
                return Err(format!(
                    "the report reached roots {:?}, not this book's genesis roots {:?}",
                    self.roots,
                    RootsRef::of(want)
                ));
            }
        }
        Ok(())
    }
}

/// Install `report` in the core host's state directory `state` (module documentation): checked
/// against `commitment` and, when given, `roots`, then written atomically as [`VERIFIED_FILE`].
/// Returns the report.
pub fn install(
    report: &[u8],
    state: &Path,
    commitment: &SpecCommitment,
    roots: Option<&Roots>,
) -> Result<Report, String> {
    let r = Report::parse(report)?;
    r.check(commitment, roots)?;
    fs::create_dir_all(state).map_err(|e| format!("{}: {e}", state.display()))?;
    let path = state.join(VERIFIED_FILE);
    let tmp = state.join(format!("{VERIFIED_FILE}.tmp"));
    let bytes = r.to_json().into_bytes();
    (|| -> std::io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(&bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, &path)?;
        if let Ok(d) = fs::File::open(state) {
            d.sync_all()?;
        }
        Ok(())
    })()
    .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(r)
}

/// The gate a serving core host holds for a chainspec book (module documentation).
#[derive(Debug)]
pub struct GenesisGate {
    path: PathBuf,
    commitment: SpecCommitment,
    roots: Roots,
    open: AtomicBool,
}

impl GenesisGate {
    /// The gate of the book whose genesis commits `commitment` and was rebuilt at `roots`, over
    /// the report in `state`.
    pub fn new(state: &Path, commitment: SpecCommitment, roots: Roots) -> GenesisGate {
        GenesisGate {
            path: state.join(VERIFIED_FILE),
            commitment,
            roots,
            open: AtomicBool::new(false),
        }
    }

    /// Whether the book takes commands: a report for its chainspec and genesis roots is installed.
    /// Once open, open for good; while closed, the file is read again at each call.
    pub fn is_open(&self) -> bool {
        if self.open.load(Ordering::Acquire) {
            return true;
        }
        let ok = self.read().is_ok();
        if ok {
            self.open.store(true, Ordering::Release);
        }
        ok
    }

    /// The installed report, checked; or why the book is still closed.
    pub fn read(&self) -> Result<Report, String> {
        let bytes = fs::read(&self.path).map_err(|e| {
            format!(
                "no verify-genesis report at {}: {e} (aledgent-core-host install-verified)",
                self.path.display()
            )
        })?;
        let r = Report::parse(&bytes)?;
        r.check(&self.commitment, Some(&self.roots))?;
        Ok(r)
    }

    /// Where the report is read from.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The roots a `build-spec` manifest (`genesis.json`) states, checked to be of the chainspec
/// `commitment` names.
pub fn manifest_roots(manifest: &[u8], commitment: &SpecCommitment) -> Result<Roots, String> {
    let m: serde_json::Value =
        serde_json::from_slice(manifest).map_err(|e| format!("the manifest: {e}"))?;
    let h = |v: &serde_json::Value, what: &str| {
        v.as_str()
            .and_then(unhex32)
            .ok_or_else(|| format!("the manifest's {what}"))
    };
    if h(&m["chainspec"]["digest"], "chainspec digest")? != commitment.digest {
        return Err("the manifest of another chainspec".into());
    }
    Ok(Roots {
        kernel: h(&m["roots"]["kernel"], "kernel root")?,
        service: h(&m["roots"]["service"], "service root")?,
        audit: h(&m["roots"]["audit"], "audit root")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    fn commitment(d: u8) -> SpecCommitment {
        SpecCommitment {
            digest: [d; 32],
            chunks: 9,
        }
    }

    fn roots(r: u8) -> Roots {
        Roots {
            kernel: [r; 32],
            service: [r + 1; 32],
            audit: [r + 2; 32],
        }
    }

    fn passing(d: u8, r: u8) -> Vec<u8> {
        Report::new(&commitment(d), &roots(r), true, 9, 40, 1_000)
            .to_json()
            .into_bytes()
    }

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "core-binding-verified-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&d);
        d
    }

    /// `T-036-25`: the gate is closed until a report for the book's chainspec and genesis roots is
    /// installed, then open; the installed file survives the gate (a restart reads it again).
    #[test]
    fn t_036_25_the_gate_opens_on_a_passing_report_for_this_book() {
        let state = dir("opens");
        let gate = GenesisGate::new(&state, commitment(7), roots(3));
        assert!(!gate.is_open());
        install(&passing(7, 3), &state, &commitment(7), Some(&roots(3))).unwrap();
        assert!(gate.is_open());
        let again = GenesisGate::new(&state, commitment(7), roots(3));
        assert!(again.is_open(), "kept across a restart, with the state");
        let _ = fs::remove_dir_all(&state);
    }

    /// `T-036-25`: a report of another chainspec, of other roots, or of a failed run is refused at
    /// install and nothing is written; one copied in by hand does not open the gate either.
    #[test]
    fn t_036_25_a_report_for_another_genesis_is_refused() {
        let state = dir("refused");
        let other_spec = install(&passing(8, 3), &state, &commitment(7), None).unwrap_err();
        assert!(other_spec.contains("not of this book's"), "{other_spec}");
        let other_roots =
            install(&passing(7, 4), &state, &commitment(7), Some(&roots(3))).unwrap_err();
        assert!(other_roots.contains("genesis roots"), "{other_roots}");
        let failed = Report::new(&commitment(7), &roots(3), false, 9, 40, 1)
            .to_json()
            .into_bytes();
        assert!(install(&failed, &state, &commitment(7), None).is_err());
        assert!(install(b"{}", &state, &commitment(7), None).is_err());
        assert!(!state.join(VERIFIED_FILE).exists(), "nothing installed");
        // By hand: the serving host checks it against the roots it rebuilt.
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join(VERIFIED_FILE), passing(7, 4)).unwrap();
        let gate = GenesisGate::new(&state, commitment(7), roots(3));
        assert!(!gate.is_open());
        assert!(gate.read().unwrap_err().contains("genesis roots"));
        let _ = fs::remove_dir_all(&state);
    }

    #[test]
    fn a_report_round_trips_and_its_format_is_checked() {
        let bytes = passing(1, 2);
        let r = Report::parse(&bytes).unwrap();
        assert_eq!(r.roots.roots(), Some(roots(2)));
        let mut v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        v["format"] = "aledgent/verify-genesis/v0".into();
        assert!(Report::parse(&serde_json::to_vec(&v).unwrap()).is_err());
        v["format"] = FORMAT.into();
        v["extra"] = 1.into();
        assert!(Report::parse(&serde_json::to_vec(&v).unwrap()).is_err());
    }

    #[test]
    fn the_manifest_names_its_chainspec_and_roots() {
        let m = serde_json::json!({
            "chainspec": { "digest": hex(&[7; 32]) },
            "roots": RootsRef::of(&roots(3)),
        });
        let bytes = serde_json::to_vec(&m).unwrap();
        assert_eq!(manifest_roots(&bytes, &commitment(7)), Ok(roots(3)));
        assert!(manifest_roots(&bytes, &commitment(8)).is_err());
    }
}
