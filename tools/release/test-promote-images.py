#!/usr/bin/env python3
# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
"""Tests of tools/release/promote-images.py: when a release may take a pre-release's images, and when it must build.

    python3 tools/release/test-promote-images.py

Synthetic repositories built with git (the helpers of tools/ci/test-tested-tree.py), a fake GitHub API,
a fake registry; no network. Every case that promotes has neighbours that must not, because a wrong
promotion ships images nobody built for that tree.
"""
from __future__ import annotations

import argparse
import importlib.util
import io
import json
import re
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

HERE = Path(__file__).resolve().parent


def load(name: str, path: Path):
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


pi = load("promote_images", HERE / "promote-images.py")
ttt = load("test_tested_tree", HERE.parent / "ci" / "test-tested-tree.py")

REPOSITORY = "o/r"


class World:
    """A repository with develop and main, and a registry and a Releases API that agree with it (or not)."""

    def __init__(self, tmp: str):
        self.r = ttt.Repo(Path(tmp))
        self.base = self.r.commit(ttt.files("0.1.0"), "base")
        self.r.git("branch", "develop")
        self.r.git("checkout", "-q", "develop")
        self.runs: dict[str, list[dict]] = {}
        self.jobs: dict[int, list[dict]] = {}
        self.images_json: dict[str, str] = {}
        self.tags: dict[str, str] = {}  # registry: "repo:tag" -> digest
        self.configs: dict[str, dict] = {}  # "repo@digest" -> config
        self.copies: list[tuple[str, str]] = []
        self.copy_fails = False
        self.copy_lies = False

    # develop -----------------------------------------------------------------------------------
    def develop_commit(self, changes: dict[str, str | None], msg: str = "c", prerelease: bool = True) -> str:
        sha = self.r.commit(changes, msg)
        if prerelease:
            self.publish_prerelease(sha)
        return sha

    def publish_prerelease(self, sha: str, version: str = "0.2.0") -> str:
        tag = f"v{version}-dev+{sha[:7]}"
        self.r.git("tag", tag, sha)
        images = {}
        for n in pi.NAMES:
            digest = "sha256:" + (f"{pi.NAMES.index(n) + 1:x}" + sha)[:64].ljust(64, "0")
            images[n] = f"{pi.REGISTRY}/aledgent-{n}@{digest}"
            self.tags[f"{pi.REGISTRY}/aledgent-{n}:sha-{sha}"] = digest
            labels = {
                "org.opencontainers.image.revision": sha,
                "org.opencontainers.image.version": tag[1:],
            }
            if n in pi.WITH_ROOTS:
                labels["app.aledgent.roots"] = "v2"
            self.configs[images[n]] = {"config": {"Labels": labels}}
        self.images_json[tag] = json.dumps({"version": tag, "commit": sha, "images": images})
        run_id = int(sha[:6], 16)
        self.runs[sha] = [{"id": run_id, "head_sha": sha, "event": "push", "head_branch": "develop",
                           "conclusion": "success"}]
        self.jobs[run_id] = [{"name": pi.PUBLISH_JOB, "conclusion": "success"}]
        return tag

    # main --------------------------------------------------------------------------------------
    def release(self, v_old: str = "0.1.0", v_new: str = "0.2.0", extra: dict[str, str | None] | None = None,
                upto: str = "develop") -> str:
        """Promote develop to main by a merge commit, then release-please's commit. Returns the latter."""
        self.r.git("checkout", "-q", "main")
        self.r.git("merge", "-q", "--no-ff", "-m", "promote", upto)
        changes = ttt.release_bump(v_old, v_new)
        changes.update(extra or {})
        sha = self.r.commit(changes, f"chore(main): release {v_new}")
        self.r.git("checkout", "-q", "develop")
        return sha

    # fakes -------------------------------------------------------------------------------------
    def env(self) -> "pi.Env":
        def api(path: str):
            m = re.search(r"workflows/release\.yml/runs\?head_sha=(\w+)&event=push", path)
            if m:
                return {"workflow_runs": self.runs.get(m.group(1), [])}
            m = re.search(r"actions/runs/(\d+)/jobs", path)
            if m:
                return {"jobs": self.jobs.get(int(m.group(1)), [])}
            raise AssertionError(f"unexpected API call {path}")

        def images_json(tag: str) -> str:
            if tag not in self.images_json:
                raise pi.Refusal("no such release")
            return self.images_json[tag]

        def digest(ref: str) -> str:
            if "@" in ref:
                return ref.split("@", 1)[1]
            if ref not in self.tags:
                raise pi.Refusal(f"no such tag {ref}")
            return self.tags[ref]

        def config(ref: str) -> dict:
            return self.configs[ref]

        def copy(src: str, dst: str) -> None:
            if self.copy_fails:
                raise pi.Refusal("registry said no")
            self.copies.append((src, dst))
            self.tags[dst] = "sha256:" + "e" * 64 if self.copy_lies else src.split("@", 1)[1]

        return pi.Env(api=api, images_json=images_json, digest=digest, config=config, copy=copy)

    def find(self):
        return pi.find_source(self.r.root, REPOSITORY, self.release_sha, "develop", self.env())


