// Copyright (c) 2026 Pagos Cube LLC
// SPDX-License-Identifier: Apache-2.0 OR MIT
//! `opsctl`, the binary (`T-018-01`, `T-018-08`): the runbooks' commands verify a backup and
//! refuse a changed one, say which backups are due, and print the PostgreSQL restore script.

use std::process::Command;

use ops::backup::{self, StoreKind};

fn opsctl(args: &[&str]) -> (bool, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_opsctl"))
        .args(args)
        .output()
        .expect("opsctl runs");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), text)
}

#[test]
fn t_018_08_opsctl_verifies_backups_and_says_what_is_due() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (dir, _) =
        backup::write(root, StoreKind::Audit, "a1", 1, &[("x.bin", b"x")], None).unwrap();
    let (ok, out) = opsctl(&["backup", "verify", dir.to_str().unwrap()]);
    assert!(ok, "{out}");
    assert!(out.contains("1 artifacts verified"), "{out}");
    std::fs::write(dir.join("x.bin"), b"y").unwrap();
    let (ok, out) = opsctl(&["backup", "verify", dir.to_str().unwrap()]);
    assert!(!ok && out.contains("checksum"), "{out}");
    // Nothing verified: every store is due.
    let (ok, out) = opsctl(&["backup", "due", "--root", root.to_str().unwrap()]);
    assert!(ok, "{out}");
    for s in StoreKind::ALL {
        assert!(out.lines().any(|l| l == s.as_str()), "{out}");
    }
    let (ok, out) = opsctl(&[
        "restore",
        "pg-script",
        "--pgdata",
        "/pg",
        "--base",
        "/b.tgz",
        "--wal",
        "/w.tgz",
        "--wal-dir",
        "/wal",
        "--start",
        "postgres",
    ]);
    assert!(
        ok && out.contains("recovery.signal") && out.contains("restore_command"),
        "{out}"
    );
    let (ok, _) = opsctl(&["nonsense"]);
    assert!(!ok);
}
