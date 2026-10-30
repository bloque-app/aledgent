#!/usr/bin/env python3
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
"""Promote the images a develop pre-release built, instead of building them again. (bloque-app/bloque-ledgent#312, step 3)

    tools/release/promote-images.py --commit <release commit> --version X.Y.Z [--dry-run]
        [--repository bloque-app/aledgent] [--repo DIR] [--develop origin/develop]

Run by `.github/workflows/release.yml`, in the job `promote`, when release-please has created a
release. The release's images are the pre-release's, by digest, when and only when:

1. a commit D on `develop` has a tree that differs from the release commit's in versions and the
   changelog only, by the rule of tools/ci/tested-tree.py (`version_only`, the same function: what
   counts as a version is decided there and nowhere else);
2. D has a pre-release tag `vX.Y.Z-dev+<D7>`, and a push run of release.yml on `develop` at D finished
   successfully, publish job included (an image on a tag is not evidence: anyone with write access
   can push one);
3. the pre-release's images.json names D as its commit and its own tag as its version, and lists
   exactly the six images, each `ghcr.io/bloque-app/aledgent-<name>@sha256:<64 hex>`;
4. for each image, `:sha-<D>` resolves to that digest, and the image's config carries
   `org.opencontainers.image.revision` = D, `org.opencontainers.image.version` = the pre-release's
   version, and, for ingress, settle, ops and core, `app.aledgent.roots` = v2 (the roots every
   release is built with).

Then each image is copied by digest (`crane copy <repo>@<digest> <repo>:<tag>`: the manifest is
copied byte for byte, the index with its buildx provenance attestation, so the digest does not change)
to `sha-<release commit>` and `v<version>`, and each new tag is read back and must resolve to the
digest. Nothing is relabelled: any change to the config would change the digest and orphan the
attestation that names it.

It fails closed. It never exits non-zero: any doubt, error or exception is `promoted=false` and the
caller builds every image as before (the build pushes the same tags, so a half-done promotion is
overwritten). A mismatch is a reason, printed; it is never a failure, and the release is never held up.

Writes to $GITHUB_OUTPUT (and prints): `promoted` (true|false), `reason`, `source_tag`,
`source_commit`, `embedded_version` (what the promoted binaries report: see below), and, when
promoted, `ingress`, `settle`, `ops`, `signer`, `core`, `sidecar` (`ghcr.io/…@sha256:…`).

The version the binaries report. `ingress` serves `/openapi.json` with `info.version`, and `settle`
names itself `aledgent-settle/<version>` to the network, both from CARGO_PKG_VERSION, which is the
workspace version of the tree they were built from: D's. A promoted release therefore reports D's
workspace version, not the release's. The other images embed no version (the guest programs keep a
fixed one by design, CONTRIBUTING.md "Versions"). Nothing in bloque-ledgent's intake or deploy reads
it (intake checks images.json's version and commit, and that each digest resolves). The image labels
`org.opencontainers.image.revision` and `.version` are the pre-release's, too. What identifies the
release is its tag and its images.json (which records `promoted_from`).
"""
from __future__ import annotations

import argparse
import importlib.util
import json
import os
import re
import subprocess
import sys
import tomllib
import traceback
from collections.abc import Callable
from pathlib import Path
from typing import Any, NamedTuple

HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location("tested_tree", HERE.parent / "ci" / "tested-tree.py")
tt = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(tt)

NAMES = ("ingress", "settle", "ops", "signer", "core", "sidecar")
# The images built with the book's roots (images.yml): their config says which. The sidecar is always v2.
WITH_ROOTS = ("ingress", "settle", "ops", "core")
ROOTS = "v2"
REGISTRY = "ghcr.io/bloque-app"
RELEASE_WORKFLOW = "release.yml"
PUBLISH_JOB = "images.json and dispatch"
# How far down develop to look for D, and how many commits with D's tree to ask the registry about.
MAX_WALK = 200
MAX_CANDIDATES = 3

SHA40 = re.compile(r"[0-9a-f]{40}")
VERSION = re.compile(r"\d+\.\d+\.\d+")
DIGEST = re.compile(r"sha256:[0-9a-f]{64}")


class Env(NamedTuple):
    """Everything outside the checkout, replaceable by a test."""

    api: Callable[[str], Any]  # GitHub REST: path -> JSON
    images_json: Callable[[str], str]  # the pre-release's images.json, by tag
    digest: Callable[[str], str]  # crane digest <ref>
    config: Callable[[str], dict]  # crane config --platform linux/amd64 <ref>
    copy: Callable[[str, str], None]  # crane copy <src> <dst>


