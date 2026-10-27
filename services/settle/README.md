<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# aledgent-settle

The settlement adapter (`F-016`), and until VOS drains an Agent's outbox into a host extension, the
executor of the call outbox (`F-027`, `DEC-18`): the one component of aledgent that touches the
settlement layer. It holds the signing keys, apart from the core; it reaches the core only through
ingress, as the principal the settlement configuration names; it takes every external movement and
anchor from the core's call outbox as an envelope, executes them in batches on the settlement
contract (`F-026`) as the deployment's community PASS account, and reports what the layer recorded
as observations, never stating an outcome. `features/F-016-settlement-adapter/NOTES.md` and
`features/F-027-settlement-extension/NOTES.md` say what was built and why.

## Running it

```sh
cargo run -p settle --bin aledgent-settle -- settle.json    # or ALEDGENT_SETTLE_CONFIG=settle.json
```

`settle.json` is a `settle::config::ProcessConfig`. Keys are named by where they are, never by
value (the PASS account's user id and the contract's account below are examples):

```json
{
  "profile": "production",
  "adapter": {
    "holder": "settle-0",
    "genesis": "<the declared genesis hash, hex>",
    "instance": 1,
    "assets": [{ "asset": "USD", "chain": "Here(1)" }],
    "keys": [
      {
        "user": "5151515151515151515151515151515151515151515151515151515151515151",
        "device": { "file": "/run/secrets/lane-0.key" }
      }
    ],
    "fee_floor": 1000000000,
    "lease_ttl": 30000,
    "status_lease": 120000,
    "page": 32,
    "max_era": 64,
    "challenge_window": 256
  },
  "connection": "light-client",
  "chain_spec": "/etc/aledgent/kreivo.json",
  "relay_chain_spec": "/etc/aledgent/kusama.json",
  "deployments": [
    { "instance": 1, "contract": "c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0" }
  ],
  "contracts_pallet": "Contracts",
  "assets_pallet": "Assets",
  "ingress": "https://ingress.example.com",
  "ingress_token_file": "/run/secrets/settle-ingress.token",
  "database": "postgres://settle@db/settle?sslmode=require",
  "interval": 1000,
  "wake_poll": 250
}
```

`interval` is the longest the adapter goes without a step. Within it, every `wake_poll` milliseconds
(default 250; 0 turns it off) it checks for news: the finalized or best head moved, or a call arrived
in the outbox. News starts a step at once, so a withdrawal is signed as soon as the core puts it in the
outbox and observed final as soon as the light client has the finalized block, not up to an interval
later (`NFR-15`, T-032-12). A check reads two heads and the outbox; nothing is proved or dry-run.

Each entry of `adapter.assets` maps a book asset to its settlement-layer location, and carries **no
precision** (`DEC-20`, `F-029`, amendment 0011). Every step while serving, the adapter reads what Kreivo
states about each location (the asset's status, and its metadata's `symbol` and `decimals` fields; never
its name), at the finalized head through the light client, and attests it to the core
(`POST /v1/settlement/attestations`). A book-only asset's binding (`bind_asset`) reads the latest
attestation; once bound, the core ignores them. While a book-only asset could not be bound at its
location, the adapter keeps an `asset:<ASSET>` notice open saying why.

**Bindings** go out on the anchor key's lane: the call outbox's `bind` call becomes the contract's
`Bind { asset, amount }`, stored before it is broadcast (`settle_binds`), one extrinsic of the anchor key
in flight at a time, signed again only if it provably did not execute. The contract's `Bound` entry, or
its refusal (`HoldingMismatch`, `AlreadyBound`), is reported as an observation like any other.

**Secrets are named, never written here.** The ingress token comes from `ingress_token_file` or,
where a deployment hands its secrets over as environment variables (the canary: Vault, into the
environment), from the variable `ingress_token_env` names. The store's connection string is
`database` or the variable `database_env` names; a string with a password in it belongs in the
environment. Each pair takes exactly one; both, or neither, stops the process at start-up. A device
key is named the same way (`{ "env": "VAR" }`). `deploy/templates/settle.json` uses the variables.

**`profile`** is `"production"` or `"development"`, with no default (`T-016-13`). In production the
adapter refuses to start unless its store's connection string says `sslmode=require` (the owner's
ruling of 2026-10-02): every connection is then TLS, its server certificate checked against the
Mozilla roots and Amazon RDS's bundle, and its host name against the certificate.
`"development"` takes the string's `sslmode` as written, for the test suites and local runs.

