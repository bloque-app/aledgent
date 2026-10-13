<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Governance

aledgent is developed in the open and maintained by Bloque. This document says who decides what, and
how that can change.

## Roles

| Role | Who | Decides |
|---|---|---|
| **Contributor** | Anyone who opens an issue, a discussion or a pull request | Nothing alone; proposes |
| **Maintainer** | Listed in `CODEOWNERS`. Today, members of Bloque's aledgent team | Merges pull requests; rules on plan changes (`PLAN.md`, `DEC-*`, features); triages issues; cuts releases |
| **Spec owner** | One maintainer, named in `CODEOWNERS` for `SPEC.md` and `amendments/` | Rules on amendments to `SPEC.md` and closes open questions (`OQ-*`). Where documents say "the owner ruled", this is the role they mean |
| **Upstream maintainers** | The maintainers of VOS, CipherClerk and Kreivo | Their own projects. aledgent proposes; they decide |

## How decisions are made

- **Code and documentation.** A pull request is merged when a maintainer approves it and CI passes.
  Maintainers work by lazy consensus: a change that nobody objects to within a reasonable review
  window may be merged. An objection must give a reason that can be acted on.
- **Plan changes.** A change to `PLAN.md` or to a decision `DEC-*` needs a maintainer's approval and
  must not contradict `SPEC.md`.
- **Spec changes.** Only through an amendment (`CONTRIBUTING.md`), ruled by the spec owner. The
  ruling and its reasons are recorded in the amendment itself, so that the record of why the spec
  says what it says is public.
- **Guardrails.** The guardrails in `CLAUDE.md` and the non-goals in `SPEC.md` §13 change only by
  amendment. A proposal that needs to break one, for example an administrative path that moves value
  outside the command surface, is declined.
- **Disputes.** If contributors and maintainers disagree, the maintainers decide. If maintainers
  disagree, the spec owner decides.

## Becoming a maintainer

A contributor who has made sustained, high-quality contributions, and reviewed others', can be
proposed as a maintainer by an existing maintainer. The maintainers decide by consensus. Maintainers
who have been inactive for six months may be moved to emeritus status.

## Contributions

Contributions are accepted under the Developer Certificate of Origin (`CONTRIBUTING.md`, "Sign-off"):
inbound licence equals outbound licence, and there is no contributor licence agreement. The copyright
in each contribution stays with its author; the project's own code is held by Pagos Cube LLC,
Bloque's operating company.

## Releases

Maintainers cut releases from `main` with a `v*` tag. A release passes the end-to-end suite
(`.github/workflows/e2e-zombienet.yml`), which is the release gate, and its notes name the licence of
each artefact (`docs/oss/LICENSING.md`).

## Trademarks

"aledgent" and "Bloque" are names used by Pagos Cube LLC (Bloque). The open-source licences cover
the code and the documents. They do not grant the right to use those names to endorse or promote a
derived product. A fork should use a different name.

## Changes to this document

This document changes by pull request, approved by the spec owner.
