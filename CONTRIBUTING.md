<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Contributing to aledgent

Thank you for your interest. aledgent is a ledger of record, so the bar for a change is that it can
be traced to a requirement, reviewed, and tested at the layer the requirement names. This guide
says how to propose a change at each level and what a pull request needs.

By taking part you agree to follow the `CODE_OF_CONDUCT.md`. Report security issues privately, as
`SECURITY.md` says, never in a public issue.

## How changes flow

Work is spec-driven. A change enters at the highest level it affects, and flows down:

    SPEC.md            what aledgent does and why (no technology named)
      -> amendments/   a reviewed change to SPEC.md
      -> PLAN.md       how it is built: decisions (DEC-*), features (F-*), milestones (M-*)
        -> features/F-NNN-*/plan.md, tasks.md
          -> issue -> pull request -> code and tests

If code, plan and spec disagree, the spec wins. Behaviour that the spec does not describe is not
built: a gap found while coding becomes an amendment first. `CLAUDE.md` lists the vocabulary and the
guardrails that every change must respect. It is written for coding agents, and it is the shortest
complete statement of the project's rules.

## Which kind of change is yours?

| You want to… | Start with | Who decides |
|---|---|---|
| Report a bug: the code does not do what `SPEC.md` says | A **bug report** issue | Maintainers |
| Fix a bug, add a test, improve docs or tooling | A **pull request**, citing the issue | Maintainers |
| Change *how* something is built (a dependency, a data structure, a decision `DEC-*`) | A **plan change** issue, then a PR to `PLAN.md` or the feature's `plan.md` | Maintainers |
| Change *what* aledgent does (a requirement, an invariant, an acceptance criterion) | An **amendment proposal** issue, then a PR adding `amendments/NNNN-*.md` | The spec owner (`GOVERNANCE.md`) |
| Answer an open question (`OQ-*` in `SPEC.md` §14) | An **amendment proposal** that closes it | The spec owner |
| Change VOS, CipherClerk or Kreivo | A proposal in `proposals/`, then the upstream project's own process | The upstream maintainers |
| Propose a roadmap theme | An issue labelled `roadmap` | Maintainers, then the spec owner if it needs an amendment |

### Amendments

An amendment is one file, `amendments/NNNN-short-title.md`, numbered after the last one. It states:

1. the problem, with evidence (a measurement, a failing case, a contradiction in the spec);
2. the exact text it changes in `SPEC.md`, as before and after;
3. the identifiers it adds, changes or withdraws. Identifiers are never renumbered or reused: a
   withdrawn requirement stays withdrawn;
4. what it costs: the "trade-off, stated plainly" that the spec and plan always carry;
5. its status: `Proposed`, then `Ruled` (with the date) or `Declined`.

Discussion happens on the pull request. When the spec owner rules it, the amendment is applied to
`SPEC.md` in the same PR, the version is bumped, and `PLAN.md` is updated in a follow-up if needed. A
plan site that waits on an amendment is marked `[PENDING AMENDMENT NNNN-An]`.

### Features and tasks

A feature lives in `features/F-NNN-name/`: `plan.md` (what and how, with its trace to `SPEC.md` and
`PLAN.md`), `tasks.md` (tasks `T-NNN-XX`, each tracing to a `REQ-*`, `INV-*` or `AC-*`) and
`NOTES.md` (decisions taken while building, and why). A task that traces to nothing in the spec is
invalid. Scope changes are made in the feature files, never only in an issue.

## Pull requests

Before you open one:

    cargo fmt --all
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace
    tools/oss/spdx.py --check          # licence headers (tools/oss/spdx.py --fix adds them)

Other workspaces (`agents/`, `contracts/`) have their own commands in `README.md`.

A pull request:

- does one thing, and cites the task (`T-NNN-XX`) and the requirement it serves (`REQ-*`, `INV-*`,
  `AC-*`) in its title or description;
- names its tests after the `AC-*`, `INV-*`, `REQ-*` or `ERR-*` they verify, and tests an invariant at
  the layer `SPEC.md` §9 names;
- adds no `unsafe` code and no floating point anywhere near an amount (`INV-7`);
- puts an SPDX header on every new file (`docs/oss/LICENSING.md`). Code under `agents/` is
  AGPL-3.0-or-later; other code is Apache-2.0 OR MIT; documents are CC-BY-4.0. No crate outside
  `agents/` may depend on VOS;
