#!/usr/bin/env python3
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
"""Tests of tools/ci/tested-tree.py: when CI may skip its heavy checks, and when it must not.

    python3 tools/ci/test-tested-tree.py

Synthetic repositories in a temporary directory, built with git; no network, a fake API. Every case
that must skip has a neighbour that must not, because a wrong `skip` is the failure that matters.
"""
from __future__ import annotations

import importlib.util
import json
import re
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
_spec = importlib.util.spec_from_file_location("tested_tree", HERE / "tested-tree.py")
tt = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(tt)

CARGO_TOML = """[workspace]
members = ["crates/a"]

[workspace.package]
version = "{v}"
edition = "2024"
"""
# `a` is a released member; `guest` is compiled into a guest program and keeps a version of its own.
CARGO_LOCK = """version = 4

[[package]]
name = "a"
version = "{v}"
dependencies = ["serde"]

[[package]]
name = "guest"
version = "0.0.1"

[[package]]
name = "serde"
version = "1.0.0"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "abc"
"""
OPENAPI = {"info": {"title": "t", "description": "d", "version": "{v}"}, "paths": {}, "n": 1}
RELEASE_CONFIG = {
    "packages": {
        ".": {
            "release-type": "simple",
            "extra-files": [
                {"type": "toml", "path": "Cargo.toml", "jsonpath": "$.workspace.package.version"},
                {"type": "toml", "path": "Cargo.lock",
                 "jsonpath": "$.package[?(!@.source && (@.name.value=='a'))].version"},
                {"type": "toml", "path": "agents/Cargo.lock",
                 "jsonpath": "$.package[?(!@.source && (@.name.value=='a'))].version"},
            ],
        }
    }
}


def files(v: str = "0.1.0") -> dict[str, str]:
    openapi = json.loads(json.dumps(OPENAPI).replace("{v}", v))
    return {
        "Cargo.toml": CARGO_TOML.format(v=v),
        "Cargo.lock": CARGO_LOCK.format(v=v),
        "agents/Cargo.lock": CARGO_LOCK.format(v=v),
        "services/ingress/openapi.json": json.dumps(openapi, indent=2) + "\n",
        ".release-please-manifest.json": json.dumps({".": v}) + "\n",
        "release-please-config.json": json.dumps(RELEASE_CONFIG) + "\n",
        "CHANGELOG.md": f"# Changelog\n\n## {v}\n",
        "crates/a/src/lib.rs": "pub fn f() {}\n",
        ".github/workflows/ci.yml": "name: CI\n",
    }


class Repo:
    def __init__(self, root: Path):
        self.root = root
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.email", "t@example.com")
        self.git("config", "user.name", "t")
        self.git("config", "commit.gpgsign", "false")

    def git(self, *args: str) -> str:
        return subprocess.run(
            ["git", "-C", str(self.root), *args], capture_output=True, text=True, check=True
        ).stdout.strip()

    def commit(self, changes: dict[str, str | None], msg: str = "c") -> str:
        for path, text in changes.items():
            p = self.root / path
            if text is None:
                p.unlink()
                continue
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(text)
        self.git("add", "-A")
        self.git("commit", "-q", "--allow-empty", "-m", msg)
        return self.git("rev-parse", "HEAD")

    def merge(self, branch: str) -> tuple[str, str]:
        """Check out the merge of `branch` into the current branch, detached: a pull request's merge ref."""
        main_tip = self.git("rev-parse", "HEAD")
        self.git("merge", "-q", "--no-ff", "-m", "merge", branch)
        merged = self.git("rev-parse", "HEAD")
        self.git("checkout", "-q", "--detach", merged)
        return main_tip, self.git("rev-parse", f"{merged}^2")


def release_bump(v_old: str, v_new: str) -> dict[str, str]:
    new, old = files(v_new), files(v_old)
    out = {p: t for p, t in new.items() if t != old[p]}
    out["CHANGELOG.md"] = old["CHANGELOG.md"] + f"\n## {v_new}\n\n* a change\n"
    return out


def event(head_ref: str, head_sha: str, base_ref: str = "main", fork: bool = False) -> dict:
    return {
        "pull_request": {
            "base": {"ref": base_ref, "repo": {"full_name": "o/r"}},
            "head": {
                "ref": head_ref,
                "sha": head_sha,
                "repo": {"full_name": "someone/r" if fork else "o/r"},
            },
        }
    }


