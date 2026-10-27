<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-044 — The settlement adapter on native accounts, its keys in Vault Transit

| | |
|---|---|
| Milestone | M-3 — Production V0 (no key material in any process: the adapter's keys never leave Vault) |
| Track | B |
| Home | `services/settle` (its signer boundary, the transaction, the fake layer, the configuration, the binary), `services/settle/kreivo-pin` (the native vector) |
| Depends on | `F-027` (the executor and its transaction), `F-042` (signature engines, the Vault Transit engine), `F-043` (the adapter it is built on) |
| Unblocks | Turning settlement on again with keys that cannot leave Vault; removing the PASS form once the zombienet suite moves (`T-044-06`) |
| Realises | `NFR-8`, `REQ-SA-8`, `REQ-SA-9`, `REQ-AU-8` |
| Status | **Built** (the pull request that carries this file). The owner's ruling of 2026-10-25 (bloque-ledgent#287): the lane and anchor keys move to Vault Transit, and so to Ed25519 accounts that sign standard transactions. No `SPEC.md` change: `NFR-8` is met more strictly (no key ever reaches a process), and `REQ-SA-9`'s confirmation applies to the new form unchanged |

## Goal

The adapter's lanes and its anchor key are **native accounts**: each an Ed25519 key acting as its own
account on Kreivo, whose key is made in Vault Transit and never leaves it. Each batch and each anchor is a
**v4 signed transaction** from that account. The community PASS account (`F-027` T-027-03, the owner's ruling
of 2026-09-24) was an owner's choice, not a chain requirement: a native account already gives a signed
origin, which is all the settlement contract's key list, community membership and the usage allowance
ask for. Leaving PASS also removes the adapter's exposure to changes in the PASS authenticator's message
(amendment 0031: the message order that `kreivo-132` and `kreivo-133` differ in).

Why Ed25519: Vault Transit has no secp256k1, and a key that never leaves Vault is the reason to move.

## Design

**The key.** `F-042`'s Vault Transit engine, used by the adapter itself (S1 of `F-042`: "Transit opens for
the adapter's keys in the native-accounts feature"; D7: the signer service does not host Transit keys). The
adapter logs in to Vault under AWS IAM auth with its task's role, which Vault binds to the settle signer
role, whose policy may sign with `lane-1` and `anchor` and read their public halves, nothing else. A native
account's key may use the sidecar or Transit; never in process, never from the environment; a file in
development only. The version is pinned in the configuration: a new version is a new account.

**The account.** `MultiSigner::Ed25519(public).into_account()`: the public key's own 32 bytes. The contract
lists it (`set_keys`), it pays its fees, its nonce moves.

**The transaction.** Kreivo's `UncheckedExtrinsic` in its v4 signed form:

```text
compact(len) ‖ 0x84 ‖ MultiAddress::Id(account) ‖ MultiSignature::Ed25519(sig)
  ‖ None (PassAuthenticate) ‖ Era ‖ compact(nonce) ‖ compact(tip) ‖ None (fee asset) ‖ call
```

The key signs `SignedPayload` (`call ‖ explicit ‖ implicit` of every extension, hashed past 256 bytes),
exactly as `UncheckedExtrinsic::check` rebuilds it. `PassAuthenticate` with no credential passes a signed
origin through unchanged (`fc-pallet-pass`). The message order of the PASS authenticator plays no part, so
one encoding serves every runtime whose surface the adapter knows; the surface still selects the format
(`T-027-12`) and its extensions.

**Confirmation** (`REQ-SA-9`). The layer confirms the native form on each runtime before the first
transaction signed in it is broadcast, under a name of its own (`kreivo-133+native`): a runtime confirmed for
PASS transactions is not taken as confirmed for native ones, nor the reverse.

**One form per file.** `accounts` and `anchor_account` (native) or `keys` and `anchor_key` (PASS), never
both: a configuration that mixes them does not start. The PASS form stays, unchanged, while the zombienet
suite still provisions PASS accounts (`e2e/`, owned by other work); `T-044-06` removes it.

**Failure.** A key whose engine does not sign fails the step that needed the signature
(`ChainError::Signer`): nothing is stored or broadcast, the calls wait, and the next step asks again. It is
not the layer's unavailability, so it does not count toward the binary's restart after three minutes
without the layer. The Transit engine's own readiness (Vault down past its window, a key no longer what was
pinned) is logged when it changes; the binary has no readiness endpoint.

**Operators.** `aledgent-settle <config> keys` opens the keys and prints each lane's and the anchor key's
account and public key, and exits: no layer, store or ingress. It is the smoke test of the AWS IAM login to
Vault and of the keys' reads, and the source of the accounts `set_keys` lists.

## Trade-offs, stated plainly

- Every signature is a round trip to Vault: Vault is in the path of settlement, and its outage stops
  signing (not observing). Settlement's rate is low, and a stall only delays it.
- The adapter's own process, not only the sidecar, holds a Vault token that can sign with its keys. It could
  already ask the sidecar to sign anything; what changes is that the boundary that holds the key is Vault,
  whose audit log is the separate audit `NFR-8` asks for.
- Moving accounts is an on-chain change: `set_keys` with the new accounts (an admin call, a referendum of the
  admin community in production) and funding them for fees. Those are operations, outside this feature; the
  deployment's runbook carries them.

## Out of scope

- The on-chain cutover (`set_keys`, funding), and the deployment's configuration: the deployment's own
  repository.
- Removing the signer sidecar from the adapter's task: an infrastructure change once no key uses it.
- The core host's and ingress's keys on Transit (bloque-ledgent#287, items 6 and 7).