- is signed off (below);
- has a [Conventional Commits](https://www.conventionalcommits.org/) title, `type(scope)!: summary`
  (`fix(ingress): refuse an empty client key`). Pull requests are squash-merged, the title becomes
  the commit's subject, and the release is cut from those subjects (below). The `conventional PR
  title` check enforces it.

A maintainer reviews it. `CODEOWNERS` routes reviews: changes to `SPEC.md` and `amendments/` need the
spec owner's approval.

## Releases

Two long-lived branches: `develop`, where pull requests land, and `main`, which holds releases. Both
are protected alike (one approving review, code owners, the required checks, signed commits).

- **Pre-releases.** Every push to `develop` publishes `vX.Y.Z-dev+<sha7>`, a GitHub pre-release,
  where `X.Y.Z` is the version the next release would have (`tools/release/next-version.sh`).
- **Releases.** `develop` is promoted to `main` by a pull request merged with a merge commit, never
  squashed, so that `main` keeps the subjects. release-please (`release-please-config.json`) keeps a
  release pull request open against `main` that bumps the workspace version and `CHANGELOG.md`: a
  `feat` is a minor release, a `fix` or anything else a patch, a breaking change (`feat!:`, or a
  `BREAKING CHANGE:` footer) a major one, or a minor one before 1.0.0. Merging it tags `vX.Y.Z` and
  publishes the release. release-please opens that pull request as the `aledgent-release` GitHub
  App, so its checks start on their own. Only if the App's secrets are missing from the `release`
  environment does it fall back to the Actions token (the release workflow warns), and GitHub does
  not run checks on a pull request that token opened: then close and reopen the release pull
  request to run them. After a release, merge `main` back into `develop`.
- **Images.** A release or a pre-release publishes every image to `ghcr.io/bloque-app/aledgent-<name>`
  (ingress, settle, ops, signer, core, sidecar) as `sha-<commit>` and `v<version>` (`+` becomes `_`
  in an image tag), and attaches `images.json`, each image by digest (`.github/workflows/release.yml`).
  A release does not build the core image again when it can take the pre-release's: if the release
  commit's tree is a `develop` commit's tree but for versions and the changelog
  (`tools/ci/tested-tree.py`'s rule), that commit's pre-release has passed its own checks and its
  labels agree with its `images.json`, `tools/release/promote-images.py` copies the core image by
  digest to the release's tags, with the buildx provenance attestation it was built with, and the
  17-minute core build is skipped; `images.json` records `promoted_from`. The other five are always
  built: ingress, settle and the sidecar report the crate version (`ingress`' and the sidecar's
  `/openapi.json` `info.version`, `settle`'s network name), and a release must report its own; ops
  and signer are compiled in the same job, so taking them would save nothing. Anything that does not
  match, a hotfix on `main` included, builds everything as before; the repository variable
  `RELEASE_REBUILD=true` forces it. The promoted core's labels (`org.opencontainers.image.version`,
  `.revision`) name the pre-release, which a copy does not change; the release's tag and
  `images.json` identify it.
- **Core releases.** A change to the core (`core.vos`) can ship without a new image and without
  restarting the host: `.github/workflows/core-release.yml` builds it as the next rules version of the
  code a book already runs, has the node's host write the code manifest, and publishes the three
  files, each named by its blake2b-256 digest, as `ghcr.io/bloque-app/aledgent-core-code@sha256:…`
  with a provenance attestation. It prints the code version the operator submits as
  `Upgrade { code, from }`. It never touches the `aledgent-core` image, deploys nothing and holds no
  cloud credential. A run follows one predecessor (the package it claims the state of), so test and prod,
  founded on different packages, each get their own run. `GENERATION` in `agents/core/src/lib.rs` is
  raised in the commit that needs the upgrade. Host-only releases keep the path above.
- **Versions.** The workspace's crates take their version from `[workspace.package]`, except those
  compiled into a guest program (`core.vos`, the shard's), which keep a fixed one: a crate's version
  reaches the compiled program, whose bytes are its identity, and a release must not change the core
  it ships when no line of the core changed. `tools/release/check-config.py` checks it.
- **Public API.** `cargo semver-checks` compares `verify`, `compact-smt` and `ledger-core` with the
  last release, at the release type the title implies: a breaking change needs a title that says so.

## Sign-off: the Developer Certificate of Origin

Every commit must carry a `Signed-off-by:` line with your real name and email, which certifies the
[Developer Certificate of Origin 1.1](https://developercertificate.org/): you wrote the change, or
have the right to submit it under the project's licences. `git commit -s` adds it. Contributions are
accepted under the licence of the files they change (inbound = outbound). There is no separate
contributor licence agreement: the project uses the DCO (`GOVERNANCE.md`).

The `DCO` check (`.github/workflows/dco.yml`) fails a pull request with a commit whose
`Signed-off-by:` does not name that commit's author. To fix one, amend it with `git commit --amend -s`,
or sign off a whole branch with `git rebase --signoff <base>`, and push again.

## Good first contributions

- Conformance cases: every `AC-*` and `INV-*` in `SPEC.md` should have a case in `crates/conformance`.
  The suite reports the ones still pending.
- The verifier (`crates/verify`): independent implementations and test vectors are welcome.
- Documentation: `OVERVIEW.md` and the integration guides on the roadmap.

## Questions

Open a discussion, or an issue labelled `question`. Please do not email maintainers privately about
anything except security.
