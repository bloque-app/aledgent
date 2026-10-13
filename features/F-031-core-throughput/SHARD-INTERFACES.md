<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-031 — the shard Agents' interfaces

What a book-shard Agent and the principals Agent take and give (`T-031-20`, `DEC-22`), stated
apart from how today's host carries it, so that another runtime or topology (an aledgent runtime
forked from VOS's standard one; settlement and audit Agents beside the shards) can carry the same
messages. Every type named here is in `crates/shards` (Apache-2.0 OR MIT, `no_std + alloc`), encoded
with postcard; the transition that consumes them is `shards::agent::transition`, and the only VOS
contract it relies on is: one Linear invocation commits the Agent's rows and its reply together,
and only the founding driver may invoke it.

## 1. Partitions and routing (`REQ-RT-9`, `c-09`, `c-13`)

| Item | Definition | Where |
|---|---|---|
| Partition of an account | first two bytes of `blake2b-256("aledgent/partition/v1" ‖ account id)`, 2¹⁶ partitions | `ledger_kernel::partition::Partition::of` |
| Shard of a partition | shards hold contiguous ranges; `Directory { starts }`, shard `i` holds `starts[i] .. starts[i+1]` | `model::Directory` |
| Home of a command | `Command::home()`: the payer of a transfer, the account of an open or an issue | `model::Command` |
| Operation reference | the payer's partition in the top 16 bits, 112 random bits after (`c-13`); nothing parses it: routing uses `home()` | `host::Router::mint` |
| Pools | one member per shard, `blake2b-256("aledgent/pool-member/v1" ‖ pool ‖ i)[..16]` landing in that shard; credits to the pool → the member in the payer's shard; debits draw on `drawn_by(op)` | `pool::Pool` |

A shard refuses a command whose home is not in its range (`Refusal::WrongShard`): a mis-route
commits nothing.

## 2. One invocation of a book shard: a round

`agent::Envelope::Round { round: decide::Round, private: BlobKey }`, applied in this order and
committed as one kernel commit:

| Field | Type | Meaning |
|---|---|---|
| `feed` | `Vec<FeedEntry>` | principals changes, any order, any repetition; applied once, in version order, from the shard's last version + 1 |
| `credits` | `Vec<(ShardId, Vec<Credit>)>` | credits by source, any order, any repetition; applied once, in sequence per source; a credit past a gap, or to an account the shard has not opened yet, waits |
| `trims` | `Vec<(ShardId, u64)>` | for each peer, the sequence it has applied this shard's credits through: the shard forgets them |
| `orders` | `Vec<Order>` | commands with their authority stamps, decided in order |

The private input (`BlobKey` names a caller blob, never stored): `agent::Private { root, openings }`
— the key ring's root and the openings (17-byte row key, 96-byte sums) of the kernel accounts the
round touches (`decide::touched_by_round`).

The reply, `agent::Reply::Round(decide::RoundReply)`:

| Field | Meaning |
|---|---|
| `feed_through` | the last principals version applied |
| `applied` | per source delivered, the last credit sequence applied: what that source may trim |
| `outcomes` | one `Outcome` per order: `Final`, `Debited { to, seq }`, `Opened`, `Issued`, `Refused(Refusal)` |
| `credits` | the credits this round's debits enqueued, committed with them |
| `root`, `commit` | the kernel's composite root and commit number after the round: the anchor's input |

## 3. Credit messages (`REQ-RT-11`, `INV-9`, `INV-18`)

`model::Credit { from, seq, op, payee, asset, amount }`. `seq` numbers the credits of one
(source, destination) pair from 1 without a gap. The source commits the debit (payer → its
in-transit position for the destination) and the credit in one transition; from then the transfer
is final. The destination applies `seq = applied_from(from) + 1, + 2, …` and posts its in-transit
position for the source → payee; it records `applied_from(from)` in the same transition. Delivery
may duplicate, reorder, drop or delay; the effect is exactly once. A carrier needs only:

