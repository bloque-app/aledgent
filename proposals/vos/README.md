<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# VOS proposals

Changes aledgent writes for VOS and proposes upstream (`DEC-16`, `F-025`, issue #66). Each is one
branch on aledgent's VOS fork, cut from the current `saga/agents` tip, with one commit and its tests in VOS's
own suite. The numbered drafts here are the full working record of each change. The pull requests
were opened from the fork (#6–#10, below); the description to use for each is in `pr/`.

All branches start from `saga/agents` at `2bb78760df07a47aaa92b1fb74998815195ebab7` (2026-09-23; still the
tip when 0005 was cut on 2026-09-24).

| # | Draft | Branch | Fork | Open the pull request | Task |
|---|---|---|---|---|---|
| 0001 | [Compact acknowledged results](0001-compact-acknowledged-results.md) | `agent/compact-acknowledged-results` | [fork](https://codeberg.org/pandres95/VOS/src/branch/agent/compact-acknowledged-results) | [compare](https://codeberg.org/Virto/VOS/compare/saga/agents...pandres95/VOS:agent/compact-acknowledged-results) | `T-025-01` |
| 0002 | [Curve precompile on the Agent path](0002-curve-precompile-on-agent-path.md) | `agent/curve-precompile-dispatch` | [fork](https://codeberg.org/pandres95/VOS/src/branch/agent/curve-precompile-dispatch) | [compare](https://codeberg.org/Virto/VOS/compare/saga/agents...pandres95/VOS:agent/curve-precompile-dispatch) | `T-025-02` |
| 0003 | [Actor-declared program, heap and stack](0003-actor-program-and-heap-limits.md) | `agent/actor-declared-limits` | [fork](https://codeberg.org/pandres95/VOS/src/branch/agent/actor-declared-limits) | [compare](https://codeberg.org/Virto/VOS/compare/saga/agents...pandres95/VOS:agent/actor-declared-limits) | `T-025-09` |
| 0004 | [Prepared actor cache](0004-prepared-actor-cache.md) | `agent/prepared-actor-cache` | [fork](https://codeberg.org/pandres95/VOS/src/branch/agent/prepared-actor-cache) | [compare](https://codeberg.org/Virto/VOS/compare/saga/agents...pandres95/VOS:agent/prepared-actor-cache) | `T-025-03` |
| 0005 | [Recompiled Agent execution](0005-recompiled-agent-execution.md) | `agent/recompiled-agent-execution` | [fork](https://codeberg.org/pandres95/VOS/src/branch/agent/recompiled-agent-execution) | [compare](https://codeberg.org/Virto/VOS/compare/saga/agents...pandres95/VOS:agent/recompiled-agent-execution) | `T-025-06` |

### The per-invocation cost (2026-10-03), reframed for JAM (2026-10-04)

0006–0010 are commits on one branch, `perf/agent-stack` on the fork: `saga/agents` at `e6f2bb45`
with 0001–0005 merged (#6–#9, and #10's tests, since upstream now selects native Refine execution).
aledgent pins it (`agents/Cargo.toml`). Measurements: `notes/MEASUREMENTS-F031.md` §8 and, for the JAM-compatible default, §10.

**The owner's ruling of 2026-10-04**, agreed with @olanod (VOS's maintainer): VOS improvements stay within
JAM's restrictions, meaning host calls in the Gray Paper's set, so that one runtime artifact runs
off-chain, under a proof and on JAM. Each change below is sorted by that rule. The JAM-compatible
set is aledgent's default everywhere (the core image, the stress and shard harnesses, aledgent's
runtime); the parked ones stay on the fork, off by default.

#### JAM-compatible: the default

| # | Draft | Commits on `perf/agent-stack` | What it buys on aledgent's core | Why it stays within JAM |
|---|---|---|---|---|
| 0006 | [Hash the actor program once per invocation](0006-hash-the-program-once.md) | `a75b1316` | runtime gas per Invoke 1 142 → 269 M | Guest logic only: validate once, the public I/O by reference, the program by its verified `BlobRef` |
| 0007 | [A BLAKE2b compression scheduled for the PVM](0007-blake2b-for-the-pvm.md) (the compression only) | `a97f0921`, `2ef31b17` | 9 700 → 4 800 gas a block; 269 → 175 M | Guest code: the same digests, metered, no host call |
| 0008 | [JAM-native runtime profile](0008-runtime-profiles.md) (the default build) | `a97f0921` | 175 M runtime gas per Invoke | The program travels in the work and the guest verifies it; calls 9–14 only |
| 0009 | [Per-Agent execution limits](0009-per-agent-execution-limits.md) | `6c794311`, `b4dddcc0` | batches past three reservations: 11.9 → 15.4 inputs/s at 32 | Agent state and runtime bounds; no host call |
| 0010 | [The 4 608-KiB program limit](0010-native-curve-precompiles.md) (its second commit's limit only) | `b77fe2e2` | room for the `opt-level = 3` core (4.33 MB) | A runtime constant |

#### Parked: outside JAM

| # | Draft | Option | Why it is parked |
|---|---|---|---|
| 0007 | [The precompile option](0007-blake2b-for-the-pvm.md) | `blake2b-precompile`: Refine serves VOS's call 100 | Not a Gray Paper call; JIP-1 assigns 100 to `log` |
| 0008 | [VOS-native profile](0008-runtime-profiles.md) | `vos-native`: the program by `historical_lookup` | Call 7 with the Gray Paper's registers, but keyed by ProgramId (GP 0.8.0 eq. 9.5 keys a preimage by its BLAKE2b-256) and priced 600 + 248/KiB (GP: 1 125 + 264/KiB). Allowed by the ruling once it keys and prices as the Gray Paper does |
| 0010 | [The curve precompiles served natively](0010-native-curve-precompiles.md) | `native-curve`: Refine serves VOS's calls 110–114 | Not Gray Paper calls |

## Pull requests

Opened on `Virto/VOS` against `saga/agents`. The description to paste into each is in [`pr/`](pr/):
short, written for the maintainer, and without aledgent's internal identifiers. The numbered drafts
above keep the full working record.

| PR | Title | Branch | Description | Draft |
|---|---|---|---|---|
| [#6](https://codeberg.org/Virto/VOS/pulls/6) | Compact acknowledgements for sequenced invocation identities | `agent/compact-acknowledged-results` | [`pr/06-compact-acknowledged-results.md`](pr/06-compact-acknowledged-results.md) | 0001 |
| [#7](https://codeberg.org/Virto/VOS/pulls/7) | Let actors declare program, heap and stack within owner ceilings | `agent/actor-declared-limits` | [`pr/07-actor-declared-limits.md`](pr/07-actor-declared-limits.md) | 0003 |
| [#8](https://codeberg.org/Virto/VOS/pulls/8) | Load actor programs in place and reuse their preparation | `agent/prepared-actor-cache` | [`pr/08-prepared-actor-cache.md`](pr/08-prepared-actor-cache.md) | 0004 |
| [#9](https://codeberg.org/Virto/VOS/pulls/9) | Discussion: serve curve precompile ECALLs 110–114 to Agent actors (not for merge) | `agent/curve-precompile-dispatch` | [`pr/09-curve-precompile-dispatch.md`](pr/09-curve-precompile-dispatch.md) | 0002 |
| [#10](https://codeberg.org/Virto/VOS/pulls/10) | Opt-in recompiled execution for Agents, gas-exact | `agent/recompiled-agent-execution` | [`pr/10-recompiled-agent-execution.md`](pr/10-recompiled-agent-execution.md) | 0005 |

VOS's own suites and the Gray Paper v0.8.0 vectors ran on x86-64 Linux on #10's head on 2026-09-25, with no
failures (`notes/MEASUREMENTS-F002.md` §15.2). The descriptions in `pr/` were rewritten on 2026-09-29 at the
maintainer's request, in full; the numbered drafts are the working record and are not kept in step with them.

## How the branches relate

Each branch applies alone to `saga/agents`. Some share a file, never a region: 0004 and 0001
share `vos/src/agent/standard.rs`; 0003 shares `vos-agent-sdk/src/runtime.rs` and
`vos/src/agent/wire.rs` with 0001, and `vos/Cargo.toml` and `vos/src/agent/execution.rs` with 0002.
All four merge together without conflict (`git merge-tree`, pairwise and all at once).
0005 merges cleanly with 0001, 0002 and 0003. **It conflicts with 0004** in `pvm/runtime/src/refine.rs`,
`refine_host.rs` and `inner.rs` (nine hunks). Both add a per-preparation cache of inner programs, and the
two should become one. Whichever is merged second is rebased onto the first.

0003 and 0004 interact. 0003's hard maximum (1 792 KiB) is what the released runtime's
arena holds at the state ceiling. With 0004 merged, the runtime holds 2 560 KiB there, and
raising the constant is a one-line follow-up.

No branch rebuilds or repins the bundled runtime (`vosx/blobs/agent_runtime.pvm`,
`support/production-artifacts.toml`). That is the maintainer's release step once a change is accepted.

The before and after numbers are in `notes/MEASUREMENTS-F002.md` §12 (0001, 0002), §13 (0003,
0004) and §14 (0005: differential results, and timing under emulation as a ratio only). How they were
produced is in `spikes/README.md`, "F-025 changes", "F-025 changes 3–4" and "F-025 change 5".