def fake_api(
    passed: set[str],
    failing: dict[str, str] | None = None,
    main_latest: str | None = None,
    calls: list | None = None,
):
    """A CI API in which the push runs of the commits in `passed` succeeded, those in `failing` failed
    in the job named (or lack the job, for "missing"), and `main_latest` is the commit of the latest
    finished push run on main."""
    failing = failing or {}

    def run_of(sha: str) -> dict:
        return {"id": int(sha[:6], 16), "head_sha": sha, "event": "push", "html_url": f"https://run/{sha[:7]}"}

    def api(path: str) -> dict:
        if calls is not None:
            calls.append(path)
        m = re.search(r"workflows/ci\.yml/runs\?head_sha=(\w+)&event=push", path)
        if m:
            sha = m.group(1)
            return {"workflow_runs": [run_of(sha)] if sha in passed or sha in failing else []}
        if "workflows/ci.yml/runs?branch=main&event=push" in path:
            return {"workflow_runs": [run_of(main_latest)] if main_latest else []}
        m = re.search(r"actions/runs/(\d+)/jobs", path)
        if m:
            for sha in list(passed) + list(failing):
                if int(sha[:6], 16) == int(m.group(1)):
                    bad = failing.get(sha)
                    return {"jobs": [{"name": n, "conclusion": "failure" if n == bad else "success"}
                                     for n in tt.REQUIRED_CI_JOBS if n != "tests" or bad != "missing"]}
        raise AssertionError(f"unexpected API call {path}")

    return api


