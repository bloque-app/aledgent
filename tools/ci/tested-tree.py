#!/usr/bin/env python3
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
"""Is this pull request's tree one that CI has already tested? (bloque-app/bloque-ledgent#312, step 2)

    tools/ci/tested-tree.py          # in a pull request's merge checkout; writes `mode` and `reason`

Prints and writes to $GITHUB_OUTPUT one of two modes:

- `full`: run every check. The answer for everything that is not provably redundant, and the answer
  whenever anything goes wrong (an API error, an unexpected checkout): the script never fails, and
  never says `skip` on a doubt.
- `skip`: the heavy checks are redundant, and report success without running. Two cases only, both
  on a pull request into `main` from a branch of this repository, both decided by comparing trees:

  (a) a release-please pull request (head branch `release-please--*`) whose merge changes nothing
      but versions and the changelog: what merging does to `main`'s tree (the merge commit against
      its first parent) is version-only (below). `main`'s own tree was tested when it landed.
  (b) the promotion `develop` -> `main` whose merge result is, but for version-only differences, the
      tree of a `develop` commit D whose own CI run (the push to `develop`; `.github/workflows/ci.yml`)
      finished with every required job of that workflow successful. A pull request's own run is not
      evidence: a skipped run reports success, and a skip must never certify the next one.

"Version-only" means, file by file, on parsed content and not on lines:

    CHANGELOG.md                     anything
    .release-please-manifest.json    only the value of "."
    Cargo.toml                       only workspace.package.version
    Cargo.lock, agents/Cargo.lock    only `version` of the packages without a `source` (the workspace's)
    services/ingress/openapi.json    only info.version

Any other path, an added or removed file (the changelog apart), a mode change: not version-only.
The branch name is a precondition, never the proof: the diff decides.

The CI jobs this skips are those that take minutes to an hour. The cheap checks (rustfmt, the
structural gates, licences, DCO, the conventional title) run on every pull request.
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import tomllib
from collections.abc import Callable
from pathlib import Path

FULL, SKIP = "full", "skip"

# The required checks of `main` that ci.yml produces (branch protection, `gh api
# repos/bloque-app/aledgent/branches/main/protection/required_status_checks`), by job name. DCO and
# the conventional title are other workflows' and run live on every pull request, so they are not
# evidence here. test-tested-tree.py checks that every name is a job of ci.yml.
REQUIRED_CI_JOBS = [
    "rustfmt",
    "clippy",
    "tests",
    "no floating point in crates",
    "kernel feature graph",
    "no production crate enables conformance-ports",
    "VOS boundary and guest determinism",
    "agents workspace (core host)",
    "conformance (current gate profile)",
    "pass-import (F-028 migration tool)",
    "licence headers, REUSE and dependency licences",
]
CI_WORKFLOW = "ci.yml"
# How far down develop to look for a commit with the tree, and how many such commits to ask the API about.
MAX_WALK = 60
MAX_API_CANDIDATES = 8

Api = Callable[[str], dict]


# --------------------------------------------------------------------------------------------------
# Trees: what differs between two commits, and whether it is only a version.
# --------------------------------------------------------------------------------------------------
def git(repo: Path, *args: str, check: bool = True) -> str:
    out = subprocess.run(
        ["git", "-C", str(repo), *args], capture_output=True, text=True, check=check
    )
    return out.stdout


def _blob(repo: Path, rev: str, path: str) -> str:
    return git(repo, "show", f"{rev}:{path}")


def _cargo_toml(a: str, b: str) -> bool:
    docs = [tomllib.loads(t) for t in (a, b)]
    for d in docs:
        pkg = d.get("workspace", {}).get("package")
        if isinstance(pkg, dict):
            pkg.pop("version", None)
    return docs[0] == docs[1]


def _cargo_lock(a: str, b: str) -> bool:
    docs = [tomllib.loads(t) for t in (a, b)]
    for d in docs:
        for pkg in d.get("package", []):
            if "source" not in pkg:  # a workspace member (or path dependency): its version is ours
                pkg["version"] = ""
    return docs[0] == docs[1]


def _openapi(a: str, b: str) -> bool:
    docs = [json.loads(t) for t in (a, b)]
    for d in docs:
        d["info"]["version"] = ""
    return docs[0] == docs[1]


def _manifest(a: str, b: str) -> bool:
    docs = [json.loads(t) for t in (a, b)]
    for d in docs:
        d["."] = ""
    return docs[0] == docs[1]


# path -> (does the file differ in its version only?)
VERSION_FILES: dict[str, Callable[[str, str], bool]] = {
    "Cargo.toml": _cargo_toml,
    "Cargo.lock": _cargo_lock,
    "agents/Cargo.lock": _cargo_lock,
    "services/ingress/openapi.json": _openapi,
    ".release-please-manifest.json": _manifest,
}
FREE_FILES = {"CHANGELOG.md"}


def changed(repo: Path, a: str, b: str) -> list[tuple[str, str, str, str]]:
    """(status, old mode, new mode, path) of every difference between two commits."""
    raw = git(repo, "diff", "--raw", "--no-renames", "-z", a, b).split("\0")
    out = []
    i = 0
    while i + 1 < len(raw) and raw[i].startswith(":"):
        meta = raw[i][1:].split()
        out.append((meta[4], meta[0], meta[1], raw[i + 1]))
        i += 2
    return out


def version_only(repo: Path, a: str, b: str) -> tuple[bool, str]:
    """Whether commits a and b differ in versions and the changelog only, and the first reason if not."""
    for status, old_mode, new_mode, path in changed(repo, a, b):
        if path in FREE_FILES:
            continue
        check = VERSION_FILES.get(path)
        if check is None:
            return False, f"{path} is not a version file"
        if status != "M" or old_mode != new_mode:
            return False, f"{path} is {status} (mode {old_mode} -> {new_mode}), not an edit"
        if not check(_blob(repo, a, path), _blob(repo, b, path)):
            return False, f"{path} differs in more than its version"
    return True, "differs in versions and the changelog only"


# --------------------------------------------------------------------------------------------------
# Evidence: did the push CI of a develop commit pass?
# --------------------------------------------------------------------------------------------------
def gh_api(path: str) -> dict:
    out = subprocess.run(
        ["gh", "api", "-H", "Accept: application/vnd.github+json", path],
        capture_output=True, text=True, check=True,
    )
    return json.loads(out.stdout)


def ci_passed(api: Api, repository: str, commit: str) -> str | None:
    """The URL of a finished push run of ci.yml on `commit` whose required jobs all succeeded."""
    runs = api(
        f"repos/{repository}/actions/workflows/{CI_WORKFLOW}/runs"
        f"?head_sha={commit}&event=push&status=completed&per_page=20"
    ).get("workflow_runs", [])
    for run in runs:
        if run.get("head_sha") != commit or run.get("event") != "push":
            continue
        jobs = api(
            f"repos/{repository}/actions/runs/{run['id']}/jobs?filter=latest&per_page=100"
        ).get("jobs", [])
        result = {j["name"]: j.get("conclusion") for j in jobs}
        if all(result.get(name) == "success" for name in REQUIRED_CI_JOBS):
            return run.get("html_url") or str(run["id"])
    return None


# --------------------------------------------------------------------------------------------------
# The decision.
# --------------------------------------------------------------------------------------------------
def classify(repo: Path, event_name: str, event: dict, repository: str, api: Api) -> tuple[str, str]:
    """(mode, reason). Raises on anything unexpected: `decide` turns that into `full`."""
    if event_name != "pull_request":
        return FULL, f"{event_name} is not a pull request: nothing is skipped"
    pr = event["pull_request"]
    base_ref, head_ref = pr["base"]["ref"], pr["head"]["ref"]
    if base_ref != "main":
        return FULL, f"the pull request is into {base_ref}, not main"
    if pr["head"]["repo"] is None or pr["head"]["repo"]["full_name"] != pr["base"]["repo"]["full_name"]:
        return FULL, "the pull request comes from a fork"

    # The checkout is the merge of the pull request into main: its first parent is main, its second
    # the pull request's head. Anything else is not what was meant to be judged.
    parents = git(repo, "rev-list", "--parents", "-n", "1", "HEAD").split()[1:]
    if len(parents) != 2 or parents[1] != pr["head"]["sha"]:
        return FULL, "the checkout is not the merge of the pull request's head into its base"
    main_tip, head = parents

    if head_ref.startswith("release-please--"):
        ok, why = version_only(repo, main_tip, "HEAD")
        if ok:
            return SKIP, f"release-please pull request: merging it {why}"
        return FULL, f"{head_ref}: merging it changes more than versions ({why})"

    if head_ref == "develop":
        base = git(repo, "merge-base", main_tip, head).strip()
        walk = git(repo, "rev-list", "--first-parent", f"-n{MAX_WALK}", head, f"^{base}").split()
        asked = 0
        for commit in walk:
            ok, _ = version_only(repo, commit, "HEAD")
            if not ok:
                continue
            if asked == MAX_API_CANDIDATES:
                break
            asked += 1
            url = ci_passed(api, repository, commit)
            if url:
                return SKIP, (
                    f"promotion: the merge differs from develop's {commit[:12]} in versions and the "
                    f"changelog only, and that commit's CI run passed ({url})"
                )
        return FULL, (
            f"promotion: no develop commit among the {len(walk)} newest has this tree and a passed CI "
            f"run ({asked} candidate(s) asked)"
        )

    return FULL, f"{head_ref} is neither a release-please branch nor develop"


def decide(repo: Path, event_name: str, event_path: str | None, repository: str, api: Api) -> tuple[str, str]:
    try:
        event = json.loads(Path(event_path).read_text()) if event_path else {}
        return classify(repo, event_name, event, repository, api)
    except Exception as e:  # noqa: BLE001 - fail closed on anything at all
        return FULL, f"could not decide ({type(e).__name__}: {e}): nothing is skipped"


def main() -> int:
    mode, reason = decide(
        Path.cwd(),
        os.environ.get("GITHUB_EVENT_NAME", ""),
        os.environ.get("GITHUB_EVENT_PATH"),
        os.environ.get("GITHUB_REPOSITORY", ""),
        gh_api,
    )
    reason = " ".join(reason.split())  # one line: it is written to $GITHUB_OUTPUT
    print(f"mode={mode}\nreason={reason}")
    if out := os.environ.get("GITHUB_OUTPUT"):
        with open(out, "a") as f:
            f.write(f"mode={mode}\nreason={reason}\n")
    if summary := os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(summary, "a") as f:
            f.write(f"### Tested tree: `{mode}`\n\n{reason}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
