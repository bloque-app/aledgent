<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Runbook: the hotfix lane

bloque-app/bloque-ledgent#312 (release speed) · `.github/workflows/ci.yml`, `tools/ci/tested-tree.py`,
`.github/workflows/release.yml` · **Owner:** platform.

A fix for a production incident reaches `main` without waiting for the whole suite. The lane lowers
**what runs before the merge**; it does not lower who may merge, it does not touch the release
workflow, and **it ships a fix that the full suite has not yet judged** (see "What ships untested").

## When to use it

- A **production incident** whose fix cannot wait for the regular bump (`develop` promoted to `main`).
  A defect that is merely annoying, a feature, a refactor and anything that waits a day take the
  regular path ([CONTRIBUTING](../../../CONTRIBUTING.md), "Releases").
- Not for a change that needs a decision first: a change to `SPEC.md` or `amendments/`, or a new
  rules version of the core ([core-halt](core-halt.md) names when a build is the only way on). The
  lane does not take them: such a diff runs the full suite.

## Who may use it

The owner, or the owner's agent **on the owner's go**, given in the incident's thread before the pull
request is opened. The lane enforces part of it itself: it applies only when the pull request's
**author and the pusher of the commit being checked** (the event's sender) are both listed in the
repository variable `HOTFIX_AUTHORS` (comma-separated logins). Anyone else, or an empty variable, gets
the full suite, whatever the label says.

What nothing enforces: **`--admin`**. Any repository admin can merge past the review and the checks,
with or without this lane, and **removing the `hotfix` label after the checks are green leaves them
green** (the next push re-judges the pull request, but the checks already reported stay). The control is
the human who merges, and the thread where the go was given.

Set once, by an admin (the lane is closed until then):

    gh variable set HOTFIX_AUTHORS -R bloque-app/aledgent --body "<owner login>,<agent login>"
    gh label create hotfix -R bloque-app/aledgent --color B60205 \
      --description "Production incident fix: the hotfix lane (ops/runbooks/release/hotfix.md)"

## What runs before the merge

A pull request into `main`, from a branch `hotfix/<slug>` of this repository, opened and pushed by
logins in `HOTFIX_AUTHORS`, with the label `hotfix` (read from the API when the gate runs). The `gate
(tested tree)` job says which of this it found; its summary is the line to read.

| Check | In the lane |
|---|---|
| rustfmt | runs |
| clippy | runs the workspace's (`--all-targets`); the all-features, `no_std` kernel and `cargo doc` runs are left to `main` |
| tests | `cargo test -p <crate> ...` for the crates the diff touches (workspace members, plus the crates whose tests read a path outside them: `deploy/templates/` → signer, ingress, settle, ops; `ops/runbooks/` → core-binding, ops; `contracts/settlement/` → verify). A diff that touches the root `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`, `.cargo/` or `.github/` runs `--workspace`; one that touches only `docs/` or `notes/` runs none |
| agents workspace (core host) | runs only when the diff touches `agents/`, `proposals/cipherclerk/` or `tools/cipherclerk-patched.sh` |
| conformance (current gate profile) | reports success without running |
| code history | runs on a Rust-touching diff, as always (it is the spec owner's) |
| no floating point, kernel feature graph, conformance-ports, VOS boundary, licences, pass-import | run (seconds) |
| DCO sign-off, conventional PR title | run |

**Every path of the diff must be owned** by one of those rules. One that is not (the `tools/` and
`deploy/` files besides the templates, `e2e/`, `spikes/`, a crate-less README, and `SPEC.md`,
`amendments/` and `agents/core/src/lib.rs`, which conformance reads) sends the whole pull request to the
full suite, and the gate's summary names the path. The list of readers outside the crates is grep's, in
`tools/ci/tested-tree.py` (`EXTERNAL_READERS`): add to it when a test starts reading a new path.

The checks the lane does not run still report success under their own names, so branch protection is
satisfied, and a notice on the job says why. The **full suite runs on the push to `main`** after the
merge ([CI](../../../.github/workflows/ci.yml)).

Where the pieces come from. The workflow files of a pull request into `main` are the merge commit's.
The **judge** (`tools/ci/tested-tree.py` and `.github/actions/tested-tree`) is `main`'s copy, never the
pull request's: a pull request cannot supply its own. So the lane works once the gate is on `main`, and
changes to the judge take effect only after they are there.

## Steps

1. **Decide and say so.** The owner's go, in the incident's thread. Name the fix in a sentence.
2. **Branch from `main`, and keep the fix minimal.**

        git fetch origin
        git switch -c hotfix/<slug> origin/main

   One fix, no clean-up with it: every extra crate it touches is more to test, and a hotfix is read
   in minutes. Cite the `T-*`, `INV-*` or `REQ-*` it serves, as any commit does. Commit signed and
   signed off (`git commit -S -s`), with a Conventional Commits subject (`fix(ingress): ...`).
3. **Open the pull request into `main` with the label already on it.**

        gh pr create --base main --label hotfix \
          --title "fix(<scope>): <what it fixes>" --body "<the incident, the fix, the check you ran>"

   The title is the squash merge's subject and release-please reads it: `fix` is a patch release.
   `labeled` is not a trigger: if the label went on later, close and reopen the pull request. Open the
   `gate (tested tree)` job's summary: it must say `hotfix lane for <you>`. If it says anything else it
   ran everything, and the reason is the line next to it (author or pusher not listed, no label, a
   path the lane does not cover, a fork).
4. **Wait for the fast checks.** `gh pr checks <n> --watch`. They are bounded by clippy and by the build
   and tests of the crates you touched. Never merge over a red check.
5. **Merge with `--admin`.** The checks are green; what `--admin` passes is the approving review and the
   code owners' review, which an incident cannot wait for. Tell the thread you did.

        gh pr merge <n> -R bloque-app/aledgent --squash --admin

6. **Watch the full suite on `main`.** `gh run list -R bloque-app/aledgent -w CI --branch main`. It is
   the first real judgement of what the lane skipped. A red run on the hotfix's commit is a defect
   shipped: fix forward with another hotfix, or revert it the same way, and tell the thread.
7. **Release.** The push to `main` makes release-please open (or update) the release pull request
   (`release-please--branches--main--components--aledgent`). Check that it changes versions and the
   changelog only (`gh pr diff <n> --name-only`: `.release-please-manifest.json`, `CHANGELOG.md`,
   `Cargo.toml`, `Cargo.lock`, `agents/Cargo.lock`, `services/ingress/openapi.json`). Merging it tags
   `vX.Y.Z`, builds and pushes the images and sends bloque-ledgent its release dispatch
   (`.github/workflows/release.yml`, whose own checks the lane leaves as they are). Intake and the
   production deploy are bloque-ledgent's.

   **Do not wait for step 6 unless you can afford to** (the suite takes about 40 minutes, and the lane
   exists to save them). The release pull request skips its heavy checks only when `main`'s tip, or an
   ancestor with its tree but for versions (the develop tip a promotion merged), has a finished, passed
   push run, and it refuses to while the latest finished push run on `main` is red. After a hotfix the
   tip is the hotfix's commit, and its run is still going when release-please opens the pull request,
   so the gate answers `full` at that moment and **does not look again by itself**.
   - Waiting: once `main`'s run is green, re-run the release pull request's checks (close and reopen
     it, or "Re-run all jobs" on its CI run). The gate asks the API again and skips; the pull request
     merges in minutes, with no `--admin`.
   - Not waiting: merge the release pull request with `--admin` once its cheap checks are green, and
     **release before the suite has judged the fix** (below). If step 6 goes red after that, stop the
     deploy and revert the way step 6 says.
8. **Bring the fix to `develop`, the same day.** Until then the next promotion either conflicts on it
   or ships a `develop` without it.
   - Preferred: after the release, merge `main` back into `develop` (CONTRIBUTING, "Releases"). That
     carries the fix and the release commit together.
   - If `develop` cannot take that merge yet: cherry-pick the fix onto a branch from `origin/develop`
     (`git cherry-pick -x -S -s <commit>`) and open a normal pull request into `develop`.

## What ships untested

Between the merge (step 5) and a green full suite (step 6), the release carries a fix that has had:
rustfmt, the workspace clippy, the tests of the crates it touches, the structural gates, licences, DCO
and the title. It has **not** had: the tests of every other crate (a change in `ledger-core` does not
run `ingress`'s tests), the feature-flagged runs (`ingress`, `native-decide`, `net`, v1 roots), the
conformance suite and its assurance declaration, the all-features and `no_std` clippy, `cargo doc`, the
agents workspace (unless touched), the restore drill, the S3 and Vault jobs, and the stateful-upgrade
job. That is the price of the lane, and why it is for incidents.

## How long it takes, as of now

The merge no longer waits for the whole suite, but the **release does not become 15 to 20 minutes with
this lane alone**: after the merge, the release still rebuilds every image (about 18 minutes on 29 Oct)
and intake plus the production deploy take about 6 more. Promoting the images of a tested pre-release by
digest instead of rebuilding is step 3 of #312. Until it lands, count the lane as the pull-request half.

## If the lane did not engage

The gate falls back to the full suite on any doubt, so a wrong setup costs time and never rigour:
read the reason in the gate's summary. Then either fix the cause (variable, label, branch name, the
fix itself) and reopen, or wait for the full suite.

## Rehearsing the modes

The skip and hotfix paths of the required checks can be run for real, on a throwaway branch, without a
pull request into `main` (whose judge would be `main`'s):

    git switch -c rehearsal/<name> <your branch>
    git commit --allow-empty -s -S -m "chore: rehearsal"       # a head that is no pull request's
    git push -u origin rehearsal/<name>
    gh workflow run ci.yml -R bloque-app/aledgent --ref rehearsal/<name> -f rehearse=skip
    gh workflow run ci.yml -R bloque-app/aledgent --ref rehearsal/<name> -f rehearse=hotfix \
      -f rehearse_test_args="-p compact-smt"

The run is refused unless the branch is `rehearsal/*` and its head belongs to no pull request. It judges
no tree: it forces the mode, so its green checks prove nothing about the code. Delete the branch after.
