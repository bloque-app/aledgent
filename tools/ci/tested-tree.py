#!/usr/bin/env python3
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
"""Is this pull request's tree one that CI has already tested? (bloque-app/bloque-ledgent#312, step 2)

    tools/ci/tested-tree.py [--repo DIR]    # DIR: a pull request's merge checkout (default: here)

Prints and writes to $GITHUB_OUTPUT one of two modes:

- `full`: run every check. The answer for everything that is not provably redundant, and the answer
  whenever anything goes wrong (an API error, an unexpected checkout): the script never fails, and
  never says `skip` on a doubt.
- `skip`: the heavy checks are redundant, and report success without running. Two cases only, both
  on a pull request into `main` from a branch of this repository, both decided by comparing trees:

  (a) a release-please pull request (head branch `release-please--*`) whose merge changes nothing
      but versions and the changelog (version-only, below), on a `main` whose tip is proven: a push
      run of `.github/workflows/ci.yml` finished with every required job successful on the tip, or
      on an ancestor (the develop tip a promotion merged, say) whose tree is the tip's but for
      versions, and the latest finished push run on `main` did not fail. A tip whose run is still
      going, or red, and no such ancestor: not proven, `full`. The gate decides when its run starts:
      after a hotfix, re-run the release pull request's checks once `main`'s run is green.
  (b) the promotion `develop` -> `main` whose merge result is, but for version-only differences, the
      tree of a `develop` commit D whose own push run of ci.yml finished with every required job
      successful. A pull request's own run is not evidence: a skipped run reports success, and a skip
      must never certify the next one.

  For both, the merge result's versions must agree: Cargo.toml's workspace version, openapi.json's
  info.version and the manifest's ".".

"Version-only" means, file by file, on parsed content (compared with its types: 1 is not 1.0 is not
true) and not on lines:

    CHANGELOG.md                     anything, as an edit
    .release-please-manifest.json    only the value of "."
    Cargo.toml                       only workspace.package.version
    Cargo.lock, agents/Cargo.lock    only `version` of the packages release-please-config.json bumps
                                     in that file (and which have no `source`)
    services/ingress/openapi.json    only info.version

Any other path, an added or removed file, a mode change: not version-only. The branch name is a
precondition, never the proof: the diff decides.

Who judges. This script and the action that runs it are the BASE's copy, never the pull request's
(the gate job checks the base out and the merge beside it): a pull request cannot supply its own
judge. A base without them means no gate, so `full`. The release-please configuration is read from
`main`'s tip, too.

The CI jobs this skips are those that take minutes to an hour. The cheap checks (rustfmt, the
structural gates, licences, DCO, the conventional title) run on every pull request.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import tomllib
from collections.abc import Callable
from pathlib import Path
from typing import Any

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
# How far down a branch to look for a commit with the tree, and how many such commits to ask the API about.
MAX_WALK = 60
MAX_API_CANDIDATES = 8

Api = Callable[[str], Any]
# lock file -> the packages release-please bumps in it
Released = dict[str, frozenset[str]]


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


def canon(v: Any) -> Any:
    """A value with its types in it: Python says 1 == 1.0 == True, and a lock file must not."""
    if v is None:
        return ("null",)
    if isinstance(v, bool):
        return ("bool", v)
    if isinstance(v, int):
        return ("int", v)
    if isinstance(v, float):
        return ("float", repr(v))
    if isinstance(v, str):
        return ("str", v)
    if isinstance(v, dict):
        return ("map", tuple(sorted((k, canon(x)) for k, x in v.items())))
    if isinstance(v, (list, tuple)):
        return ("list", tuple(canon(x) for x in v))
    return (type(v).__name__, str(v))  # a TOML date or time


def same(a: Any, b: Any) -> bool:
    return canon(a) == canon(b)


def released_members(repo: Path, rev: str) -> Released:
    """The packages release-please-config.json bumps in each lock file, as of `rev`."""
    config = json.loads(_blob(repo, rev, "release-please-config.json"))
    out: dict[str, set[str]] = {}
    for package in config["packages"].values():
        for extra in package.get("extra-files", []):
            if isinstance(extra, dict) and extra.get("type") == "toml" and extra["path"].endswith("Cargo.lock"):
                out.setdefault(extra["path"], set()).update(
                    re.findall(r"name\.value=='([^']+)'", extra["jsonpath"])
                )
    return {path: frozenset(names) for path, names in out.items()}


def _cargo_toml(a: str, b: str, _released: Released) -> bool:
    docs = [tomllib.loads(t) for t in (a, b)]
    for d in docs:
        pkg = d.get("workspace", {}).get("package")
        if isinstance(pkg, dict):
            pkg.pop("version", None)
    return same(docs[0], docs[1])


def _cargo_lock_of(path: str) -> Callable[[str, str, Released], bool]:
    def check(a: str, b: str, released: Released) -> bool:
        names = released.get(path, frozenset())
        docs = [tomllib.loads(t) for t in (a, b)]
        for d in docs:
            for pkg in d.get("package", []):
                # a workspace member that release-please bumps: its version is the release's.
                # Any other package, a guest program's included, keeps its own.
                if "source" not in pkg and pkg.get("name") in names:
                    pkg["version"] = ""
        return same(docs[0], docs[1])

    return check


def _openapi(a: str, b: str, _released: Released) -> bool:
    docs = [json.loads(t) for t in (a, b)]
    for d in docs:
        d["info"]["version"] = ""
    return same(docs[0], docs[1])


def _manifest(a: str, b: str, _released: Released) -> bool:
    docs = [json.loads(t) for t in (a, b)]
    for d in docs:
        d["."] = ""
    return same(docs[0], docs[1])


def _anything(_a: str, _b: str, _released: Released) -> bool:
    return True


# path -> does the file differ in its version only?
VERSION_FILES: dict[str, Callable[[str, str, Released], bool]] = {
    "Cargo.toml": _cargo_toml,
    "Cargo.lock": _cargo_lock_of("Cargo.lock"),
    "agents/Cargo.lock": _cargo_lock_of("agents/Cargo.lock"),
    "services/ingress/openapi.json": _openapi,
    ".release-please-manifest.json": _manifest,
    "CHANGELOG.md": _anything,
}


def changed(repo: Path, a: str, b: str) -> list[tuple[str, str, str, str]]:
    """(status, old mode, new mode, path) of every difference between two commits."""
    raw = git(repo, "diff", "--raw", "--no-renames", "-z", a, b).split("\0")
    if raw and raw[-1] == "":
        raw.pop()
    out = []
    i = 0
    while i < len(raw):
        if not raw[i].startswith(":") or i + 1 >= len(raw):
            raise ValueError(f"unexpected `git diff --raw -z` output at field {i}: {raw[i][:60]!r}")
        meta = raw[i][1:].split()
        if len(meta) != 5:
            raise ValueError(f"unexpected `git diff --raw -z` record: {raw[i][:80]!r}")
        out.append((meta[4], meta[0], meta[1], raw[i + 1]))
        i += 2
    return out


def version_only(repo: Path, a: str, b: str, released: Released) -> tuple[bool, str]:
    """Whether commits a and b differ in versions and the changelog only, and the first reason if not."""
    for status, old_mode, new_mode, path in changed(repo, a, b):
        check = VERSION_FILES.get(path)
        if check is None:
            return False, f"{path} is not a version file"
        if status != "M" or old_mode != new_mode:
            return False, f"{path} is {status} (mode {old_mode} -> {new_mode}), not an edit"
        if not check(_blob(repo, a, path), _blob(repo, b, path), released):
            return False, f"{path} differs in more than its version"
    return True, "differs in versions and the changelog only"


def versions_agree(repo: Path, rev: str) -> bool:
    """Cargo.toml's workspace version, openapi.json's info.version and the manifest's "." are one."""
    cargo = tomllib.loads(_blob(repo, rev, "Cargo.toml"))["workspace"]["package"]["version"]
    openapi = json.loads(_blob(repo, rev, "services/ingress/openapi.json"))["info"]["version"]
    manifest = json.loads(_blob(repo, rev, ".release-please-manifest.json"))["."]
    return all(isinstance(v, str) and v for v in (cargo, openapi, manifest)) and cargo == openapi == manifest