class Base(unittest.TestCase):
    def setUp(self):
        self._tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self._tmp.cleanup)
        self.w = World(self._tmp.name)

    def promoted(self, d: str, version: str = "0.2.0", upto: str = "develop") -> None:
        self.w.release_sha = self.w.release(v_new=version, upto=upto)
        source, reason = self.w.find()
        self.assertIsNotNone(source, reason)
        self.assertEqual(source.commit, d)

    def refused(self, contains: str) -> None:
        self.w.release_sha = self.w.release()
        source, reason = self.w.find()
        self.assertIsNone(source, reason)
        self.assertIn(contains, reason)


class Finding(Base):
    def test_the_pre_release_of_the_tree_released(self):
        d = self.w.develop_commit({"crates/a/src/lib.rs": "pub fn g() {}\n"})
        self.promoted(d)

    def test_found_below_later_develop_commits(self):
        d = self.w.develop_commit({"crates/a/src/lib.rs": "pub fn g() {}\n"})
        self.w.develop_commit({"crates/a/src/lib.rs": "pub fn h() {}\n"})  # not promoted yet
        self.promoted(d, upto=d)

    def test_the_source_reports_develops_workspace_version(self):
        d = self.w.develop_commit({"crates/a/src/lib.rs": "pub fn g() {}\n"})
        self.w.release_sha = self.w.release(v_new="0.5.0")
        source, _ = self.w.find()
        self.assertEqual(source.commit, d)
        self.assertEqual(source.embedded_version, "0.1.0")  # the pre-release was built before the bump

    def test_a_commit_only_on_main_means_no_match(self):
        # A hotfix, or the signoff fix of #42: main has a change develop has not.
        self.w.develop_commit({"crates/a/src/lib.rs": "pub fn g() {}\n"})
        self.w.release_sha = self.w.release(extra={"crates/b/src/lib.rs": "pub fn hotfix() {}\n"})
        source, reason = self.w.find()
        self.assertIsNone(source)
        self.assertIn("has the release's tree", reason)

    def test_no_pre_release_tag(self):
        d = self.w.develop_commit({"crates/a/src/lib.rs": "pub fn g() {}\n"}, prerelease=False)
        self.w.release_sha = self.w.release()
        source, reason = self.w.find()
        self.assertIsNone(source)
        self.assertIn(f"{d[:7]}: 0 pre-release tags", reason)

    def test_a_tag_for_another_commit_is_not_the_pre_release(self):
        d = self.w.develop_commit({"crates/a/src/lib.rs": "pub fn g() {}\n"}, prerelease=False)
        self.w.r.git("tag", "v0.2.0-dev+0000000", d)  # the wrong seven hex
        self.refused("0 pre-release tags")

    def fresh(self, source: str = "pub fn g() {}\n") -> str:
        """A new world with one develop commit and its pre-release: for a test that tries several variants."""
        self._tmp.cleanup()
        self.setUp()
        return self.w.develop_commit({"crates/a/src/lib.rs": source})

    def test_the_run_must_have_succeeded_on_develop_and_published(self):
        for name, mutate in {
            "failed": lambda r: r.update(conclusion="failure"),
            "other branch": lambda r: r.update(head_branch="feature"),
            "dispatch": lambda r: r.update(event="workflow_dispatch"),
            "other commit": lambda r: r.update(head_sha="f" * 40),
        }.items():
            with self.subTest(name):
                d = self.fresh()
                mutate(self.w.runs[d][0])
                self.refused("no successful push run")

    def test_a_run_whose_publish_job_did_not_succeed(self):
        d = self.w.develop_commit({"crates/a/src/lib.rs": "pub fn g() {}\n"})
        self.w.jobs[self.w.runs[d][0]["id"]] = [{"name": pi.PUBLISH_JOB, "conclusion": "skipped"}]
        self.refused("no successful push run")

    def test_images_json_of_another_commit(self):
        d = self.w.develop_commit({"crates/a/src/lib.rs": "pub fn g() {}\n"})
        tag = f"v0.2.0-dev+{d[:7]}"
        doc = json.loads(self.w.images_json[tag])
        doc["commit"] = "a" * 40
        self.w.images_json[tag] = json.dumps(doc)
        self.refused("another version or commit")

    def test_images_json_of_another_version(self):
        d = self.w.develop_commit({"crates/a/src/lib.rs": "pub fn g() {}\n"})
        tag = f"v0.2.0-dev+{d[:7]}"
        doc = json.loads(self.w.images_json[tag])
        doc["version"] = "v0.9.9"
        self.w.images_json[tag] = json.dumps(doc)
        self.refused("another version or commit")

    def test_an_image_missing_or_extra(self):
        for name, edit in {"missing": lambda i: i.pop("sidecar"), "extra": lambda i: i.update(extra=i["ops"])}.items():
            with self.subTest(name):
                d = self.fresh()
                tag = f"v0.2.0-dev+{d[:7]}"
                doc = json.loads(self.w.images_json[tag])
                edit(doc["images"])
                self.w.images_json[tag] = json.dumps(doc)
                self.refused("exactly")

    def test_an_image_from_another_repository_or_without_a_digest(self):
        d = self.w.develop_commit({"crates/a/src/lib.rs": "pub fn g() {}\n"})
        tag = f"v0.2.0-dev+{d[:7]}"
        doc = json.loads(self.w.images_json[tag])
        doc["images"]["ops"] = doc["images"]["ops"].replace("aledgent-ops", "elsewhere-ops")
        self.w.images_json[tag] = json.dumps(doc)
        self.refused("is not")

    def test_the_sha_tag_must_resolve_to_the_listed_digest(self):
        d = self.w.develop_commit({"crates/a/src/lib.rs": "pub fn g() {}\n"})
        self.w.tags[f"{pi.REGISTRY}/aledgent-core:sha-{d}"] = "sha256:" + "b" * 64
        self.refused("core: sha-")

    def test_the_labels(self):
        cases = {
            "revision": ("org.opencontainers.image.revision", "c" * 40),
            "version": ("org.opencontainers.image.version", "0.2.0"),
            "roots": ("app.aledgent.roots", "v1"),
        }
        for name, (label, value) in cases.items():
            with self.subTest(name):
                d = self.fresh()
                tag = f"v0.2.0-dev+{d[:7]}"
                ref = json.loads(self.w.images_json[tag])["images"]["settle"]
                self.w.configs[ref]["config"]["Labels"][label] = value
                self.refused("settle:")

    def test_the_sidecar_has_no_roots_label(self):
        d = self.w.develop_commit({"crates/a/src/lib.rs": "pub fn g() {}\n"})
        self.promoted(d)  # World gives the sidecar none

    def test_a_not_version_only_difference_in_a_version_file_is_refused(self):
        # openapi.json changing more than its version is not a version bump.
        self.w.develop_commit({"crates/a/src/lib.rs": "pub fn g() {}\n"})
        openapi = json.loads(ttt.files("0.1.0")["services/ingress/openapi.json"])
        openapi["paths"] = {"/new": {}}
        self.w.release_sha = self.w.release(extra={"services/ingress/openapi.json": json.dumps(openapi) + "\n"})
        source, _ = self.w.find()
        self.assertIsNone(source)

    def test_disagreeing_versions_in_the_release(self):
        self.w.develop_commit({"crates/a/src/lib.rs": "pub fn g() {}\n"})
        self.w.release_sha = self.w.release(extra={".release-please-manifest.json": '{".": "0.9.9"}\n'})
        source, reason = self.w.find()
        self.assertIsNone(source)
        self.assertIn("disagree", reason)

    def test_gives_up_after_a_few_candidates(self):
        # Several commits with the release's tree, none with a pre-release: asks about three only.
        for i in range(pi.MAX_CANDIDATES + 2):
            self.w.develop_commit({"CHANGELOG.md": f"# Changelog\n\n{i}\n"}, prerelease=False)
        self.w.release_sha = self.w.release()
        source, reason = self.w.find()
        self.assertIsNone(source)
        self.assertIn("stopped after", reason)


