<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-029 notes

## Decisions

1. **The zero-divergence check is the contract's, not the core's.** The core could compare an
   attested holding with the book, but a holding read is stale by the time the binding lands. The
   contract checks `holding == recorded position + amount` at the block that records the binding, so
   the equality and the binding are one final fact under one reference (`REQ-SL-3`). The core freezes
   issuance of the asset while the binding is pending, so `amount` cannot move meanwhile.
2. **A library genesis may declare an asset bound.** `AssetDecl.location` is optional. The test
   suites' deployments (ingress's testkit, the conformance harness, the zombienet driver) start with
   `USD`, `EUR` bound, because their positions on the layer start empty with the book: the equality
   holds by construction at genesis. A deployment's configuration (`ingress.json`) has no field for it:
   production assets are book-only and bound by `bind_asset`. Question for the owner if this should
   be stricter.
3. **Funding is a plain transfer.** The issuer puts the outstanding balance in the contract's
   account by `Assets.transfer` (or a mint to it), which writes no inbox entry and credits nobody.
   The alternative, a deposit to a reserved account reference, would make the core tell a funding
   deposit from a holder's.
4. **Bind calls share the anchor key's lane**, one extrinsic of that key in flight at a time. A bind
   is rare (once per asset); a separate key would need its own funding and contract registration.
5. **A refused binding is final, not retried.** `HoldingMismatch` and `AlreadyBound` are the layer's
   answer: the asset is book-only again and the issuer asks again once the holding is right. Only an
   extrinsic that provably did not execute is signed again.
6. **Issuance continues after binding** (question `k-01`, defaulted): `REQ-AS-6` is unchanged. The
   third axis compares deposits, withdrawals and the binding's amount with the contract's position;
   in-book issuance of a bound asset after its binding is not backed until the issuer deposits it.
7. **No conversion is assumed.** Conversions happen outside aledgent (the owner, 2026-10-03); a
   binding takes one asset's outstanding balance and nothing else.

## Facts

Kreivo mainnet, spec 133 (2026-09-30): `Here(100)` COPM (symbol `COPM`, decimals 2, live); some
asset ids carry no metadata at all. Each name reads `SYMBOL/decimals`; nothing reads it.