# --------------------------------------------------------------------------------------------------
# Evidence: did the push CI of a commit pass?
# --------------------------------------------------------------------------------------------------
def gh_api(path: str) -> Any:
    out = subprocess.run(
        ["gh", "api", "-H", "Accept: application/vnd.github+json", path],
        capture_output=True, text=True, check=True,
    )
    return json.loads(out.stdout)


def run_required_ok(api: Api, repository: str, run: dict) -> bool:
    jobs = api(f"repos/{repository}/actions/runs/{run['id']}/jobs?filter=latest&per_page=100").get("jobs", [])
    result = {j["name"]: j.get("conclusion") for j in jobs}
    return all(result.get(name) == "success" for name in REQUIRED_CI_JOBS)


def ci_passed(api: Api, repository: str, commit: str) -> str | None:
    """The URL of a finished push run of ci.yml on `commit` whose required jobs all succeeded."""
    runs = api(
        f"repos/{repository}/actions/workflows/{CI_WORKFLOW}/runs"
        f"?head_sha={commit}&event=push&status=completed&per_page=20"
    ).get("workflow_runs", [])
    for run in runs:
        if run.get("head_sha") != commit or run.get("event") != "push":
            continue
        if run_required_ok(api, repository, run):
            return run.get("html_url") or str(run["id"])
    return None


