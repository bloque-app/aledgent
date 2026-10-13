<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="docs/brand/lockup-dark.svg">
    <img alt="aledgent by bloque" src="docs/brand/lockup.svg" height="48">
  </picture>
</p>

# aledgent

aledgent is a private, provable ledger of record. Identities own **ledger accounts**. Value moves
between them instantly and confidentially, and it is written to **the book**, the one record of every
balance and operation. One **settlement layer** is the only place value enters, leaves and is anchored;
the **settlement adapter** is the only component that touches it. Customers are humans, enterprises
and agents, each with its own credential and a bounded, revocable authority.

aledgent is built by [Bloque](https://bloque.app) and developed in the open. This repository holds
the specification, the plan and the code. Words such as *book*, *operation*, *observation*,
*finality* and *anchor* have exact meanings, defined in `SPEC.md` §2.

> **Status: pre-release, in production at Bloque since 12 October 2026, no external audit.** The core,
> ingress, the settlement adapter and the settlement contract are built and tested; nothing here has
> had an external security audit yet, so review it before you hold real value with it. `ROADMAP.md`
> says what comes next.

## How the project is organised

Work is spec-driven. Changes flow down this chain and never back up:

    notes/                 measurements and design notes (informative)
      -> SPEC.md           what aledgent does and why; names no technology
        -> amendments/     changes to SPEC.md, one file per review pass
        -> PLAN.md         how it is built; binds the spec to VOS, CipherClerk and Kreivo
          -> features/     one directory per feature: plan.md, tasks.md, NOTES.md
            -> issue       one GitHub issue per feature
              -> PR        code, tests and the task id (T-<feature>-<seq>) it serves

If code, plan and spec disagree, the spec wins. Behaviour that is not in the spec is not built; a gap
becomes an amendment first. `CONTRIBUTING.md` explains how to propose a change at each level, and
`GOVERNANCE.md` who decides.

Three things are named only in the plan and the code, never in `SPEC.md`:

- **Kreivo**, the Polkadot parachain that is the settlement layer. Its runtime lives in `virto-network/kreivo`.
- **VOS**, the actor runtime the core runs on. It is AGPL-3.0-or-later, so it is confined to `agents/`.
- **CipherClerk**, the confidential double-entry kernel that holds the balances.

## Map of the code

| Path | What it is |
|---|---|
| `crates/ledger-kernel` | The binding to CipherClerk: accounts, postings, the leaf-store trait (`F-001`) |
| `crates/ledger-core` | **The core.** The host-independent library with every rule of the spec: identities, authority, reservations, the operation lifecycle, transfers, evidence, custody, anchoring |
| `crates/core-binding` | The half of the core's VOS Agent binding that does not link VOS |
| `crates/leaf-store` | The leaf store: the book's leaves, history and indexes, held outside the core and rebuildable from its delta log |
| `crates/audit-store` | The append-only, hash-chained audit trail |
| `crates/verify` | The anchor and disclosure verifier; a recipient runs it without aledgent |
| `crates/conformance` | The conformance suite: a case per invariant, error and acceptance criterion, and the CI gate |
| `crates/authn` | Credential schemes at the boundary: passkeys, API keys, sessions |
| `crates/shards` | A prototype of the book in shards of partitions, with the credit path |
| `services/ingress` | **Ingress.** Every command and query enters here; each command is logged durably before it is answered |
| `services/settle` | The settlement adapter: holds the signing keys, submits calls to the settlement contract, reads its ordered inbox and reports observations |
| `services/recovery` | The recovery-authority service, run apart from the operator |
| `contracts/settlement` | The ink! settlement contract on Kreivo: one position per deployment, deposits by account reference, batched withdrawals, anchors, an ordered inbox |
| `agents/` | The VOS Agent core (`core/`), its host (`core-host/`), the shard Agent and aledgent's AgentRuntime: a separate AGPL workspace |
| `e2e/` | The zombienet end-to-end suite, the stress harness and the CI sidecar |
| `ops/` | Backup and restore drill, the six operating signals, alerts, reconciliation (`opsctl`) and runbooks |
| `deploy/` | Container images (`deploy/docker`) and configuration templates (`deploy/templates`) |
| `spikes/` | Measurement code for `F-002`; never shipped |
| `tools/` | CI gate scripts, the licence-header check and the PDF renderer |
| `amendments/`, `features/`, `notes/`, `proposals/` | The chain above. `proposals/` holds changes aledgent proposes to upstream VOS and CipherClerk |

## Build, test and lint

The root workspace uses the stable Rust toolchain. CI (`.github/workflows/ci.yml`) runs these:

    cargo build --workspace
    cargo test --workspace
    cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings
    cargo test -p conformance              # the gate profile of the current milestone
    tools/ci/check-kernel-features.sh      # no experimental CipherClerk layer
    tools/ci/check-conformance-ports.sh    # no production crate enables the raw test ports
    tools/ci/check-no-float.sh crates      # no floating point (also run on services and ops)
    tools/ci/check-no-vos.sh               # VOS appears only in agents/ and spikes/
    tools/oss/spdx.py --check && reuse lint  # every file carries its licence

Other workspaces build on their own (details in each README):

    # contracts/settlement: Rust 1.91 and cargo-contract 5.0.3
    cd contracts/settlement && cargo test && cargo contract build --release

    # agents/: nightly-2026-03-20, and a patched CipherClerk first
    tools/cipherclerk-patched.sh
    cd agents && cargo test --locked && (cd core && cargo actor)


The end-to-end suite needs an x86-64 Linux host or an Apple silicon Mac, `protoc`, Node 22 and
`cargo-contract`. CI runs it on a self-hosted runner labelled `zombienet`, by manual dispatch and on
every `v*` release tag. To run it by hand, follow "By hand" in `e2e/README.md`.

## Licence

aledgent is open source, copyright Pagos Cube LLC (Bloque), with copyleft only where it is forced:

- `agents/**` links VOS and is **AGPL-3.0-or-later**;
- the rest of the code is **Apache-2.0 OR MIT**, at your option;
- the specification, plan and other documents are **CC-BY-4.0**.

Every file says which applies (an SPDX header, or `REUSE.toml`). `docs/oss/LICENSING.md` explains the
choices and the third-party licences, including the GPL-3.0-only parts compiled into the built
settlement contract.

## Where to read next

- `SPEC.md`: what aledgent does. `OVERVIEW.md` is a shorter digest.
- `PLAN.md`: decisions (`DEC-*`), features (`F-*`), milestones (`M-*`) and risks.
- `ROADMAP.md`: the public roadmap.
- `CONTRIBUTING.md`, `GOVERNANCE.md`, `CODE_OF_CONDUCT.md` and `SECURITY.md`: how to take part, and how
  to report a vulnerability privately.
- `notes/`: measurements and design notes. `proposals/`: changes for upstream projects.
