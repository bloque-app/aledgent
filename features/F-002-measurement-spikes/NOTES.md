<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-002 — implementation notes

Written while implementing `F-002` (issue #2), 2026-09-22/23. The plan's proposals were followed as
accepted (Zipf skew, 12-month horizon, a named reference host). Where the plan was silent or could
not be followed as written on the reference host, the choice made is listed here for review. The
results and their reading are in `notes/MEASUREMENTS-F002.md`.

## Decisions taken during implementation

### Where things live

1. **Results file name.** The plan and its exit criterion name `notes/MEASUREMENTS-F002.md`; the
   implementation brief said `notes/MEASUREMENTS.md`. The plan's name was kept, so the exit
   criterion's path is literal.
2. **`spikes/` is its own workspace**, excluded from the root one (`Cargo.toml`), with a `spikes` CI
   job (fmt, clippy, tests). The VOS guest and host crates under `spikes/guest` and `spikes/vos-host`
   need the pinned VOS checkout at `/vos` and two nightlies, so CI does not build them; the README
   gives every command. They link VOS and say `AGPL-3.0-or-later` in their manifests; they are never
   distributed.

### The rules

3. **Rule choice 7 withdrawn before any guest run** (`notes/MEASUREMENTS-F002.md` §2.1, committed
   separately as `9aac1f3`). Its premise — guest time here inflated by a missing recompiler — is
   false for Agent invocations, which are interpreter-only on every target.

### S2 — the kernel in the guest

4. **The unit's pre-state travels in the message, not in actor state.** The plan says the actor
   applies a pre-built unit to "an in-state `MemStore`". `ledger-kernel` (and so `MemStore`) cannot be
   built for the guest, because its `signing` dependency pulls `getrandom` (finding S2-1). The probe
   therefore runs CipherClerk's own `apply_account_creations`/`apply_batch` over a `VecLedger` of the
   touched leaves, which is the kernel work the plan meant to isolate; the kernel's statuses and the
   `VecLedger` root are checked against the host's run of the same input, and the host's `Book`
   applies the same unit.
5. **Guest batch sizes.** Batch 1 for every shape, and 4 and 10 for transfers and 4 for pooled
   reserves. Batch 64 was not built: at about 1.2 KB per posting it is about 75 KB, over the 16 KiB
   invocation message (a finding, not an omission).
6. **Sample counts in the guest.** 1 000 invocations for the three shapes that enter the budget
   (transfer and both reserve options) and for S3 at 1 000 000 accounts; 100 for the other shapes and
   for S3 at 10 000 and 100 000. Guest gas is exactly repeatable (it is reported per phase and does not
   vary), and each guest invocation takes about half a second of interpreter time on this host.

### S3 — witnessed leaves

7. **FileStore at 10 000 accounts only.** `FileStore` writes 128 node rows per leaf; the 10 000-account
   book (50 003 leaves) is 1.03 GiB, 0.85 GiB after compaction, about 18 KB per leaf. At 100 000 and
   1 000 000 accounts that is 9 and 90 GB, beyond the host's free disk. Those two books are built,
   through the same `Book::apply`, over `IndexStore`, a spike-only in-memory `LeafStore` rooted by a
   compact index. A test builds the same seeded book on both stores and checks the roots agree, and
   the index is checked against CipherClerk's `SparseMerkleTree`/`BatchProof`.
8. **The witness is built from the index, then decoded as CipherClerk's own type.** CipherClerk's
   producer (`SuccinctTransitionWitness::from_full`) needs the whole book as a `VecLedger` and re-hashes
   it per witness; `FileStore` has no proof read. The spike serialises a field-for-field mirror and
   decodes it as `SuccinctTransitionWitness`; every witness is then checked by CipherClerk's own
   `verify_transition`, and the root after, derived from the proofs alone, is asserted equal to the
   store's root after commit.
9. **The per-operation reserve is "creation beside the witness".** CipherClerk's succinct witness
   covers `apply_batch` only. For a reserve into a new holding position the witness proves the
   position absent at `root_before`, carries the signed creation, and the chain is verified from the
   post-creation root derived from the same proofs.
10. **Point reads at 100 000 and 1 000 000** are measured on a leaf-only redb table of the accounts
    (leaf bytes and leaf hash, checked on read as `FileStore` checks a leaf against its node), since
    there is no `FileStore` at those sizes. They are a first number toward `NFR-3`, not a test of it.

### S4 — one Local Agent invocation

11. **Two paths.** (a) The bundled standard AgentRuntime driven through its canonical ABI, exactly as
    the Local host's journal driver does, with the same outer gas and the same preparation cache,
    recording the driver's own debug fields: this is where 1 000 invocations per size were possible.
    (b) `vosx space invoke-local`, the canonical CLI path, on a real Local Agent: 12 invocations
    completed before the Authority path returned 503 (finding S4-3).
12. **State held as 32 KiB storage rows, written two rows per invocation.** One inline field may not
    exceed 48 KiB, and an invocation that writes four 32 KiB rows panics the actor.
13. **The post-fill state is restored every window of invocations.** The standard runtime retains at
    most 32 positive acknowledgements and 32 unacknowledged results per lane and evicts neither
    (finding S4-2). Measuring 1 000 invocations on one Agent is impossible without restoring the
    state; restoring the same state is deterministic and does not change what one invocation costs.
    The largest state reached is 3.93 MiB (4 121 295 B), filled by 62 invocations of which 31 are
    acknowledged and 31 left as results.

### Machine sharing

14. **Runs overlapped.** To finish in a day, guest runs in two containers, the 1 000 000-account
    generation and the VOS builds shared the machine (as did unrelated containers of the owner's).
    p50s are stable; p95/p99 carry that noise. S1's first run, made while the harnesses were still
    being built, was discarded; its recorded run overlapped the S3 guest runs and the 1–4 MiB S4 runs in the
    Linux VM (at most two single-threaded processes).