def latest_main_run_failed(api: Api, repository: str) -> bool:
    """Whether the latest finished push run of ci.yml on main did not pass every required job."""
    runs = api(
        f"repos/{repository}/actions/workflows/{CI_WORKFLOW}/runs"
        f"?branch=main&event=push&status=completed&per_page=1"
    ).get("workflow_runs", [])
    return bool(runs) and not run_required_ok(api, repository, runs[0])


def tested_commit(
    repo: Path, api: Api, repository: str, walk: list[str], target: str, released: Released
) -> tuple[str | None, str | None, int]:
    """(commit, run URL, candidates asked): the first commit of `walk` whose tree is `target`'s but
    for versions and whose push run passed."""
    asked = 0
    for commit in walk:
        ok, _ = version_only(repo, commit, target, released)
        if not ok:
            continue
        if asked == MAX_API_CANDIDATES:
            break
        asked += 1
        url = ci_passed(api, repository, commit)
        if url:
            return commit, url, asked
    return None, None, asked


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
    released = released_members(repo, main_tip)

    if head_ref.startswith("release-please--"):
        ok, why = version_only(repo, main_tip, "HEAD", released)
        if not ok:
            return FULL, f"{head_ref}: merging it changes more than versions ({why})"
        if not versions_agree(repo, "HEAD"):
            return FULL, f"{head_ref}: Cargo.toml, openapi.json and the manifest do not agree on the version"
        if latest_main_run_failed(api, repository):
            return FULL, "the latest finished push run of ci.yml on main did not pass: main is not proven"
        # Every ancestor, not the first-parent line: main's tip is usually the promotion's merge
        # commit, whose second parent is the develop tip that was tested. Whatever ancestor is
        # accepted must have main's tree but for versions, and a passed push run of its own.
        walk = git(repo, "rev-list", f"-n{MAX_WALK}", main_tip).split()
        commit, url, asked = tested_commit(repo, api, repository, walk, main_tip, released)
        if commit is None:
            return FULL, (
                f"main's tip {main_tip[:12]} has no finished, passed push run of ci.yml (running, red or "
                f"none; {asked} candidate(s) asked): nothing proves the tree this pull request versions"
            )
        return SKIP, (
            f"release-please pull request: merging it {why}, on a main whose tip equals "
            f"{commit[:12]} but for versions, and that commit's push run passed ({url})"
        )

    if head_ref == "develop":
        if not versions_agree(repo, "HEAD"):
            return FULL, "promotion: Cargo.toml, openapi.json and the manifest do not agree on the version"
        base = git(repo, "merge-base", main_tip, head).strip()
        walk = git(repo, "rev-list", "--first-parent", f"-n{MAX_WALK}", head, f"^{base}").split()
        commit, url, asked = tested_commit(repo, api, repository, walk, "HEAD", released)
        if commit is not None:
            return SKIP, (
                f"promotion: the merge differs from develop's {commit[:12]} in versions and the "
                f"changelog only, and that commit's push run passed ({url})"
            )
        return FULL, (
            f"promotion: no develop commit among the {len(walk)} newest has this tree and a passed push "
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
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--repo", type=Path, default=Path.cwd(), help="the pull request's merge checkout")
    args = parser.parse_args()
    mode, reason = decide(
        args.repo,
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
