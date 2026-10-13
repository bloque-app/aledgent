<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0027 — A `register` challenge bound to one identity

**Status:** drafted and applied on `feat/bind-register-challenge` on 2026-10-09, at the owner's request, as
a hardening of amendment 0025's clause R2. Applied to `SPEC.md` **v0.4.20**.
**Target:** `SPEC.md` v0.4.18 as on `main` (amendment 0025 applied). Version v0.4.19 is amendment 0026's
(#215), so this one takes the spec to **v0.4.20**, whichever of the two lands first.
**Date:** 2026-10-09
**Rationale in one line:** a `register` challenge proves a ceremony is fresh but says nothing about whose set
the new credential joins, so under clause R2 a challenge obtained for one user can carry a credential into
another's.

---

## The problem

Ingress issues a WebAuthn challenge on request (`POST /v1/auth/challenge`). It is single-use, expires after
the deployment's challenge TTL (120 s by default), and is MAC'd over its nonce, its expiry and its purpose.
It names no identity. A `register` challenge requested while onboarding user A therefore serves a
registration into user B's set just as well, within its TTL.

Before amendment 0025 that did not matter: a registration went into the caller's own set, so the challenge
and the set had the same owner. Clause R2 lets the identities principal (the onboarding service) add a credential to any
identity it manages, acting for it. The ceremony then runs in one user's browser while the principal that
names the set is a service acting for many users. A challenge issued in A's flow and routed, by a fault in
the integrating party or by someone who intercepts it, into a call acting for B, puts A's passkey in B's set.

## `REQ-AU-20` (one sentence added)

After "… and to the identity it acted for (`REQ-AU-7`). *Amendment 0025, clause R2 (ruling `pk-register`).*":

> The challenge of a ceremony that adds a credential MAY name the identity whose set it is for; a proof over
> a challenge that names one identity MUST then be refused with `ERR-CredentialInvalid` for any other
> identity's set, and a challenge that names none serves any set. *Amendment 0027.*

Nothing else changes. Binding is optional: a caller that names no identity gets exactly the challenge it
got before, so no existing integration changes.

*Trade-off, stated plainly.* The binding is opt-in, so it protects only the integrating parties that ask
for it. Making it mandatory for registrations acting for another identity would close the gap for everyone,
at the cost of breaking every caller that does not yet send the identity. That can follow once the onboarding service sends
it; this amendment does not decide it.

## Carried to the code

- `POST /v1/auth/challenge` takes an optional `identity` (`did:aledgent:id:…`), for `register` only; with
  `authenticate`, or unparseable, the request is refused with `ERR-CredentialInvalid` (as every failure of
  that route is).
- A bound challenge is `nonce ‖ expires_at ‖ purpose ‖ SHA-256(identity) ‖ HMAC`, the digest inside the
  MAC, so it cannot be removed or swapped. An unbound one keeps its form and its MAC input.
- At registration, the set is the one the core will write: the identity in `on-behalf-of` for a principal
  that is not an agent (clause R2), the caller's own otherwise. A bound challenge for another set is refused
  before it is used, so a misrouted attempt does not burn the right user's challenge; it is still single-use.
- Tests: `amendment_0027_a_bound_challenge_admits_only_its_identity_and_cannot_be_unbound` (`authn`),
  `amendment_0027_a_bound_register_challenge_serves_only_its_identity` (ingress),
  `amendment_0027_a_challenge_bound_to_one_user_does_not_register_on_another` (sidecar, the onboarding service acting for
  two users).
