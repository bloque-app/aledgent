<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-035 — The key boundary: a signer service, and an authenticated core link

| | |
|---|---|
| Milestone | M-3 — Production V0 (the `M-3` exit: no key material in the book's process, none readable from a process environment, `NFR-8`; ruling 2026-09-24 as amended by `key-custody-opt-in-kms`, 2026-10-04: KMS is not required) |
| Track | B |
| Home | `services/signer` (new), `crates/ledger-kernel` (`keys`), `crates/core-binding` (`envelope`, `host`, `core`), `agents/core-host` (`serve`, `keys`, `driver`, `package`), `services/settle` (`signer`, `config`), `services/ingress` (`remote`, `config`) |
| Depends on | `F-005` (decisions 15–17), `F-016` (decision 24: the `KmsSigner` stub), `F-017` (the clerk key), `F-027` (PASS device keys) |
| Unblocks | The release checklist's key-boundary item, once the operator provisions the signer's Vault mount, IAM-auth roles and policies |
| Realises | Guardrail 13; `REQ-AR-3`, `NFR-8`, `REQ-SA-8`, `REQ-AU-8`; `DEC-3` and `DEC-12` unchanged (the core's keys, the clerk's signed assertions), moved out of the book's process |

## Goal

No process that holds the book holds a key. One service, `aledgent-signer`, holds every key and no book
state, and signs for the processes that do: the core host (the key ring's keys, signatures and blindings;
the clerk's disclosure signatures; the Agent's Authority receipts and runtime package) and the settlement
adapter (lane and anchor extrinsics). The link between ingress and the core host is authenticated.

## Spec traces

- `REQ-AR-3`, `NFR-8`: settlement-layer key material in a boundary apart from the book, auditable apart,
  never readable from a process environment, never in a log, URL or error.
- Guardrail 13 (and `PLAN.md`'s `M-3` exit): no key material in the book's process at all, the key-ring
  root and the clerk key included.
- `REQ-SA-8`: the adapter's credentials scoped to what it executes: it holds key identifiers, and is
  granted the lane and anchor keys only.
- `REQ-AU-8`: no credential in a log, URL, error or default; a deployment fails to start rather than fall
  back (the signer's configuration, the core host's `core_keys` and `core_link`, the adapter's
  `development_keys`).
- `REQ-RT-3`, `REQ-RT-6`, `REQ-RT-7`: the Agent's transition stays deterministic and its private inputs stay
  out of its state; they now carry the signer's answers, never a key.

## Design

1. **The key ring behind a custody** (`ledger_kernel::keys`). The book needs five answers about the root:
   the registrar's public key, an account's public key, a registrar signature, an account signature, and
   an entry's blinding. `KeyRequest` names them, `KeyAnswer` carries the reply, and `Custody` answers. A
   `KeyRing` is the root (`RootKeys`, tests and the signer itself) or any custody. The derivations are
   unchanged, so a book moved behind the signer is the same book, byte for byte.
2. **The Agent never sees a key** (`core_binding`). The host's replica runs over a recording custody; the
   driver hands the Agent, with each input, the answers its native run obtained (`Private::keys`) and the
   clerk's signatures (`Private::clerk`), not the root or the clerk key. The Agent's key ring answers from
   them alone, and panics (aborting the invocation) on a question its host did not answer.
3. **The signer** (`services/signer`). A Unix socket; peer credentials (user id) and a token per caller;
   grants per key; reload on `SIGHUP`, with a removed token or grant refused on the very next request.
   Backends (owner ruling `key-custody-opt-in-kms`): **Vault** by default, read by the signer itself at
   start after a login with Vault's AWS IAM auth (an STS request signed with its instance or task role, so
   no secret in its environment), KV v1 with values under `.data`; **AWS KMS** as an opt-in per key, for
   secp256k1 lane and anchor keys only; keys from a file for development only, refused unless declared.
   Every refusal and every non-key-ring signature is logged; key-ring answers are counted per caller.
4. **The callers.** The core host builds its keys from `core_keys` and refuses to start with a key in its
   environment; its clients fail closed (wait a minute, then stop) rather than make an answer up. The
   adapter signs through `ServiceSigner`; a file or environment key needs `"development_keys": true`.
5. **The core link** (`ingress::remote`). Every connection's first frame is a token whose SHA-256 the core
   host lists (`core_link`), compared in constant time; on a Unix socket the peer's user id must be listed
   too. A connection that fails gets nothing run. A TCP link must be loopback.

## Which backend for which key

See `NOTES.md` decisions 1 and 11. Every key rests in Vault: the
key-ring root, the clerk key and the Agent's two Ed25519 keys in `the core signer's secret`, the lane
and anchor keys in `the settle signer's secret`. A lane or anchor key may opt into AWS KMS
(`ECC_SECG_P256K1`); nothing else can. Holders' passkeys (P-256) are theirs, never the operator's, and out
of the signer's scope.

## Exit criterion

- Signing round trips for every scheme and backend through the signer; unauthenticated and ungranted
  callers refused; tokens and keys rotate online.
- The book runs (a deployment over the core Agent) with its keys in another process, and the book's
  process has loaded no root.
- The core link refuses an unauthenticated caller before it runs anything.
- The release docs describe the shape that goes live.

Infrastructure (the Vault mount, IAM-auth roles and policies, the node's metadata-service restriction, the
sidecar, the ECR repository, and KMS keys for opted-in lanes) is out of this repo.
