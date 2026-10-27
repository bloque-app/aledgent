<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-045 — tasks

The design is the owner's ruling of 2026-10-25 (bloque-ledgent#287) on `F-042`'s S1. `T-045-01` to
`T-045-05` land in one pull request; `T-045-06` is a follow-up. All traces: `NFR-8`, `REQ-SA-8`, `REQ-SA-9`,
`REQ-AU-8`.

| ID | Task | Done when |
|---|---|---|
| T-045-01 | **The signer boundary takes two schemes.** `settle::signer::Signer` signs a message as `MultiSignature::verify` checks it (Ed25519 over the bytes, ECDSA over their blake2b-256) and gives a `Public` of either scheme; the account of an Ed25519 key is the key. `EngineEd25519` over any engine's Ed25519 key, through `try_sign` only; `DevEd25519` from a file. PASS devices unchanged in what they sign | Unit tests: an Ed25519 account is its public key and signs the bytes; ECDSA signatures still recover to the device; the PASS vector is unchanged; a native lane signs through the signer service |
| T-045-02 | **The native transaction.** v4 signed, `MultiAddress::Id`, `MultiSignature`, `PassAuthenticate(None)`, the signed payload hashed past 256 bytes; decoded for the fake layer; `sign` chooses the form by the signer. Pinned in `kreivo-pin/vectors-native.json` and checked by `kreivo-pin` with `sp-runtime`'s own types | Round trip and verification tests, both payload lengths; the vector test; `kreivo-pin` decodes the bytes as Kreivo's `UncheckedExtrinsic` and verifies the signature over the payload `check` builds; a read-only validation on Kreivo mainnet answers `Payment` (decoded, signature accepted) for a good signature and `BadProof` for a flipped one |
| T-045-03 | **The fake layer and the confirmation.** The fake layer authorises a native transaction by its signature over the signed payload and runs the call as its account; the runtime's confirmation is recorded under `<format>+native` | End-to-end tests: a withdrawal settles from a native account, its nonce moves, its runtime row says `+native`; anchors go out from the native anchor account |
| T-045-04 | **The configuration.** `accounts` and `anchor_account`, one form per file; a native account's key on the sidecar or Transit (`ed25519`), never in process or from the environment, a file in development only; Transit in the adapter's default build | Config tests: Transit and the sidecar accepted; in process, the environment, a mixed file, an anchor without lanes refused; a missing version refused; a PASS key on Transit still refused |
| T-045-05 | **The binary.** Native lanes and anchor; `keys` prints accounts and readiness and exits; a signer that does not sign fails the step as `ChainError::Signer`, not the layer's unavailability; the engines' readiness raises and clears the alert `signer:engines`, and `AledgentAdapterSignerNotReady` pages on it | A test: a signer that does not answer gives `ChainError::Signer`; a restart keeps the native confirmation; `ops`' alert test pages for kind `signer`; every crate that links `settle` (`e2e/aledgent-driver`, `ops`, `conformance`) builds with no edit to its code |
| T-045-06 | **Follow-up.** Remove the PASS form (`PassAccount`, `PassKey`, the v5 PASS transaction, `kreivo-pin-132`) once the zombienet suite provisions native accounts | No PASS code in `services/settle`; the zombienet suite green on native accounts |
