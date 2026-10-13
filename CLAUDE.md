<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# CLAUDE.md

Operating instructions for agents working in this repository.
This file is *not* documentation. `SPEC.md` is the source of truth for what
aledgent does and why. Read it before doing anything substantive.

## What this project is

aledgent is a **private, provable ledger of record** that third parties
integrate — identities that own accounts, value that moves between them
instantly and confidentially, and **one settlement layer** through which value
enters, leaves and is anchored. `SPEC.md` is self-contained and names no
technology, network or prior system; `PLAN.md` binds it to Kreivo (the
settlement layer), VOS (the runtime) and CipherClerk (the kernel).

Customers are humans, enterprises **and agents**. An agent is a principal with
its own credential and a bounded, revocable authority — never a persona acting
under a human's key (`SPEC.md` §3, Epic G).

## Spec-driven development — the rule that matters

The chain is:

    notes/               (measurements and design notes — informative)
      → SPEC.md          (what; self-contained, product- and technology-agnostic)
        → amendments/    (changes to SPEC.md, one file per review pass)
        → PLAN.md        (how; binds VOS, CipherClerk, Kreivo)
          → features/NNN-*/plan.md
            → features/NNN-*/tasks.md
              → GitHub issue
                → PR → code

Changes flow **down** this chain, never up.

- Behaviour is decided in `SPEC.md`. If code, `PLAN.md` and the spec disagree,
  the spec wins and the others are wrong.
- `PLAN.md` MUST NOT contradict `SPEC.md`. If a technical fact makes a
  requirement unsatisfiable, write an amendment in `amendments/` first, mark
  the plan site `[PENDING AMENDMENT NNNN-An]`, and plan against the amended
  text. A decision made in a plan or in code is a decision nobody reviewed.
- Never implement behaviour that is not specified. Found a gap mid-task? Stop
  and propose an amendment.
- Do not resolve an open question (`OQ-*`) by implementing one of its
  options. The ones `SPEC.md` §14 marks as blocking V0 must be closed by the
  spec owner (`GOVERNANCE.md`) before the V0 freeze.
- Never silently widen scope. Anything in `SPEC.md` §12 (Deferred) or §13
  (Non-goals) is out until the spec says otherwise.
- Work inside **one feature directory at a time**.

## Traceability

Every unit of work cites the identifier it serves:

- Task ids are `T-<feature>-<seq>` (e.g. `T-004-02`). Stable forever.
- Commits and PRs carry the task id and reference the `US-*` / `REQ-*` /
  `INV-*` they implement.
- Tests name the `AC-*` or `INV-*` they verify. `INV-*` tests must exercise the
  invariant at the **layer `SPEC.md` §9 names** (kernel, store, service,
  credential) — `REQ-AR-4` forbids satisfying an invariant at a weaker layer.
- A `tasks.md` entry that traces to nothing in `SPEC.md` is invalid.

Identifier namespaces are defined in `SPEC.md` §0 (`US`, `AC`, `REQ`, `CTR`,
`INV`, `ERR`, `NFR`, `OQ`, `DEF`) and `PLAN.md` §0 (`DEC`, `F`, `M`, `RISK`).
Never renumber an existing identifier. Add, don't renumber.

## Vocabulary — use these exact names

`SPEC.md` §2 is the glossary and lists the retired legacy names. The ones
most likely to slip back in:

| Say | Never say | Why |
|---|---|---|
| **book** | the ledger (meaning the record) | "Ledger" is the service; the kernel's own word "ledger" means a *currency* |
| **ledger account** | pocket, smart account, pass account, movements account, address | Four things called "account" today; only this one is a balance owner |
| **available / reserved / encumbered** | current, pending, hold, `:pending`, balance *layer* | The three balance **kinds**; `total` is their sum; how they are realised is a plan choice (`REQ-KER-3`) |
| **reservation / capture / release** | hold, settle, refund, unreserve | The reservation *is* the balance effect, taken immediately |
| **operation reference** vs **client key** | `reference`, `Idempotency-Key` | Two keys, never conflated (`REQ-OP-10`) |
| **observation / confirmation / finality / settlement** | confirmed, settled (as one word) | Three distinct conditions (`REQ-IN-7`) |
| **authority / credential** | signer, delegate, proxy, controller | "Signer" meant three things |
| **the settlement layer** / **settlement adapter** | rail, rail name, chain operator, Kreivo (in `SPEC.md`) | One settlement layer, defined by requirements; named only in `notes/` and `PLAN.md` |
| **`did:`-shaped identifier** | URN | Identifiers are `did:`-shaped |
| **issuance counterparty** | `@world`, the zero address | The named position value enters from and leaves to |

Say "aledgent" for the service; say "the book" for the record.

## Guardrails

These are the mistakes most likely to look like good engineering. Do not make
them without an explicit spec change:

1. **`SPEC.md` is self-contained and names no technology, no network and
   no prior system.** Not VOS, not CipherClerk, not Kreivo, not the services
   being replaced. Requirements state capabilities; `PLAN.md` binds them;
   `notes/` carries measurements and technical constraints. A "today X
   happens" justification belongs in `notes/`, not in a requirement.
2. **No entry against an external counterparty before finality** (`INV-10`,
   `REQ-OP-1`). Everything before that is a reservation against the payer.
3. **No caller may assert an outcome** (`INV-11`). The settlement adapter
   and every caller submit *observations at coordinates*; aledgent derives the
   status.
