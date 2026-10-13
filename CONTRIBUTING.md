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
- is signed off (below).

A maintainer reviews it. `CODEOWNERS` routes reviews: changes to `SPEC.md` and `amendments/` need the
spec owner's approval.

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
