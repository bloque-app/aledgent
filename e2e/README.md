<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# aledgent's zombienet end-to-end suite

The release gate (`PLAN.md` §5, "Release gate"): no release reaches the Kreivo testnet or mainnet
unless this suite passes. It is `T-026-04` (issue #75) and the first part of `T-027-06` (issue #76).
`.github/workflows/e2e-zombienet.yml` runs it on the self-hosted `zombienet` fleet: by manual
dispatch, on every release tag (`v*`), and from any release workflow that calls it.

Its network, artifacts and CI discipline come from an earlier zombienet suite that ran the same
network on the same kind of runner for weeks.

## What runs

| Step | What | Where |
|---|---|---|
| 1. Network | A relay chain (`rococo-local`, alice and bob) and Kreivo (para 2281): the collator `kreivo01`, archive, and a full node `kreivo-pruned` that keeps 16 blocks of state. No Asset Hub: the contract moves Kreivo assets only | `network/` (`spawn_network`, zombienet-sdk 0.4.15) |
| 2. Deploy | Through the contracts store, with the steps a mainnet deployment runs, in order (`contracts/settlement/README.md`): `preflight`, `community` (finds the community and funds it), `publish`, `license`, `instantiate`, `bound`. A fresh network has no community, so `community` first registers it | `contracts/settlement/scripts/e2e.mjs deploy`, which calls `drive.mjs`'s own steps |
| 3. Drive | `drive.mjs`'s `deposit`, `execute`, `read` and `events` as an operator runs them; then a deposit with an account reference, a withdrawal, a withdrawal the bound refuses and one Kreivo refuses (the contract's account frozen), an anchor, a full batch of 32 withdrawals, a deposit of a non-sufficient asset, and a deposit Kreivo refuses (it would leave the depositor below the minimum). Records the weight and storage deposit of a deposit and of a full batch | `e2e.mjs drive` (script driver), or `drivers/aledgent.sh drive` with `aledgent-driver/` (the aledgent driver: the deposit, the withdrawal, a refusal and an anchor, through aledgent; then the book-only asset `BKO`: registered book-only, issued and moved, bound one unit short (refused on chain), funded with polkadot-api (`papi/fund-contract.mjs`), bound at zero divergence, anchored, and compared at zero difference: `F-029`, amendment 0011) |
| 3. Check | At a finalized block, reading the contract's child trie as the adapter does: the chain's head (amendment 0040) by raw key and by query; the contract's events folded from the chain's start, every position once and in order, to that head; one outcome per reference with the right kind, amount and reason among the folded entries; no entry stored; each position equal to the contract's holding | `e2e.mjs check` |
| 4. Faults | Drop the connection mid-submit; kill the submitter (the adapter) between signing and submitting; starve the light client of peers (aledgent driver); prune node state; a deposit in the same gap as a withdrawal. Then the checks again, over the final state | `e2e.mjs faults` (script driver) or `drivers/aledgent.sh faults` (every case through the adapter), then `e2e.mjs check final` |
| Upgrade (optional) | With the `upgrade_from` input (`210ca4b`, v2): v2 as a second app of the community, with entries it stores; `publish_upgrade` and `upgrade` by the community, every cell byte for byte and the chain's head `(n, zeros, n)`; entries after, and a call that fails in its own frame (its events gone with it); governance named, `set_anchor_head` refused as anyone else, then the full reset (v2's entries removed, the deposit to the origin, the head `(0, zeros, 0)`); entries after it. The events fold to the head at every step | `e2e.mjs upgrade` (`V2_BUNDLE`) |

What a run found is recorded in `features/F-026-settlement-contract/NOTES.md`, "The first real run".

### The genesis

Kreivo's genesis is the released runtime's own `development` preset (zombienet cannot select its
`local` one), plus one additive override: three assets owned by //Alice, `Here(1)` (sufficient),
`Here(2)` (not sufficient) and `Here(3)` (sufficient; symbol `BKO`, decimals 2, name `BKO/9` on purpose:
a binding reads the symbol and decimals fields only), minimum balance 1, 100 000 000 000 of each held
by //Alice. //Alice
administers the community and the contract, //Bob is the contract's key, //Dave the payee.

