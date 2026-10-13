<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# aledgent as a CI service container

`ghcr.io/bloque-app/aledgent-sidecar` runs aledgent next to another repository's tests: one
container, ready to serve as soon as it is healthy, with **no chain, no PostgreSQL, no Vault and no
cloud credentials**. It is ingress (F-009) and the in-process core in one process (`e2e/sidecar`,
the `aledgent-sidecar` binary), **book only**: there is no settlement adapter, so nothing enters or
leaves through the settlement layer. The book lives in memory and is gone when the container stops.

**Test only.** Its operator token and key-ring root are public constants of `e2e/sidecar`. They
exist so that a test suite can call it without a secret, which `REQ-AU-8` forbids any real
deployment: never run this image anywhere that holds value. The production images
(`aledgent-ingress` and the rest) carry none of it.

## Tags

Built and pushed by `.github/workflows/images.yml`, job `sidecar` (GitHub-hosted runners,
`linux/amd64`). It is on GHCR, unlike the deployment images (Amazon ECR in the test account), so
that another repository's Actions job can pull it with its own `GITHUB_TOKEN`:

| Tag | What |
|---|---|
| `edge` | the latest push to `main` (every merge) |
| `sha-<commit>` | that commit of `main`, or of a `v*` tag or a dispatch; pin this in CI |
| `v*` | a release tag |

Pull requests build and test the image but push nothing.

## What it seeds before it listens

It listens only once the book is founded and seeded, so "healthy" means "ready".

- **Assets** (all registered `stable`): `COPM/2`, `TSTA/2`, `TSTB/2`, `TSTC/6`. Amounts are integer
  minor units as decimal strings: `"150000"` COPM is 1 500.00 COPM; `"1000000"` TSTC is 1 TSTC.
- **The operator**, founded by genesis, the same identifiers on every start (`aledgent-sidecar
  describe` prints them):
  - recognised under the bearer token **`aledgent-sidecar-test-operator-token`** (test only), as a
    service principal;
  - one **issuance** grant per asset (`issue`, `redeem`), so it can issue into any account;
  - one **operator** grant over every account in delegated custody (`transfer`, `reserve`,
    `capture`, `release`, `convert`, `encumber`, `disencumber`, `close_account`), so it reaches the
    accounts it opens for its customers: reads their balances and moves value between them.
  - Every grant is bounded at 10^15 minor units a day (`REQ-AU-2`: none is unbounded).
- **Default deadlines** of one hour for reservations, transfers and redemptions.

