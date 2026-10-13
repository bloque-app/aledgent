// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! The command-line verifier's logic (feature `std`), the `aledgent-verify` binary's whole body
//! (feature `cli`).
//!
//! ```text
//! aledgent-verify <disclosure-file> [--clerk-key <hex>] (--rpc <url> | --anchors <file>)
//! ```
//!
//! - `<disclosure-file>`: the disclosure's bytes, raw or as hex text.
//! - `--clerk-key`: the deployment's clerk public key, 32 bytes of hex. Without it, the key the
//!   deployment publishes on its instance record is used; with it, a different published key
//!   fails the check.
//! - `--rpc`: any node of the settlement layer, or a light client's RPC; the anchor is read at its
//!   finalized head (feature `rpc`).
//! - `--anchors`: a read saved earlier, one line per fact: `anchor <venue> <number>
//!   <commitment-hex> <stored-at> <read-at>` or `key <venue> <key-hex>`, where `<venue>` is the
//!   settlement pallet's instance (`1`) or `contract:<account-hex>` for the settlement contract
//!   (`F-027`). It is only as good as whoever made it.
//!
//! It prints the report: the verdict, the signed-assertion label, the assertion, the checkpoint,
//! the assurance level and every check. Exit status: 0 pass, 1 fail, 2 the input could not be
//! read.

use std::boxed::Box;
use std::format;
use std::string::{String, ToString};
use std::vec::Vec;

use crate::source::{AnchorSource, MemAnchors, PublicAnchor, Venue};
use crate::verify;

/// The usage line.
pub const USAGE: &str =
    "usage: aledgent-verify <disclosure-file> [--clerk-key <hex>] (--rpc <url> | --anchors <file>)";