### Drivers

Steps 3 and 4 have two drivers, chosen by the workflow's `driver` input. Both leave the same
`expect.json` (one object per operation: its kind, reference, amount, reason), and `check` reads
only that and the chain, so switching drivers changes nothing downstream.

- `script` (default): `contracts/settlement/scripts/e2e.mjs drive` and `faults`, submitting
  through the collator's RPC.
- `aledgent`: `aledgent-driver/`, a root-workspace binary hosting ingress and the in-process core,
  with **the settlement adapter as it runs in production**: the `aledgent-settle` binary
  (`services/settle`, F-016 and F-027, built with `fault-injection`), its own process with its own
  PostgreSQL store, reaching the core over ingress's HTTP surface and Kreivo over its light client
  (`aledgent_connection=light-client`, the default) or, opted into, over the collator's RPC
  (`rpc`). `drivers/aledgent.sh drive` first has `e2e.mjs aledgent-setup` register the adapter's
  PASS accounts (its test constants) with their secp256k1 devices on Kreivo's Pass pallet, fund
  them, and make them the contract's keys with the core's clerk key as its attestation key; then it
  starts the driver, which lives across steps 3 and 4 (the core is in its memory; marker files in
  `$E2E_OUT/aledgent` hand it the phases). Through ingress: a customer's deposit to an account
  reference (the customer is `e2e.mjs deposit-to`), credited at finality; a withdrawal, settled; a
  withdrawal the contract refuses (paid to the contract itself), reported with its reason and, a
  proved rejection, released (amendment 0006 item 3); the core's checkpoint, anchored and final. `drivers/aledgent.sh faults` then runs every
  fault case through the adapter, and `stop` removes what the driver started.

**Where the adapter runs.** In a container of the build's image, in a network namespace of its
own that a long-lived container holds (with `iptables`, and `NET_ADMIN` in that namespace only),
on a Docker network of the run's own with its PostgreSQL. The nodes listen on every interface, so
the chain specs' boot nodes (one per chain: smoldot-light 0.17.2 panicked with two, F-026 NOTES
finding 5) are rewritten to that network's gateway, and so is ingress's address. The harness can
then cut every peer without touching the runner's network (everything leaving the namespace is
rejected except PostgreSQL and ingress), kill the adapter and start it again over the same store,
and block the collator's parachain port so that the light client reaches Kreivo through the
pruned node alone. The adapter's logs are kept in `$E2E_OUT/aledgent/adapter-*.log`. Its dry runs
go over its light client too, with no `dry_run_endpoint`, as the canary runs: the workspace's
patched smoldot dry-runs batches of several calls (F-027 NOTES, decisions 42 and 43). It runs with `--restart on-failure`, as its
supervisor would restart it: it aborts on any panic, and smoldot panics (decision 37).

### Fault cases (T-027-06)

Each checks that nothing is taken as "not executed" without both proofs (the owner's ruling of
2026-09-24): the chain has no entry for the reference at finality (its events, folded to the
head), and the key's nonce is unconsumed; and that the final state is correct.

