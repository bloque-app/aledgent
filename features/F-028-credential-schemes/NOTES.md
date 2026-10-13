<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-028 — implementation notes

Decisions taken during implementation where the plan left room, each the conservative reading.
Questions only the owner can settle are in `plan.md`, "Open questions for the owner".

## Decisions

1. **Where each check runs (`DEC-19`).**
   - **Ingress** verifies passkey assertions and registrations, and API keys (`crates/authn`).
   - **The core** checks a new verifier's shape wherever one joins a set: `open_identity`,
     `register_credential` and `initiate_recovery`. It still verifies `key` reclaim proofs itself.
   - **Reclaim by passkey.** Ingress verifies the assertion and logs
     `ReclaimCustodyCommand { passkey_checked: true }`. The core accepts that mark for a
     `webauthn-es256` credential only (`Proof::PasskeyChecked`). No caller's body can set the mark:
     `ReclaimCustodyBody` has no such field and refuses unknown fields.
2. **`key` verifiers are not shape-checked.** Every verifier written before `F-028` decodes as `key`,
   and many are not 32 bytes: tests, and the operator's genesis verifier. A malformed key still
   never verifies a proof.
3. **A malformed new verifier is `ERR-CredentialInvalid`**, "the credential presented is invalid".
   An unknown scheme name at ingress is refused the same way.
4. **An API key never proves a reclaim** (the owner, 2026-10-02). It is a bearer secret, not a
   holder's device.
5. **Sign counts are durable, per sign-in** (the owner, 2026-10-02).
   - **Where.** Each accepted sign-in raises the passkey's count in ingress's store
     (`passkey_counters`, migration 0002), in one statement that only ever raises it. Two replicas
     racing on one assertion cannot both win, and a count another replica accepted is refused
     everywhere.
   - **The floor** is the count at registration or import.
   - **Two counts of zero pass**, and nothing is written: synced passkeys always report zero, and
     passkeys on Kreivo mainnet do.
6. **Challenges and sessions.** Both are MAC'd under `ALEDGENT_INGRESS_AUTHN_KEY`, so any replica
   checks what another issued.
   - **A challenge is used once**, on every replica: its nonce is kept in ingress's store
     (`used_challenges`) until it expires.
   - **A session is a short-lived token derived per sign-in.** It serves any number of actions for
     15 min and is not renewed.
7. **Repeats of credential commands.** A credential command with work left before it is logged (a
   passkey registration, an API key, a passkey reclaim) is deduplicated on what the caller sent:
   the wire body before ingress fills it, plus a digest of the proof or registration.
   - Ingress looks the client key up first (`Store::find`, new). So a repeat gets the stored reply
     instead of re-verifying a single-use proof (`REQ-OP-10`).
   - An API key is shown on the reply that decides it. A repeat returns the credential without the
     key. A key lost to `ERR-DecisionUnavailable` is removed and issued again.
8. **The kind of a session or an API key.** It is `service` if a configured token names the same
   identity as a service principal, `agent` for an agent, and `holder` otherwise. `holder` and
   `agent` need the `DEP-7` switch.
9. **The two auth routes are in `ROUTES`.** The OpenAPI document lists them with `security: []`.
   Anything but a well-formed request this deployment accepts is `ERR-CredentialInvalid`, so the
   `NFR-7` test over every route holds unchanged. A deployment with no `passkeys` configuration
   refuses both.
10. **Export format 7.** A verifier is its scheme's tag, then its material. A tag this build does
    not know, or material not well formed for its scheme, does not decode.
11. **`approval_message` v2 binds the scheme.** `services/recovery`'s `ApprovalRequest` now carries
    a `Verifier`.
12. **Every person in the random book holds one credential of each scheme**
    (`conformance::tools::book`). They are registered through the person's own credential. So
    `AC-K5.2`'s export round trip covers all three.
13. **Import, not a several-credential opening** (the owner, 2026-10-02; amendment 0010).
    - **The operation.** `import_credentials` is an operator command with its own grant kind,
      `ImportCredentials`. Its records are audited as operator interventions.
    - **What it adds.** Each verifier joins the set marked `Imported`, at ordinals 0, 1, … of the
      operation.
    - **Refusals.** Any account of the identity in owned custody is `ERR-NotAuthorised` (`REQ-MG-8`).
      A malformed, held or repeated verifier is `ERR-CredentialInvalid`.
    - **What ingress adds.** It refuses a passkey whose `rpId` is not configured: the deployment's
      declaration of accepted credentials (`REQ-MG-7`).
    - **`open_identity`** takes one verifier again.