fn hex_bytes(s: &str) -> Result<Vec<u8>, String> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if s.len() % 2 != 0 || !s.is_ascii() {
        return Err("not hex".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

fn hex32(s: &str) -> Result<[u8; 32], String> {
    hex_bytes(s.trim())?
        .try_into()
        .map_err(|_| "a key or commitment is 32 bytes".to_string())
}

/// A saved read of the settlement layer (see the module documentation).
pub fn saved(text: &str) -> Result<MemAnchors, String> {
    let mut m = MemAnchors::new();
    for (n, line) in text.lines().enumerate() {
        let f: Vec<&str> = line.split_whitespace().collect();
        let num = |s: &str| s.parse::<u64>().map_err(|e| format!("line {}: {e}", n + 1));
        let venue = |s: &str| -> Result<Venue, String> {
            match s.strip_prefix("contract:") {
                Some(account) => Ok(Venue::Contract {
                    account: hex32(account).map_err(|e| format!("line {}: {e}", n + 1))?,
                }),
                None => Ok(Venue::Pallet {
                    instance: s
                        .parse::<u32>()
                        .map_err(|e| format!("line {}: {e}", n + 1))?,
                }),
            }
        };
        match f.as_slice() {
            [] => {}
            ["anchor", v, number, commitment, stored_at, read_at] => {
                m.put_in(
                    venue(v)?,
                    num(number)?,
                    PublicAnchor {
                        commitment: hex32(commitment)?,
                        stored_at: num(stored_at)?,
                        read_at: num(read_at)?,
                    },
                );
            }
            ["key", v, key] => {
                m.publish_key_in(venue(v)?, hex32(key)?);
            }
            _ => return Err(format!("line {}: not a saved read", n + 1)),
        }
    }
    Ok(m)
}

/// A disclosure's bytes from a file's contents: hex text, or the raw bytes.
pub fn disclosure_bytes(raw: &[u8]) -> Result<Vec<u8>, String> {
    if let Ok(text) = core::str::from_utf8(raw) {
        let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        let digits = compact.strip_prefix("0x").unwrap_or(&compact);
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_hexdigit()) {
            return hex_bytes(digits);
        }
    }
    Ok(raw.to_vec())
}

#[cfg(feature = "rpc")]
fn rpc_source(url: &str) -> Result<Box<dyn AnchorSource>, String> {
    Ok(Box::new(crate::rpc::RpcAnchors::new(url)))
}

#[cfg(not(feature = "rpc"))]
fn rpc_source(_: &str) -> Result<Box<dyn AnchorSource>, String> {
    Err("this build reads no node: it was built without the `rpc` feature".into())
}

/// Run the verifier on `args` (without the program name). Returns the exit status and what to
/// print: the report on 0 and 1, the error on 2.
pub fn run(args: &[String]) -> (u8, String) {
    match run_inner(args) {
        Ok((passed, text)) => (if passed { 0 } else { 1 }, text),
        Err(e) => (2, format!("aledgent-verify: {e}\n")),
    }
}

fn run_inner(args: &[String]) -> Result<(bool, String), String> {
    let mut file = None;
    let mut key = None;
    let mut rpc = None;
    let mut anchors = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--clerk-key" => key = Some(hex32(it.next().ok_or(USAGE)?)?),
            "--rpc" => rpc = Some(it.next().ok_or(USAGE)?.clone()),
            "--anchors" => anchors = Some(it.next().ok_or(USAGE)?.clone()),
            "-h" | "--help" => return Ok((true, format!("{USAGE}\n"))),
            f if file.is_none() && !f.starts_with("--") => file = Some(f.to_string()),
            _ => return Err(USAGE.into()),
        }
    }
    let path = file.ok_or(USAGE)?;
    let raw = std::fs::read(&path).map_err(|e| format!("{path}: {e}"))?;
    let bytes = disclosure_bytes(&raw)?;
    let source: Box<dyn AnchorSource> = match (rpc, anchors) {
        (Some(url), None) => rpc_source(&url)?,
        (None, Some(path)) => {
            let text = std::fs::read_to_string(&path).map_err(|e| format!("{path}: {e}"))?;
            Box::new(saved(&text)?)
        }
        _ => return Err(USAGE.into()),
    };
    let report = verify(&bytes, key, &source.as_ref()).map_err(|e| e.to_string())?;
    Ok((report.passed(), report.render()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claim::{Claim, SignedClaim};
    use crate::envelope::{
        Assertion, Assurance, AssuranceKind, BalanceKind, CheckpointRef, Disclosure, Recipient,
        VERSION,
    };
    use crate::{SIGNED_ASSERTION_LABEL, commitment};
    use cipher_clerk::crypto::SecretKey;
    use std::vec;

    fn sign(sk: &SecretKey, msg: &[u8]) -> [u8; 64] {
        let s = sk.sign(msg, &mut rand_core::OsRng);
        let mut out = [0u8; 64];
        out[..32].copy_from_slice(&s.r);
        out[32..].copy_from_slice(&s.s);
        out
    }

    fn disclosure(sk: &SecretKey) -> Disclosure {
        let root = [3u8; 32];
        let claim = Claim::BalanceAtLeast {
            account: [2; 16],
            layer: 0,
            threshold: 50,
            state_root: root,
        };
        let mut d = Disclosure {
            version: VERSION,
            instance: 1,
            venue: Venue::Pallet { instance: 1 },
            subject: "did:aledgent:acct:x".into(),
            recipient: Recipient::Reference([1; 32]),
            assertion: Assertion::HoldsAtLeast {
                asset: "USD".into(),
                amount: 50,
                kind: BalanceKind::Available,
            },
            checkpoint: CheckpointRef {
                number: 4,
                entry_sequence: 20,
                kernel_root: root,
                commitment: commitment(crate::CONSTRUCTION_V2, 1, 4, 20, &root),
                construction: crate::CONSTRUCTION_V2,
            },
            assurance: Assurance {
                declaration_digest: [9; 32],
                summary: format!("{SIGNED_ASSERTION_LABEL} Assurance m2."),
                kind: AssuranceKind::SignedAssertion,
            },
            issued_at: 5,
            claims: vec![SignedClaim {
                claim,
                signature: sign(sk, &claim.public_bytes()),
            }],
            signature: [0; 64],
        };
        d.signature = sign(sk, &d.signed_bytes());
        d
    }

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn t_017_06_the_cli_passes_a_disclosure_on_a_saved_read_and_fails_a_tampered_one() {
        let sk = SecretKey::from_bytes([5; 32]).unwrap();
        let d = disclosure(&sk);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("disclosure.hex");
        std::fs::write(&file, hex(&d.to_bytes())).unwrap();
        let read = dir.path().join("anchors.txt");
        std::fs::write(
            &read,
            format!(
                "anchor 1 4 {} 10 12\nkey 1 {}\n",
                hex(&d.checkpoint.commitment),
                hex(&sk.public_key().0)
            ),
        )
        .unwrap();
        let (code, text) = run(&args(&[
            file.to_str().unwrap(),
            "--anchors",
            read.to_str().unwrap(),
        ]));
        assert_eq!(code, 0, "{text}");
        assert!(text.contains("verdict: PASS"));
        assert!(text.contains(SIGNED_ASSERTION_LABEL));
        assert!(text.contains("Assurance m2."));
        // Raw bytes work too; a tampered one fails.
        let mut bytes = d.to_bytes();
        let n = bytes.len();
        bytes[n - 3] ^= 1;
        let raw = dir.path().join("tampered.bin");
        std::fs::write(&raw, &bytes).unwrap();
        let (code, text) = run(&args(&[
            raw.to_str().unwrap(),
            "--anchors",
            read.to_str().unwrap(),
        ]));
        assert_eq!(code, 1, "{text}");
        assert!(text.contains("verdict: FAIL"));
        // Another key given than the one published: a failure.
        let other = SecretKey::from_bytes([6; 32]).unwrap().public_key().0;
        let (code, _) = run(&args(&[
            file.to_str().unwrap(),
            "--clerk-key",
            &hex(&other),
            "--anchors",
            read.to_str().unwrap(),
        ]));
        assert_eq!(code, 1);
        // Unreadable input.
        let (code, text) = run(&args(&["--anchors"]));
        assert_eq!(code, 2, "{text}");
        let (code, _) = run(&args(&["/no/such/file", "--anchors", "/no/such/read"]));
        assert_eq!(code, 2);
    }

    #[test]
    fn t_027_04_a_saved_read_names_the_contract_venue() {
        let account = [0xc0u8; 32];
        let m = saved(&format!(
            "anchor contract:{} 4 {} 10 12\nkey contract:{} {}\n",
            hex(&account),
            hex(&[7; 32]),
            hex(&account),
            hex(&[8; 32])
        ))
        .unwrap();
        let venue = Venue::Contract { account };
        assert_eq!(
            m.anchor(&venue, 4).unwrap().map(|a| a.commitment),
            Some([7; 32])
        );
        assert_eq!(m.attestation_key(&venue), Ok(Some([8; 32])));
        assert_eq!(m.anchor(&Venue::Pallet { instance: 1 }, 4), Ok(None));
        assert!(saved("anchor contract:00 4 00 1 2\n").is_err());
    }
}
