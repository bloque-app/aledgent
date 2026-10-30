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
members = ["crates/a", "crates/b", "crates/core-binding", "crates/verify", "services/signer", "services/ingress", "services/settle", "ops"]

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


MEMBERS = ["crates/a", "crates/b", "crates/core-binding", "crates/verify", "services/signer",
           "services/ingress", "services/settle", "ops"]


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
        **{f"{d}/Cargo.toml": f'[package]\nname = "{d.rsplit("/", 1)[-1]}"\nversion.workspace = true\n' for d in MEMBERS},
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


def event(head_ref: str, head_sha: str, base_ref: str = "main", fork: bool = False,
          user: str = "pablo", sender: str | None = None) -> dict:
    return {
        "sender": {"login": sender or user},
        "pull_request": {
            "number": 7,
            "user": {"login": user},
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
    def run_pr(self, head_ref: str, changes: dict, api=None, authors: str = "", **ev):
        self.r.git("checkout", "-q", "-b", "pr", self.base)
        self.r.commit(changes)
        self.r.git("checkout", "-q", "main")
        main_tip, head = self.r.merge("pr")
        path = self.r.root / "event.json"
        path.write_text(json.dumps(event(head_ref, head, **ev)))
        # by default, main's tip has a passed push run, and it is main's latest
        api = api or fake_api({self.base}, main_latest=self.base)
        return tt.decide(self.r.root, "pull_request", str(path), "o/r", api, authors)

    def mode(self, *a, **kw) -> str:
        return self.run_pr(*a, **kw).mode

    # (a) release-please
    def test_release_please_version_only_on_a_proven_main_skips(self):
        d = self.run_pr("release-please--branches--main--components--aledgent", release_bump("0.1.0", "0.2.0"))
        self.assertEqual(d.mode, "skip", d.reason)

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
        self.assertEqual(d.mode, "skip", d.reason)

    def promoted_main(self):
        """main = base + a promotion merge of a develop branch with one tested commit, as after a promotion."""
        self.r.git("checkout", "-q", "-b", "develop")
        tested = self.r.commit({"crates/a/src/lib.rs": "pub fn g() {}\n"}, "tested feature")
        self.r.git("checkout", "-q", "main")
        self.r.git("merge", "-q", "--no-ff", "-m", "promote develop", "develop")
        return tested

    def release_pr_on(self, api):
        self.r.git("checkout", "-q", "-b", "pr", "main")
        self.r.commit(release_bump("0.1.0", "0.2.0"))
        self.r.git("checkout", "-q", "main")
        _, head = self.r.merge("pr")
        path = self.r.root / "event.json"
        path.write_text(json.dumps(event("release-please--x", head)))
        return tt.decide(self.r.root, "pull_request", str(path), "o/r", api)

    def test_after_a_promotion_the_tested_develop_tip_is_the_proof(self):
        # main's tip is the merge commit and has no run yet; its second parent had one
        tested = self.promoted_main()
        d = self.release_pr_on(fake_api({tested}, main_latest=tested))
        self.assertEqual(d[0], "skip", d[1])

    def test_a_hotfix_on_top_of_a_promotion_is_not_proven_by_it(self):
        tested = self.promoted_main()
        self.r.commit({"crates/a/src/hotfix.rs": "pub fn hotfix() {}\n"}, "hotfix on main")
        d = self.release_pr_on(fake_api({tested}, main_latest=tested))
        self.assertEqual(d[0], "full", d[1])

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


class HotfixScope(unittest.TestCase):
    members = {"crates/a": "a", "crates/b": "b", "crates/core-binding": "core-binding", "crates/verify": "verify",
               "services/signer": "signer", "services/ingress": "ingress", "services/settle": "settle", "ops": "ops"}

    excluded = ["agents", "contracts", "spikes", "services/settle/kreivo-pin", "e2e/network"]

    def scope(self, *paths: str):
        return tt.hotfix_scope(list(paths), self.members, self.excluded)

    def test_a_nested_excluded_workspace_is_not_its_enclosing_members(self):
        # `-p settle` does not build services/settle/kreivo-pin, which has a job of its own
        args, agents, unowned = self.scope("services/settle/kreivo-pin/src/lib.rs")
        self.assertEqual((args, unowned), ("", ["services/settle/kreivo-pin/src/lib.rs"]))
        self.assertEqual(self.scope("services/settle/src/lib.rs")[:3], ("-p settle", False, []))

    def test_the_crates_the_diff_touches_sorted(self):
        self.assertEqual(self.scope("crates/b/src/x.rs", "crates/a/src/y.rs", "crates/a/tests/z.rs")[:2], ("-p a -p b", False))

    def test_the_deepest_member_owns_a_path(self):
        members = {**self.members, "services/settle/inner": "inner"}
        self.assertEqual(tt.hotfix_scope(["services/settle/inner/src/lib.rs"], members)[0], "-p inner")

    def test_a_readme_in_a_crate_is_that_crates(self):
        self.assertEqual(self.scope("services/settle/README.md")[0], "-p settle")

    def test_templates_belong_to_the_services_that_include_them(self):
        self.assertEqual(self.scope("deploy/templates/signer.json")[0], "-p ingress -p ops -p settle -p signer")

    def test_runbooks_belong_to_core_binding_and_ops(self):
        self.assertEqual(self.scope("ops/runbooks/release/hotfix.md")[0], "-p core-binding -p ops")

    def test_workspace_wide_files_run_the_workspace(self):
        for path in ("Cargo.lock", "Cargo.toml", "rust-toolchain.toml", ".github/workflows/ci.yml", ".cargo/config.toml"):
            self.assertEqual(self.scope("crates/a/src/x.rs", path)[0], "--workspace", path)

    def test_prose_runs_no_test_and_is_owned(self):
        args, agents, unowned = self.scope("docs/x.md", "notes/y.md")
        self.assertEqual((args, agents, unowned), ("", False, []))

    def test_agents_and_the_patched_cipherclerk_are_the_agents_workspaces(self):
        for path in ("agents/shard/src/lib.rs", "proposals/cipherclerk/x.patch", "tools/cipherclerk-patched.sh"):
            args, agents, unowned = self.scope(path)
            self.assertEqual((args, agents, unowned), ("", True, []), path)
        self.assertFalse(self.scope("crates/a/src/x.rs")[1])

    def test_unknown_paths_are_unowned(self):
        for path in ("deploy/docker/core.Dockerfile", "tools/ci/x.sh", "e2e/network/x.rs", "services/settle/kreivo-pin/a.rs".replace("services/settle", "services/other"),
                     "README.md", "CONTRIBUTING.md", "spikes/x.rs", "contracts/other/x.rs"):
            self.assertEqual(self.scope(path)[2], [path], path)

    def test_the_spec_the_amendments_and_the_cores_generation_leave_the_lane(self):
        for path in ("SPEC.md", "amendments/0042-x.md", "agents/core/src/lib.rs"):
            self.assertEqual(self.scope(path)[2], [path], path)

    def test_the_settlement_contract_belongs_to_verify(self):
        self.assertEqual(self.scope("contracts/settlement/src/lib.rs")[0], "-p verify")

    def test_a_package_name_that_is_not_plain_runs_the_workspace(self):
        self.assertEqual(tt.hotfix_scope(["x/a.rs"], {**self.members, "x": "a; rm -rf /"})[0], "--workspace")

    def test_a_rule_naming_a_crate_the_workspace_lacks_raises(self):
        with self.assertRaises(ValueError):
            tt.hotfix_scope(["crates/a/src/x.rs"], {"crates/a": "a"})


def commit(author: str | None = "pablo", committer: str | None = "pablo", sha: str = "a" * 40) -> dict:
    return {"sha": sha, "author": {"login": author} if author else None,
            "committer": {"login": committer} if committer else None}


def labels_api(*names: str, fail: bool = False, commits: list | None = None):
    """The hotfix lane's API: the labels of pull request 7 and its commits (one, by pablo, by default)."""
    commits = [commit()] if commits is None else commits

    def api(path: str):
        if fail:
            raise OSError("down")
        if path.startswith("repos/o/r/issues/7/labels"):
            return [{"name": n} for n in names]
        if path.startswith("repos/o/r/pulls/7/commits"):
            return commits
        raise AssertionError(f"unexpected API call {path}")

    return api


class Hotfix(Classify):
    def hotfix(self, changes, api=None, head_ref="hotfix/fix-it", authors="pablo, agent", **ev):
        return self.run_pr(head_ref, changes, api or labels_api("hotfix"), authors, **ev)

    def test_a_labelled_hotfix_by_an_allowed_author_runs_the_crates_it_touches(self):
        d = self.hotfix({"crates/a/src/lib.rs": "pub fn g() {}\n", "services/settle/src/x.rs": "\n"})
        self.assertEqual((d.mode, d.test_args, d.agents), ("hotfix", "-p a -p settle", "false"), d.reason)

    def test_the_agents_workspace_is_asked_for_only_when_touched(self):
        d = self.hotfix({"agents/shard/src/lib.rs": "\n"})
        self.assertEqual((d.mode, d.test_args, d.agents), ("hotfix", "", "true"))

    def test_a_lock_file_change_runs_the_workspace(self):
        d = self.hotfix({"Cargo.lock": files()["Cargo.lock"] + "\n# x\n"})
        self.assertEqual((d.mode, d.test_args), ("hotfix", "--workspace"))

    def test_one_unowned_path_leaves_the_lane(self):
        d = self.hotfix({"crates/a/src/lib.rs": "\n", "SPEC.md": "x\n"})
        self.assertEqual(d.mode, "full")
        self.assertIn("SPEC.md", d.reason)
        self.assertEqual(self.hotfix_fresh({"crates/a/src/lib.rs": "\n", "tools/ci/x.sh": "x\n"}).mode, "full")

    def hotfix_fresh(self, changes):
        # a second pull request in the same repository
        self.r.git("checkout", "-q", "main")
        self.r.git("reset", "-q", "--hard", self.base)
        self.r.git("branch", "-q", "-D", "pr")
        return self.hotfix(changes)

    def test_another_author_runs_everything(self):
        self.assertEqual(self.hotfix({"crates/a/src/lib.rs": "\n"}, user="mallory").mode, "full")

    def test_another_pusher_runs_everything(self):
        d = self.hotfix({"crates/a/src/lib.rs": "\n"}, user="pablo", sender="mallory")
        self.assertEqual(d.mode, "full")
        self.assertIn("pushed by mallory", d.reason)

    def test_an_event_without_a_sender_runs_everything(self):
        self.r.git("checkout", "-q", "-b", "pr", self.base)
        self.r.commit({"crates/a/src/lib.rs": "\n"})
        self.r.git("checkout", "-q", "main")
        _, head = self.r.merge("pr")
        ev = event("hotfix/x", head)
        del ev["sender"]
        path = self.r.root / "event.json"
        path.write_text(json.dumps(ev))
        self.assertEqual(tt.decide(self.r.root, "pull_request", str(path), "o/r", labels_api("hotfix"), "pablo").mode, "full")

    def test_a_commit_of_someone_else_runs_everything(self):
        # reopening a pull request makes the reopener the sender: the commits are checked too
        for bad in (commit(author="mallory"), commit(committer="mallory"), commit(committer="web-flow"),
                    commit(author=None), commit(committer=None)):
            api = labels_api("hotfix", commits=[commit(), bad])
            self.r.git("checkout", "-q", "main")
            self.r.git("reset", "-q", "--hard", self.base)
            self.r.git("branch", "-q", "-D", "pr") if "pr" in self.r.git("branch", "--format=%(refname:short)").split() else None
            d = self.hotfix({"crates/a/src/lib.rs": "\n"}, api=api)
            self.assertEqual(d.mode, "full", (bad, d.reason))
            self.assertIn("HOTFIX_AUTHORS", d.reason)

    def test_no_commits_or_too_many_to_list_runs_everything(self):
        self.assertEqual(self.hotfix({"crates/a/src/lib.rs": "\n"}, api=labels_api("hotfix", commits=[])).mode, "full")

    def test_an_empty_allow_list_closes_the_lane(self):
        self.assertEqual(self.hotfix({"crates/a/src/lib.rs": "\n"}, authors="").mode, "full")

    def test_no_label_runs_everything(self):
        self.assertEqual(self.hotfix({"crates/a/src/lib.rs": "\n"}, api=labels_api("bug")).mode, "full")

    def test_the_label_on_another_branch_does_nothing(self):
        self.assertEqual(self.hotfix({"crates/a/src/lib.rs": "\n"}, head_ref="fix/x").mode, "full")

    def test_a_label_lookup_that_fails_runs_everything(self):
        self.assertEqual(self.hotfix({"crates/a/src/lib.rs": "\n"}, api=labels_api(fail=True)).mode, "full")

    def test_a_fork_runs_everything(self):
        self.assertEqual(self.hotfix({"crates/a/src/lib.rs": "\n"}, fork=True).mode, "full")

    def test_another_base_runs_everything(self):
        self.assertEqual(self.hotfix({"crates/a/src/lib.rs": "\n"}, base_ref="develop").mode, "full")

    def test_the_other_modes_ask_for_nothing_to_test(self):
        d = self.run_pr("release-please--x", release_bump("0.1.0", "0.2.0"))
        self.assertEqual((d.mode, d.test_args, d.agents), ("skip", "", "false"))


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
        self.assertEqual(d.mode, "skip", d.reason)

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
        self.assertEqual(d.mode, "full")
        self.assertIn("rate limited", d.reason)

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
    def test_the_lane_always_runs_the_two_cross_crate_scanners(self):
        # ledger-core's privileged_reads (REQ-PV-4) and core-binding's upgrade_authority (T-037-05) read
        # the sources of other crates, so a diff elsewhere can break them: the lane runs them whatever
        # the diff touches.
        ci = (ROOT / ".github/workflows/ci.yml").read_text()
        for cmd, file in (("cargo test -p ledger-core --test privileged_reads", "crates/ledger-core/tests/privileged_reads.rs"),
                          ("cargo test -p core-binding --test upgrade_authority", "crates/core-binding/tests/upgrade_authority.rs")):
            self.assertIn(cmd, ci)
            self.assertTrue((ROOT / file).is_file(), file)

    def test_every_required_job_is_a_job_of_ci_yml(self):
        ci = (ROOT / ".github/workflows/ci.yml").read_text()
        names = set(re.findall(r"^    name: (.+)$", ci, re.M))
        for job in tt.REQUIRED_CI_JOBS:
            self.assertIn(job, names, f"{job!r} is no longer a job name in ci.yml: keep REQUIRED_CI_JOBS in step")


if __name__ == "__main__":
    unittest.main()
