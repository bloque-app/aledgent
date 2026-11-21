<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# Amendment 0044 — The machine-readable API description is public

**Status:** **ruled by the owner on 2026-11-18** (`q17-openapi-public`). **Applied** to `SPEC.md` **v0.4.32** in
the same pass: `NFR-7` and one sentence of `REQ-AU-21`. The build follows it in `services/ingress`.
**Target:** `SPEC.md` v0.4.31 as on `develop` (at `e935c21`); amendment 0043 is the last on `develop`.
**Date:** 2026-11-18
**Number:** 0043 is the last on `develop`; no open pull request claims 0044.
**Amends:** `NFR-7` (one sentence added), `REQ-AU-21` (one clause). No command, query, error condition or
invariant changes, and no identifier is renumbered.
**Rationale in one line:** a client, a tool or a person who has no credential yet needs the description of the API to
learn how to ask for one, and the description holds nothing worth protecting.

---

## The problem

`NFR-7` says every route is covered by a default-deny mechanism, and the build kept it to the letter: the generated
OpenAPI document, served at `GET /openapi.json`, was refused without a credential like every other route. The
document is the same file that is committed in the repository and published with the documentation. Asking a caller
for a credential before it may read the description of how to present one helps nobody, and it stops generated
clients and the usual API tools from working without a secret.

## The change

`NFR-7` gains one sentence: the machine-readable description of the API (`GET /openapi.json`) is public. It describes
the routes, their bodies and the refusals, and carries no ledger fact, no deployment data and no credential. The
public route is `GET /openapi.json`, and `HEAD` with it, which a GET route answers; no other method or path.
**Nothing else becomes public because of it.** Every other route, an unknown route and a wrong method included, stays
refused without a credential with `ERR-CredentialInvalid`, and a missing check on a new route still fails closed,
because the exception is one path, named in the requirement.

`REQ-AU-21`'s statement that every call is authenticated at the one component that receives commands and queries
gains the matching clause.

## What does not change

- Every route other than `GET /openapi.json` needs a credential, as before.
- `/ready` and `/live`, which are served beside the surface and carry no ledger fact, are not changed.
- The document's content is generated from the route table and the wire types, so it cannot name a deployment, a
  key or a book. Two statements in it change with this amendment: the `/openapi.json` operation carries
  `security: []` (it needs no bearer), and the description in `info` names it as public beside the two routes under
  `/v1/auth`.