class Promoting(Base):
    def run_main(self, *extra: str) -> tuple[int, dict[str, str]]:
        out = io.StringIO()
        with tempfile.NamedTemporaryFile("r+") as gh_out, redirect_stdout(out), redirect_stderr(io.StringIO()):
            import os
            os.environ["GITHUB_OUTPUT"] = gh_out.name
            try:
                rc = pi.main(
                    ["--commit", self.w.release_sha, "--version", "0.2.0", "--repository", REPOSITORY,
                     "--repo", str(self.w.r.root), "--develop", "develop", *extra],
                    env=self.w.env(),
                )
            finally:
                del os.environ["GITHUB_OUTPUT"]
            gh_out.seek(0)
            written = dict(line.rstrip("\n").split("=", 1) for line in gh_out)
        printed = dict(line.split("=", 1) for line in out.getvalue().splitlines() if "=" in line)
        self.assertEqual(written, printed)
        return rc, written

    def setUp(self):
        super().setUp()
        self.d = self.w.develop_commit({"crates/a/src/lib.rs": "pub fn g() {}\n"})
        self.w.release_sha = self.w.release()

    def test_copies_each_image_by_digest_to_both_release_tags(self):
        rc, out = self.run_main()
        self.assertEqual(rc, 0)
        self.assertEqual(out["promoted"], "true", out)
        self.assertEqual(len(self.w.copies), 2 * len(pi.NAMES))
        for n in pi.NAMES:
            ref = out[n]
            self.assertRegex(ref, rf"^{re.escape(pi.REGISTRY)}/aledgent-{n}@sha256:[0-9a-f]{{64}}$")
            dsts = {d for s, d in self.w.copies if s == ref}
            self.assertEqual(dsts, {f"{pi.REGISTRY}/aledgent-{n}:sha-{self.w.release_sha}",
                                    f"{pi.REGISTRY}/aledgent-{n}:v0.2.0"})
            self.assertEqual(self.w.tags[f"{pi.REGISTRY}/aledgent-{n}:v0.2.0"], ref.split("@")[1])
        self.assertEqual(out["source_commit"], self.d)
        self.assertEqual(out["source_tag"], f"v0.2.0-dev+{self.d[:7]}")
        self.assertEqual(out["embedded_version"], "0.1.0")

    def test_a_dry_run_copies_nothing(self):
        rc, out = self.run_main("--dry-run")
        self.assertEqual(rc, 0)
        self.assertEqual(out["promoted"], "false")
        self.assertIn("would promote", out["reason"])
        self.assertEqual(self.w.copies, [])

    def test_a_failed_copy_is_not_promoted_and_the_job_does_not_fail(self):
        self.w.copy_fails = True
        rc, out = self.run_main()
        self.assertEqual(rc, 0)
        self.assertEqual(out["promoted"], "false")
        self.assertIn("the images are built", out["reason"])
        self.assertNotIn("ingress", out)

    def test_a_tag_that_does_not_read_back_as_the_digest_is_not_promoted(self):
        self.w.copy_lies = True
        rc, out = self.run_main()
        self.assertEqual(rc, 0)
        self.assertEqual(out["promoted"], "false")
        self.assertIn("after the copy", out["reason"])
        self.assertNotIn("ingress", out)

    def test_no_pre_release_is_not_promoted(self):
        self.w.images_json.clear()
        rc, out = self.run_main()
        self.assertEqual(rc, 0)
        self.assertEqual(out["promoted"], "false")
        self.assertEqual(self.w.copies, [])

    def test_any_exception_is_not_promoted(self):
        def boom(_path):
            raise RuntimeError("API down")

        env = self.w.env()._replace(api=boom)
        out = io.StringIO()
        with redirect_stdout(out), redirect_stderr(io.StringIO()):
            rc = pi.main(["--commit", self.w.release_sha, "--version", "0.2.0", "--repo", str(self.w.r.root),
                          "--develop", "develop"], env=env)
        self.assertEqual(rc, 0)
        self.assertIn("promoted=false", out.getvalue())
        self.assertIn("RuntimeError", out.getvalue())

    def test_a_bad_version_is_not_promoted(self):
        source, _ = self.w.find()
        with self.assertRaises(pi.Refusal):
            pi.promote(source, self.w.release_sha, "0.2.0; rm", self.w.env())
        self.assertEqual(self.w.copies, [])

    def test_a_bad_commit_is_not_promoted(self):
        out = io.StringIO()
        with redirect_stdout(out), redirect_stderr(io.StringIO()):
            rc = pi.main(["--commit", "HEAD", "--version", "0.2.0", "--repo", str(self.w.r.root),
                          "--develop", "develop"], env=self.w.env())
        self.assertEqual(rc, 0)
        self.assertIn("promoted=false", out.getvalue())