class Refusal(Exception):
    """This candidate cannot be promoted: the message is why."""


class Source(NamedTuple):
    commit: str
    tag: str
    images: dict[str, str]  # name -> ghcr.io/…@sha256:…
    embedded_version: str


# --------------------------------------------------------------------------------------------------
# The real environment.
# --------------------------------------------------------------------------------------------------
def _run(*cmd: str) -> str:
    out = subprocess.run(cmd, capture_output=True, text=True)
    if out.returncode != 0:
        raise Refusal(f"`{' '.join(cmd[:3])} …` failed: {out.stderr.strip()[:300]}")
    return out.stdout


def real_env(repository: str) -> Env:
    return Env(
        api=tt.gh_api,
        images_json=lambda tag: _run("gh", "release", "download", tag, "-R", repository, "-p", "images.json", "-O", "-"),
        digest=lambda ref: _run("crane", "digest", ref).strip(),
        config=lambda ref: json.loads(_run("crane", "config", "--platform", "linux/amd64", ref)),
        copy=lambda src, dst: _run("crane", "copy", src, dst) and None,
    )


# --------------------------------------------------------------------------------------------------
# Finding and checking the pre-release.
# --------------------------------------------------------------------------------------------------
def git(repo: Path, *args: str) -> str:
    return tt.git(repo, *args)


def develop_walk(repo: Path, develop: str) -> list[str]:
    """develop's first-parent history, newest first: every pull request lands as one commit on it."""
    return git(repo, "rev-list", "--first-parent", f"--max-count={MAX_WALK}", develop).split()


def workspace_version(repo: Path, rev: str) -> str:
    return tomllib.loads(git(repo, "show", f"{rev}:Cargo.toml"))["workspace"]["package"]["version"]


def check_candidate(repo: Path, repository: str, commit: str, env: Env) -> Source:
    """The pre-release of `commit`, or why there is none to trust. Reads; changes nothing."""
    short = commit[:7]
    tags = [t for t in git(repo, "tag", "--points-at", commit).split() if re.fullmatch(rf"v\d+\.\d+\.\d+-dev\+{short}", t)]
    if len(tags) != 1:
        raise Refusal(f"{len(tags)} pre-release tags (v…-dev+{short})")
    tag = tags[0]

    # A pre-release is evidence only if the workflow that makes them ran for this commit on develop
    # and published: a tag and a release asset can be made by hand.
    runs = env.api(
        f"repos/{repository}/actions/workflows/{RELEASE_WORKFLOW}/runs"
        f"?head_sha={commit}&event=push&status=completed&per_page=20"
    ).get("workflow_runs", [])
    good = [r for r in runs if r.get("head_sha") == commit and r.get("event") == "push"
            and r.get("head_branch") == "develop" and r.get("conclusion") == "success"]
    published = False
    for run in good:
        jobs = env.api(f"repos/{repository}/actions/runs/{run['id']}/jobs?filter=latest&per_page=100").get("jobs", [])
        if any(j.get("name") == PUBLISH_JOB and j.get("conclusion") == "success" for j in jobs):
            published = True
            break
    if not published:
        raise Refusal(f"no successful push run of {RELEASE_WORKFLOW} on develop at {short} that published its images")

    try:
        listed = json.loads(env.images_json(tag))
    except (ValueError, Refusal) as e:
        raise Refusal(f"{tag}: images.json unreadable ({e})") from e
    if not isinstance(listed, dict) or listed.get("version") != tag or listed.get("commit") != commit:
        raise Refusal(f"{tag}: images.json names another version or commit")
    images = listed.get("images")
    if not isinstance(images, dict) or set(images) != set(NAMES):
        raise Refusal(f"{tag}: images.json does not list exactly {', '.join(NAMES)}")

    for name in NAMES:
        ref = images[name]
        repo_name = f"{REGISTRY}/aledgent-{name}"
        if not isinstance(ref, str) or not ref.startswith(repo_name + "@") or not DIGEST.fullmatch(ref.split("@", 1)[1]):
            raise Refusal(f"{tag}: {name} is not {repo_name}@sha256:<64 hex>")
        digest = ref.split("@", 1)[1]
        by_sha = env.digest(f"{repo_name}:sha-{commit}")
        if by_sha != digest:
            raise Refusal(f"{name}: sha-{short} is {by_sha}, images.json says {digest}")
        labels = (env.config(ref).get("config") or {}).get("Labels") or {}
        if labels.get("org.opencontainers.image.revision") != commit:
            raise Refusal(f"{name}: its revision label is not {short}")
        if labels.get("org.opencontainers.image.version") != tag[1:]:
            raise Refusal(f"{name}: its version label is not {tag[1:]}")
        if name in WITH_ROOTS and labels.get("app.aledgent.roots") != ROOTS:
            raise Refusal(f"{name}: built with roots {labels.get('app.aledgent.roots')!r}, a release has {ROOTS}")
    return Source(commit, tag, dict(images), workspace_version(repo, commit))


