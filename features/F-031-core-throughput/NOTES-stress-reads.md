<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-031 — stress fixes: reads off the decision lock, the API-key cache, memory

Notes of `T-031-12` to `T-031-14`: findings F3, F4 and F6 of `F-032` (in the stress workflow's run summaries).
To be folded into this feature's `NOTES.md`.

## T-031-12: reads off the decision lock (F3)

Both cores decide under an exclusive lock and answer reads under a shared one: `ingress::gate::Gate`,
a read-write lock that lets a waiting writer in before any reader that arrives after it.

- **Writer preference is explicit.** `std::sync::RwLock` leaves its policy to the platform (Linux's
  futex implementation prefers writers; the queue elsewhere promises nothing), so the gate does not
  rely on it: a writer announces itself, and new readers wait until it holds the lock. A writer waits
  for at most the reads already in flight. The workspace's `parking_lot` is only a transitive
  dependency; adding it directly would have changed the `agents/` workspace's lock file too.
- **`DEC-4` holds on the Agent core.** An input takes the executor out of the gate for its whole run
  (native run, the Agent's commit, or the rebuild after a refusal), so a read never answers at a
  replica state the Agent has not committed. `RecStore` and `RecVault` record through a mutex instead
  of a `RefCell`, so the replica is `Sync`.
- **Reads leave the async runtime's threads** (`spawn_blocking`) on both cores, so a read waiting
  for an invocation does not hold a tokio worker.

Measured (`services/ingress/tests/reads.rs`, M2 Pro, debug build, each input journaled with a
2 ms sync): about 333 decisions a second alone and about 296 a second under about 5 500 reads a
second from 16 readers. A first version over the file journal and 32 readers gave about 190 and 170
locally, but failed on CI's 2-vCPU runner (2 990 alone, 257 under the storm): there the readers took
the CPU from the decisions, which is not what the test is about, so the decision path now waits on
I/O, not the CPU. With the old single mutex the same storm let nothing through: a 1.5 s window of
decisions did not complete in three minutes behind the file journal (ten behind a journal in
memory). On the Agent core over the native port with 5 ms added per invocation: 107 decisions a
second alone, 106 under 2 300 reads a second.

**Still open (deferred).** On the core Agent a read still waits for the invocation in flight: it is
answered at the committed state, and that state is only known once the Agent has committed. Serving
reads from a snapshot of the last committed state (a copy-on-write replica) would take reads off the
invocation's time too; clients that poll should move to notifications or a long-poll
(`throughput.md` §5).

## T-031-13: the API-key cache and the acknowledgement series (F4)

- `ingress::credentials::VerifierCache`, held by the `Authenticator`: what the core answered for an
  API key's credential (its public body, or "no working credential"), reused for a TTL. Default
  500 ms; `api_key_cache_ms` in the deployment's configuration; 0 reads the core every time; above
  1 000 ms (`NFR-20`, amendment 0012, proposed) ingress refuses to start. The TTL runs from before
  the read was sent. At most 65 536 entries; a failed core read is not cached; the cache holds
  verifiers (hashes), never a secret.
- **What the lag covers.** Only ingress's authentication step: the core still checks the credential
  on every command and every query (`REQ-AU-3`), so a removed key's queries were already refused at
  once, and its commands are refused at decision. Within the TTL, a removed key can still have a
  command logged and acknowledged; it is then refused. That is the short lag the owner accepted
  (ruling split-principals, 2026-10-03).
- **`NFR-1`'s series starts at arrival**, before authentication (`surface::handle_command` passes
  its `started` into `command_reply`).

Measured: a removed key stopped authenticating 512 ms after the removal with the 500 ms default, at
once with 0 (`tests/api_keys.rs`); with a 150 ms lookup the acknowledgement series reads 151 ms,
where before it started after authentication.

## T-031-14: memory (F6)

`crates/core-binding/tests/memory.rs` records a realistic workload through a deployment
(onboarding, transfers between holders, reservations and captures, issuance, ticks, delivery
records), replays it input by input into each core under a counting allocator (`peak_alloc`), and
reports the bytes held per command over the second half of the run.

**What dominated.** The book's leaves and their Merkle nodes: six depth-128 sparse Merkle trees
whose node tables gain about a hundred nodes per new leaf. In the in-process core the kernel's
`MemStore` alone was 26.4 KB of the 32 KB per command; in the core host the replica's
`BookStore` and service tree, in redb's in-memory backend, were the same share. The replies were
1.3–1.5 KB per command; the service store's leaves are about 0.8 KB per command canonically.

**What changed.** Both disk modes are opt-in since #139: the stress suite measured them slower
than the memory they save is worth on a 2-vCPU runner (below, "Measured under load").

1. The in-process core can keep its leaves in a file: `InProcessCore::on_disk` over `F-001`'s
   `FileStore`, opened with the new `FileStoreOptions` (unsynced commits, a bounded redb cache,
   64 MiB by default). The book is rebuilt from the journal on every start anyway, so the factory
   deletes the file first and its commits need no fsync; the journal is what is durable. The
   openings vault stays in memory (one row per account). `aledgent-ingress serve` uses it when
   `core_book` names a file; absent, the leaves stay in memory.
2. The core host can keep the replica's stores on disk (`serve --replica-on-disk`): `ReplicaStores::Disk` (`AgentCore::open_in`,
   `Driver::found_in`, `Driver::recover_in`, `replay_in`), under `--state <dir>/replica/`, three
   redb files with an 8 MiB cache each (32 MiB at first: an hour's soak did not fill it), an archive with `fsync: false`, wiped and rebuilt on every
   start and after every refused input. New constructors `RedbEngine::open_cached` and
   `OpeningsStore::open_cached` set the cache (redb's default is 1 GiB per file).
3. The Agent core keeps the 4 096 most recent replies in memory and every key it answered; an older
   reply is read from the log (`HostLog::reply`) when its key is retried, and a key answered whose
   reply the log cannot give is `Unavailable`, never run again (`INV-18`).

**Before and after**, bytes held per command (debug build, 1 MiB cache per file so that a short run
shows the slope a long one has once the cache is full):

| Run | In-process, leaves in memory | In-process, on disk | Core host, replica in memory | Core host, on disk |
|---|---:|---:|---:|---:|
| 12 rounds (481 commands, 1 220 inputs) | 32 040 | 4 903 | 27 218 | 4 547 |
| 60 rounds (2 257 commands, 5 996 inputs) | 29 422 | 3 380 | 27 590 | 2 815 |

In the 60-round run the core host's recent replies are full (4 096), so its 2.8 KB is what stays:
the replica's service store and the key index. The in-process core's 3.4 KB includes its replies
(below).

**Trade-offs, stated plainly.**

- **Disk instead of memory.** The in-process core's leaf file grows by about 35 KB per command
  (redb pages, unsynced commits whose freed pages are not reused), the core host's replica by about
  15 KB. At the soak's 2 × the reference hour that is about 4.8 GB in four hours for the in-process
  core. Both are rebuilt on every start, so a restart reclaims them; a long-running deployment needs
  the disk, or a store whose nodes are compressed (single-leaf subtrees not stored node by node),
  which is a kernel and leaf-store change, not done here. Syncing one commit in 256 to let redb reuse
  pages made the file larger in a short run, and was dropped.
- **The in-process core keeps every reply** (about 1.3 KB per command). It has no durable reply log
  to fall back to: a reply evicted and then asked for again could only be answered by running the
  input again, which `INV-18` forbids. Deferred: a reply log beside the journal, or answering from
  ingress's decided row.
- **The service store stays in memory** (`ledger_core::state::State`), by design today: about 0.8 KB
  per command canonically, roughly 2 KB in memory with its maps. Not rewritten here.
- The core host's archive (`archive.bin`) and replies (`replies.jsonl`) were already on disk.

## Measured under load: why the disk modes are opt-in

The stress suite on GitHub's 2-vCPU runners (`F-032`), the same code with the in-process core's
leaves on disk or in memory:

| Run | Leaves | Decided | Ack p99 | Decided reply p99 | Completion p99 | Errors | Memory |
|---|---|---:|---:|---:|---:|---:|---|
| Sustained 10 ×, 10 min (37123728717) | disk | 79 % of the offered rate | 2 231 ms | 30 s | 130 s | 93 ‰ | — |
| Sustained 10 ×, 10 min (37123730023) | memory | all offered | 6.1 ms | 18.7 ms | 56 ms | 0 | — |
| Soak 2 ×, 1 h (37123728717) | disk | all offered | 1 769 ms | 344 ms | 369 ms | 0 | 15 → 519 MB |
| Soak 2 ×, 1 h (37123730023) | memory | all offered | 3.6 ms | 10.6 ms | 54 ms | 0 | 15 → 923 MB |
| Soak 2 ×, 4 h (37109753823) | disk | all offered | 65 ms | 232 ms | 358 ms | 0 | 14 MB → 965 MB (before: → 3.6 GB) |

On disk the core no longer holds 10 × the reference hour, and at 2 × it misses `NFR-1`. The likely
cause is redb's write amplification (every leaf rewrites about 129 node rows, copy on write) competing
with PostgreSQL's syncs on the same disk. The core host's replica on disk added about 150 MB of
resident memory at start and slowed decisions (runs
37115138309,
37119831658), with no clear saving
within an hour at 0.1 × the reference hour.

So F6 stays open for the default configuration. The fix that does not trade memory for disk is to
stop storing the node chains that a single leaf determines: a pruned in-memory node table (below).

## The pruned node table

`ledger_kernel::store::smt::MemNodes`, the in-process core's node tables, keeps every leaf's hash
and the nodes down to depth 32 (`MEM_NODES_DEPTH`) only. A deeper node is computed when asked for:
from no leaf (a range lookup: the empty subtree), or from the leaves under its prefix, hashing up
from them (at most 96 hashes for one leaf). `set_leaf` asks for the siblings on its path; below
depth 32 a sibling holds a leaf only when another key shares more than 32 leading bits with the one
written, about `n / 2^33` of the time. `FileStore` keeps its full node table; nothing else reads
`MemNodes`.

- **Equivalence**: `the_pruned_table_answers_as_a_full_one` writes 400 keys, a third of them
  sharing 40, 64 or 120 leading bits with an earlier one and a fifth overwriting one, into a full
  table, the pruned one and CipherClerk's `SparseMerkleTree`; the roots agree after every write,
  and the pruned table answers every node of the full one with the same hash. The store suites
  (`MemStore::recomputed_root` against CipherClerk's `composite_state_root`) and the conformance
  suite run over it unchanged.
- **Memory**, the in-process core with its leaves in memory (`tests/memory.rs`, debug build): 32 040
  → 15 077 bytes per command (12 rounds); 31 015 → 15 342 (30 rounds, release). What is left of
  the kernel's store, about 10.6 KB a command, is mostly its records and its delta log
  (`LogRecord`s, read by the trial balance and the export), not nodes: cutting the table at depth 8
  instead of 32 saved only 2.7 KB more.
- **Under load** (run 37128520169,
  2-vCPU runner): sustained 10 × held, every offered command decided, acknowledgement p99 6.3 ms, no
  error (6.1 ms unpruned, run 37123730023); soak 2 × for an hour, acknowledgement p99 4.2 ms,
  decided reply 11.2 ms, no error, memory 14 → 452 MB (923 MB unpruned, 519 MB on disk at 1.8 s
  p99): about half the growth, with `NFR-1` held.
