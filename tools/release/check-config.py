#!/usr/bin/env python3
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
"""release-please's configuration still matches the workspace (release-please-config.json).

    tools/release/check-config.py           # check; exit 1 on a mismatch
    tools/release/check-config.py --fix     # rewrite the lock files' filters in the configuration

A release bumps `[workspace.package] version` in Cargo.toml and, in every lock file that holds them,
the released members' own entries, found by name. A member is released unless it is compiled into a
guest program (the core's `core.vos`, the shard's): those keep a fixed version of their own, because
Cargo hashes a crate's version into what the compiler builds it with, and a guest package's bytes are
the installed program's identity (its ProgramId, which the core host checks on every restart). A
release that changed them would change the core it ships without a line of the core changing.

It fails when:

- a released member does not take its version from the workspace (`version.workspace = true`), or a
  guest member does (it must name its own);
- a lock file's filter does not name exactly the released members;
- a lock file holds a released member and the configuration does not update it;
- .release-please-manifest.json and Cargo.toml disagree on the current version.
"""
from __future__ import annotations

import json
import re
import subprocess
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CONFIG = ROOT / "release-please-config.json"
# The guest programs' own workspaces: what their lock files hold by path is in their packages.
GUEST_LOCKS = ["agents/core/Cargo.lock", "agents/shard/Cargo.lock"]


def path_packages(rel: str) -> set[str]:
    """The packages a lock file holds by path (no `source`): this repository's own crates."""
    lock = tomllib.loads((ROOT / rel).read_text())
    return {p["name"] for p in lock.get("package", []) if "source" not in p}


def jsonpath(names: list[str]) -> str:
    """The released members' entries in a lock file (release-please's TOML values are tagged)."""
    alternatives = " || ".join(f"@.name.value=='{n}'" for n in names)
    return f"$.package[?(!@.source && ({alternatives}))].version"


def main() -> int:
    fix = "--fix" in sys.argv[1:]
    errors: list[str] = []
    root = tomllib.loads((ROOT / "Cargo.toml").read_text())
    version = root["workspace"]["package"]["version"]

    members: dict[str, str] = {}  # name -> its directory
    for m in root["workspace"]["members"]:
        manifest = tomllib.loads((ROOT / m / "Cargo.toml").read_text())
        members[manifest["package"]["name"]] = m
    guest = {n for rel in GUEST_LOCKS for n in path_packages(rel)} & members.keys()
    released = sorted(members.keys() - guest)

    for name, m in sorted(members.items()):
        v = tomllib.loads((ROOT / m / "Cargo.toml").read_text())["package"].get("version")
        if name in guest and not isinstance(v, str):
            errors.append(
                f"{m}/Cargo.toml: {name} is compiled into a guest program; give it a fixed "
                "`version = \"…\"` of its own, not the workspace's"
            )
        elif name not in guest and v != {"workspace": True}:
            errors.append(f"{m}/Cargo.toml: set `version.workspace = true`")

    config = json.loads(CONFIG.read_text())
    package = config["packages"]["."]
    manifest_version = json.loads((ROOT / ".release-please-manifest.json").read_text())["."]
    if manifest_version != version:
        errors.append(
            f".release-please-manifest.json says {manifest_version}, Cargo.toml says {version}"
        )

    # Every lock file in the repository that holds a released member must be updated with it.
    tracked = subprocess.run(
        ["git", "ls-files", "*Cargo.lock"], cwd=ROOT, check=True, capture_output=True, text=True
    ).stdout.split()
    holding = sorted(rel for rel in tracked if path_packages(rel) & set(released))

    if fix:
        extra = [e for e in package.get("extra-files", []) if not e["path"].endswith("Cargo.lock")]
        extra += [{"type": "toml", "path": rel, "jsonpath": jsonpath(released)} for rel in holding]
        package["extra-files"] = extra
        CONFIG.write_text(json.dumps(config, indent=2) + "\n")

    updated: set[str] = set()
    has_root = False
    for extra in package.get("extra-files", []):
        path = extra["path"]
        if path == "Cargo.toml":
            has_root = True
            if extra.get("jsonpath") != "$.workspace.package.version":
                errors.append("Cargo.toml: the jsonpath must be $.workspace.package.version")
            continue
        if not path.endswith("Cargo.lock"):
            continue
        updated.add(path)
        if extra.get("jsonpath") != jsonpath(released):
            named = set(re.findall(r"@\.name\.value=='([^']+)'", extra.get("jsonpath", "")))
            errors.append(
                f"{path}: the filter must name exactly the released members (extra: "
                f"{sorted(named - set(released))}, missing: {sorted(set(released) - named)}); "
                "run tools/release/check-config.py --fix"
            )
    if not has_root:
        errors.append("extra-files: Cargo.toml ($.workspace.package.version) is missing")
    for rel in holding:
        if rel not in updated:
            errors.append(f"{rel} holds released members: run tools/release/check-config.py --fix")

    for e in errors:
        print(f"::error::{e}")
    if not errors:
        print(
            f"release configuration matches the workspace: {version}, {len(released)} released "
            f"members, {len(guest)} with a fixed version (guest: {', '.join(sorted(guest))})"
        )
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
