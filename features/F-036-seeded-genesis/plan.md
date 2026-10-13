<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->
# F-036 — Seeded genesis: aledgent's chainspec

| | |
|---|---|
| Milestone | M-3 — the window of `NFR-17`; before a first deployment's re-genesis |
| Track | B |
| Home | `crates/ledger-core` (`chainspec`, `ledger::seeding`, the service store's chainspec record), `crates/core-binding` (`StateGenesis`, `build_genesis`, the genesis check), `crates/core-archive` (the chainspec beside the segments), `services/ingress` (`genesis.chainspec`, `opening_sources`), `agents/core-host` (`build-spec`, `serve`/`restore --chainspec`, `verify-genesis`, the grown-state benches), `ops` (`opsctl --chainspec`), the operator's migration tooling (`chainspec build`, `history`), `e2e/stress` (`GROWN`), `.github/x86-bench` (`chainspec-genesis`) |
| Depends on | `F-014` (the opening set, its gate and its postings), `F-005` and `F-031` (the core Agent and its inputs), `F-035` (the core host's keys), PR #198 (the migration tooling) |
| Unblocks | A first deployment's re-genesis from a prior system's book; later deployments' from their own |
| Realises | `REQ-RC-6` (amendment 0022, draft), `REQ-RC-4`, `REQ-RT-3`, `INV-17`, `INV-18`, `NFR-17`; owner rulings of 2026-10-08: one verifier for many identities, `seed-history-commitment`, `genesis-chainspec` |

## Goal

The book's first state is defined by a chainspec of the prior system's identities, accounts, aliases,
opening balances and grants, committed by digest at genesis, and **built natively** with the same library
code, as a Substrate chain's genesis state is: not reached by executing every record as an input in
the PVM (measured on a rehearsal deployment at 17–23 records a second). Built natively in about 40 seconds
for a book of that size on a laptop.

## Spec traces

- `REQ-RC-6`: the seed (chainspec), its commitment, a first state derived from it with the commands'
  effects, sealed before any command, genesis only, replayable.
- `REQ-RC-4`: the opening set reconciled before anything is built, one `Opening` per pair, no history
  replayed; the history committed by a root.
- `REQ-RT-3`: replay is the genesis rebuilt from the chainspec, then every input after it.
- `INV-17`: aliases keep a digest; no history record is an input.
- `INV-18`: on the chunk-by-chunk path, a chunk applies once.
- `NFR-17`: the genesis is the window; after it, nothing of it remains possible.

## Design

1. **The chainspec** (`ledger_core::chainspec`, `spec.md` §1–§4): sections in order, sorted, exact minor
   units; a header with the registry, the counts, the opening commitment and the history commitment;
   chunks under a Merkle tree; one digest. Every reference derives from genesis's operation reference.
2. **Genesis commits it.** `Genesis::chainspec` (`{digest, chunks}`); `Genesis::spec`, the file itself,
   never serialised, when the state is built here.
3. **The genesis state is built** (`spec.md` §5): `Ledger::genesis` with the chainspec applies every chunk
   through `apply_spec_chunk`, natively; the result is sealed. `aledgent-core-host build-spec` writes the
   leaf store and the manifest of roots.
4. **The Agent starts from it in one input**: `Envelope::StateGenesis` carries the commitment, the
   configuration, the limits and the hot row. The core host builds the same state in its replica before
   the Agent's first input, refuses to start unless its roots are the manifest's, and checks the Agent's
   roots against its rebuild on every start.
5. **Replay and restore**: the chainspec kept in the state directory and the archive, by digest; `restore`
   and `opsctl` rebuild genesis from it.
6. **`verify-genesis`**: the second execution, chunk by chunk inside the PVM; the book takes no command
   until its report is installed (`install-verified`, `T-036-25`).
7. **Memory** (`spec.md` §6): streamed but for the book's service store, which the replica holds whole;
   `--max-memory`.
8. **Tooling**: `migrate chainspec build` from a prior system's extract or a synthetic book (to 10×),
   `migrate history`; grown state for the stress benches.
9. **Re-genesis and anchoring**: the new book starts where the contract stands (amendment 0022, option
   (e)); specified, code later.

## Out of scope

- Replaying history (`REQ-RC-4`); reading the pre-cutover archive (ruled, `prq-18b`).
- A disk-backed service store (`T-036-17`).
- The anchoring base of a re-founded book (`T-036-14` to `T-036-16`).
