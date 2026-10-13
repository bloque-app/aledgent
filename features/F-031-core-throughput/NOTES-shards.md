<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-031 — lever 3: account-partitioned keys and the shard prototype

Notes of `T-031-09` to `T-031-11` (`DEC-22`, amendment 0012). To be folded into this feature's
`NOTES.md` when the design PR (#124) lands.

## T-031-09: account-partitioned keys

Every key ordered against a ledger account now carries the account's partition in its top sixteen
bits (`ledger_kernel::partition`): `Partition::of(id)` is the first two bytes of
`blake2b-256("aledgent/partition/v1" ‖ id)`, over 2¹⁶ partitions.

| Key | Partition | Derivation |
|---|---|---|
| Kernel account of a ledger account, an issuance counterparty | its subject's (the ledger account) | `kernel_account_id_in`: partition ‖ `blake2b-256("aledgent/kernel-account/v2" ‖ role ‖ subject ‖ asset)[..14]` |
| Holding position of a reservation | its payer's (`KernelAccountSpec::in_partition`) | the same |
| Transfer (and its pending and voided markers) | its first debit's account's (`PostingDraft::home`) | partition ‖ `blake2b-256("aledgent/transfer-id/v2" ‖ external id)[..14]` |
| Service leaves of kinds keyed by a ledger account (`kind::ACCOUNT_SCOPED`: ledger account, reserved total, open reservations, running balance, custody open, successor, custody epoch, opening loaded) | the account's | `library_id`: partition ‖ digest[..14]; `service_key`: kind ‖ partition ‖ digest[..13] |

A sparse Merkle tree reads key bits most significant first, so a partition is one subtree at depth
sixteen of each kernel tree, and at depth twenty-four (after the kind) of the service tree. A shard
holding a range of partitions holds whole subtrees; a split moves them and renames nothing.

Decisions, each the conservative reading:

1. **What is not partitioned yet.** Kinds keyed by an operation (operation, reservation, client key,
   deadline, attribution, settlement records…) and the posting journal (`kind::POSTING`, keyed by
   one global posting sequence) keep their hashed keys. Partitioning them needs operation references
   that carry their payer's partition: ingress mints them from the OS random source and "nothing
   parses" them (`REQ-OP-10`), so that is a spec question, not a key change (owner question `c-13`).
   The external-id tree's slot (`external_id_key`) is CipherClerk's hash, and the journal is one per
   book: both stay per shard, rebuilt on a split.
2. **Holdings keep the trait.** `ReservationBook::holding` is unchanged; `reserve` places the spec
   in the payer's partition. A placement is part of the spec, its equality and its export encoding.
3. **Format 11.** The export writes a kernel account's placement after its balance rule, `None` for
   the subject's own (one spelling per spec). This build reads only format 11 (amendment 0011 and F-029 took 8 to 10).
4. **Golden roots re-recorded.** `ledger-kernel`'s golden vectors (`book::differential`) changed,
   as every kernel id did. A book founded before this change cannot be opened by it: a test
   deployment is re-founded, or exported and imported (`REQ-RC-3`). No production book exists.

## T-031-10: the prototype (`crates/shards`)

On the kernel binding (`ledger-kernel`, CipherClerk) without the service library and without VOS: a
shard is one kernel book over a contiguous range of partitions, and its commands are steps of one
unit per batch (lever 1's isolation: a refused step refuses alone). A transfer between shards posts
payer → the in-transit position for the payee's shard and, in the same commit, enqueues a credit
numbered per (source, destination); the destination applies credits from each source in sequence,
once, waiting at a gap. Issuance is a pool, one member per shard. The relay carries credits after
commit and may duplicate, reorder, drop and lose acknowledgements.

## T-031-11: tests and measurement

`crates/shards/tests/credit_path.rs`: a transfer is final at its debit and credited once under
duplicate delivery; credits wait at a gap; twenty concurrent spends of 15 against 100 on one payer
give exactly six; a lagging directory copy refuses and never strands a credit; the trial balance is
zero after every commit and the in-transit positions sum to zero once delivered; and a property
test (2 to 4 shards, batches of 1 to 16, every fault) checks exactly-once (each destination's applied
sequence equals the last debit's) and **zero divergence**: every balance equals one book's, fed the
commands the shards accepted in the order they decided them.

Throughput against the shard count (`crates/shards/tests/throughput.rs`, release, 4 000 transfers
among 256 accounts, batches of 32, credits applied by each destination in parallel; an Apple M-series
laptop with 12 cores, under a load average of 55 from other builds, so a lower bound):

| Shards | ops/s | Speed-up | Transfers between shards | Commits |
|---:|---:|---:|---:|---:|
| 1 | 874 | 1.0× | 0 % | 125 |
| 2 | 1 637 | 1.9× | 44 % | 252 |
| 4 | 2 225 | 2.5× | 68 % | 506 |
| 8 | 2 554 | 2.9× | 83 % | 982 |

A transfer between shards costs two postings (debit, then credit), so the work per transfer rises
from one posting to 1.83 at eight shards; and eight shards on twelve loaded cores do not get eight
cores. The curve is the split's, natively, about 1.1 ms per kernel posting with its signatures; it
says nothing of an Agent's invocation cost, which levers 1 and 2 address. More shards are not
justified until the core's own per-command cost (lever 2) and the Agent path are measured: the
prototype stays a prototype.

## T-031-20: the shards as VOS Agents

**What runs.** One Space, N book-shard Agents (`aledgent-book-<n>`) and one principals Agent
(`aledgent-principals`), Local profile, single node, each installed with the `aledgent-shard` actor
(`agents/shard`): one program, two roles fixed at genesis (`shards::agent`). A shard's kernel book
is in committed maps in its Linear rows (`core_binding::rowstore`, lever 2); beside it, its state as
rows (`shards::kv`): open accounts, the directory copy, outbound credits, sequences applied per
source, revocations, the feed version. The principals Agent holds the registry, the grants, the
revocations and the feed (`shards::principals`). The key ring's root and the openings of the
accounts a round touches are a caller blob, never a row (`REQ-RT-6`).

**One round, one commit.** Each step, every shard with work gets one Linear invocation carrying, in
order, the principals feed from the last version it applied, the credits from each source, the
trims its peers have acknowledged, and up to a batch of commands (`decide::run_round`). The round is
one kernel commit (`Book::begin_batch`, `T-031-19`) and one Linear transition: the debits, the
credits they enqueue, the credits applied and the sequences recorded commit together
(`REQ-RT-1`).

**The credit path is a host relay over committed outboxes** (owner question `s-01`). VOS at the
pinned commits has no message from one Agent to another in the runtime ABI (a transition is state
and outcome; nothing produces `Await`; an outbox under consensus is future work), and a Merge lane
takes events only from the Agent's own replicas, ordered by causal height, so it cannot apply
another Agent's credits in their sequence. What VOS does guarantee is that a Linear transition
commits the Agent's rows and its reply atomically, and retains the reply until acknowledged. So:
the source's round commits the debit and the numbered credit (rows `q`, reply `credits`); the host
carries each pair's credits in the destination's next round; the destination applies those past
its last sequence from that source, in order, once, and replies the sequence it reached; the host
carries that back to the source as a trim. The host holds nothing the Agents do not:
`Cluster::recover` rebuilds the relay from `Query::Outbound` and `Query::Applied`.

**Replicas check every reply.** The host keeps a native replica of every Agent and requires the
same reply from both, byte for byte (outcomes, credits, sequences applied, root, commit); a
difference stops the host (`HostError::Divergence`).

**Ingress's side** (`shards::host::Router`). It resolves the caller's grant from the principals
Agent, refuses past the grant's bound, stamps the command with the principals version read, mints
the operation reference with the payer's partition in its top sixteen bits (`c-13`, owner question
`s-03`), maps pools to members, and routes to the payer's shard. A shard refuses a stamp older than
a revocation it has applied (`c-10`). It stands in the shard host, not in `services/ingress`, which
still serves the single core: the shard Agents decide opens, issues and transfers on the kernel,
and the service library is not partitioned yet (`s-04`).

**Accounts** are opened in the principals Agent's registry; every shard learns them from the feed;
the account's own shard creates its kernel account when it applies the entry. A credit to an
account its shard has not opened yet waits, as at a gap (`s-06`).

**Pools** (`shards::pool`, `c-06`). One member per shard, a function of the pool's identifier. A
credit to a pool goes to the member in the payer's shard, in the same posting as the debit; a debit
draws on the member its reference picks, and is refused if that member does not cover it (`s-07`);
the operator rebalances on a threshold with ordinary transfers.

**The anchor** (`shards::anchor`) is a binary Merkle root over `(shard, commit, kernel root)` for
every shard at one cut, with proofs; the in-transit positions are kernel accounts, so it commits
to every balance (`s-09`).

The interfaces, apart from the host that carries them (for another runtime or topology):
`SHARD-INTERFACES.md`.

### Tests (`crates/shards/tests/agents.rs`, through `NativePort`: the guest's code over rows in memory)

- `req_rt_11_credits_through_agents_apply_exactly_once_under_every_fault`: duplication,
  reordering, dropped heads and lost trims at once; every destination's last sequence per source is
  the last debit's; balances equal one book's.
- `nfr_9_zero_divergence_through_the_agents` (property, 2 to 4 shards, batches 1 to 12, every
  fault): exactly once, balances equal one book fed what the shards accepted in decision order,
  the trial balance holds.
- `req_rt_11_the_relay_recovers_from_the_agents_rows`: the relay forgets everything mid-run and
  rebuilds from the Agents; exactly once, zero divergence.
- `inv_5_the_trial_balance_holds_with_credits_in_transit`: in transit equals what is outstanding.
- `req_au_3_a_revocation_fences_older_authority_within_one_round`: a command stamped before the
  revocation and decided after is refused by its shard; every shard applies the revocation in the
  step it commits in; a new grant is a new authority; the grant's bound is enforced at ingress.
- `c_13_operation_references_carry_the_payers_partition`, `inv_19_one_anchor_over_every_shard_root`,
  `req_rt_12_a_pool_takes_credits_locally_and_is_rebalanced`,
  `req_rt_9_an_account_opened_through_the_principals_agent_lives_in_its_shard`,
  `the_agent_answers_only_its_driver_and_is_founded_once`,
  `req_rt_6_no_private_value_reaches_the_agents_rows`, `a_divergent_agent_stops_the_host`.

### Measured natively (`tests/agents_throughput.rs`, an observation)

4 000 transfers among 256 accounts, batches of 64, the Agents' transition over committed maps and
each round again on its replica, on the 12-core laptop (load average 4 to 11):

| Shard Agents | Decided/s | Between shards | Credit lag p99 |
|---:|---:|---:|---:|
| 1 | 859 | 0 % | |
| 2 | 1 235 | 44 % | 102 ms |
| 4 | 2 074 | 68 % | 114 ms |
| 8 | 3 537 | 83 % | 124 ms |

On VOS (`notes/MEASUREMENTS-F031.md` §7, a 2-vCPU GitHub-hosted runner, AMD EPYC 7763, VOS
`perf/agent-stack` `6c794311`): 48, 34, 33 and 33 decided/s on 1, 2, 4 and 8 shard Agents in the
JAM-native profile, 54, 36, 34 and 34 in the VOS-native one; about 100 M of actor gas a transfer,
and two vCPU are one core, so shards do not add throughput there. Every run: exactly once, the trial
balance with credits in transit, every reply equal to its replica's.

## T-031-22: the consistent cut (amendment 0015)

The prototype anchored only when nothing was in flight (`Cluster::anchor` read every replica's latest root),
and nothing defined the book's total across shards for a binding. Now:

- **Heads in every round.** `RoundReply.heads` (`cut::Heads`) is computed by the transition from the shard's
  rows after the round, so the replica check covers it byte for byte; a new row `u` counts rounds (the audit
  sequence), and `t` (trimmed) is now read back. `Hot.last_root` lets `Query::Heads` answer from the rows.
- **The registry's digest.** The principals Agent keeps a running digest of its feed (row `D`); its reply carries
  it, and the host checks it against the replica.
- **The relay carries audit records too**, with the same faults (dropped heads every other step, reordering,
  duplication) and a new `Cluster::lag` (steps a credit or a record waits before it is carried). The records stand
  for each shard's committed audit outbox, so `Cluster::recover` does not lose them.
- **`Cluster::anchor` is at the live cut**: the assembler's greatest consistent cut, then settled as the floor.
  The core host's `shards` command keeps calling it at the end of a count; nothing else changed there.
- **Bindings.** `Change::Binding` / `BindingEnded` (appended to the enum: earlier variants keep their postcard
  index), `PrincipalsCommand::Bind` / `EndBinding`, row `b` ‖ asset in a shard and `B` ‖ asset in the principals
  Agent. The round that applies `Binding` reads the shard's issuance member (its opening is in the private input:
  `touched_by_round` adds it) and reports a `Share`.

Decisions, each the conservative reading:

1. **One in-transit position per (shard, peer), both directions.** The kernel account `transit(at, peer)` is
   credited by debits sent to the peer and debited by credits applied from it, so the identity the tests check is
   per unordered pair: `transit(i, j) + transit(j, i)` = value in transit i → j plus j → i at the cut. Splitting it
   into two accounts would make each side checkable alone but changes the kernel ids (a re-genesis); not done.
2. **The left inequality (trimmed ≤ applied) is part of the rule**, so that a cut's credits in transit can always be
   listed from their sources' outboxes (`NFR-9`'s second axis), not only valued. It costs nothing on the write path.
3. **Shares travel in the reply**, standing for `settle/<n>` messages; the settlement Agent is `DEC-23`'s and not in
   the prototype. `Cluster::bind_amount` plays its part over the host's assembler.
4. **The freeze is per shard**, from the commit that applies `Binding`; an issue decided earlier in another shard is
   in the amount. The owner's question `gc-05` is whether `NFR-20`'s bound is the right one for that delay.

Tests (`crates/shards/tests/cut.rs`, through `NativePort`): every live cut is checked against an independent
ledger of every credit enqueued; property tests over 2 to 4 shards, batches 1 to 12, every relay fault and a lag of
0 to 3 steps for both credits and audit records; a deterministic case shows why the rule exists (records read at
two moments count a transfer at both ends, and the assembler moves the destination back).