**The adapter connects through a light client**: subxt's embedded smoldot, which checks what it
reads against the chain's own consensus instead of trusting one node's answers. `"connection"` is
`"light-client"` by default and may be left out. `chain_spec` is the settlement layer's chain spec,
a JSON file with its boot nodes; for a parachain such as Kreivo, `relay_chain_spec` is its relay
chain's (Kusama's). For a chain of its own, leave `relay_chain_spec` out.

A node's RPC is an explicit, opt-in fallback, for development or while a light client cannot
serve:

```jsonc
  "connection": "rpc",
  "endpoint": "ws://127.0.0.1:9944",
```

The adapter never falls back from one to the other by itself. A light client that cannot sync
leaves the adapter not serving, as any unavailable settlement layer does, and a configuration that
mixes the two (an `endpoint` without `"connection": "rpc"`, or a chain spec with it) is refused at
start-up. The `light-client` feature is on by default; it includes the RPC transport.

**Dry runs go over the light client too** (`T-027-09`, `T-027-10`, `T-027-11`). Released smoldot,
once a call has finished, recalculates the root of each child trie the call wrote, and the node's
call proof holds nothing for that: a dry run of `execute` failed as soon as a batch wrote several
inbox entries (F-027 NOTES, decision 42). The workspace builds smoldot from a fork that skips that
work (`[patch.crates-io]` in `Cargo.toml`, decision 43), and the adapter dry-runs every batch over
its light client. Optionally, a node can be named for dry runs, and for nothing else:

```jsonc
  "dry_run_endpoint": "wss://kreivo-rpc.example",
```

It then only sizes and prices batches: every read that decides an outcome stays on the light
client. A dishonest or stale node there can make a batch fail whole (nothing moves; its calls go to
review), never move value.

**Observation checks the contract's chain of entries at every sync** (`F-043`, `T-043-04`,
amendment 0040). The contract stores no entry: it keeps one head, `(position, digest, start)`, in
which each entry's digest covers the one before it, and emits each entry as an event. At each step
the adapter reads that head at the finalized block (`chain_head()`). If it is the head the adapter
last accepted, that is all: **an idle sync is one read**. Otherwise it reads the events of the
blocks since the one it accepted the head at, walking back by parent hash, rebuilds each entry from
its event and its block's number, and folds them from the accepted digest. If they lead exactly to
the head, it reports them in order and accepts the head once the core has taken in every one
(`settle_cursors`, `migrations/0006_chain.sql`). If not, it reads them once more; still not, it
raises `chain:<instance>:<position>` (kind `chain`, which pages) and reports nothing past that first
new position, while submitting and anchoring go on. A full node keeps about 256 blocks of events, so
routine settlement needs no archive node. An adapter further behind reads the older blocks from a
**history endpoint**, a node that keeps them all, used for nothing else and trusted for nothing (what
it serves is folded against the head the light client reads):

```jsonc
  "history_endpoint": "wss://kreivo-archive.example",
```

Without one, it raises `unreadable:<instance>:<position>` and waits. When no reader can serve the
range, or a mismatch does not clear, the operator has the adapter record it as a gap through its
own source, after which it accepts the layer's head as it stands, unverified for that range; the
range must be exactly the one the alert names:

```sh
aledgent-settle settle.json gap <from> <to>
```

Both `gap` and `refound` (below) hold the adapter's lease, so the running adapter is stopped first;
each runs once and exits.

