<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-027 — Settlement extension and call outbox

| | |
|---|---|
| Milestone | M-2 — On VOS, end to end |
| Home | `agents/settle-extension` (AGPL, links VOS), with the envelope types in `crates/ledger-core` |
| Depends on | `F-005`, `F-016` (the guarantees it carries over), `F-026`, `DEC-18`; upstream: VOS draining an Agent's outbox into an extension (`F-025`) |
| Unblocks | the end-to-end run, `F-017` anchors on the contract |

## Goal
Agents write calls, each with our reference, to one outbox. A VOS host extension executes them on the
current venue through a light client and reports one outcome per reference. The venue can change from
the contract to Spaces without changing any agent.

## Scope
- **The call envelope:** reference, target, call, expiry. The same for every agent and every venue.
- **The extension:** it holds the keys and signs as the deployment's community PASS account with Kreivo's
  transaction extensions. It batches calls per block, stores each signed transaction before submitting,
  replays the same bytes after a restart, and reads outcomes in order at finality.
- **Until VOS can drain an Agent's outbox into an extension,** `services/settle` plays the extension's part
  against the same envelope.
- **Reference implementation for Kreivo signing and transaction extensions** (the owner's decision,
  2026-09-24). How to sign as a PASS account is already solved in a previous chain operator's signer, and
  is reused as the reference:
  - its PASS signer signs v5 General extrinsics authorised by `PassAuthenticate`: a
    device keypair signs a challenge bound to the exact call, and the runtime swaps the origin to the PASS
    account.
  - its Kreivo connection has a subxt `Config` naming Kreivo's
    transaction extensions: `F-016`'s open `TODO(F-016)`.

  This is an **owner-sanctioned exception** to `CLAUDE.md`'s rule that the replaced services'
  implementation must not inform the plan. It is limited to the Kreivo signing and transaction-extension
  mechanics, not the ledger's design. No code is copied until `T-027-03`.

## Open points
- The VOS side (outbox-to-extension after commit, extension limits) is designed with @olanod (VOS's maintainer).
- Anchors record their venue, so the verifier knows where to read them after a venue change.
