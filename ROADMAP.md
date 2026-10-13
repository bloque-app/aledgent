<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Roadmap

This is aledgent's public roadmap: where the project is, and the themes it works on next. It is
derived from `PLAN.md` §4 and §5 (features `F-*`, milestones `M-*`) and `SPEC.md` §12 (deferred scope,
`DEF-*`). Those documents are authoritative; this one is a map of them.

The roadmap has **no dates**. Work is ordered by dependency, and a milestone is done when its gate
passes, not when a calendar says so. To propose a change to the roadmap, open an issue with the
`roadmap` label (`CONTRIBUTING.md`).

## Where we are

| Milestone | Claim that becomes true | State |
|---|---|---|
| `M-0` Foundations | The kernel is bound and measured. The conformance suite exists | **Done** |
| `M-1` The book, in a process | Every V0 story that needs neither the runtime nor the settlement layer passes against the library in a plain process | **Done** |
| `M-2` On VOS, end to end | A deposit and a withdrawal complete through the settlement contract on a local Kreivo, with the core as a VOS Agent | **Mostly done**: the core Agent, ingress, the leaf store, the settlement adapter, anchoring, the contract and the settlement extension are built. Upstream VOS and CipherClerk changes are in review |
| `M-3` Production V0 | A first operator runs its book on aledgent, with the three reconciliation axes at zero divergence | **In progress**: operations, credential schemes, book-only assets and the stress suite are built or under way |
| `M-4` Replicated | The core survives the loss of a node without a restore, and scales by adding shards and ephemeral replicas | **Designed**, with a working shard prototype |

## Themes

### 1. A production-ready V0

- Operations: backup and verified restore, the six operating signals, reconciliation that pages on a
  difference of one minor unit (`F-018`).
- Credential schemes at the boundary: passkeys, API keys, sessions, credential import (`F-028`).
- Assets that live in the book first and bind to the settlement layer later, at zero divergence
  (`F-029`).
- A stress suite with fault injection, and a published throughput design (`F-032`).
- An external security review of the core, the settlement adapter and the contract, before any
  deployment holds real value.

### 2. Throughput and scale

- Many commands per invocation, state in committed maps, and the book in shards of accounts
  (`F-031`).
- aledgent's own runtime on VOS: composite invocations, ordered and exactly-once streams between
  the book's writers (`F-033`).
- Replicated core Agents and ephemeral horizontal scaling, so that any node can be replaced online
  (`F-021`, `NFR-21`).

### 3. Proofs and disclosure

- Transition proofs produced by the core Agent and kept beside the anchors that cover them (`F-030`).
- Disclosures a recipient can check by a construction, not by aledgent's signature (`F-023`,
  `DEF-3`).

### 4. Custody

- Owned custody with holder-held kernel keys (`F-020`), and owned custody as an option a deployment
  can make the default (`DEF-2`).
- A KMS-backed signer for the settlement adapter, so that no signing key is held in a process's
  environment.

### 5. Settlement

- Cross-deployment settlement between operators who run their own books (`F-022`, `DEF-1`).
- Covering settlement-layer costs from a metered allowance (`DEF-10`).

### 6. Upstream

aledgent depends on VOS, CipherClerk and Kreivo, and proposes the changes it needs to them in the
open (`proposals/`, `F-025`). Upstream changes are written as proposals that the upstream
maintainers decide on; aledgent carries a pinned fork only until they land.

### 7. The project itself

- A first tagged release with signed, reproducible container images and a published source offer
  for the AGPL components.
- Integration guides: the HTTP surface (`services/ingress/openapi.json`), the CI sidecar
  (`deploy/docker/SIDECAR.md`), the verifier (`crates/verify`) and the conformance suite as a test kit
  for other implementations.

## Not on the roadmap

`SPEC.md` §13 lists the non-goals. Among them: no administrative path that moves value outside the
command surface, and no claim that anchoring proves solvency. Proposals that need one of these are
declined, whatever the reason given.
