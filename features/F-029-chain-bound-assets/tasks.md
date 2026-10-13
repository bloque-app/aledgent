<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-029 tasks

| Task | What | Traces | Done when |
|---|---|---|---|
| `T-029-01` | ~~PLAN `DEC-20`, assets bound to their location~~ Superseded: `DEC-20` reworked in the amendment 0011 PR | `REQ-AS-1`, `AS-2` | — |
| `T-029-02`–`T-029-08` | The first design (#112): location at registration, attested precision, holds, `reconcile_asset`. Kept in history; reworked by `T-029-09` onward | `REQ-AS-1`–`AS-3` | — |
| `T-029-09` | The registry book-only: `AssetBinding`, `set_binding`, digest, leaf, export format 10; `register_asset` with its precision again; `AssetDecl.location` optional | `REQ-AS-1`, `AS-7`, `INV-24` | Registry tests round-trip every binding; one location, one asset |
| `T-029-10` | The contract's `Bind` call, `Bound` entry, `AlreadyBound`, `HoldingMismatch`, `bound(asset)` (`F-026` decision 24) | `REQ-AS-8`, `REQ-SL-3` | Contract tests: zero divergence, one unit off, twice, after a deposit, zero |
| `T-029-11` | Attestations of the symbol and decimals fields, no name, no hold; `admit` | `REQ-AS-2`, `AS-8`, `AS-9`, `INV-11` | `AC-F5.2` |
| `T-029-12` | `bind_asset`: issuance authority, the outstanding balance, `Binding` freezing issue/redeem, the bind call in the outbox; finality from the `Bound` or refused record; the third axis's baseline; legs for bound assets only | `REQ-AS-7`, `AS-8`, `NFR-9` | `AC-F5.1`, `AC-F5.3`, `AC-F5.4` in the core and the conformance suite |
| `T-029-13` | Ingress: `POST /v1/ops/assets/{asset}/bind`, `binding` in `GET /v1/assets`, `bound`/`bind_refused` observations, `Bind` in the call outbox, attestation wire without name and sufficiency; openapi | `REQ-AS-8` | Ingress tests, openapi check |
| `T-029-14` | The adapter: the bind lane on the anchor key, `settle_binds`, decoding of `Bound` and refused binds | `REQ-SA-3`, `SA-5`, `SA-6` | `flow.rs`: a book-only asset bound at zero divergence through the fake contract, after a refusal one unit off |
| `T-029-15` | Canary templates: COPM/2, TSTA/2, TSTB/2, TSTC/6 book-only, mapped to `Here(100)`–`Here(103)` | `REQ-AS-7` | The template test |
| `T-029-16` | Zombienet: `BKO` at `Here(3)` (name `BKO/9`, decimals 2); the aledgent driver registers it book-only, issues, moves, binds one unit short (refused), funds the unit (polkadot-api), binds, anchors, compares at zero | `US-F5`, `NFR-9`, `REQ-PR-8` | The suite's step 3 green on the self-hosted x86-64 runners with `driver=aledgent` |
