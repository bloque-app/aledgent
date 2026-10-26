<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-042 — Signature engines (Vault Transit first)

| | |
|---|---|
| Milestone | M-3 — Production V0 (a stronger form of the `M-3` exit's "no key material in the book's process", for the keys that can use it; nothing in it is on `M-3`'s critical path) |
| Track | B |
| Home | `services/signer` (the engine seam, beside `client`), `services/ingress`, `services/settle`, `agents/core-host`, `ops` (their key configuration, one step at a time), `deploy/templates`, `ops/runbooks/release/key-rotation.md`, `.github/workflows/ci.yml` |
| Depends on | `F-035` (the key boundary and its Vault login), `F-038` (ingress's feed key) |
| Unblocks | The settlement adapter's Ed25519 keys in Transit, in the feature that moves it to native accounts; AWS KMS and other providers as engines, without touching a component |
| Realises | `NFR-8`, `REQ-SA-8`, `REQ-AR-3`, `REQ-AU-8`, `REQ-RT-6` |
| Status | **Design, revised after the design review of 2026-10-25** (rulings on D1 to D7 recorded at the end; D6 changed). No `SPEC.md` change: `NFR-8` is unchanged, and a key that never leaves Vault meets it more strictly than a key read into a process. It is an amendment to the plan (`F-035`'s sentence on where keys rest, and this feature's row) |

## Goal

Every key a component signs with is reached through one common interface, a **signature engine**, and a
component's configuration says, key by key, which engine holds the key. The engines that exist at first:

- **in-process**: the key is in the memory of the process that uses it, read from a file. Supported for components other than the core host; never from the environment;
- **sidecar**: today's `aledgent-signer`, on its Unix socket;
- **Vault Transit**: an API provider. The key rests in Vault, never leaves it, and each signature is a call to Vault.

Later engines (AWS KMS used directly, an HSM, an operating system keystore) are added without changing a
component. The reason to do it now: the first ask was "put ingress's feed key in Transit". Doing that as one
more special case beside the signer would be the third key path in the tree (the signer's client, KMS inside
the signer, Transit), each with its own configuration and its own checks. One interface makes the third path
the same as the first two.

## The kinds of engine

| Kind | Where the key is | Built first? | Production? |
|---|---|---|---|
| **In-process** | In the memory of the process that signs, **read from a file** at start. Never from an environment variable: the engine has no such locator, and never will | Yes (it is the code behind the signer's own keys and the development key files) | **Yes, in a component whose keys are allowed to be of this kind** (see "Which engine kinds a key may use"), with the key from a file. **Never in the core host**: `NFR-8` bars key material in the book's process. The signer service's own use of it is what the signer is, and is unchanged |
| **Sidecar** | In another process on the same host: the signer service | Yes (today's `SignerClient` and its typed wrappers) | Yes. The default |
| **API provider** | In a service the component calls over the network: Vault Transit first; later AWS KMS, an HSM's API, others | Vault Transit only | Yes, for the schemes the provider has |
| **In-system** | In the operating system's keystore, or a local agent | No | Supported by the seam, **not recommended**: another process of the same user can often ask it to sign, and it has no audit of its own. Not built; it needs a reason and an owner's ruling |

## What an engine offers: schemes

A scheme is a kind of key and the question it answers. The four that exist (`signer::proto::Scheme`) and the
call each answers (`signer::proto::Op`):

| Scheme | The call it answers | Answer |
|---|---|---|
| `key_ring` | A kernel key request (registrar key, account key, signatures, blindings) | An encoded answer |
| `clerk` | `sign(bytes)`: CipherClerk's Schnorr signature over Ristretto | 64 bytes |
| `ed25519` | `sign(bytes)`: pure Ed25519 over the bytes | 64 bytes |
| `secp256k1` | `sign_digest(32 bytes)`: a recoverable ECDSA signature, `s` low | 65 bytes: `r ‖ s ‖ v` |

Every key also answers `public`. **Which engine offers which scheme** (an engine says so; a component never
guesses):

| Engine | `key_ring` | `clerk` | `ed25519` | `secp256k1` |
|---|---|---|---|---|
| In-process | yes | yes | yes | yes |
| Sidecar | yes | yes | yes | yes, for the keys the signer holds |
| Vault Transit | no | no | yes | no |
| AWS KMS (later) | no | no | only if the provider has it | yes: KMS signs a digest, and the engine makes `s` low and finds the recovery id, as the signer's KMS backend does today |

A **key ring** is not a signature at all but the core's key derivation, which is why only an engine that
holds a root secret in memory can offer it: the sidecar and in-process. A **clerk** signature is a scheme of
the CipherClerk library that no provider implements. So the core host, which needs both, qualifies for the
sidecar and in-process on schemes alone. Schemes are not the whole rule, though: in-process offers all four,
and Transit offers `ed25519`, which two of the core host's keys need. What keeps the core host's keys on the
sidecar is the second check, the **engine kinds a key may use**, next.

## Which engine kinds a key may use

Schemes say what an engine can do. They do not say where a key may rest. So each component also declares,
for each key, **which kinds of engine that key may use**, and the config check enforces it. A key is
accepted only if the engine's scheme list has the key's scheme **and** the engine's kind is allowed for that
key.

| Component | Key | Scheme | Allowed kinds | Why |
|---|---|---|---|---|
| Core host (and `opsctl restore-core`) | `key_ring` | `key_ring` | sidecar | Only the sidecar and in-process offer the scheme, and in-process is barred below |
| | `clerk` | `clerk` | sidecar | The same |
| | `agent_authority` | `ed25519` | sidecar | The core host cannot reach Vault or AWS credentials (the node's metadata service is locked to it), so Transit is not reachable from it; and `NFR-8` bars key material in the book's process, so in-process is out |
| | `agent_package` | `ed25519` | sidecar | The same |
| Ingress | `feed` | `ed25519` | sidecar, Transit, in-process | The feed key signs what ingress forwards; ingress holds no book |
| Settlement adapter | its lane and anchor keys | `secp256k1` | sidecar (see S1 below) | Unchanged from today; nothing else offers the scheme yet |

The **cargo features stay as defence in depth** (the core host's build has no Transit engine and no Vault
client), but they are not the rule. The root workspace builds `ingress`, `ops` and `signer` in one command, so
their features unify, and `opsctl`, which reads `core_keys`, gets whatever ingress enables. The rule is the
check.

**S1, for the owner (does not block).** `NFR-8` says settlement-layer key material is held "in a boundary
that is separate from the book and can be audited separately". An in-process lane or anchor key in the
adapter has no audit apart from the adapter's. The table gives the adapter's keys the sidecar only, which is
today's behaviour and the stricter reading. If the owner rules that ruling D6 covers them, the table gains
"in-process" for those keys and nothing else changes. The signer service's own `file` key source stays
development only; this feature does not touch it.

## The interface

Two traits, small on purpose, because the same traits are what a later engine has to implement and what a
plugin would be held to (see "The seam for more engines").

```rust
/// One way of holding keys and signing with them.
pub trait Engine: Send + Sync {
    /// Its kind: in-process, in-system, sidecar, api provider.
    fn kind(&self) -> EngineKind;
    /// The schemes it can answer at all. A list per engine instance, so an engine for a provider can
    /// answer for what that provider has. What a given sidecar or plugin actually holds is known only
    /// at `open`.
    fn schemes(&self) -> &[Scheme];
    /// Find the key and check it now: that it exists, is of this scheme, and cannot leave. A
    /// component that needs a key does not start without it.
    fn open(&self, scheme: Scheme, locator: &serde_json::Value) -> Result<Arc<dyn KeyHandle>, ClientError>;
    /// Whether the engine can sign now, with no I/O. Ready unless said otherwise.
    fn health(&self) -> Health { Health::ready() }
    /// The engine's own metrics, as Prometheus text. None unless said otherwise.
    fn metrics_text(&self) -> String { String::new() }
}

/// One open key. There is no way to ask it for its secret.
pub trait KeyHandle: Send + Sync + Debug {
    /// The kind of engine it came from (the patience rule below reads it).
    fn kind(&self) -> EngineKind;
    fn scheme(&self) -> Scheme;
    /// The public half, in the scheme's encoding (32 bytes for Ed25519, 33 for secp256k1, ...).
    fn public(&self) -> Vec<u8>;
    /// The same call the signer's wire protocol carries: bytes in, bytes out.
    fn call(&self, op: &Op) -> Result<Vec<u8>, ClientError>;
}
```

What the configuration check needs *before* anything is connected is not on `Engine` (an engine is
connected to the network when it is built): each kind has a static description, `KindInfo`, which the
registry holds: its name in the file (`"sidecar"`, `"in_process"`, `"vault_transit"`), its kind, the
schemes it can offer, and three functions: check the engine's own configuration, check a key's locator,
and connect. A component's needs are `KeyNeed { name, scheme, kinds }`, with the name borrowed so that a
component can name keys at run time (the adapter's lanes).

The decisions in it, and the reason for each (D1 to D5 in "Decisions for the reviewer"):

- **Blocking, not async.** Every caller is blocking today: `Custody::answer`, `ClerkSigner::sign`,
  `FeedSigner::sign`, the adapter's signer, the core host's Agent thread. The sidecar's client is blocking
  I/O on a Unix socket. An engine that is async inside (Transit, KMS) keeps a thread of its own with a small
  runtime and the caller waits on it, so a caller on an async runtime's blocking pool, or off any runtime,
  signs the same way. The cost is a thread per API engine, not per request.
- **One call, the existing `Op`.** `call(&Op)` is what the signer's wire protocol already carries
  (`Public`, `KeyRing`, `Sign`, `SignDigest`), bytes in and bytes out. It keeps the trait four
  operations wide however many schemes there are, and it means an engine can sit on the far side of a
  process boundary without a second description of its calls (the plugin seam relies on this). The typed
  wrappers below give callers the typed methods they have today.
- **The error is `ClientError`, as now**, with its `Display` made engine-neutral (it says "the signer is
  unreachable" and "the signer refused" today, which misleads when the engine is Vault). It already has `is_transient()`, which is what `Patience` uses to
  decide between waiting and stopping. An engine maps its failures: a provider that does not answer (network,
  timeout, 5xx, a lease running out) is `Refused(Unavailable)`, and so transient; a signature that does not
  verify, a version that is not the pinned one, a key that became exportable are `Protocol`, and not. No new
  error type, and no caller changes how it handles one.
- **No secret crosses it.** `KeyHandle` has no export, no `Debug` that shows more than the identifier, and
  every engine verifies what a provider returns against the public key it read, before it hands it on.
- **An API engine's handle is used through `try_*` only.** `Patience::PRODUCTION` retries a transient error
  for a minute and then stops the process (`exit(70)`), which is right for a sidecar and wrong for a key
  in Vault: a failed request must stay a failed request, and a retry loop around the engine's own bounded
  retries would multiply the calls to Vault during an outage. `from_key(handle, patience)` refuses a handle
  of an API-provider kind together with a patience that stops the process (`OnFailure::Exit`); ingress
  signs with `try_sign` and is unaffected.
- **`health` and `metrics_text` are optional** and cost nothing to an engine that has nothing to say. A
  component that serves `/ready` and `/metrics` (ingress does) asks each engine it uses.

### Typed wrappers: how today's callers move without changing

The wrappers stay, with their names, their constructors and their behaviour, and sit on `KeyHandle`:

| Today | After |
|---|---|
| `RemoteEd25519::new(client, id, patience)`, `public()`, `try_sign()`, `sign()` | Holds an `Arc<dyn KeyHandle>` of scheme `ed25519` and a `Patience`. The old constructor builds the handle from the sidecar engine; a new `RemoteEd25519::from_key(handle, patience)` takes any. Same signature check against the public key. The core host's `AgentSigner` impl and ingress's `FeedSigner` impl are untouched |
| `RemoteClerk` (`ClerkSigner`) | The same, scheme `clerk` |
| `RemoteCustody` (`Custody`) | The same, scheme `key_ring`. It still caches the public answers |
| `RemoteSecp256k1`, used by the adapter's `signer.rs` | The same, scheme `secp256k1`, with its recovery check |
| `SignerClient` | Becomes the sidecar engine's connection. Its public API stays |
| The signer's KMS backend (`Backend::Secp256k1Kms`, `Kms`, `AwsKms`) | Unchanged in the signer. Wrapped later as the KMS engine, so that a component can use it without the sidecar; the signer's own use of it is not touched |
| The signer's own `Key` (`Key::answer(&Op)`, which is async) | The in-process engine reuses `Key::from_secret`'s in-memory backends on a synchronous path. Hosting any engine's handles inside the signer is later, and needs `spawn_blocking` around a blocking handle (see "What the signer service becomes") |

The first pull request changes where these types get their key from and nothing about what they do; every
existing test of them (`services/signer/tests`, the adapter's `signer_service`, the core host's) passes
unchanged, which is the proof.

## How a component says which engine holds which key

A component **declares what it needs** in code: a name and a scheme for each key.

| Component | Keys it needs | Schemes |
|---|---|---|
| Core host (and `opsctl` restore and drill) | `key_ring`, `clerk` (optional), `agent_authority`, `agent_package` | `key_ring`, `clerk`, `ed25519`, `ed25519` |
| Ingress | `feed` (when it forwards to a core in another process) | `ed25519` |
| Settlement adapter | its lane and anchor keys | `secp256k1` now; `ed25519` after its move to native accounts |

Its configuration then has two parts, written once per file: the **engines** it uses, with their connection
details, and the **keys**, each naming an engine and the engine's own locator for the key.

```json
{
  "engines": {
    "local-signer": { "sidecar": { "socket": "/run/aledgent-signer/signer.sock",
                                   "client": "ingress", "token_file": "/etc/aledgent/ingress-signer.token" } },
    "vault":        { "vault_transit": { "vault": { "address": "https://vault.example.com",
                                          "auth": { "aws_iam": { "mount": "aws", "role": "feed-signer",
                                                                 "region": "us-east-1", "server_id": "vault.example.com" } } },
                                         "mount": "transit" } }
  },
  "keys": {
    "feed": { "engine": "vault", "key": { "name": "lane-1", "version": 1 } }
  }
}
```

The same key on the sidecar would be `"feed": { "engine": "local-signer", "key": { "id": "ingress-feed" } }`; in
process, `"feed": { "engine": "files", "key": { "file": "/etc/aledgent/feed.key" } }` (an engine entry
`"files": { "in_process": {} }`). The in-process locator is `file` and nothing else: an `env` field is an
unknown field and refused, in every profile.

**One file serves several components, and each checks only its own keys.** The core host reads the same
deployment file as ingress. So a component's check:

- reads only the entries of `keys` that it declares, and builds and validates only the engines those keys
  name;
- keeps every other entry of `engines` and `keys` as unparsed JSON, **neither parsed nor refused**: ingress's
  `feed` on Transit in a file the core host loads is not an error, and neither is an engine kind the core
  host's build lacks, as long as none of the core host's own keys names it;
- refuses a key it declares and the file does not give. A key the file gives that this component does not
  declare is another component's, and is ignored (a misspelt name is therefore found as a missing key).

**The config check** (offline, at load, so a bad file does not start) refuses, for the component's own keys:

1. a key that is not given;
2. an engine name that is not declared, or declared with a kind this build does not have, **when one of the
   component's own keys names it**;
3. **a key whose scheme the chosen engine does not offer**: `keys.clerk: the engine "vault" (vault_transit)
   offers ed25519 only; clerk needs the sidecar`;
4. **a key whose engine kind is not allowed for it** (previous section): `keys.agent_authority: the engine
   "vault" is of kind vault_transit; this key may use: sidecar`;
5. a locator the kind rejects (`KindInfo::check_locator`): for Transit, a missing or zero `version`, a mount
   or key name with `..` or `?`; for in-process, anything but `file`; for a sidecar, an empty id;
6. the rules of the engine's connection: a Vault address that is not `https://`, a Vault token in a file, or
   a missing STS `region`, outside the development profile.

Capability has two levels. The engine's `schemes()` is what the engine can do at all (the sidecar's list is
all four); what a given signer actually holds, or what a given Vault has, is known only when the key is
opened. The offline check cannot see the second; the start does (next).

**The start** (online) calls `open` for every one of the component's own keys, so a key that is absent, of
another type, or exportable stops the process before it serves.

**The configuration written today keeps working.** The adapter names an engine key with
`{ "engine": "<key>" }` as a lane's `device`, beside `{ "signer": { "key": … } }`. `core_keys.signer` with `key_ring`, `clerk`, `agent_*`
and `stream.feed_key.signer` with `key` are read as a sidecar engine with those keys, for as long as a
release says, so nobody has to rewrite a file to upgrade. A file that gives both the old and the new form for
one component is refused. Each component moves to the new form in its own step (see the tasks).

### Known gaps against NFR-8: keys read from the environment today

`NFR-8` says key material is never readable from a process environment. Three places still read it, none of
them through the interface, and **this feature does not change them**. They stay outside the engine model
(the in-process engine has no environment locator, so moving a component onto an engine never carries an
environment source along), and each has a follow-up task so that none is forgotten:

| Where | What it reads | Today's guard | Follow-up |
|---|---|---|---|
| The settlement adapter (`KeySource::Env`, `DevSigner::from_env`) | A lane or anchor key from a named variable | Refused unless `development_keys` | `T-042-09`: remove `Env`; a development key comes from a file |
| Ingress's in-process core | The key-ring root and the clerk key from `ALEDGENT_CORE_ROOT` and `ALEDGENT_CLERK_KEY` | A warning at start, "development only" | `T-042-10`: refuse them outside the development profile; the core host is the production path |
| `opsctl restore-core`, and its fallback when there is no `core_keys` | The same two variables | None beyond the operator's own use | `T-042-11`: take the keys from `core_keys` (the sidecar) or a file, and drop the fallback |

## Vault Transit as an engine

Scheme: `ed25519` only. This is what the prototype already proved on mock servers, to be re-landed on the
interface above.

- **The locator is `name` and `version`; the engine is the mount and the Vault.** The version is required and
  never zero: there is no "latest". A new version is a new public key, and a rotation is a decision made in
  the configuration. A file with no version, or version zero, does not load.
- **At open**, with the version pinned: read `<mount>/keys/<name>` and check that the type is `ed25519`; the
  key is **not exportable**; it is **not derived** (a derived key's public half depends on a context);
  `allow_plaintext_backup` is false; it supports signing; version `<version>` exists and is not below
  `min_encryption_version` or `min_available_version`. Take that version's public key. Anything else
  refuses the start with the reason in words.
- **`exportable` can be turned on later.** Vault does not let it go from false to true at creation, but
  `keys/<name>/config` can set it afterwards (and never back). That is why the heartbeat reads the key
  again. No routine policy grants `<mount>/keys/<name>/config`, `<mount>/export/*` or `<mount>/backup/*`.
- **To sign**: `POST <mount>/sign/<name>` with `{"input": base64(payload), "key_version": <version>}`, and no
  `prehashed` and no `hash_algorithm`, so Vault signs the bytes as they are, pure Ed25519. The answer is
  `vault:v<N>:<base64>`: the engine checks that `N` is the pinned version and **verifies the signature
  against the public key it read, before it returns it**. A Vault, or anything between, that gives a wrong
  signature or a wrong version is caught.
- **While idle**, a heartbeat every 15 seconds keeps the token fresh and reads the key again: one key read
  per key per period, and one login when the token is past two thirds of its lease. A key that became
  exportable, derived or backup-able, whose pinned version's public key changed, or whose pinned version
  fell below `min_encryption_version` or `min_available_version`, makes the engine not ready. Signatures are still verified, so
  none that is wrong gets out.
- **The signing body carries `input` and `key_version` and nothing else.** A test pins it: no `prehashed`,
  no `hash_algorithm`.
- **The payload, the signature and the token are never logged** at `info` or above, nor put in an error.
- **Failure ends the request, not the process.** On 5xx, 429 or a timeout: bounded retries with a doubling
  backoff (three attempts, each given up after two seconds, with pauses of 100 ms and then 200 ms), and the
  whole signature is bounded at eight seconds, inside the ten seconds the signer client allows. Then that
  one request fails with a clear error: a refusal that remains after the one new login (403) is
  `Refused(NotGranted)` and not transient; a 400 from Vault is `Protocol`; Vault not answering is
  `Refused(Unavailable)` and transient; a signature that does not verify, or of another version, is
  `Protocol`.
  The next request tries again. Ed25519 is deterministic, so a repeat is safe.

### The token's life (the Vault session)

The Vault roles issue short tokens (60 s, 120 s at most). A session object, shared by every user of Vault in
the crate, does this:

- **The AWS IAM login is written once.** The code that signs the STS request and logs in today
  (`signer/src/vault.rs`) moves into a module the signer's KV reads and the Transit engine both use. The
  signer's KV reads move onto the session; their tests are the proof that nothing changed.
- It keeps the token with its lease and logs in again at about **two thirds of the lease**, before sending a
  request that finds the token past that, and in the heartbeat while idle. If the renewal fails and the
  token still has life, the token is used and the renewal tried again soon.
- When Vault refuses the token (403: `permission denied` or `invalid token`), the request logs in and tries
  **once more**; a second 403 is the answer. A token issued less than a second ago is not replaced because
  Vault refused it: a policy that denies a request is not a reason to log in on every request.
- **One login at a time.** Requests that meet an expired token together wait for the login in flight and
  share its token. A login that failed is not tried again for a second by the requests queued behind it.
- **The lock covers the token and the login only.** Today's KV read holds its mutex across the whole HTTP
  read, which would put every signature one behind the other. The session keeps the token, its lease and a
  generation number under a short lock, copies the token out, and signs without any lock held; only a login
  takes the single-flight lock, and a request that finds the generation moved on uses the new token.

### Metrics and readiness

Integers only, on atomics, in the style of the signer's counters and of ingress's metrics (no floating point
in `services/`):

- the latency of each signature as a **histogram in microseconds**, retries and logins included;
- **failures by reason**: `unreachable`, `timeout`, `unavailable`, `denied`, `login`, `rejected`,
  `version_mismatch`, `bad_signature`, `malformed`;
- signatures, logins, logins after a refusal, and `vault_ready` (1 or 0).

**Readiness** is the engine's `health()`: not ready while Vault does not answer (the last request or
heartbeat was a network error, a timeout or a 5xx), or while the key is not what was pinned; ready again as
soon as Vault answers. A component that serves readiness reports it: ingress's `/ready` is 503 and its
`/metrics` carries the engine's text. The signer service gets **no new listener**: it serves no HTTP, and a
listener on the key boundary is an attack surface this feature does not need.

### Keeping the test-only login out of a release

The end-to-end test logs in to a dev-mode Vault, where the AWS login cannot run, through a constructor on the
session (`with_test_login`) that exists only under the cargo feature `test-vault-auth`. Three guards, and CI
proves each on every change:

1. **The crate does not compile with the feature unless debug assertions are on** (`compile_error!`), so no
   release profile builds it. CI: a release `cargo check` with the feature must fail with that message.
2. **No configuration can name it.** It is a constructor, not a login method in the file's vocabulary: a
   binary built with it, which only a debug build can be, still takes its login from the file alone.
3. **Nothing enables it but a command line.** The feature is in no `default`, no dependency's feature list
   and no dev-dependency. CI: the workspace's feature graph without dev-dependencies must not contain it, and
   the same query must find it when asked for it (so the check can fail).

The honest limit: a release profile with `debug-assertions = true` defeats (1), and a source change defeats
anything. The gate is against an accident, not against someone with write access to the repository.

## Testing

- **A common test suite for every engine**, run against each built-in: a round trip for each scheme the
  engine offers, checked against the public key; a scheme it lacks refused; an unknown key refused; failures
  classified as transient or not as documented; no secret in an error or in `Debug`. A later engine must
  pass the same suite, which is how it is accepted.
- **The existing tests** of the sidecar path, unchanged: `services/signer/tests/signer.rs` and
  `key_absent.rs`, the adapter's `signer_service.rs`, the core host's. They are the proof that moving the
  wrappers onto the interface changes nothing.
- **Transit against a mock Vault** (unit tests with an HTTP server in the test process): a good signature; a
  version mismatch; a bad signature from the server, caught by the local check; an expired token, a new
  login, and the request succeeding; twelve concurrent requests meeting one expired token, one login;
  renewal at two thirds of the lease; a 5xx, three attempts, the request failed and the process still signing
  afterwards; a denied request that does not log in again and again; a login Vault refuses; an exportable key,
  a key of another type and a missing version refused; the heartbeat noticing a key that changed. TTLs and
  backoff are values a test sets, so none of these sleep long; none uses a paused clock with real sockets.
- **Config check tests** per component: the scheme the engine lacks (the core host naming Transit for
  `key_ring` is refused); **the kind a key may not use** (Transit for `agent_authority` refused with the kind
  named, in-process for `key_ring` refused); a missing version; both old and new forms; an in-process locator
  with an `env` field refused in every profile; **the core host loads a file in which ingress's feed key is on
  Transit**, without parsing or refusing it.
- **The end-to-end CI job `vault transit (F-042)`**, against a real Vault:
  - The image is `public.ecr.aws/hashicorp/vault:2.0.3`, **pinned by digest**
    (`@sha256:a296a888b118615dc01d5f1a6846e6d4a7277946caaed5b447008fff5fe06b54`, the multi-architecture
    index; ECR Public, not Docker Hub, which rate-limited CI before, #220). A missing image fails the job; it
    does not change what the job tests.
  - Vault runs in **dev mode** (`server -dev -dev-listen-address=0.0.0.0:8200 -dev-root-token-id=…`); the job
    mounts Transit and creates an Ed25519 key with `exportable=false`, with `curl` (the runner has no
    `vault` command). The test adds a second version, an exportable key and an ECDSA key, and a policy that
    lets a role read and sign with the one key.
  - It proves: signatures that verify independently (with `ed25519-dalek`) and through Vault's own
    `transit/verify`, per pinned version; an exportable or non-Ed25519 key refused; a role's policy that
    reaches its own key and no other; a token renewed at two thirds of a short lease; a token **revoked in
    Vault** and met with a real 403, one new login, and a signature.
  - The login is the test-only one above. The job's last step runs the three release guards. The check on the
    feature graph uses `cargo tree -e normal,build,features` and never `--all-features` (the clippy and
    documentation jobs build with `--all-features`, in a debug build, which does compile the feature).
  - **The AWS IAM login is never run against a real Vault in CI**: dev mode cannot do it. It is covered by
    tests of the request it builds and by a mock. So a staging smoke test of the real login, with a real role,
    is required before a deployment uses Transit in production.
  - Without `ALEDGENT_TEST_VAULT` the test skips locally; `ALEDGENT_REQUIRE_VAULT_TEST=1`, which CI sets,
    makes a missing Vault a failure, as the S3 job does.
- **A prototype exists**, outside this pull request: the Transit client as a stand-alone key type, with 13
  mock-server tests and the end-to-end test, written against the earlier design. Its mock tests pass; its
  end-to-end test **has not yet run against a real Vault**. Its code is re-landed on the interface here, not
  merged as it is.

## The seam for more engines

What a new engine is, concretely: a type that implements `Engine` and `KeyHandle`, and one line that
registers it under a name (`"vault_transit"`, `"aws_kms"`) in the table the component builds its engines
from. The component's code does not change, because it only knows schemes and `KeyHandle`; its configuration
changes by naming the new engine's kind. What keeps that true:

- **Engines are looked up by kind name from a registry**, not named in component code. The built-ins register
  themselves behind cargo features (`in_process`, `sidecar`, `vault_transit`), so a build that lacks one
  refuses a file that names it (the core host's build has no Transit engine, as today it has no Vault
  client).
- **The trait is bytes in, bytes out** (`Op`, a public key in a documented encoding, text errors). Nothing in
  it is a Rust type that could not cross a process boundary.
- **The sidecar protocol is already a plugin protocol.** A program that speaks the signer's socket protocol
  (versioned by `proto::VERSION`, authenticated by peer credentials and a token) is an engine to every
  component through the sidecar engine, with no change here. For an engine that should not run in the
  component's process, this is the route, and the recommended one. Three caveats:
  1. The frames are `postcard`, which has little support outside Rust. So it is a Rust-first route today;
     "any language" needs a written frame and schema specification, which this feature does not write.
  2. `Scheme` is a closed enum. A new scheme is a change to the signer crate (not to components), and an old
     client that decodes a handshake listing a key of a variant it does not know fails the whole handshake.
     A new scheme is therefore introduced by granting it only to clients that know it, or by raising
     `proto::VERSION`; the first such change has to say which.
  3. Capability has the two levels described under the config check.
- **Loading code at run time** (a dynamic library, WebAssembly) is possible later on the same traits, and
  is **not built, not designed here, and not wanted for production keys without an owner's ruling**: a
  library loaded into the process that signs runs with that process's access. If it is ever built, it has
  to be in the open, versioned and reviewed like a built-in, and subject to the same common test suite.

## What the signer service becomes

The signer service already is a host of keys: its `Key` holds a scheme, a public half and answers an `Op`
(asynchronously), from a Vault KV field, a file, or AWS KMS. In a later step it hosts `KeyHandle`s from any
engine, calling a blocking handle through `spawn_blocking`, so a key
that rests in Transit can be served over the sidecar socket as well. That is **possible but not
recommended** (a network hop and a Vault role in the signer for no gain over a component using Transit
directly), and it is not in the first steps. The signer's configuration (`keys[].source`: `vault`, `kms`,
`file`) does not change in this feature.

## Trade-offs, stated plainly

- **Vault is in the path of every signature.** The sidecar's call is on a local socket; Transit's is a
  network round trip, a few milliseconds when Vault is near and an outage's worth when it is not. Vault
  down is the component down. Ingress signs the items of a feed one by one, so a batch of 32 is 32 round
  trips one after another. The cost lands on the decision latency (`NFR-16`), not on the acknowledgement
  (`NFR-1`: a command is acknowledged when it is durably taken in, before it is forwarded and signed).
  Feeds are signed again on every send, so a resend of N items costs N Transit calls. Transit takes a batch
  in one request (`batch_input`); using it is the first thing to do if the figure matters, and it is not done
  here. At the size of a deployment with a handful of users it does not matter. **Nothing is measured yet.** The CI job proves behaviour, not speed.
- **A component that uses Transit holds a Vault role.** Ingress then holds a credential that reaches one key:
  it may read it and sign with it, and nothing else. The signer's model (a token per caller, grants per
  key, revocation on the next request) is replaced, for that key, by Vault's: the role's policy, its audit
  log (which records each signature) and the token's lease.
- **A stronger key and a process no safer.** Transit protects the key: it cannot be copied out of Vault by
  ingress or by anyone. A compromised ingress can still ask Vault to sign anything while it runs, exactly
  as a compromised caller can ask the signer.
- **An interface has a price.** Four operations behind one call (`Op`) is less typed than four methods, and
  an engine that does not offer a scheme is found when the file loads and not by the compiler. The typed
  wrappers and the config check keep the callers' view as typed and as early as it is today. The price is
  one more layer between a component and the signer's socket, which the first pull request must show costs
  nothing a test can see.
- **Rotation is never automatic.** A new Transit version is a new public key; for a key that names an
  account on a chain, it is a new account. A pinned version signs until the file moves; `auto_rotate_period`
  stays off.
- **More ways to hold a key are more to operate.** The sidecar over Vault KV stays the default.

## Operator notes (for the pull request that adds the Transit engine)

- Create the key with `exportable=false` and `auto_rotate_period=0`. (`exportable` can still be turned on
  later through `keys/<name>/config`, and never back; the engine notices and reports not ready.) Do not
  rotate it unless a rotation is intended; if it is, the runbook is
  [key rotation](../../ops/runbooks/release/key-rotation.md), which gets a row for a Transit key.
- The role the component logs in with needs `read` on `<mount>/keys/<name>` and `update` on
  `<mount>/sign/<name>`, and nothing else on that mount. In particular no `keys/<name>/config`,
  `export/*` or `backup/*`.
- Pin the version in the file; it is required. The core host's `stream.ingress_keys` lists the public key of
  the pinned version (hex), as for the signer's feed key; `vault read transit/keys/<name>` shows it in
  base64.
- An example of a Transit key entry goes in `deploy/templates` as a file of its own: the existing templates
  are strict JSON that tests parse, and JSON holds no comment.

## Decisions, and the rulings (design review of 2026-10-25)

| | Question | Ruling |
|---|---|---|
| **D1** | Blocking or async interface? | **Blocking.** API engines keep a thread and a small runtime |
| **D2** | A new error type? | **No.** Reuse `ClientError` |
| **D3** | One call (`call(&Op)`) or a method per operation? | **One call**, with typed wrappers |
| **D4** | Engines declared once, or inline in each key? | **Declared once** |
| **D5** | Does the old configuration keep working? | **Yes**, read as a sidecar engine |
| **D6** | In-process keys in a component? | **Changed by the ruling:** a supported engine kind, not development only, for every component but the core host; the key comes from a file and never from the environment. `NFR-8` still applies, so the config check refuses an in-process key in the core host and any in-process key whose material would come from the environment. Today's environment reads are listed as known gaps, each with a follow-up |
| **D7** | Does the signer host Transit keys over its socket? | **Not now** |
| **D8** | (From the review.) Beyond schemes, which engine kinds may each key use? | Each component declares it, and the check enforces it. The core host's four keys: the sidecar only. Ingress's feed key: sidecar, Transit or in-process. The adapter's keys: the sidecar (S1 open for the owner) |
| **D9** | (From the review.) Does one file serve several components? | Yes. Each component checks only the keys it declares; other entries stay unparsed JSON |

## Out of scope

- The plugin loader. The seam is described above; nothing loads code.
- An operating system keystore engine, an HSM engine, AWS KMS used directly by a component (the interface
  makes them possible; each is a feature with its own owner's ruling).
- The settlement adapter's change to Ed25519 keys and native accounts (a feature of its own). Its
  secp256k1 path moves onto the interface in the first steps, unchanged in behaviour.
- Batching Transit signatures; measuring the cost.
- A listener on the signer for metrics or readiness.
- Removing the environment reads listed under "Known gaps against NFR-8" (follow-ups `T-042-09` to `T-042-11`).
- Any change to the signer's wire protocol or its configuration.
- Reading a key's material from any engine: there is no such call.