class Wiring(unittest.TestCase):
    """The workflow still says what this script's contract needs."""

    @classmethod
    def setUpClass(cls):
        cls.release = (HERE.parents[1] / ".github" / "workflows" / "release.yml").read_text()

    def test_the_images_are_built_unless_promoted(self):
        images = re.search(r"\n  images:\n(.*?)\n  publish:", self.release, re.S).group(1)
        self.assertIn("needs.promote.outputs.promoted != 'true'", images)
        self.assertNotIn("!failure()", images)  # a crashed promote must not skip the build
        self.assertIn("promote", re.search(r"needs: \[(.*?)\]", images).group(1))

    def test_publish_takes_either_set_of_digests(self):
        publish = self.release.split("\n  publish:\n", 1)[1]
        for n in pi.NAMES:
            self.assertIn(f"needs.promote.outputs.{n}", publish)
            self.assertIn(f"needs.images.outputs.{n}", publish)

    def test_promote_reads_the_actions_api_and_writes_packages(self):
        promote = re.search(r"\n  promote:\n(.*?)\n  images:", self.release, re.S).group(1)
        for perm in ("actions: read", "packages: write", "contents: read"):
            self.assertIn(perm, promote)
        self.assertNotIn("contents: write", promote)  # nothing here may touch a release or a tag
        self.assertIn("RELEASE_REBUILD", promote)  # the off switch


if __name__ == "__main__":
    unittest.main()