4. **No release on a timer alone** (`REQ-OP-9`). Absence of evidence is not
   evidence of failure (`REQ-OP-6`); the answer is `Needs-Review` with an
   owner and a service level, not a refund.
5. **A reservation and its capture or release are one object, matched by
   identity** (`INV-14`). An amount supplied by a confirming party never
   decides what is released.
6. **No external leg on a path that has not reserved** (`INV-13`).
7. **No administrative path that moves value outside the command surface**
   (`REQ-AT-3`, §13). No storage repair, no support override. This will be
   asked for with a good reason; the answer is the same.
8. **One book, one settlement layer** (`INV-25`, §1.7). Nothing partitions
   the book; nothing but the settlement adapter touches an external network.
9. **No settlement-layer record per operation, account or identity**
   (`REQ-SL-5`). Internal transfers never touch the settlement layer.
10. **Amounts are exact integer minor units** (`INV-7`, `REQ-AS-4`). No
    floating point anywhere; excess precision is rejected, never rounded.
11. **Do not claim anchoring proves solvency** (`REQ-PR-3`). It proves
    non-revision. Do not claim a disclosure is independently checkable when
    it is aledgent's signature (`REQ-PR-6`).
12. **No personal data in identifiers or anchored values** (`INV-17`).
13. **Key material never lives in the process that holds the book** (§4.1,
    `NFR-8`, `REQ-SA-8`), and no credential ever reaches a log, URL or default
    (`REQ-AU-8`).
14. **Do not use experimental privacy features with real value.** The kernel's
    graph-, metadata- and payee-hiding layers are unaudited; `DEF-4` defers
    them and `REQ-AR-5` requires the assurance level to say what is enforced.

## Upstream repositories

| Repo | What it is | Role here |
|---|---|---|
| `virto-network/kreivo`, `virto-network/frame-contrib` | The chain runtime and its pallets | Bound by `PLAN.md` to `REQ-CH-*` |
| `codeberg.org/Virto/VOS` | The actor runtime | Bound by `PLAN.md` to `REQ-RT-*`. Study its actors and extensions; do not assume they are complete |
| `codeberg.org/Virto/CipherClerk` | The confidential double-entry kernel | Bound by `PLAN.md` to `REQ-KER-*` |

Codeberg forbids automated scraping: clone VOS and CipherClerk into a
temporary folder instead of fetching pages.

## Implementation constraints

Decided; not open for re-litigation in a plan or a task.

1. **The runtime is VOS, the kernel is CipherClerk, the chain is Kreivo.**
   `SPEC.md` does not name them; `PLAN.md` binds them. If a binding cannot
   satisfy a requirement, the plan says who builds the gap, in which repo
   (VOS, CipherClerk, frame-contrib, or here), and the fallback — it never
   assumes the capability into existence.
2. **An operator's migration is a plan concern, not a spec concern.**
   The spec offers only a generic initial-state load (`REQ-RC-4`). An
   existing book must be extracted and retained immutably before anything
   else, and cutover requires zero divergence on `NFR-9`'s three axes,
   sustained.

## Upstream projects

VOS, CipherClerk, Kreivo and frame-contrib are upstream. Where aledgent needs
a change in one of them, draft it in `proposals/` (changes flowing **up**);
`amendments/` is for changes to `SPEC.md` itself. Do not mix them, and do not
open issues or PRs upstream without being asked.

## Issues

Issues live in GitHub, one per feature, with milestones `M-0`…`M-4` and `Later`.
The issue body is generated from `features/F-NNN-*/plan.md` and `tasks.md`. `tasks.md` owns scope and
traceability; the issue owns execution. Never change scope in an issue — edit the feature files and
re-sync. Open points that need the spec owner are raised as issues labelled `decision`.

## Scratch space

`.scratch/` is gitignored and local to the checkout.

| Folder | Holds | Remove when |
|---|---|---|
| `.scratch/sessions/` | Records of a working session: rulings given, questions asked, state reached | The PR the session produced is merged or closed |
| `.scratch/decisions/` | Decisions raised but not yet taken, or taken but not yet landed in `SPEC.md`, an amendment, `PLAN.md`, or code | The decision lands in one of those |

- Write to `.scratch/` *before* a long answer, not after.
- Nothing in `.scratch/` is authoritative until it lands in an amendment or
  `SPEC.md`.
- Name files `YYYY-MM-DD-<slug>.md`. Each file says, at the top, what event
  removes it.

## Conventions

- British English in `SPEC.md`, `PLAN.md` and amendments (organiser,
  authorised, behaviour). 
- RFC 2119 keywords in normative text only. They mean what they say.
- Spec and plan prose states trade-offs plainly, including unflattering ones —
  "Trade-off, stated plainly" callouts. Keep that register; do not sand it
  down.
- Numbers from a deployment are cited as constraints, never as designs.
- No file paths in `SPEC.md`; the spec cites requirements and reasons.

## Commands

One Rust workspace (`DEC-15`), stable toolchain (`rust-toolchain.toml`). CI runs the same commands
(`.github/workflows/ci.yml`).

    build:  cargo build --workspace
    test:   cargo test --workspace
    lint:   cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings
    gates:  tools/ci/check-kernel-features.sh   # no experimental CipherClerk layer (DEC-11, DEF-4)
            tools/ci/check-conformance-ports.sh # no production crate enables the raw ports (REQ-CS-5)
            tools/ci/check-no-float.sh crates   # no f32/f64 (INV-7)
            tools/oss/spdx.py --check           # every source file carries its licence header
    conformance: cargo test -p conformance   (gate profile of the current milestone)