def find_source(repo: Path, repository: str, release: str, develop: str, env: Env) -> tuple[Source | None, str]:
    """(source, reason): the pre-release whose images the release may take, or why none."""
    if not SHA40.fullmatch(release):
        return None, "the release commit is not 40 hex"
    if not tt.versions_agree(repo, release):
        return None, "the release's versions disagree (Cargo.toml, openapi.json, manifest)"
    released = tt.released_members(repo, release)
    reasons: list[str] = []
    asked = 0
    for commit in develop_walk(repo, develop):
        if commit == release:
            continue
        same, _ = tt.version_only(repo, commit, release, released)
        if not same:
            continue
        if asked == MAX_CANDIDATES:
            reasons.append(f"stopped after {MAX_CANDIDATES} commits with the release's tree")
            break
        asked += 1
        try:
            return check_candidate(repo, repository, commit, env), f"{commit[:7]} has the release's tree but for versions"
        except Refusal as e:
            reasons.append(f"{commit[:7]}: {e}")
    if not reasons:
        reasons.append(f"none of develop's last {MAX_WALK} commits has the release's tree but for versions")
    return None, "; ".join(reasons)


# --------------------------------------------------------------------------------------------------
# Promotion.
# --------------------------------------------------------------------------------------------------
def promote(source: Source, release: str, version: str, env: Env) -> dict[str, str]:
    """Copy every image by digest to its release tags, read each tag back. Raises Refusal if any step fails."""
    if not VERSION.fullmatch(version):
        raise Refusal(f"version {version!r} is not X.Y.Z")
    out: dict[str, str] = {}
    for name in NAMES:
        repo_name = f"{REGISTRY}/aledgent-{name}"
        ref = source.images[name]
        digest = ref.split("@", 1)[1]
        for tag in (f"sha-{release}", f"v{version}"):
            env.copy(ref, f"{repo_name}:{tag}")
        for tag in (f"sha-{release}", f"v{version}"):
            got = env.digest(f"{repo_name}:{tag}")
            if got != digest:
                raise Refusal(f"{name}:{tag} is {got} after the copy, not {digest}")
        out[name] = f"{repo_name}@{digest}"
    return out


# --------------------------------------------------------------------------------------------------
# Entry point.
# --------------------------------------------------------------------------------------------------
def emit(values: dict[str, str]) -> None:
    for k, v in values.items():
        print(f"{k}={v}")
    path = os.environ.get("GITHUB_OUTPUT")
    if path:
        with open(path, "a") as f:
            for k, v in values.items():
                f.write(f"{k}={v.replace(chr(10), ' ')}\n")


def decide(args: argparse.Namespace, env: Env) -> dict[str, str]:
    repo = Path(args.repo)
    source, reason = find_source(repo, args.repository, args.commit, args.develop, env)
    if source is None:
        return {"promoted": "false", "reason": reason}
    info = {"source_tag": source.tag, "source_commit": source.commit, "embedded_version": source.embedded_version}
    if args.dry_run:
        return {"promoted": "false", "reason": f"dry run: would promote {source.tag} ({reason})", **info}
    try:
        refs = promote(source, args.commit, args.version, env)
    except Refusal as e:
        return {"promoted": "false", "reason": f"the copy failed, the images are built: {e}", **info}
    return {"promoted": "true", "reason": f"promoted {source.tag} ({reason})", **info, **refs}


def main(argv: list[str] | None = None, env: Env | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--commit", required=True, help="the release commit (40 hex)")
    ap.add_argument("--version", required=True, help="the release's version, X.Y.Z (no v)")
    ap.add_argument("--repository", default=os.environ.get("GITHUB_REPOSITORY", "bloque-app/aledgent"))
    ap.add_argument("--repo", default=".", help="a checkout with the whole history and the tags")
    ap.add_argument("--develop", default="origin/develop")
    ap.add_argument("--dry-run", action="store_true", help="decide and verify; copy nothing")
    args = ap.parse_args(argv)
    try:
        values = decide(args, env or real_env(args.repository))
    except BaseException as e:  # noqa: BLE001 - fail closed: whatever it was, the caller builds
        traceback.print_exc()
        values = {"promoted": "false", "reason": f"error, the images are built: {type(e).__name__}: {e}"}
    emit(values)
    return 0


if __name__ == "__main__":
    sys.exit(main())
