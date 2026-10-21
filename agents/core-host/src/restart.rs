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

use vos::agent_sdk::BlobRef;

use crate::code_source::Fetcher;

/// The packages a host holds beside the genesis one, by the digest they were fetched under.
pub trait Held {
    /// The digests there are entries for, in a fixed order.
    fn digests(&self) -> Vec<[u8; 32]>;
    /// The bytes of the entry for `digest`, if they hash to it.
    fn read(&self, digest: &[u8; 32]) -> Option<Vec<u8>>;
}

impl Held for Fetcher {
    fn digests(&self) -> Vec<[u8; 32]> {
        self.held()
    }
    fn read(&self, digest: &[u8; 32]) -> Option<Vec<u8>> {
        self.read_held(digest)
    }
}

/// The bytes of the package the runtime has installed, which `installed` refers to: the genesis
/// package (`--actor`) if it is that, else the entry of the code cache whose bytes are the ones
/// the reference names (hash and length). Every candidate is compared byte for byte by VOS's own
/// reference; nothing is guessed from a name.
///
/// # Errors
///
/// The installed package is neither the genesis one nor held: the host cannot attach it, and says
/// where it was looked for.
pub fn package_installed(
    installed: &BlobRef,
    genesis: &[u8],
    held: Option<&dyn Held>,
) -> Result<Vec<u8>, String> {
    if installed.matches(genesis) {
        return Ok(genesis.to_vec());
    }
    let Some(held) = held else {
        return Err(format!(
            "the Agent has installed a package of {} bytes that is not the --actor package, and \
             this host has no code cache to look for it in: give --code-config naming the cache \
             that holds the versions it switched to (T-037-21)",
            installed.len
        ));
    };
    for digest in held.digests() {
        if let Some(bytes) = held.read(&digest)
            && installed.matches(&bytes)
        {
            return Ok(bytes);
        }
    }
    Err(format!(
        "the Agent has installed a package of {} bytes that is not the --actor package and that \
         the code cache holds no copy of: restore the cache that holds the versions this host \
         switched to (T-037-21)",
        installed.len
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A cache of fixed entries.
    struct Cache(BTreeMap<[u8; 32], Vec<u8>>);

    impl Cache {
        fn of(entries: &[&[u8]]) -> Self {
            Cache(
                entries
                    .iter()
                    .map(|b| (ledger_core::code::artefact_digest(b), b.to_vec()))
                    .collect(),
            )
        }
    }

    impl Held for Cache {
        fn digests(&self) -> Vec<[u8; 32]> {
            self.0.keys().copied().collect()
        }
        fn read(&self, digest: &[u8; 32]) -> Option<Vec<u8>> {
            self.0.get(digest).cloned()
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
    fn t037_21_a_copy_that_is_not_the_installed_one_is_never_attached() {
        let g = b"package of g";
        // Same length as the installed package, other bytes.
        let cache = Cache::of(&[b"package of X"]);
        let err = package_installed(&BlobRef::of_bytes(b"package of H"), g, Some(&cache));
        assert!(err.unwrap_err().contains("holds no copy"));
    }

    #[test]
    fn t037_21_a_host_that_cannot_find_the_installed_package_says_where_to_look() {
        let g = b"package of g";
        let installed = BlobRef::of_bytes(b"package of H");
        let none = package_installed(&installed, g, None).unwrap_err();
        assert!(none.contains("--code-config"), "{none}");
        let empty = Cache::of(&[]);
        let held = package_installed(&installed, g, Some(&empty)).unwrap_err();
        assert!(held.contains("code cache"), "{held}");
    }
}