class Base(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.r = Repo(Path(self._tmp.name))
        self.base = self.r.commit(files("0.1.0"), "base")

    def vo(self, a: str, b: str) -> tuple[bool, str]:
        return tt.version_only(self.r.root, a, b, tt.released_members(self.r.root, self.base))


class VersionOnly(Base):
    def check(self, changes: dict[str, str | None]) -> bool:
        tip = self.r.commit(changes)
        return self.vo(self.base, tip)[0]

    def test_a_release_bump_is_version_only(self):
        self.assertTrue(self.check(release_bump("0.1.0", "0.2.0")))

    def test_identical_trees(self):
        self.assertTrue(self.check({}))

    def test_changelog_edited(self):
        self.assertTrue(self.check({"CHANGELOG.md": "other\n"}))

    def test_changelog_removed_or_mode_changed_is_not_an_edit(self):
        self.assertFalse(self.check({"CHANGELOG.md": None}))

    def test_changelog_mode_change_is_not(self):
        self.r.git("update-index", "--chmod=+x", "CHANGELOG.md")
        self.r.git("commit", "-q", "-m", "mode")
        self.assertFalse(self.vo(self.base, "HEAD")[0])

    def test_a_source_change_with_the_bump_is_not(self):
        self.assertFalse(self.check({**release_bump("0.1.0", "0.2.0"), "crates/a/src/lib.rs": "pub fn g() {}\n"}))

    def test_a_workflow_change_is_not(self):
        self.assertFalse(self.check({".github/workflows/ci.yml": "name: CI2\n"}))

    def test_a_new_file_is_not(self):
        self.assertFalse(self.check({"new.txt": "x\n"}))

    def test_cargo_toml_beyond_the_version(self):
        edited = files("0.2.0")["Cargo.toml"].replace('edition = "2024"', 'edition = "2021"')
        self.assertFalse(self.check({"Cargo.toml": edited}))
        deps = files("0.1.0")["Cargo.toml"] + '\n[workspace.dependencies]\nserde = "1"\n'
        self.assertFalse(self.check({"Cargo.toml": deps}))

    def test_a_crates_own_manifest_is_not_a_version_file(self):
        self.assertFalse(self.check({"crates/a/Cargo.toml": '[package]\nversion = "0.2.0"\n'}))

    def test_lock_only_the_released_members_versions(self):
        self.assertTrue(self.check({"Cargo.lock": files("0.2.0")["Cargo.lock"]}))
        # a registry package moving is a dependency change, not a release
        bumped = files("0.1.0")["Cargo.lock"].replace('version = "1.0.0"', 'version = "1.0.1"')
        self.assertFalse(self.check({"Cargo.lock": bumped}))
        # nor a source-less package release-please does not bump (a guest program's crate)
        guest = files("0.1.0")["Cargo.lock"].replace('version = "0.0.1"', 'version = "0.0.2"')
        self.assertFalse(self.check({"Cargo.lock": guest}))
        self.assertFalse(self.check({"agents/Cargo.lock": guest}))
        # nor a released member's dependency list
        deps = files("0.2.0")["Cargo.lock"].replace('dependencies = ["serde"]', "dependencies = []")
        self.assertFalse(self.check({"Cargo.lock": deps}))
        self.assertFalse(self.check({"agents/Cargo.lock": deps}))

    def test_a_lock_file_the_config_does_not_name_blanks_nothing(self):
        config = json.loads(files()["release-please-config.json"])
        config["packages"]["."]["extra-files"] = config["packages"]["."]["extra-files"][:2]  # no agents lock
        self.r.commit({"release-please-config.json": json.dumps(config)})
        base = self.r.git("rev-parse", "HEAD")
        tip = self.r.commit({"agents/Cargo.lock": files("0.2.0")["agents/Cargo.lock"]})
        self.assertFalse(tt.version_only(self.r.root, base, tip, tt.released_members(self.r.root, base))[0])

    def test_openapi_only_info_version(self):
        self.assertTrue(self.check({"services/ingress/openapi.json": files("0.2.0")["services/ingress/openapi.json"]}))
        doc = json.loads(files("0.2.0")["services/ingress/openapi.json"])
        doc["info"]["description"] = "changed"
        self.assertFalse(self.check({"services/ingress/openapi.json": json.dumps(doc)}))
        doc = json.loads(files("0.1.0")["services/ingress/openapi.json"])
        doc["paths"]["/new"] = {}
        self.assertFalse(self.check({"services/ingress/openapi.json": json.dumps(doc)}))

    def test_values_are_compared_with_their_types(self):
        # Python: 1 == 1.0 and 1 == True. A document where one became the other is not unchanged.
        for other in ("1.0", "true"):
            text = files("0.1.0")["services/ingress/openapi.json"].replace('"n": 1', f'"n": {other}')
            self.assertFalse(self.check({"services/ingress/openapi.json": text}), other)

    def test_manifest_only_the_root_value(self):
        self.assertTrue(self.check({".release-please-manifest.json": json.dumps({".": "9.9.9"})}))
        self.assertFalse(self.check({".release-please-manifest.json": json.dumps({".": "0.1.0", "x": "1"})}))

    def test_a_mode_change_is_not(self):
        self.r.git("update-index", "--chmod=+x", "Cargo.toml")
        self.r.git("commit", "-q", "-m", "mode")
        self.assertFalse(self.vo(self.base, "HEAD")[0])

    def test_malformed_content_raises(self):
        tip = self.r.commit({"Cargo.toml": "not = [toml"})
        with self.assertRaises(Exception):
            self.vo(self.base, tip)

    def test_a_diff_that_is_not_fully_understood_raises(self):
        junk = ":100644 100644 aaa bbb M\0Cargo.toml\0trailing\0"
        with mock.patch.object(tt, "git", return_value=junk):
            with self.assertRaises(ValueError):
                tt.changed(self.r.root, "a", "b")


class Versions(Base):
    def test_agreeing_versions(self):
        self.assertTrue(tt.versions_agree(self.r.root, self.base))

    def test_a_manifest_that_lags(self):
        tip = self.r.commit({**release_bump("0.1.0", "0.2.0"), ".release-please-manifest.json": json.dumps({".": "0.1.0"})})
        self.assertFalse(tt.versions_agree(self.r.root, tip))

    def test_an_openapi_that_lags(self):
        tip = self.r.commit({"Cargo.toml": files("0.2.0")["Cargo.toml"]})
        self.assertFalse(tt.versions_agree(self.r.root, tip))


class Classify(Base):
    def run_pr(self, head_ref: str, changes: dict, api=None, **ev):
        self.r.git("checkout", "-q", "-b", "pr", self.base)
        self.r.commit(changes)
        self.r.git("checkout", "-q", "main")
        main_tip, head = self.r.merge("pr")
        path = self.r.root / "event.json"
        path.write_text(json.dumps(event(head_ref, head, **ev)))
        # by default, main's tip has a passed push run, and it is main's latest
        api = api or fake_api({self.base}, main_latest=self.base)
        return tt.decide(self.r.root, "pull_request", str(path), "o/r", api)

    def mode(self, *a, **kw) -> str:
        return self.run_pr(*a, **kw).__getitem__(0)

    # (a) release-please
    def test_release_please_version_only_on_a_proven_main_skips(self):
        mode, why = self.run_pr("release-please--branches--main--components--aledgent", release_bump("0.1.0", "0.2.0"))
        self.assertEqual(mode, "skip", why)

    def test_release_please_branch_with_other_changes_runs_everything(self):
        self.assertEqual(self.mode("release-please--branches--main",
                                   {**release_bump("0.1.0", "0.2.0"), "crates/a/src/lib.rs": "pub fn g() {}\n"}), "full")

    def test_release_please_with_versions_that_disagree_runs_everything(self):
        changes = release_bump("0.1.0", "0.2.0")
        del changes[".release-please-manifest.json"]
        self.assertEqual(self.mode("release-please--x", changes), "full")

    def test_main_tip_without_a_finished_passed_run_is_not_proven(self):
        # a hotfix just merged: its push run is still going
        self.assertEqual(self.mode("release-please--x", release_bump("0.1.0", "0.2.0"), api=fake_api(set())), "full")

    def test_main_tip_whose_run_failed_is_not_proven(self):
        api = fake_api(set(), failing={self.base: "clippy"}, main_latest=self.base)
        self.assertEqual(self.mode("release-please--x", release_bump("0.1.0", "0.2.0"), api=api), "full")

    def test_a_failed_latest_run_on_main_refuses_even_if_the_tip_passed(self):
        newer = "f" * 40
        api = fake_api({self.base}, failing={newer: "tests"}, main_latest=newer)
        self.assertEqual(self.mode("release-please--x", release_bump("0.1.0", "0.2.0"), api=api), "full")

    def test_an_ancestor_equal_but_for_versions_is_proof_of_the_tip(self):
        # main moved by a changelog edit since its tested commit
        self.r.commit({"CHANGELOG.md": "# Changelog\n\nedited\n"}, "docs on main")
        self.base_before = self.base
        tested = self.r.git("rev-parse", "HEAD~1")
        self.r.git("checkout", "-q", "-b", "pr", "HEAD")
        self.r.commit(release_bump("0.1.0", "0.2.0") | {"CHANGELOG.md": "# Changelog\n\nedited\n\n## 0.2.0\n"})
        self.r.git("checkout", "-q", "main")
        main_tip, head = self.r.merge("pr")
        path = self.r.root / "event.json"
        path.write_text(json.dumps(event("release-please--x", head)))
        d = tt.decide(self.r.root, "pull_request", str(path), "o/r", fake_api({tested}, main_latest=tested))
        self.assertEqual(d[0], "skip", d[1])

    def test_the_name_is_not_the_proof_and_a_version_only_diff_is_not_enough_on_another_branch(self):
        self.assertEqual(self.mode("feat/bump", release_bump("0.1.0", "0.2.0")), "full")

    def test_release_please_from_a_fork_or_into_another_base_runs_everything(self):
        self.assertEqual(self.mode("release-please--x", release_bump("0.1.0", "0.2.0"), fork=True), "full")

    def test_release_please_into_another_base_runs_everything(self):
        self.assertEqual(self.mode("release-please--x", release_bump("0.1.0", "0.2.0"), base_ref="develop"), "full")

    def test_a_checkout_that_is_not_the_merge_runs_everything(self):
        self.r.commit(release_bump("0.1.0", "0.2.0"))
        path = self.r.root / "event.json"
        path.write_text(json.dumps(event("release-please--x", self.r.git("rev-parse", "HEAD"))))
        self.assertEqual(tt.decide(self.r.root, "pull_request", str(path), "o/r", fake_api(set()))[0], "full")

    def test_not_a_pull_request_runs_everything(self):
        self.assertEqual(tt.decide(self.r.root, "push", None, "o/r", fake_api(set()))[0], "full")
        self.assertEqual(tt.decide(self.r.root, "schedule", None, "o/r", fake_api(set()))[0], "full")

    def test_an_unreadable_event_runs_everything(self):
        self.assertEqual(tt.decide(self.r.root, "pull_request", "/nonexistent", "o/r", fake_api(set()))[0], "full")

    def test_malformed_manifest_in_the_pull_request_runs_everything(self):
        self.assertEqual(self.mode("release-please--x", {"Cargo.toml": "not = [toml"}), "full")

    def test_a_pull_request_cannot_widen_what_is_released(self):
        # it edits the config to name `guest` too, and bumps it: the config read is main's, not its own
        config = json.loads(files()["release-please-config.json"])
        for e in config["packages"]["."]["extra-files"][1:]:
            e["jsonpath"] = e["jsonpath"].replace("=='a'", "=='a' || @.name.value=='guest'")
        changes = {**release_bump("0.1.0", "0.2.0"),
                   "release-please-config.json": json.dumps(config),
                   "Cargo.lock": files("0.2.0")["Cargo.lock"].replace('version = "0.0.1"', 'version = "0.0.2"')}
        self.assertEqual(self.mode("release-please--x", changes), "full")


class Promotion(Base):
    """develop -> main. main = base + a release (versions); develop = base + features, one tested."""

    def build(self, develop_after_tested: dict | None = None):
        self.r.git("checkout", "-q", "-b", "develop")
        self.tested = self.r.commit({"crates/a/src/lib.rs": "pub fn g() {}\n"}, "tested feature")
        self.top = self.tested
        if develop_after_tested:
            self.top = self.r.commit(develop_after_tested, "after")
        self.r.git("checkout", "-q", "main")
        # main moved by a release in the meantime: versions and the changelog only
        self.r.commit(release_bump("0.1.0", "0.2.0"), "release")

    def promote(self, api, branch: str = "develop") -> tuple[str, str]:
        main_tip, head = self.r.merge(branch)
        path = self.r.root / "event.json"
        path.write_text(json.dumps(event(branch, head)))
        return tt.decide(self.r.root, "pull_request", str(path), "o/r", api)

    def test_tested_tree_skips_even_though_main_carries_a_release(self):
        self.build()
        d = self.promote(fake_api({self.tested}))
        self.assertEqual(d[0], "skip", d[1])

    def test_a_failed_required_job_does_not(self):
        self.build()
        self.assertEqual(self.promote(fake_api(set(), failing={self.tested: "clippy"}))[0], "full")

    def test_a_missing_required_job_does_not(self):
        self.build()
        self.assertEqual(self.promote(fake_api(set(), failing={self.tested: "missing"}))[0], "full")

    def test_no_run_does_not(self):
        self.build()
        self.assertEqual(self.promote(fake_api(set()))[0], "full")

    def test_an_api_error_does_not(self):
        self.build()

        def boom(path: str) -> dict:
            raise OSError("rate limited")

        d = self.promote(boom)
        self.assertEqual(d[0], "full")
        self.assertIn("rate limited", d[1])

    def test_a_newer_untested_change_does_not(self):
        self.build({"crates/a/src/lib.rs": "pub fn h() {}\n"})
        self.assertEqual(self.promote(fake_api({self.tested}))[0], "full")

    def test_a_tested_commit_followed_by_a_change_and_its_revert_skips(self):
        self.build({"scratch.txt": "x\n"})
        self.r.git("checkout", "-q", "develop")
        self.r.commit({"scratch.txt": None}, "revert")
        self.r.git("checkout", "-q", "main")
        self.assertEqual(self.promote(fake_api({self.tested}))[0], "skip")

    def test_main_with_a_change_develop_lacks_does_not(self):
        self.build()
        self.r.commit({"crates/a/src/hotfix.rs": "pub fn hotfix() {}\n"}, "hotfix on main")
        self.assertEqual(self.promote(fake_api({self.tested}))[0], "full")

    def test_only_a_push_run_counts(self):
        self.build()
        calls: list[str] = []
        self.promote(fake_api({self.tested}, calls=calls))
        self.assertTrue(all("event=push" in c or "/jobs" in c for c in calls), calls)

    def test_a_feature_branch_into_main_is_not_a_promotion(self):
        self.build()
        self.r.git("branch", "feat/x", "develop")
        self.assertEqual(self.promote(fake_api({self.tested}), branch="feat/x")[0], "full")


class Contract(unittest.TestCase):
    def test_every_required_job_is_a_job_of_ci_yml(self):
        ci = (ROOT / ".github/workflows/ci.yml").read_text()
        names = set(re.findall(r"^    name: (.+)$", ci, re.M))
        for job in tt.REQUIRED_CI_JOBS:
            self.assertIn(job, names, f"{job!r} is no longer a job name in ci.yml: keep REQUIRED_CI_JOBS in step")


if __name__ == "__main__":
    unittest.main()