| Case | Script driver | aledgent driver |
|---|---|---|
| Drop the connection mid-submit | Two variants: after the pool took the extrinsic, and before any answer. Recovery from the chain alone; resubmitting the same bytes is refused | Every peer of the adapter's light client is cut the moment it stores a signed batch, for 30 s. Back, it settles the withdrawal from that one signed batch |
| Kill the submitter between signing and submitting | A child process signs and dies by SIGKILL. Both proofs hold but the bytes still validate; only consuming the nonce makes "not executed" final | The adapter's process dies (`ALEDGENT_SETTLE_CRASH_AT=after-persist`) with the signed batch stored and neither recorded with the core nor sent. Both proofs hold and the core releases nothing; a new process over the same store sends the same bytes (no new signature) and the withdrawal settles once |
| Starve the light client of peers | n/a (no light client) | Every peer cut for 90 s while the chain goes on, with a withdrawal submitted meanwhile: neither settled nor released while blind (or refused while the adapter declares the layer unavailable). Peers back, it resyncs and the withdrawal settles from one signed batch |
| Prune node state | Through `kreivo-pruned`: a past block's events are gone; the chain's head at its finalized head equals the collator's, byte for byte, and the collator's events fold to it. With no stored entries the pruned node can say whether anything is new, not what: a reader behind its window needs the history (settle's history endpoint, `T-043-04`) | The adapter reaches Kreivo through `kreivo-pruned` alone. A deposit made while it was down, whose block's events that node no longer serves, is credited (the inbox is read by position at the finalized head); a withdrawal settles through it |
| A deposit in the same gap as a withdrawal | Two signers, submitted together: the entries folded from the cursor are both, positions move by both | A customer's deposit lands right behind a batch the adapter signed: both reported, the ledger account moves by both |

### Stress scenario c (F-032)

With the aledgent driver and the `stress_phases` input (`name:commands_per_min:secs,...`), a step
after step 3 runs the stress harness (`features/F-032-stress/plan.md`) inside the driver's process,
against ingress's router, while a withdrawal is submitted every twenty seconds and settled through
the adapter's own process (`aledgent.sh stress`). The harness's `report.json` and `summary.md`, and
the withdrawals' settlement times (`withdrawals.json`), are in `$E2E_OUT/aledgent/stress/`; the
withdrawals are also in `expect.json`, so `e2e.mjs check` covers them.

```sh
gh workflow run e2e-zombienet.yml --ref <branch> -f driver=aledgent -f stress_phases=c-1x:300:300,c-5x:1500:300
```

## CI

One job on `zombienet` (16 vCPU): builds `spawn_network` and the contract in a `rust:1-bookworm`
container (the fleet has no Rust toolchain for aledgent), fetches the Kreivo release and the
polkadot-sdk binaries, spawns the network, runs the phases, writes the job summary, uploads
`report.json`, and always stops the network and deletes its job-local directory.

Inputs: `kreivo_ref` (build Kreivo's runtime from a commit instead, as Kreivo's release workflow
builds it: about seven minutes on the fleet), `aledgent_connection`, `kreivo_release` (default
`0.17.0`, `3e0a5c3`, spec 133: the runtime Kreivo mainnet runs from 2026-09-30, and the one the
canary deploys on; a release tag gates on it), `psdk_release` (default `polkadot-stable2606-2`, which
0.17 needs; the collator must be at least as new as the runtime needs), `driver`, and
`mainnet_parity` (a run warns, and a release tag fails, when the runtime under test is not the one
Kreivo mainnet runs).

Kreivo `0.16.9` (spec 132) stays selectable, with its own collator:

```sh
gh workflow run e2e-zombienet.yml --ref <branch> -f driver=aledgent     # 0.17.0 on stable2606-2
gh workflow run e2e-zombienet.yml --ref <branch> -f kreivo_release=0.16.9 \
  -f psdk_release=polkadot-stable2603-4 -f driver=aledgent
```

## By hand

On an x86-64 Linux host (or an Apple silicon Mac; `prepare.sh` picks the platform's binaries), with
Rust, `protoc`, Node 22, and `cargo-contract` 5.0.3:

```sh
OUT=$PWD/target/zombienet-artifacts ./e2e/scripts/prepare.sh
(cd contracts/settlement && cargo contract build --release)

export ZOMBIENET_KREIVO_WASM=$PWD/target/zombienet-artifacts/kreivo_runtime.compact.compressed.wasm
export ZOMBIENET_POLKADOT_BIN=$PWD/target/zombienet-artifacts/bin/polkadot
export ZOMBIENET_POLKADOT_PARACHAIN_BIN=$PWD/target/zombienet-artifacts/bin/polkadot-parachain
export PATH=$PWD/target/zombienet-artifacts/bin:$PATH
ZOMBIENET_ENV_FILE=/tmp/endpoints cargo run --manifest-path e2e/network/Cargo.toml --bin spawn_network &

set -a; . /tmp/endpoints; set +a
cd contracts/settlement/scripts && npm ci
export BUNDLE=../target/ink/aledgent_settlement_contract.contract COMMUNITY_ID=4242 COMMUNITY_NAME="aledgent Test"
node e2e.mjs all && node e2e.mjs check final
```

The network is heavy (three node processes, two of them embedding a relay node); prefer the fleet.