- the source's outbox: `Reply.credits` of each round, or `Query::Outbound { to, after, max }`
  (committed, not yet trimmed);
- the destination's progress: `RoundReply.applied`, or `Query::Applied { from }`;
- to deliver every credit with `seq > applied` to the destination, and the destination's
  `applied` back to the source as a trim.

Today the carrier is the shard host's relay (`host::Cluster::step`, `recover`). An outbox under
consensus, or Agent-to-Agent messages, can replace it without a change to these types.

## 4. Revocation notices (`REQ-AU-3`, `NFR-20`, `c-10`)

The principals Agent (`agent::Envelope::Principals(Vec<PrincipalsCommand>)`: `Open`, `Grant`,
`Revoke`) gives every change the next version and appends `FeedEntry { version, change }`, with
`Change::Opened { account, asset }`, `Change::Granted { principal, payer, limit }` or
`Change::Revoked { principal }` (`Query::Feed { after, max }` reads it). Ingress resolves an
authority (`Query::Authority { principal, payer }` → `Authority { limit, version }`) and sends the
command as `Order { command, stamp: Some(Stamp { principal, version }) }`. A shard that has applied
`Revoked { principal }` at version `r` refuses every order of that principal stamped `< r`
(`Refusal::Revoked`). The revocation lag is the time from the principals Agent's commit to the
feed's application in every shard.

## 5. Stream heads, the cut and the anchor (`INV-19`, `REQ-RT-14`, amendment 0015)

Every round's reply carries `RoundReply.heads: cut::Heads { shard, seq, commit, root, feed, enqueued,
applied, trimmed }`: the shard's audit sequence (its rounds committed), its kernel commit and root, the last
principals version applied, and per peer the last credit sequence enqueued for it, applied from it, and trimmed
towards it (`Query::Heads` reads the same from the rows). The principals Agent's reply carries
`PrincipalsReply.digest`, the running digest of its feed; with its invocation count and version it is
`anchor::RegistryRoot { seq, version, digest }`. Both travel to the anchor's assembler as audit records
(`audit/<n>`, `audit/principals`), exactly once and in sequence.

A cut is one record per shard and the registry's (`cut::Cut`); it is consistent (`Cut::check`) when for every
ordered pair (i, j) `trimmed_i[j] ≤ applied_j[i] ≤ enqueued_i[j]` and every shard's `feed ≤ registry.version`.
`cut::Assembler` keeps each writer's records since the last anchored cut and returns the greatest consistent cut
(`Assembler::cut`); `Assembler::settle` makes an anchored cut the floor. The anchor is `Cut::anchor()`, a binary
Merkle root over `anchor::Leaf::Shard(ShardRoot { shard, seq, commit, root, heads })` per shard
(`blake2b-256("aledgent/anchor/shard/v2" ‖ …)`, `heads` the record's digest) and
`anchor::Leaf::Registry(RegistryRoot)` last, with `anchor::prove` / `anchor::verify` for any leaf. In-transit
positions are kernel accounts, so the roots commit to every balance; one position per (shard, peer) carries both
directions, so a pair's two positions hold the value of the credits in transit both ways at the cut.

A binding (`REQ-AS-8`): `PrincipalsCommand::Bind { asset }` appends `Change::Binding { asset }`; a shard applying
it refuses `Issue` in the asset (`Refusal::NotAuthorised`) and reports `RoundReply.shares: cut::Share { asset,
version, issued }` in that round (in `DEC-23`, a `settle/<n>` message); `Change::BindingEnded` releases it.
`cut::bind_amount(cut, shares, asset, version)` is the negation of the shares' sum at a consistent cut whose every
record has `feed ≥ version`.

## 6. Reads (`agent::Query`, Query mode, writes nothing)

`Hot` (role, driver, last commit, next posting, rounds, last root), `Outbound`, `Applied`, `FeedApplied`,
`Heads` (shards); `Feed`, `Authority` (principals). Answers are `agent::Answer`.