- **Notification recipients**, only those `ALEDGENT_SIDECAR_RECIPIENTS` names (see
  [Notifications](#notifications)). With none, notifications are kept and never pushed.
- **Service principals**, those of a configuration (`ALEDGENT_SIDECAR_SERVICE_PRINCIPALS` or
  `ALEDGENT_SIDECAR_SERVICE_PRINCIPALS_FILE`), each **its own** principal under a name and a token
  of its own, with the grants of its role (see [Service principals](#service-principals)). By
  default, the sample configuration `e2e/sidecar/principals.json`: `issuer`, `custodian` and
  `onboarding`, each under its public test token.
- **System accounts**, only those `ALEDGENT_SIDECAR_SYSTEM_ACCOUNTS` names: each opened under the
  operator's identity, named by its role and granted, account-scoped, to the service principal that
  operates it.

Not available: withdrawals and deposits (no settlement adapter) and disclosures (no clerk key).

## Environment

| Variable | Default | What |
|---|---|---|
| `ALEDGENT_SIDECAR_LISTEN` | `0.0.0.0:8080` | where the HTTP surface listens; `health` probes the same port on `127.0.0.1` |
| `ALEDGENT_SIDECAR_ASSETS` | `COPM/2,TSTA/2,TSTB/2,TSTC/6` | the registry, `SYMBOL/precision` comma separated |
| `ALEDGENT_SIDECAR_EXTRA_TOKENS` | none | extra bearer tokens, comma separated. Each is recognised as the operator, the same principal and credential as the test token, which stays. A CI job gives its run a token of its own this way. Values are never logged or served: `/sidecar` shows only how many (`extra_tokens`) |
| `ALEDGENT_SIDECAR_MAX_LEGS` | `10` (the kernel's default) | the book's `max_legs`: the most legs one transfer submission may carry. One leg more is refused `ERR-BatchTooLarge` (HTTP 413) before anything is looked up. A whole number of at least 1; a command is also capped at 16 KiB, so a very large value is bounded by that. `/sidecar` shows it as `max_legs` |
| `ALEDGENT_SIDECAR_RECIPIENTS` | none | notification recipients to register at start, `;` separated, each `[PRINCIPAL=]ENDPOINT#SECRET_VAR` ([Notifications](#notifications)). `SECRET_VAR` is the **name** of another variable of the container holding the secret, never the secret. A variable that is not set fails the start. `/sidecar` shows `recipients` (how many) and `recipient_endpoints` (principal and endpoint of each), never a secret |
| `ALEDGENT_SIDECAR_SERVICE_PRINCIPALS` | the sample configuration | the service principals to open at start, as **inline JSON** ([Service principals](#service-principals)): each a name of the deployment's choosing, a role, its grants' parameters, and exactly one source of its bearer token: the **name** of another variable of the container holding it (`token_env`, never the token), or its public test token (`"test_token": true`). A variable that is not set, or that holds the operator's token or another principal's, fails the start, as does any fault in the configuration. Set but blank also fails the start: unset it for the sample. `{"principals": []}` opens none. `/sidecar` shows each principal, its role and its grants under `service_principals`, keyed by name, never a token |
| `ALEDGENT_SIDECAR_SERVICE_PRINCIPALS_FILE` | none | the same configuration, from a **file** of the container (mount it, or bake it into an image of your own). Setting both variables, or either one blank, fails the start; setting neither opens the sample configuration, `e2e/sidecar/principals.json` (embedded in the binary) |
| `ALEDGENT_SIDECAR_SYSTEM_ACCOUNTS` | none | system accounts to open at start, `;` separated, each `ROLE_NAME=PRINCIPAL` (`issuer/fee=issuer`): the account's role alias (lower case, `/` separated) and the name of the service principal granted it, which must be in the configuration. `/sidecar` shows them under `system_accounts` |
| `ALEDGENT_SIDECAR_RP_ID` | `localhost` | the passkeys' relying party, its `rpId`: a lower-case DNS name ([Passkeys](#passkeys)). `/sidecar` shows it under `passkeys` |
| `ALEDGENT_SIDECAR_RP_ORIGINS` | `http://localhost` | the origins the relying party accepts, comma separated, matched exactly (port included): `https://…`, or `http://localhost[:port]`. Anything else fails the start |
| `RUST_LOG` | `info` | log filter |

## Ports and commands

- `8080/tcp`: ingress's HTTP surface (`/v1/...`, `/metrics`, `/openapi.json`); every route needs
  the bearer token. The contract is `services/ingress/openapi.json`.
- `GET /sidecar` (no token): the founded identifiers and the registry, as JSON, for a job that
  needs the operator's identity (the owner of accounts it opens for itself):
  `curl -s localhost:8080/sidecar | jq -r .operator`.
- `aledgent-sidecar [serve]` (default), `aledgent-sidecar health` (exit 0 when `GET /metrics` as the
  operator answers 200), `aledgent-sidecar describe` (the founded identifiers, the token and the
  registry, as JSON).
- The image has its own `HEALTHCHECK` (exec form, every 2 s), so a `services:` entry needs no
  health options at all. A `--health-cmd` (Docker runs it through a shell) works too: the image is
  distroless's `debug` variant, with busybox as its shell.

## In a GitHub Actions job

The package must be readable by the calling repository: on GHCR, package settings, "Manage
Actions access", add the repository with the Read role (or make the package visible to the
organisation).

```yaml
jobs:
  e2e:
    runs-on: ubuntu-latest
    permissions:
      contents: read
      packages: read
    services:
      aledgent:
        image: ghcr.io/bloque-app/aledgent-sidecar:edge   # or :sha-<commit>
        credentials:
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}
        ports:
          - 8080:8080
        options: >-
          --health-cmd "aledgent-sidecar health"
          --health-interval 2s
          --health-timeout 3s
          --health-retries 30
    env:
      ALEDGENT_URL: http://localhost:8080          # a job in a container: http://aledgent:8080
      ALEDGENT_TOKEN: aledgent-sidecar-test-operator-token
    steps:
      - run: |
          curl -sf -H "Authorization: Bearer $ALEDGENT_TOKEN" "$ALEDGENT_URL/metrics" > /dev/null
```

## Notifications

A recipient registered at start gets its principal's notifications pushed to it, signed, as a real
deployment pushes them (`aledgent-ingress recipient --principal … --endpoint … --secret-env …`; the
sidecar registers through the same code). The syntax, items separated by `;`:

```text
[PRINCIPAL=]ENDPOINT#SECRET_VAR
```

- `PRINCIPAL`: `operator`, a service principal's name, or a `did:`. Left out, the **operator**,
  which is what every call under the test token or an extra token acts as. A name names that
  service principal, so it receives the notifications of the commands the service submits with its
  own token; the name must be in the [configuration](#service-principals), or the start fails. The
  store keeps one recipient per principal, so a principal named twice fails the start.
- `ENDPOINT`: an `http://` or `https://` URL, taken up to the last `#`. Do not put a credential in
  it: `/sidecar` serves it.
- `SECRET_VAR`: the name of an environment variable of the container
  (`[A-Za-z_][A-Za-z0-9_]*`). Its value's raw UTF-8 bytes are the HMAC key, the same as
  `--secret-env`, so the recipient must verify with the same string.

Each delivery is a `POST` of the notification's JSON with `aledgent-notification-id`,
`aledgent-attempt-time` (ms since the epoch) and `aledgent-signature: v1=<hex>`, where the hex is
`HMAC-SHA256(secret, …)` over the domain `aledgent/notification/v1`, the id, the time and the body,
each length-prefixed (`services/ingress/src/delivery.rs`, `ReferenceVerifier` is the reference
check). A command sent with a `reference` has it in every one of its notifications, inside the signed
body. A `2xx` answer records the delivery; anything else is retried with backoff.

### A service under test in CI

Say the service under test takes its notifications at `<API_URL>/hooks/aledgent` and verifies them
with `NOTIFICATION_SECRET`. Give the sidecar the same value under the same name, and point the
recipient at the API **as the sidecar container sees it**, which is not `localhost` (inside the
container, that is the sidecar itself):

- **Tests run on the runner** (no `container:` on the job), the API listening on the runner: the
  sidecar reaches the host as `host.docker.internal`. On Linux (GitHub's `ubuntu-*` runners) Docker
  does not define that name by itself; add it with `--add-host=host.docker.internal:host-gateway`
  in the service's `options`. The API must listen on `0.0.0.0` (or the Docker bridge), not only on
  `127.0.0.1`, or the connection from the container is refused.
- **The API runs as another service container** of the job: use its service name as the host
  (`http://api:3000/...`); service containers share one network and resolve each other by name.
- On Docker Desktop (macOS, Windows), `host.docker.internal` already exists; no `--add-host`.

```yaml
    services:
      aledgent:
        image: ghcr.io/bloque-app/aledgent-sidecar:sha-<commit>
        credentials:
          username: ${{ github.actor }}
          password: ${{ secrets.GITHUB_TOKEN }}
        ports:
          - 8080:8080
        env:
          NOTIFICATION_SECRET: ci-only-notification-secret
          ALEDGENT_SIDECAR_RECIPIENTS: >-
            operator=http://host.docker.internal:3000/hooks/aledgent#NOTIFICATION_SECRET
        options: >-
          --add-host=host.docker.internal:host-gateway
    env:
      API_URL: http://localhost:3000                       # the tests, on the runner
      NOTIFICATION_SECRET: ci-only-notification-secret
```

A service that submits under its own principal registers under its name instead,
`custodian=http://host.docker.internal:3000/hooks/aledgent#NOTIFICATION_SECRET`.

Check what was registered with `curl -s localhost:8080/sidecar | jq '.recipients, .recipient_endpoints'`.
A recipient that cannot be reached does not fail the start: its deliveries are retried, and the
operator sees them in `/metrics` (`aledgent_ingress_delivery_failures_total`) and in
`GET /v1/notifications/undelivered`.

## Service principals

An integrating party's services may each act as **their own principal, with grants from the
operator, so that no service holds the operator's token** (`REQ-AU-7`: "the operator acted" is told
from "the holder acted"). The sidecar opens each principal of its configuration as an organisation
identity, recognises it under its own token, as a service principal, and grants it what its
**role** needs. The names are the deployment's; the roles are `SPEC.md`'s:

| Role | Grants made at start | What it does with them |
|---|---|---|
| `issuance_authority` | one **issuance** grant per asset (`REQ-AS-6`, `REQ-AU-10`): every asset of the registry, or those `assets` lists; `issue` and `redeem`, or those `ops` lists. Plus one **delegated**, account-scoped grant (`transfer`, `reserve`, `capture`, `release`) on each system account `ALEDGENT_SIDECAR_SYSTEM_ACCOUNTS` gives it | issues into and redeems from the book's assets (book-only here); acts on its own system accounts, found by role (`GET /v1/aliases?source=system-role&name=issuer/fee`) |
| `operator_authority` | one **operator** grant over every account in delegated custody, by kind (`REQ-AU-5`): `transfer`, `reserve`, `capture`, `release`, or those `ops` lists (of the operator's own kinds) | moves customers' value: transfers and batches (a transfer of several legs, up to `max_legs`), reservations and their capture or release |
| `identities_principal` | the **identities principal's role**, `open_identity`, on the operator surface (`REQ-ID-6`, amendment 0025). At most one per configuration: the role has one holder at a time | opens identities, and accounts for any identity (the customer's default account); grants the operator-authority principal `open_account` over each identity it opens; registers a customer's passkey into the customer's set, acting for it ([Passkeys](#passkeys)) |

### The configuration

JSON, inline in `ALEDGENT_SIDECAR_SERVICE_PRINCIPALS` or in a file named by
`ALEDGENT_SIDECAR_SERVICE_PRINCIPALS_FILE` (one or the other; a variable set but blank fails the
start, so an unresolved CI expression never opens the sample by accident). Neither set, the sidecar
opens the sample, `e2e/sidecar/principals.json`:

```json
{
  "principals": [
    { "name": "issuer", "role": "issuance_authority", "test_token": true },
    {
      "name": "custodian",
      "role": "operator_authority",
      "test_token": true,
      "ops": ["transfer", "reserve", "capture", "release"]
    },
    { "name": "onboarding", "role": "identities_principal", "test_token": true }
  ]
}
```

| Field | Roles | What |
|---|---|---|
| `name` | all, required | the principal's name: `[a-z][a-z0-9_-]*`, at most 64 bytes, not `operator`, once per configuration. It keys `/sidecar`'s `service_principals`, and names the principal in `ALEDGENT_SIDECAR_RECIPIENTS` and `ALEDGENT_SIDECAR_SYSTEM_ACCOUNTS` |
| `role` | all, required | `issuance_authority`, `operator_authority` or `identities_principal` |
| `token_env` | all; this or `test_token` | the **name** of the container variable holding the principal's bearer token (`[A-Za-z_][A-Za-z0-9_]*`), never the token |
| `test_token` | all; this or `token_env` | `true`: the principal is recognised under its public test token, `aledgent-sidecar-test-<name>-token` (`aledgent-sidecar-test-issuer-token` for the sample's `issuer`), the same exception as the operator's test token. An explicit opt-in, never a fallback: a principal that names neither, or both, fails the start (`REQ-AU-8`) |
| `ops` | `issuance_authority` (of `issue`, `redeem`), `operator_authority` (of `transfer`, `reserve`, `capture`, `release`, `convert`, `encumber`, `disencumber`, `close_account`) | the kinds its grants name; not empty, none twice |
| `assets` | `issuance_authority` | the assets it holds issuance authority over, each in the registry; left out, every one |
| `per_day_limit` | `issuance_authority`, `operator_authority` | each grant's bound, minor units a day, a decimal string from 1 to 18446744073709551615 (the core's bound) with no sign or leading zero; left out, 10^15 (`REQ-AU-2`: none is unbounded) |

A field the role does not take, an unknown field, a field given twice in one object, a name twice,
a second identities principal or a malformed value fails the start with an error naming the
principal and the field. No error repeats a token or a `token_env` value (an error may quote the
configuration's other values: names, roles, kinds). `{"principals": []}` opens none.

The operator itself keeps all it had. Two grants are made per customer, not at start, because they
name the customer:

- **The operator-authority principal opens a customer's further accounts** (a `virtual`) under a
  delegated grant over the customer's **identity** that names `open_account` (amendment 0025). The
  identities principal makes it, right after it opens the identity:

  ```sh
  curl -sf -H "Authorization: Bearer $ONBOARDING_TOKEN" -H "$J" $B/v1/grants -d '{"kind":"delegated",
    "principal":"'"$CUSTODIAN"'","subject":{"identity":"'"$USER_ID"'"},
    "ops":["open_account","transfer","reserve","capture","release"],
    "per_window":{"length":86400000,"limit":"1000000"}}'
  ```

  It opens customer accounts in delegated custody for that identity, and reaches none by itself. The
  other kinds it names are what the holder may delegate over the accounts it opens.
- **The opener grants the issuance-authority principal its per-account grant** (ruling
  `a0025-q4`). The identities principal does it over the default account (no `parent`). The
  operator-authority principal does it over each `virtual`, under its grant over the identity,
  which caps it: `"parent": "<that grant's id>"`, and the effective grant is never wider in kinds
  or bounds. Without one, the issuance-authority principal is refused on a customer's account;
  issuing into it needs none.
- **A justification** for an identity or account the identities principal opens goes in the
  pass-through `reference` (`{"registration":"reg-42"}`); it comes back in the reply (`a0025-q2`).
- **One holder of the role:** granting `open_identity` to another principal revokes it from the
  configured identities principal.

The service principals' ids (identity and credential) are the same on every start with the same
configuration, as the operator's are: each is opened under an operation reference derived from the
genesis and its **name** alone (the operator's command under the client key
`sidecar-service-<name>`), not one minted at random. `aledgent-sidecar describe` prints them. A
recipient for a service is registered by its name, `custodian=http://…#SECRET_VAR`, or by its
`did:`. Credits to the operator's principal still go to the recipient registered for `operator`
(ruling `prq-05-notify`).

System accounts are owned by the operator, and a grant over every delegated account never reaches an
account the operator owns (ruling `au4-q2`, amendment 0025 R3): the operator-authority principal
cannot move another service's system accounts; only the account-scoped grant can.

### What a CI gate can assert

`/sidecar` answers, for each configured name, `service_principals.<name>` = `{ role, principal,
credential, grants }`. For each service, with **its own** token, never the operator's (here the
sample's names and test tokens):

```sh
S=$(curl -sf $B/sidecar)
for name in issuer custodian onboarding; do
  tok="aledgent-sidecar-test-$name-token"
  me=$(curl -sf -H "Authorization: Bearer $tok" $B/v1/principals/self/answering | jq -r .principal)
  test "$me" = "$(jq -r --arg n "$name" '.service_principals[$n].principal' <<<"$S")"  # its own
  test "$me" != "$(jq -r .operator <<<"$S")"                                         # not the operator
done
# issuer: an issuance grant over every asset of the registry
test "$(jq '[.service_principals.issuer.grants[] | select(.kind=="issuance") | .subject.asset] | sort' <<<"$S")" \
   = "$(jq '[.assets[] | split("/")[0]] | sort' <<<"$S")"
# custodian: one operator grant over delegated accounts: transfer, reserve, capture, release
jq -e '.service_principals.custodian.grants | length == 1 and .[0].kind == "operator"
       and .[0].subject == "delegated_accounts"
       and (.[0].ops | sort) == ["capture","release","reserve","transfer"]' <<<"$S"
# onboarding: the identities principal's role
jq -e '.service_principals.onboarding.grants[0].kinds == ["open_identity"]' <<<"$S"
```

Then, optionally, one call each inside its grants and one outside (each `403` or `404`): the
identities principal opens an identity (`POST /v1/identities`), the operator-authority principal
opens nothing for it before the identities principal's grant, the issuer issues, and the custodian
cannot issue. `e2e/sidecar/tests/sidecar.rs` runs all of these, and an arbitrary name set
(`an_arbitrary_set_of_names_and_grant_parameters_is_opened_as_configured`).

A job that names its own principals and tokens:

```yaml
    services:
      aledgent:
        image: ghcr.io/bloque-app/aledgent-sidecar:sha-<commit>
        env:
          ISSUER_TOKEN: ci-issuer-token
          CUSTODIAN_TOKEN: ci-custodian-token
          ONBOARDING_TOKEN: ci-onboarding-token
          ALEDGENT_SIDECAR_SERVICE_PRINCIPALS: >-
            {"principals": [
            {"name": "issuer", "role": "issuance_authority", "token_env": "ISSUER_TOKEN"},
            {"name": "custodian", "role": "operator_authority", "token_env": "CUSTODIAN_TOKEN"},
            {"name": "onboarding", "role": "identities_principal", "token_env": "ONBOARDING_TOKEN"}
            ]}
          ALEDGENT_SIDECAR_SYSTEM_ACCOUNTS: issuer/fee=issuer
    env:
      ISSUER_TOKEN: ci-issuer-token        # each service gets its own; none gets the operator's
      CUSTODIAN_TOKEN: ci-custodian-token
      ONBOARDING_TOKEN: ci-onboarding-token
```

The tokens are test values, like the operator's: never use one anywhere that holds value.

## Passkeys

The sidecar accepts passkeys (`F-028`) of one relying party, `ALEDGENT_SIDECAR_RP_ID` (default
`localhost`) with the origins of `ALEDGENT_SIDECAR_RP_ORIGINS` (default `http://localhost`), as
`aledgent-ingress` does with a `passkeys` configuration: user verification required, challenges live two
minutes, sessions fifteen. Challenges and sessions are MAC'd under a public test key of the crate
(`TEST_AUTHN_KEY`), like the operator's token.

A registration end to end:

1. `POST /v1/auth/challenge` `{"purpose":"register"}` (no token) answers `{"challenge": <base64url>}`.
2. The browser (or a software authenticator in CI) runs `navigator.credentials.create` with that
   challenge, `rp.id` = the relying party, from one of its origins.
3. `POST /v1/credentials` with `{"marker":"ordinary","scheme":"webauthn-es256","passkey":
   {"client_data_json":…,"attestation_object":…}}` (both base64url) registers it into the caller's own
   set; the reply's `result.credential` is the passkey's credential. For a customer whose identity
   the integrating party manages, the **identities principal** (`onboarding` in the sample) sends it
   with its own token and `on-behalf-of: <the customer's did>`: the passkey joins the customer's set,
   the identities principal only authorises it, and the customer's device still runs the ceremony
   (amendment 0025 R2, ruling `pk-register`). Not for an identity with an account in
   owned custody.
4. Sign-in: `POST /v1/auth/challenge` `{"purpose":"authenticate"}`, then `POST /v1/auth/passkey`
   `{"credential":…,"assertion":{"raw_id","authenticator_data","client_data_json","signature"}}`
   answers a session token, a bearer for that identity.

The origin the browser reports must be listed exactly: a page on `http://localhost:3000` needs
`ALEDGENT_SIDECAR_RP_ORIGINS=http://localhost:3000`.

## A first call

```sh
B=http://localhost:8080
T="Authorization: Bearer aledgent-sidecar-test-operator-token"
J="Content-Type: application/json"
id=$(curl -sf -H "$T" -H "$J" -H 'client-key: id-alice' $B/v1/identities \
       -d '{"kind":"person","verifier":"0a"}' | jq -r .result.identity)
acct=$(curl -sf -H "$T" -H "$J" -H 'client-key: acct-alice' $B/v1/accounts \
       -d "{\"owner\":\"$id\",\"kind\":\"customer\",\"custody\":\"delegated\"}" | jq -r .result.account)
curl -sf -H "$T" -H "$J" $B/v1/issuances \
  -d "{\"asset\":\"COPM\",\"destination\":\"$acct\",\"amount\":\"150000\"}"
curl -sf -H "$T" "$B/v1/balances?account=$acct&asset=COPM"   # .rows[0].available == "150000"
```

A transfer or a reservation is acknowledged at `Received` (`202`); repeat the request under the
same `client-key` (or query `/v1/operations/{reference}`) for its decision.

A transfer, a reservation, a withdrawal, an issuance or a redemption may carry `reference`: any one
JSON value of at most 1 KiB, the caller's own (an order id, a graph node, a movement's description).
aledgent keeps it exactly as sent, never reads it, and gives it back in the command's reply, in
`/v1/operations/{reference}`, in every notification of the operation and in its movements, to the
principal that sent it and its integrating party only. A capture or a release carries the
reservation's. Over 1 KiB, or not one value, the command is refused with `ERR-MalformedCommand`;
the same `client-key` with another `reference` is `ERR-ClientKeyReused` (`REQ-PO-5`).
