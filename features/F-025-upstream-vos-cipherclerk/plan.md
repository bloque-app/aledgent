<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# F-025 — Upstream VOS and CipherClerk work

| | |
|---|---|
| Milestone | M-2 — On VOS, end to end |
| Track | B |
| Home | aledgent's forks (its VOS fork, `pandres95/VOS` on Codeberg, and a CipherClerk fork), drafts in `proposals/` |
| Depends on | `F-002` (the numbers), `DEC-16` |
| Unblocks | `F-005` (compaction is needed before the conformance suite can run through an Agent). No longer a gate for `M-3` (amendment 0007) |
| Realises | `DEC-4` (ruled 2026-09-23, #38), `DEC-16` |

## Goal
The reservation decision runs fast and cheaply inside a VOS `saga` Agent. Each gap `F-002` found is
closed by a change aledgent writes and proposes upstream.

*Amendment 0007 (2026-09-29):* this goal said "fits `NFR-1`'s 50 ms p99". That was a misreading:
`NFR-1` bounds the acknowledgement of a command, and the reservation decision has no budget of its
own; it is part of completion (`NFR-15`: 30 s ceiling, 19 s target, at p99). The measured decision
(236–344 ms at p99 on x86-64 with the recompiler) is well inside it. This feature's work is now about
cost and headroom, not a gate. Every change is small, standalone and tested
in VOS's own suite, with the before and after numbers from `F-002`'s method.

## Spec traces
- `NFR-15` — completion, which the reservation decision is part of (amendment 0007; this line
  named `NFR-1`'s budget, a misreading).
- `REQ-RT-3` — deterministic, replayable execution. No change may give up determinism.
- `REQ-RT-7` — no clock, OS randomness or network I/O inside the boundary.

## What F-002 found, and the change for each

Numbers are p99 on `2bb7876` (`notes/MEASUREMENTS-F002.md` §11): 666 ms for the decision, then
measured against a 50 ms budget that amendment 0007 withdrew.

| Gap | Cost today | Change | Expected effect |
|---|---|---|---|
| Acknowledgements and results are never evicted: an Agent refuses its 33rd acknowledgement and 65th invocation. Each retained result also adds about 5 M gas to every call | Functional blocker | Compact acknowledged results (`vos/src/agent/standard.rs`) | Unbounded invocations. Removes the per-result gas |
| The curve precompile (ECALLs 110–114) is not dispatched on the Agent path | ~61.6 ms reserve + ~177.1 ms witness, interpreted | Serve the ECALLs inside the runtime, gas-metered like blake2b (`proposals/vos/0002`). Done: correct, but **slower** than CipherClerk's software path, which uses precomputed tables | None on its own. Native curve math needs an outer host call, which the runtime's design and its transition proofs rule out today, or compiled execution (T-025-06). A design question for @olanod (VOS's maintainer) |
| The runtime spends about 630 gas per byte of actor program on every call. Most of it is three BLAKE2b passes over the program, and the host also predecodes the actor natively each time. The experimental state path also reloads the 1.79 MB runtime | ~427 ms fixed | Cache the prepared runtime and actor across invocations | Done (`proposals/vos/0004`): 630 → 462 gas per byte, and the predecode happens once per executor. The core's reservation goes from 2.68 to 2.36 s at p99. What is left is the hashing, which needs a design decision upstream |
| An actor's program is capped at 1 280 KiB and its heap at 256 KiB, and a larger stack costs padding. Found by `F-005` | The core does not install | Let the actor declare its program, heap and stack within the owner's ceilings (`proposals/vos/0003`) | Done (`proposals/vos/0003`): a declared actor may reach 1 792 KiB, the most the released runtime's arena holds at the state ceiling (2 560 KiB with `0004`). The core, with its stack declared, is 1 771 777 B and runs `F-005`'s reservations on the branch with no hand-raised limit |
| Whole-state transport, about 0.9 s per MiB | Grows with hot state | Finish the experimental state-block path, without its reload | Keeps the budget as the hot state grows |
| The Authority Agent runs about nine times per canonical invocation (18–37 s, fails at the 13th) | Canonical path unusable | Keep authorisation off the per-invocation path, or cache it | The canonical path matches the ABI path |
| Agents run on the interpreter only, by design (`pvm/runtime/src/refine.rs:10-12`) | What remains after the above | **Design with @olanod first:** a deterministic compiled path for Agents, gas-metered like the interpreter | Roughly 10× on the remaining guest work (to be measured) |
| CipherClerk `signing` pulls OS randomness | Guest build fails | `proposals/cipherclerk/0001` | The kernel binding builds for the guest |
| CipherClerk accepts an entry whose currency differs from its account's | `INV-4` only builder-checked | `proposals/cipherclerk/0002` | Kernel-level check |

## How each change goes upstream
1. A branch on the fork, from the current upstream `saga/agents` tip, one change per branch.
2. Tests in VOS's own suite, plus the `F-002` measurement before and after, recorded in `notes/MEASUREMENTS-F002.md`.
3. A pull request description drafted in `proposals/vos/` or `proposals/cipherclerk/`.
4. The owner opens the pull request from the fork. Nothing is merged upstream by aledgent.
5. `F-005` pins the upstream commit once merged, or the fork branch meanwhile, recorded in its notes.

## Open points
- The compiled-execution change touches VOS's determinism guarantee. It starts as a design note for VOS's
  maintainer, not a patch.
- If upstream declines a change the budget depends on, the owner rules again on `DEC-4` (`RISK-2`).