A reset of the instance (governance's `set_anchor_head`) comes only with its book's re-founding. The
first start on a reset instance folds from the reset, and a cursor kept from the chain before it is
reset to the chain's start. A reset under a running adapter raises `reset:<instance>` and stops
observation; once the adapter is pointed at the re-founded book, `aledgent-settle settle.json
refound` resets its cursor and accepted head, and the next run starts on the new chain.

**There is no periodic re-scan** (`T-043-05`): nothing re-reads a range on a timer.
`adapter.rescan_every` and `adapter.rescan_window`, in a file that still sets them, are accepted
with a warning for this release, and refused by the next.

**Run it under a supervisor that restarts it.** The binary aborts on any panic, since the embedded
smoldot panics in its own threads and would otherwise leave a process that serves nothing. A new
process resumes from the store.

**The venue is the settlement contract** (`F-026`): `deployments` names the contract account of
the deployment's own instance, and of every deployment a withdrawal may pay into. Every call goes
out in a batch: one `Contracts::call` of the contract's `execute`, in a v5 general transaction
authorised by Kreivo's `PassAuthenticate` extension, sent as the deployment's community PASS
account. Each key is a **device key of that PASS account** (`"user"`: the account's hashed user
id; the account itself is `blake2_256([0; 32] ‖ user)`, and it is what the contract lists as a
key, what pays fees and whose nonce moves).

**Each batch is sized and priced by its dry run** (`T-027-09`): the adapter dry-runs `execute` at
the finalized head (`ContractsApi_call`, as the lane) and gives the batch the gas the dry run
requires, ref time and proof size, plus half again (`batch_gas_margin_percent`, 150), never more
than the runtime's normal-extrinsic maximum at that block (`System::BlockWeights`, read from the
runtime's metadata there: `0.16.9`'s is fixed at 3 670 016 bytes of proof size, `0.17.0`'s moves
with its cores; `batch_gas_cap` can only lower it), and as its storage deposit limit the deposit the dry run reports, plus the
same margin; the lane's balance must cover that deposit besides `fee_floor`. Proof size is what
binds: about 60 KB a withdrawal, 2.05 MB for a full batch of 32. No batch is signed without its
dry run: when the client cannot give one, nothing is signed, a `dry-run` alert is raised, and none
is asked for again for a minute (the light client bans for 40 s every peer whose call proof it
could not use). A batch whose dry run fails or does not fit is split: the longest prefix that
applies and fits goes, and the rest wait for the next lane or step. A call that is no good on its
own (its own dry run fails, or it alone needs more than the cap) is not refused on chain, since
nothing moved: it is left unsubmitted (`Held`), never signed and never tombstoned, so the core
releases nothing and the operation joins the review queue at its deadline; the dry run's error is
in a `review:<operation>` alert. A batch the contract reverts as a whole (not a key, for one) holds
nothing and raises `batch:<lane>`.

**Every era is born at the finalized head** (`T-027-08`): its birth hash is the head's own, never a
block looked up by number (which the light client does not answer), and a reorganisation above
finality cannot invalidate it. The era still ends at or before the batch's earliest expiry and
within `challenge_window`, and must leave a block above the best head to be included in; when
finality lags too far for that, the batch waits for the next step.

**The transaction's format follows the runtime's transaction surface** (`T-027-07`, `T-027-12`,
`REQ-SA-9`). Kreivo spec 132 (release `0.16.9`) and spec 133 (`4aef055` onward) differ in the order
of the message the device signs. On every step the adapter reads, from the runtime's live metadata,
what a transaction must carry: its `transaction_version`, its transaction extensions in order, and a
digest of their explicit and implicit types and of `Contracts::call`'s
(`src/chain/surface.rs`). It signs each batch in the format whose surface that is (`kreivo-132`:
`transaction_version` 12; `kreivo-133`: 13), so a runtime upgrade that changes the surface switches
the next batch, and one that changes nothing a transaction carries (spec 133 to 134, Kreivo `0.17.1`)
needs no release and no configuration. **The `spec_version` decides nothing.** A surface the adapter
has no format for is refused (`unknown_surface`), and so is a new Kreivo format until it is added
and pinned (`services/settle/kreivo-pin*`).

**The layer confirms each runtime before anything is broadcast on it.** The first transaction signed
on a runtime not served before (its `spec_version`, surface and format) is validated by the runtime
itself, without being included (`TaggedTransactionQueue_validate_transaction`, over the light
client), at the block the runtime was read at: the runtime that validates it is the one it was
signed for, even when an upgrade lands in between. Valid: the runtime is recorded as served in the
store (`migrations/0005_runtimes.sql`, so a restart does not ask again) and the batch goes out.
Invalid for what it carries (a proof or signer that does not verify, an origin no extension
authorised, bytes the runtime cannot decode, which the runtime reports as a failed call): nothing is
stored or broadcast, a `runtime:<spec_version>` alert is raised, and the adapter refuses to serve on
that runtime (`unconfirmed`) until it restarts or the runtime changes. Anything else (invalid for
now: a nonce, fees, the era, the block's resources): nothing is stored or broadcast either, the calls
wait unsigned, and the next step signs and asks again. After `adapter.not_now_alert_after` such
answers in a row (default 10, about 20 s of steps) the `runtime:<spec_version>` alert is raised, naming
the last answer, and it pages: a withdrawal waits unsigned only about 4.5 minutes before it ends as
never signed, long before any other page would fire (F-027 NOTES, decision 52). The runtime's
confirmation clears it. This is what catches a format change the metadata does not show, such as the
device message's order. Over the light client, a validation smoldot could not start (or one that called
a host function it does not provide) reads as a refusal too; a restart clears it (decision 53, and
the outage runbook).

