<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-031 — the cross-host relay (an experiment, 2026-10-04)

Notes of `T-031-23` and `T-031-24`. This is an experiment under the owner's rule of 2026-10-04: the
code is merged behind a feature (`shards/net`, off by default; the core host's `fleet` command), and
these notes and `notes/MEASUREMENTS-F031.md` §12 record what was measured. The defaults it took are in
the experiment's decision record (kept with the maintainers); the outcome page is
`OUTCOME-multihost.md`. Nothing here changes `SPEC.md`, `PLAN.md`, a DEC or an amendment.

## Why

`NFR-21` (a) asks that capacity grow by adding execution nodes. Until now the shard Agents, their
replicas, the relay and the router lived in one host process (`shards::host::Cluster`), so the only
way to more capacity was one bigger machine, which the owner ruled out. One 16-vCPU fleet machine
sustains about 650 transfers a second with the PVM on the critical path (`MEASUREMENTS-F031.md` §10,
F-034's notes).

## What was built (`crates/shards/src/net`, feature `net`)

- **Layout.** Host 0 is the coordinator: the principals Agent, the router (and, in the bench, the
  load) and the anchor's assembler, plus `first` shards. Every other host holds `per_node` shards.
  Shards are numbered across hosts in contiguous blocks; the directory and every interface of
  `SHARD-INTERFACES.md` are unchanged. The relay is another carrier of the same messages, as §3 of
  that document anticipates.
- **Transport** (`net::noise`). Noise `IK_25519_ChaChaPoly_BLAKE2s` over TCP. Each host draws a
  static key; a dialler pins its peer's public key, an answering host accepts only keys of the book's
  hosts and only under that key's index; the prologue binds the book's relay id. Frames of any size
  are cut into Noise messages. A silent session dies after 5 s (heartbeats every second).
- **Links** (`net::link`). A full mesh, the higher index dialling the lower; a dialler retries until
  its peer answers. Messages wait in a per-peer queue while the peer is unreachable.
- **Streams** (`net::node`): credits (source host → destination host), trims (back), the principals
  feed (coordinator → every host), audit records with a binding's shares (every host →
  coordinator), orders (router → the payer's shard's host) and outcomes (back). Each is exactly once
  by its own numbering and applied idempotently: credits past the destination's applied sequence (the
  Agent itself applies them in sequence and once, `REQ-RT-11`), feed entries at the next version,
  audit records through the assembler, orders once by digest (a decided order is answered with the
  outcome recorded for it). A session that ends may lose what was in flight; **every session begins
  with each side's Hello**, which says what it holds of the peer's streams (the applied sequence per
  pair, the feed version, the assembler's heads), and the peer sends the rest again. The router
  sends again every order it has no outcome for.
- **Each shard runs on its own.** A local shard takes its next round as soon as its previous round
  committed and it has work. There is no step shared across shards or hosts: `Cluster::step`'s
  barrier would make every round wait for the slowest shard of the whole book.
- **The cut.** Every round's stream heads reach the coordinator's assembler, which anchors at the
  greatest consistent cut (amendment 0015). The condition is over sequences, so the hosts' clocks and
  delays do not matter. In the bench the coordinator anchors every second under load.
- **Host loss.** A host that restarts with no relay state rebuilds it from its Agents
  (`Node::resume`): it replays each shard's round log into a fresh replica (`ShardAgent::attach`),
  checks it reaches the Agent's root, and recovers what each shard enqueued, applied and decided, its
  committed outbox and its audit records. Its peers' Hellos and its own fill in the rest. The
  coordinator's own restart is not covered (its principals Agent and assembler).

## Tests (`crates/shards/tests/multihost.rs`, through the Agents' own transition)

- `nfr_21_every_stream_crosses_hosts_exactly_once`: three hosts (the coordinator with one shard, two
  hosts with two). After the load and a quiescence: every pair's applied sequence equals what its
  source enqueued, read from the Agents' heads; every outbox trimmed; the trial balance holds with
  credits in transit; every balance equals an independent ledger of the accepted transfers; every
  replica at its Agent's root; every live anchor (one per chunk of load) a consistent cut; the
  revocation reached every shard.
- `req_rt_11_exactly_once_while_sessions_drop_under_load`: the same while every host drops all its
  sessions every 40 ms.
- `nfr_21_hosts_as_processes_one_killed_and_restarted_under_load`: this test binary run three times
  as separate processes over loopback. A third of the way through the load, host 2 is killed with
  SIGKILL and started again from its Agents' rows and round logs, as a new process on a new port; the
  same checks hold at the end.
- `ingress_refuses_a_revoked_principal_on_every_host`.

Run twelve times in a row on the laptop, green each time (`cargo test -p shards --features net --test
multihost`, about 2 s).

## The fleet bench (`aledgent-core-host fleet`, stress workflow `core=shards-fleet`)

One job per machine. The core is built once on a GitHub-hosted runner and downloaded by every node.
The nodes meet through the run's artifacts: each publishes its address and public key as an artifact
name; node 0 waits for every node, late joiners included, and reports when each checked in
(`tools/ci/fleet-rendezvous.sh`). `fleet_counts` runs the same group at several node counts, one
after the other. Phases on one book: `closed:N`, `paced:R:S`, `paced:vP:S`, and
`burst:R:S:B:T:D` for F-032's gate 2 (owner ruling G-03: the burst is measured against the sharded
capacity under test). Each phase ends in a quiescence and the checks: exactly once from the Agents'
heads, the trial balance with credits in transit, the live anchors consistent.

The runner watchdog (kept with the operator's CI) now covers the `small` pool and partially started
groups: a run whose running jobs all wait at the rendezvous while the runner autoscaler leaves the rest queued is
cancelled and re-run with `--failed`, once the pool has room for the whole group.

## Results

`notes/MEASUREMENTS-F031.md` §12 and `OUTCOME-multihost.md`. On 16-vCPU machines over the fleet's
private network, the book decided 693, 1 072 and 1 373 transfers a second in a closed loop on 1, 2
and 3 hosts, and 1 426 a second while 1 500 was offered on 3. The trial balance and exactly once
held in every phase. The burst was absorbed and drained within its bound. Rounds fragment as shards
are added: that, and not the network, is where the scaling goes.

**Adopted.** The owner adopted the relay's shape for `NFR-21` (a) on 2026-10-05 (`mh-relay-shape`),
pending this run's correctness, which it passed (`OUTCOME-multihost.md`).
