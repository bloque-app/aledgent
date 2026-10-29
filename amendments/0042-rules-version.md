<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0042 — "Code generation" is renamed "rules version"

**Status:** **ruled by the owner on 2026-10-28.** The name is the owner's choice among the candidates put to him
that day. **Applied** to `SPEC.md` **v0.4.30**, to `PLAN.md`, and to the docs, the runbook and the feature plans
and tasks, in the same pass (text only; the code identifiers follow, see "Not renamed here"). It replaces the
term of amendment 0033, which stays as written.
**Target:** `SPEC.md` v0.4.29 as on `develop` (at `9c12b11`); amendment 0033 (where the term was introduced).
**Date:** 2026-10-28
**Number:** 0041 is the last on `develop` (PR #95); no open PR or remote branch claims 0042.
**Amends:** `SPEC.md` §2 (the glossary entry, renamed and extended; two other rows name it); `REQ-RT-19` and
`REQ-RT-20` (the term only); `AC-K7.9`, `AC-K7.13`, `AC-K7.16`, `AC-K7.17`, `AC-K7.18` (the term only);
`CLAUDE.md` (one row in the vocabulary table, so the old name does not slip back in). No
requirement, command, query, error condition or invariant changes meaning, and no identifier is renumbered.
**Rationale in one line:** "code generation" reads as producing code (building, compiling, code generators),
and what it names is a count.

---

## The change

The term **code generation** becomes **rules version**. Its meaning does not change: the count of a writer's
upgrades, 0 at genesis and one more after each upgrade. "Generation" used alone in that sense becomes "rules
version" too, and "code generation 1" becomes "rules version 1".

Amendment 0033 introduced the term and keeps its text. From this amendment on, wherever 0033, 0034, 0035 or
0038 say "generation" or "code generation" in that sense, read "rules version". Their text is history and is not
rewritten.

### The glossary entry

`SPEC.md` §2 gains, in place of the "Code generation" row:

> **Rules version.** The count of a writer's upgrades: 0 at genesis, and one more after each upgrade. A
> writer holds the rules version of its code in force. Each code version names one rules version, and the code
> that decides its transitions carries that rules version in its own bytes (`REQ-RT-20`). It is not the code
> version, which is the identity of the exact build: rules change, the rules version goes up; code changes,
> the code version changes. A rebuild with the same rules is a new code version but the same rules version.
> Called the code generation before amendment 0042.

### Rules version and code version

The two names differ by one word and mean different things:

| | Rules version | Code version |
|---|---|---|
| It is | a count: 0 at genesis, +1 per upgrade | a digest: the identity of the exact build |
| It answers | how many upgrades has this writer been through | which bytes, built with which settings |
| A rebuild of the same rules | does not change it | is a new code version |
| An upgrade | always raises it, by one | always names a new one |
| The fence compares | this (`REQ-RT-20`) | never this; the execution node checks it by digest |

Every writer has a rules version, not only the core. `SPEC.md` speaks of "a writer" throughout, and so does the
renamed term.

### One tension, stated plainly

The name says "rules", and `REQ-RT-20` requires every upgrade to raise the count, "even an upgrade made to
change only another of its artefacts, such as the runtime that executes that code". So a runtime-only upgrade
raises the rules version although no rule changed. The count is still the count of upgrades; the name is
loose for that case. This amendment does not reword the requirement. The glossary's line "rules change, the
rules version goes up" is the short form: it is not read as "only when rules change", and a rebuild of the same
rules cannot be installed as an upgrade, because upgraded code must carry the next rules version in its bytes.

## What does not change

- **Bytes on disk and on the wire.** The compiled record's on-disk tag `ALGGEN01` and the manifest's encoded
  field stay byte-identical. Already-built packages and recorded code versions keep verifying, and no
  checkpoint, code map, archive or anchor changes.
- **Identifiers.** None is renumbered. That covers requirement, acceptance-criterion, task and decision ids
  (`REQ-RT-20`, `AC-K7.16`, `T-037-15`, `DEC-26`) and every name in the code.
- **Behaviour.** Nothing in `SPEC.md` is decided differently.

## Not renamed here: a follow-up task

Code identifiers keep the old word for now. They are renamed in one later change, after test's core upgrade
(the rename touches the compiled package, so it must not race it):

- the `GENERATION` constant (`agents/core/src/lib.rs`);
- the `.aledgent.generation` ELF section and the exported symbol `ALEDGENT_CODE_GENERATION`;
- the manifest field `generation`, in code and in `F-037`'s task text that cites it by name; the field's
  encoded bytes stay as they are;
- the hot row's `generation` field, `generation_of` and function names such as `core_generation`, and the test
  files named after the term (`code_generation.rs`, `f043_generations.rs`);
- the metric `aledgent_writer_generation` and its help text, the ingress `generation` field and the
  descriptions in `services/ingress/openapi.json` (generated from the code);
- comments and strings in the sources, the workflows (`core-release.yml`, `images.yml`, `code-history.yml`),
  `agents/core/pvm.ld`, the release scripts and `ops/src/signals.rs`;
- the fixture directory `crates/core-binding/tests/fixtures/f043-generation-0` and its entry in `REUSE.toml`.

The follow-up carries its own proof that the bytes do not move: the compiled record and the manifest of a
release built before and after the rename are compared.

## What this pass replaced

Counted outside backtick spans, which hold identifiers and are left alone. "Code generation" and bare
"generation" are counted apart:

| File | "code generation" | "generation" alone |
|---|---|---|
| `SPEC.md` | 7 | 26 |
| `PLAN.md` | 2 | 7 |
| `docs/code-history.md` | 2 | 9 |
| `ops/runbooks/release/writer-fenced.md` | 1 | 10 |
| `features/F-037-code-history/plan.md` | 1 | 7 |
| `features/F-037-code-history/tasks.md` | 0 | 21 |
| `features/F-041-passkey-approval-core/plan.md` | 1 | 0 |
| `features/F-041-passkey-approval-core/tasks.md` | 2 | 0 |
| `features/F-043-chained-settlement/plan.md` | 0 | 1 |
| `features/F-043-chained-settlement/tasks.md` | 0 | 6 |
| `CONTRIBUTING.md` | 0 | 1 |
| `agents/README.md` | 0 | 2 |
| `deploy/docker/core.Dockerfile` (comments) | 0 | 3 |
| `crates/conformance/spec/catalogue.toml` (generated) | 2 | 6 |

`crates/conformance/spec/catalogue.toml` is generated from `SPEC.md` (`cargo xtask spec-ids`) and follows it:
five acceptance-criterion texts (`AC-K7.9`, `AC-K7.13`, `AC-K7.16`, `AC-K7.17`, `AC-K7.18`) and `spec_version`.

## Left alone, because it is not this term

- "Generation" meaning production of something: `F-006` (proof generation), `F-015` (catalogue generation),
  `F-042` (a token's generation number), `F-002` and `notes/MEASUREMENTS-F002.md` (generating accounts and
  books), `spikes/results/logs`.
- "Code generation" meaning a compiler's output: `proposals/vos/0005-recompiled-agent-execution.md` and
  `proposals/vos/pr/10-recompiled-agent-execution.md`, and "generation" in `proposals/vos/pr/08-prepared-actor-cache.md`.
- Amendments 0033, 0034, 0035 and 0038 (history; this amendment says to read them with the new term).

## Reading check next to "code version"

Each replaced sentence was read beside "code version". The sentences that still read well are not listed. The
ones that read less well, left as the mechanical replacement made them and not reworded, are:

1. `SPEC.md` §2, "Rules version" and `REQ-RT-20`'s runtime-only case: the tension above.
2. `features/F-043-chained-settlement/tasks.md`, `T-043-06` and `T-043-07`: "under the rules version rule"
   (formerly "the generation rule", the owner's rule of 2026-10-28 that a behaviour change applies from a
   rules version on, and earlier rules versions replay as decided). The doubled "rule" is clumsy.
3. `features/F-037-code-history/tasks.md`, `T-037-12`: "this build's code under both rules version numbers"
   (the old "generation numbers"). "Version numbers" now sits beside "rules version" and "code version".
4. `features/F-037-code-history/tasks.md`, `T-037-12`: "decide each way-out input as the rules version it
   replaces". The sentence means the code of the earlier rules version; it reads as if a rules version were
   replaced.
5. `features/F-043-chained-settlement/plan.md`: "the previous rules version's state under the new core" can be
   read as the previous code version's state.
6. Sentences where a package, a build or a piece of code *is* a rules version. They are the likeliest to be read
   as "code version", and read correctly only because the context says "count" or "the next one":
   `CONTRIBUTING.md` ("builds it as the next rules version of the code a book already runs");
   `agents/README.md` ("a later rules version claims the state of the previous rules version's program");
   `deploy/docker/core.Dockerfile` ("a later rules version of the code a book already runs", and "a package
   whose rules version is not the predecessor's plus one");
   `ops/runbooks/release/writer-fenced.md` ("the installed package is the old rules version", "a rules version the
   state never names", "installs the old rules version's ..."); `features/F-037-code-history/plan.md` ("installs
   a newer rules version after an upgrade"); `features/F-037-code-history/tasks.md` `T-037-19` ("installs a new
   rules version of the core in place").
7. `PLAN.md` (the `F-037` section) and the runbook still show the field `generation` in backticks beside the
   prose "rules version". That is the follow-up task, not a mistake.

No replaced sentence speaks of a non-core writer: those that name a writer say "a writer", and those that name the
core (the runbook, `CONTRIBUTING.md`, `agents/README.md`, the `Dockerfile` comments, `T-037-19`, `T-037-23`) are
about the core only.

## Rebased onto amendment 0041

The 0041 stack (PRs #95 to #99) and the grant-choice fix (#101) merged into `develop` using the term, so this
amendment was rebased onto them and the rename applied to the prose they added. `SPEC.md` is untouched by that
stack, so v0.4.30 stands. Amendment 0041 was revised through 2026-10-29, after this rename was ruled on
2026-10-28, so its own text is updated as well (the one case where an amendment's prose is
changed); no requirement, decision or ruling in it changes meaning. The replaced sites:

- `amendments/0041-the-book-follows-a-new-settlement-sequence.md`: fact 8, the adapter's table row, the
  paragraphs "What the core does with it" and "The guard", the rollout notes, decision 1 ("The rules version") and
  decision 6's rules version line.
- `features/F-043-chained-settlement/plan.md` and `tasks.md` (`T-043-14`, `T-043-17`, `T-043-18` and the order
  paragraph).
- `features/F-003-identities-accounts-assets-authority/plan.md` and `tasks.md` (`T-003-11`, from #101).
- `ops/runbooks/gap-filling.md`, `ops/runbooks/release/settlement-adapter-outage.md` and
  `services/settle/README.md` (one phrase each: "before amendment 0041's rules version").

Identifiers keep the old word, as in "Not renamed here": `GENERATION`, `core_generation`, `set_generation`,
`f043_generations.rs` and the comments of the sources and `ops/prometheus/rules/aledgent.yml`.