`spec_versions`, in a file that still has it, is **informational**: it is accepted, logged at
start-up, and gates nothing. An operator may instead name the format for a runtime whose surface
would otherwise match none, or another: `"formats": { "135": "kreivo-133" }` under `adapter`. The
named format is used for that `spec_version` whatever its surface, still only after the layer
confirms it; a name the adapter does not know stops it at start-up.

A device key in a file or in an environment variable (`{ "env": "VAR" }`) is for development only
(`NFR-8`), and the adapter refuses one unless its configuration says `"development_keys": true`.
Production keys are the signer service's (`F-035`, `services/signer`): `{ "signer": { "key":
"lane-1" } }`, with `"signer": { "socket": …, "client": "settle", "token_file": … }` naming where it
is. The signer reads them from Vault (or, opted in per key, holds them in AWS KMS as
`ECC_SECG_P256K1` keys); the adapter holds their identifiers and public halves, and checks that every signature recovers to the key. Before the adapter can serve, the operator
must configure settlement (`POST /v1/ops/settlement`, naming the adapter's identity) and register
the adapter's ingress token.

## Tests

```sh
cargo test -p settle                                  # fake settlement layer; PostgreSQL in Docker
cargo test -p conformance --test suite -- F-016       # the conformance cases
# The PASS transaction against Kreivo's own crates, each format in a workspace of its own:
cargo test --locked --manifest-path services/settle/kreivo-pin/Cargo.toml       # spec 133, 4aef055
cargo test --locked --manifest-path services/settle/kreivo-pin-132/Cargo.toml   # spec 132, 0.16.9
```

### Against a local chain (ignored by default)

`tests/live_node.rs` runs ingress, the in-process core and the adapter against a **local** Kreivo
with the settlement contract, through the light client unless told otherwise. It refuses anything
not on this machine: a chain spec with a boot node elsewhere, or an RPC endpoint elsewhere.

1. Start Kreivo on zombienet and deploy the contract through the contracts store
   (`contracts/settlement/README.md`), with the deployment's community PASS account as a key
   (`set_keys`) and a withdrawal bound for the asset (`set_withdrawal_bound`); fund the position
   (`deposit`) and the PASS account's native balance; give the asset metadata as Kreivo names its own:
   symbol `USD`, name `USD/2`, 2 decimals, at `Here(1)`, where the test genesis binds `USD` (`DEC-20`).
   Register the lane's
   device key with the PASS account (`Pass::register` or `add_device`, a substrate-key device).
2. Run it through the light client, with the raw chain specs zombienet wrote in its base directory
   (it prints the directory as it starts; `zombienet -d <dir>` chooses it), whose boot nodes are
   the local nodes:

```sh
ALEDGENT_SETTLE_CHAIN_SPEC=<zombienet dir>/<Kreivo's raw spec>.json \
ALEDGENT_SETTLE_RELAY_CHAIN_SPEC=<zombienet dir>/kusama-local.json \
ALEDGENT_SETTLE_GENESIS=<the node's genesis hash, hex> \
ALEDGENT_SETTLE_SPEC=132 \
ALEDGENT_SETTLE_INSTANCE=1 \
ALEDGENT_SETTLE_CONTRACT=<the contract's account, hex> \
ALEDGENT_SETTLE_PASS_USER=<the PASS account's hashed user id, hex> \
ALEDGENT_SETTLE_KEY_FILE=/path/to/device.key \
ALEDGENT_SETTLE_ASSET='Here(1)' \
cargo test -p settle --test live_node -- --ignored --nocapture
```

   Or over the collator's RPC instead, opted into: `ALEDGENT_SETTLE_CONNECTION=rpc` and
   `ALEDGENT_SETTLE_NODE=ws://127.0.0.1:20000` in place of the two chain specs.
   `ALEDGENT_SETTLE_SPEC` is the node's `specVersion`: 132 for Kreivo release `0.16.9`, 133 for a
   runtime built from `4aef055` or later.

The device's id is `blake2b-256` of its compressed secp256k1 public key (the account a
`MultiSignature::Ecdsa` verifies against); the PASS account is `blake2_256([0; 32] ‖ user)`. The
zombienet suite (`T-027-06`) is where this runs in CI.