14. **One notice of an import per recipient.** A custody notice is one per operation, recipient and
    event kind (`NoticeId`). So an import shows each authority holder one `CredentialRegistered`,
    naming its first credential, and the operation's record lists them all. Changing the notice's
    identity to carry the credential would change every notice's identity.
15. **Relying parties are configuration** (`passkeys.relying_parties`), not code (the owner,
    2026-10-02).
    - **Closed parties.** A party with `registrations: false` takes sign-ins and imports only.
    - **Which party a registration uses.** One origin may serve two parties: `https://api.example.com`
      serves `example.com` and the legacy `api.example.com`. So a registration takes the party whose
      `rpId` hashes to the attestation's `rpIdHash`, among those open to registrations from its
      origin.
    - **The template** (`deploy/templates/ingress.json`) has the registrable domain open and the
      legacy exact host closed, and turns holder credentials on (`DEP-7`). It needs `ALEDGENT_INGRESS_AUTHN_KEY` set
      before it is deployed.
16. **Pass accounts are classified** (the owner, 2026-10-02).
    - **With a passkey:** an **identity**. Its first passkey is the primary credential that
      `open_identity` takes, and the others are imported.
    - **With only substrate keys:** a **ledger account** (the glossary's). It is input
      to the opening load (`REQ-RC-4`), and never a credential. `tools/pass-import` lists both,
      with the substrate keys of each ledger account.
17. **On the first deployment's path, the holder's passkey joins at terms acceptance** (owner rulings
    `tos-passkey-registration` and `tos-aledgent-only`, 2026-10-08; they correct an earlier reading,
    "no passkeys in aledgent").
    - **Identities open with the operator-held key credential** (owner ruling of 2026-10-08), so no
      identity is without a credential and `T-028-10` to `T-028-12` are not built. Their accounts are
      operable under the operator-delegated grant from the start, so no user is blocked.
    - **At terms acceptance**, where a user is recognised, the onboarding service registers the user's
      passkey as a credential on their aledgent identity: `register_credential` (`T-028-05`,
      `REQ-AU-20`), authorised by the identity's operator-held key credential. Registering later works
      the same way, since the credential set is open to a new member at any time.
    - **No debit before terms acceptance** is enforced by the integrating party, with its own terms
      gate. aledgent has no requirement, task or conformance case for it.
    - **No Kreivo `device_attestation` (Pass) is written** on this path. The aledgent credential is the
      compliance record. The earlier path is unchanged.
    - **Backlog**, a spike: card-network signatures as aledgent credentials, to attach a card purchase
      to an aledgent request without the operator's principal as intermediary.

## Crates

| Crate | Version | Why |
|---|---|---|
| `p256` | `=0.13.2` | The RustCrypto P-256 that Kreivo's own WebAuthn verifier uses |
| `coset` | `=0.3.8` | COSE keys, on `ciborium` 0.2.2. What `pass-authenticators` parses with |
| `@noble/curves` | `2.2.0` | The import tool's on-curve check and point recovery |
| `polkadot-api` | `2.1.8` | The import tool's chain reads |

The guest gains no crate: `agents/core/Cargo.lock` is unchanged. `agents/Cargo.lock` gains
`authn`'s dependencies on the host side only, with no version of an existing package changed.

## What the import tool handles

- **Devices.** `Pass.Devices` holds one device per account. Substrate-key devices are skipped.
  WebAuthn devices are all ES256, synced (BE and BS set) or device-bound; sign counts are 0.
- **Truncated keys.** Some keys are stored truncated: the field holds `0x6d01` (the compact length
  91), then the first 89 bytes of the SPKI. A client wrote the key as a length-prefixed byte string
  into the fixed 91-byte field.
  - Kreivo cannot verify these devices' assertions.
  - y follows from x up to its sign, so the tool **recovers** the point: of the two points with this
    x, it takes the one whose y begins with the 30 stored bytes. It marks them `recovered: true`.
- **Classification.** Every account is either an identity or a ledger account; none is left
  unclassified.
- **No registrar.** `Pass.RegisteredAccounts` may be empty, so the chain does not say which registrar,
  and so which `rpId`, a device belongs to. Pass `--accounts` with the operator's own users' Kreivo
  addresses, so that `--rp-id` is stated only for the operator's devices.

## Not run here

- **The agents workspace** (`agents/`). VOS at the pinned `de0f12f` does not compile on macOS
  (`journal_store.rs`, platform-gated items), so it is CI's (`ubuntu-latest`). Nothing under
  `agents/` changed but the host lockfile.
- **A real device's assertion.** No real passkey signed here. The assertion path is tested with a
  software authenticator over the same curve and encodings, and a real device's stored key is
  converted by the tool.
