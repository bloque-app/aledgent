<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# spikes — F-002 measurement code

Measurement code for `F-002` (issue #2). **Never shipped:** no crate under `crates/`, `agents/` or
`services/` may depend on anything here. Results and their reading live in
`notes/MEASUREMENTS-F002.md`; raw samples in `results/`.

`spikes/` is **not** a member of the root workspace (it would add Criterion to every CI build). The
native half is its own workspace, built, linted and tested by the `spikes` CI job:

    cargo clippy --manifest-path spikes/Cargo.toml --all-targets -- -D warnings
    cargo test   --manifest-path spikes/Cargo.toml --release

The VOS half (`guest/`, `vos-host/`) needs the pinned VOS checkout and two nightly toolchains, so CI
does not build it; the commands are below.

## Layout

| Path | What | Toolchain |
|---|---|---|
| `native/` | S1 (`s1`, Criterion bench `s1_apply`), S2 inputs and the native kernel figure (`s2prep`), S3 (`s3`): shapes, golden vectors, witness index, stats, CSV | stable, this workspace |
| `guest/kernel-probe/` | S2 and S3 in the guest: CipherClerk inside a VOS `saga` Agent actor | `nightly-2026-03-20` + `rust-src`, `riscv64em-vos` |
| `guest/state-blob/` | S4: an Agent actor holding a Linear state of chosen size | same |
| `guest/curve-bench/` | F-025: a bare guest timing each curve precompile operation as the runtime computes it | same |
| `guest/build-precompile-current.sh` | F-025: `kernel-probe` with `pvm-precompile` on the current VOS shim | same |
| `vos-host/` | Drives VOS `saga`'s standard AgentRuntime through its canonical ABI (S2, S3 guest, S4), writes ATQ1 intents for `vosx`, parses `vosx` debug logs | `nightly-2025-05-09` (VOS's own) |
| `vos-external/` | The same series through VOS's opt-in external-state runtime (`experimental-state-blocks`): a patch adding one measurement test to VOS's own lifecycle fixture, and its runner (MEASUREMENTS-F002 §11) | both nightlies |
| `results/` | raw samples, one CSV per run; `witness/` and `guest/` hold the inputs the guest runs | — |

`guest/` and `vos-host/` link VOS, which is AGPL-3.0-or-later; their manifests say so. They are
measurement tools and are never distributed.

## Result format

One row per sample, in `results/<run>.csv`:

    spike,case,variant,size,sample,metric,value,unit

`value` is an unsigned integer in `unit` (`ns`, `us`, `gas`, `bytes`). Percentiles are nearest-rank.

## Reproducing

Reference host and pins: `notes/MEASUREMENTS-F002.md` §1. Release profile throughout.

### Native (macOS or Linux, stable Rust)

    # S1: Book::apply over every shape and batch, FileStore in memory and on disk (about 75 min)
    cargo run --release --manifest-path spikes/Cargo.toml --bin s1
    # the same shapes as Criterion benches (regression, not the recorded tables)
    cargo bench --manifest-path spikes/Cargo.toml --bench s1_apply

    # S2 inputs and the kernel alone, natively, on exactly those inputs
    cargo run --release --manifest-path spikes/Cargo.toml --bin s2prep

    # S3: generate (seeded) and measure. 10 000 accounts in FileStore on disk (about 1 GB);
    # 100 000 and 1 000 000 in the spike's IndexStore (FileStore at those sizes needs 10–100 GB)
    cargo run --release --manifest-path spikes/Cargo.toml --bin s3 -- --store file  --accounts 10000
    cargo run --release --manifest-path spikes/Cargo.toml --bin s3 -- --store index --accounts 100000
    cargo run --release --manifest-path spikes/Cargo.toml --bin s3 -- --store index --accounts 1000000
    # the capture for less that F-004 asked for, on the same seeded books
    cargo run --release --manifest-path spikes/Cargo.toml --bin s3 -- --store index --accounts 1000000 --capture-only

`s3` writes each witness and its roots to `results/witness/` for the guest run.

### VOS (Linux; on macOS, a Docker `aarch64` Linux container)

Local Agents are Linux-only on `saga` (`vos/src/agent/host.rs:31`). The runs used an image based on
`rust:1-bookworm` with `clang lld pkg-config libssl-dev cmake protobuf-compiler`, and the toolchains
`nightly-2025-05-09` (profile minimal) and `nightly-2026-03-20` (with `rust-src`). Mounts:

| Container path | Holds |
|---|---|
| `/vos` | VOS `saga/agents` at `7bae92694640599d5f803c8f6a40c6138a56e844`, read-only |
| `/spikes-guest` | `spikes/guest` |
| `/spikes-vos-host` | `spikes/vos-host` |
| `/out` | built packages |
| `/results` | `spikes/results` |

Then, inside the container:

    # vosx itself (LTO off to keep the build short)
    cd /vos && cargo build --locked --release -p vosx \
        --config 'profile.release.lto="off"' --config 'profile.release.codegen-units=16'
    export PATH=$CARGO_TARGET_DIR/release:$PATH

    # guest packages (signed VOS3): the scaffolding comes from `vosx actor new`
    bash /spikes-guest/build.sh state-blob
    bash /spikes-guest/build.sh kernel-probe
    bash /spikes-guest/build.sh kernel-probe signing      # S2 finding: fails (getrandom)
    bash /spikes-guest/build.sh kernel-probe precompile   # builds; panics at run time

    # S4 through the runtime ABI, four state sizes, 1000 invocations each
    FILL_ROWS=2 bash /spikes-vos-host/run.sh s4 /out/state-blob.vos 65536,1048576,2097152,4063232 1000 /results/s4.csv
    # the retention cap: where the 33rd acknowledgement and the 65th invocation are refused
    bash /spikes-vos-host/run.sh retention /out/state-blob.vos 100
    # S2 in the guest (kernel and no-op baseline counts, then the inputs from s2prep)
    bash /spikes-vos-host/run.sh s2 /out/kernel-probe.vos 1000 200 /results/s2-guest-budget.csv \
        /results/guest/s2-transfer-b1.bin /results/guest/s2-reserve-new-holding-b1.bin /results/guest/s2-reserve-pooled-b1.bin
    # S3 in the guest (the witnesses from s3)
    bash /spikes-vos-host/run.sh s3 /out/kernel-probe.vos 1000 0 /results/s3-guest.csv /results/witness/*-1000000.bin

    # S4 through `vosx` itself (the canonical path), one Local Agent:
    vosx space new demo && (RUST_LOG=info,vos::agent=debug vosx space up demo > daemon.log 2>&1 &)
    vosx space create-local-agent demo                     # prints AGENT
    vosx space install-local-actor demo AGENT /out/state-blob.vos --name state-blob
    vos-host atq1 ~/.config/vosx/identity.key SPACE AGENT fill  2 0 2   intents
    vos-host atq1 ~/.config/vosx/identity.key SPACE AGENT touch 2 0 100 intents
    vosx space invoke-local demo --intent intents/fill-00000.atq1   # and fill-00001
    bash /spikes-vos-host/vosx-e2e.sh demo intents 0 70 /results/s4-vosx.csv
    bash /spikes-vos-host/parse-debug.sh daemon.log > /results/s4-vosx-debug.csv

`vos-host` builds itself on each `run.sh` call, against `/vos`, with VOS's own `Cargo.lock`.

### Re-check on `2bb7876` (MEASUREMENTS-F002 §11)

The same container, with VOS `saga/agents` at `2bb78760df07a47aaa92b1fb74998815195ebab7` mounted at
`/vos` instead. `vos-host/run.sh` adds the manifest field that commit introduced when the mounted VOS
has it, so the commands above run unchanged; results are written with a `-2bb7876` suffix. Two
additions:

    # S4 with 31 unacknowledged results retained at every size (the §11.4 control)
    F002_RETAIN=31 FILL_ROWS=2 bash /spikes-vos-host/run.sh s4 /out/state-blob.vos \
        65536,1048576,2097152,4063232 100 /results/s4-retain31-2bb7876.csv

    # The external-state path: apply the patch to the VOS checkout first
    #   git -C <vos> apply <aledgent>/spikes/vos-external/f002-external-state.patch
    # and mount spikes/vos-external at /spikes-vos-external
    bash /spikes-vos-external/run.sh build
    F002_MODE=s4 F002_N=1000 F002_SIZES=65536,1048576,2097152,4063232 \
        F002_OUT=/results/s4x-2bb7876.csv bash /spikes-vos-external/run.sh run /out/state-blob.vos
    F002_MODE=s4 F002_N=100 F002_RETAIN=31 F002_SIZES=65536,1048576,2097152,4063232 \
        F002_OUT=/results/s4x-retain31-2bb7876.csv bash /spikes-vos-external/run.sh run /out/state-blob.vos
    F002_MODE=s2 F002_N=1000 F002_N_NOOP=200 \
        F002_INPUTS=/results/guest/s2-transfer-b1.bin,/results/guest/s2-reserve-new-holding-b1.bin,/results/guest/s2-reserve-pooled-b1.bin \
        F002_OUT=/results/s2x-guest-budget-2bb7876.csv bash /spikes-vos-external/run.sh run /out/kernel-probe.vos
    F002_MODE=s3 F002_N=100 F002_N_NOOP=0 \
        F002_INPUTS=/results/witness/transfer-1000000.bin,/results/witness/reserve-pooled-1000000.bin,/results/witness/reserve-new-holding-1000000.bin \
        F002_OUT=/results/s3x-guest-1000000-2bb7876.csv bash /spikes-vos-external/run.sh run /out/kernel-probe.vos

The external runs use the packages `guest/build.sh` produces against `2bb7876`. The patch changes no
VOS behaviour: it adds an ignored test and, in test builds only, records gas and load/run time in
`run_block_program`.

### F-025 changes (MEASUREMENTS-F002 §12)

Before and after runs for the VOS changes in `proposals/vos/` (`T-025-01`, `T-025-02`). The same
container image, with VOS `saga/agents` at `2bb7876` and the two fork branches
(`agent/compact-acknowledged-results`, `agent/curve-precompile-dispatch`) checked out side by side.
`vos-host` is always built against the compaction branch (its host-side API is a superset); the
runtime under test is chosen with `VOS_RUNTIME_PVM`. Four additions to the harness:

- `VOS_RUNTIME_PVM=<path>` selects the AgentRuntime program (the bundled one by default).
- `vos-host runtime-pvm <agent_runtime.elf> <out.pvm>` links a runtime guest ELF exactly as
  `vosx agent-runtime-pvm` does (`vos_pvm_compiler::link_elf_spi`, then the outer host-call check).
- `F002_SEQUENCED=1` gives the Linear invocations sequenced identities (the layout of
  `vos_agent_sdk::SequencedInvocation`), acknowledges every fill and stops resetting the state
  between windows, which the compaction change makes unnecessary.
- `vos-host curve-bench <curve_bench.elf> <n>` reports the interpreted gas of each curve precompile
  operation from `guest/curve-bench`, a bare guest built with the runtime's own target, linker script,
  profile and `curve25519-dalek` release.

Each runtime is built from its checkout the way `just build-agent-runtime-candidate` does, then linked:

    cd <vos>/services/agent-runtime
    CARGO_TARGET_DIR=/target/agent-runtime-<tag> cargo +nightly-2026-03-20 actor --locked
    vos-host runtime-pvm /target/agent-runtime-<tag>/riscv64em-vos/release/agent_runtime.elf \
        /out/agent_runtime-<tag>.pvm

`<tag>` is `base` (`2bb7876`), `compact` or `curve`. The `base` build is not byte-identical to the
bundled `agent_runtime.pvm`, which is pinned to the older runtime source `2ccfacb`, so "before" is the
`base` build, not the bundled program. The guest packages are rebuilt against the curve branch
(`build.sh kernel-probe`, `build.sh state-blob`), plus the precompile probe on the current shim:

    # CipherClerk e9a7b08 with one line changed, mounted at /cc (see the script)
    bash /spikes-guest/build-precompile-current.sh

Then, one run at a time:

    VOS_RUNTIME_PVM=/out/agent_runtime-base.pvm    vos-host retention /out/state-blob.vos 100
    VOS_RUNTIME_PVM=/out/agent_runtime-compact.pvm F002_SEQUENCED=1 \
        vos-host retention /out/state-blob.vos 10000
    vos-host curve-bench /out/curve_bench.elf 20
    # S2 (200 kernel and 50 no-op invocations per shape) and S3 at 1 000 000 accounts (200 each)
    VOS_RUNTIME_PVM=/out/agent_runtime-<base|curve>.pvm vos-host s2 /out/<kernel-probe|kernel-probe-precompile-current>.vos \
        200 50 /results/<out>.csv /results/guest/s2-transfer-b1.bin /results/guest/s2-reserve-new-holding-b1.bin \
        /results/guest/s2-reserve-pooled-b1.bin
    VOS_RUNTIME_PVM=... vos-host s3 /out/<probe>.vos 200 0 /results/<out>.csv /results/witness/*-1000000.bin
    # S4 (200 invocations per size), and the retained-results control on the base runtime
    VOS_RUNTIME_PVM=/out/agent_runtime-base.pvm vos-host s4 /out/state-blob.vos 65536,1048576,2097152,4063232 200 /results/<out>.csv
    F002_RETAIN=31 VOS_RUNTIME_PVM=/out/agent_runtime-base.pvm vos-host s4 ... 100 /results/<out>.csv
    F002_SEQUENCED=1 VOS_RUNTIME_PVM=/out/agent_runtime-compact.pvm vos-host s4 ... 200 /results/<out>.csv

Raw samples: `results/*-f025.csv`.

### F-025 changes 3–4 (MEASUREMENTS-F002 §13)

Before and after runs for `proposals/vos/0004` (`T-025-03`, branch `agent/prepared-actor-cache`) and
`proposals/vos/0003` (branch `agent/actor-declared-limits`). Same container image and toolchains as
above. VOS `saga/agents` at `2bb7876` is "base"; each change is its own checkout. Four more harness
commands:

- `vos-host profile <package.vos> <actor-name> <out.tsv> [payload-bytes]` runs one warm-up and one
  profiled Invoke through `RefineContext::run_observed`. It prints the runtime's gas before the
  `machine` call, from `machine` to the first `invoke`, between invokes and after the last one,
  and the gas of each inner-machine host call. It writes `pc<TAB>gas` rows for VOS's own
  diagnostic, `resolve_observed_standard_program_pcs` (`pvm/compiler/src/linker.rs`). That
  diagnostic maps PVM counters to RISC-V addresses of the exact runtime ELF, and a symbol-table
  lookup names the functions.
- `vos-host prep-bench <package.vos> <n>` times the native work the host does for an actor's inner
  machine on every invocation: `machine` (deblob) and the first use (predecode).
- `vos-host pad-package <in.vos> <out.vos> <program-bytes>` (only against a VOS with declared
  actor resources) grows the program to a given size (or one byte over). It appends unreachable `trap`s after the
  code, then declares the resources and re-signs the package. With `<program-bytes>` equal to
  the current size it only declares and re-signs.
- `F025_COMPARE_RUNTIME=<other.pvm> vos-host compare <package.vos> <actor-name> <n>` runs every
  execution (Create, Install, `n` ≤ 30 Invoke and Acknowledge pairs) on both runtimes from the same
  input and asserts that their transitions are byte-identical. The variable works with any command.
- `aledgent-core-host measure` (F-005, `agents/core-host`) for the reservation path, built against
  a VOS tree through `--config` with a `[patch."https://codeberg.org/pandres95/VOS.git"]` table
  (`vos`, `vos-pvm`, `vos-pvm-compiler` pointing at the tree).

The profile, change 3 (`base` against `cache`), with the packages of §7 (`state-blob` 66 469 B,
`kernel-probe` 291 219 B):

    VOS_RUNTIME_PVM=/out/agent_runtime-<base|cache>.pvm vos-host profile /pkgs/state-blob.vos state-blob /out/prof-sb.tsv
    VOS_RUNTIME_PVM=... vos-host profile /pkgs/kernel-probe.vos kernel-probe /out/prof-kp.tsv 1600
    # PCs covering 99.5 % of the gas, then, in the VOS checkout the runtime was built from:
    VOS_PROFILE_ELF=agent_runtime.elf VOS_PROFILE_PVM=agent_runtime.pvm VOS_PROFILE_PCS=<pcs> \
      cargo test --release -p vos-pvm-compiler --lib resolve_observed_standard_program_pcs -- --ignored --nocapture

The before and after runs, interleaved, one at a time, twice. `vos-host` is built against the checkout
whose host side is under test, so the "after" harness has the inner-program cache:

    vos-host s2 /pkgs/kernel-probe.vos 200 200 <out.csv> /results/guest/s2-reserve-new-holding-b1.bin
    vos-host s4 /pkgs/state-blob.vos 65536 200 <out.csv>
    aledgent-core-host measure agent_runtime.pvm core-full.vos <out.csv> --n 100 --warmup 3

The F-005 "before" tree is F-005's own: the fork's `agent/compact-acknowledged-results` (`de0f12f8`)
with both program limits at 2 560 KiB. "After" is the same tree with change 3's diff applied.

Replay equivalence of change 3, base against `cache`:

    VOS_RUNTIME_PVM=/out/agent_runtime-base.pvm F025_COMPARE_RUNTIME=/out/agent_runtime-cache.pvm \
      vos-host compare /pkgs/state-blob.vos state-blob 30      # and kernel-probe

Change 4 (`declared`), on `2bb7876` plus change 4 alone ("both" adds change 3). The arena probe runs a
`state-blob` padded to each size at 64 KiB and at the state ceiling, with the constant under test
raised in the checkout:

    vos-host pad-package /pkgs/state-blob.vos /out/sb-<bytes>.vos <bytes>   # 1572864 … 2621440
    VOS_RUNTIME_PVM=/out/agent_runtime-<declared|both>.pvm vos-host s4 /out/sb-<bytes>.vos 65536,4063232 5 <out.csv>

The core runs on the fork's compaction branch plus change 4, because F-005's host uses sequenced
identities. `agents/core-declared` is a scratch copy of `agents/core` whose `pvm.ld` puts read-only
data at `0x10000` and sets `__vos_stack_size = 0x40000`, and whose `vos` dependency enables
`actor-heap-4m` through a `[patch]` to that tree. Its `core-host package` declares the resources
above the standard size, as `vosx actor build` does:

    aledgent-core-host package core-declared.elf core-stack.vos
    aledgent-core-host measure agent_runtime.pvm core-stack.vos <out.csv> --n 100 --warmup 3

Raw samples: `results/*-f025b.csv`.

### F-025 change 5 (MEASUREMENTS-F002 §14)

Recompiled Agent execution, `proposals/vos/0005` (`T-025-06`, branch `agent/recompiled-agent-execution`).
The recompiler emits x86-64 only, so these runs use a **`linux/amd64`** container
(`rust:1-bookworm` with `nightly-2025-05-09`, `docker run --platform linux/amd64`). On Apple
silicon, Docker runs it under Rosetta 2. **Times under emulation are not results.** They are
reported only as a ratio between the two engines under the same emulation, run one at a time and
interleaved. `run.sh` sets `--cfg vos_backends` when the mounted VOS has `RefineBackend`, which
enables:

- `F025_BACKEND=recompiler` with any command, to run every execution natively.
- `F025_LOCKSTEP=1` with any command, to run every execution on both engines and assert the same
  exit, counter, registers, gas and output bytes.
- `vos-host backends <package.vos> <actor-name> <n> [<case.bin>]`. Create, Install and `n` ≤ 30
  Invoke and Acknowledge pairs in lockstep. Then one more Invoke is compared across engines under
  two cuts: by the runtime's outer budget (129 even points over the whole range and 129 below the
  least budget that halts, found by bisection, plus every unit around it), and by the actor's
  budget (128 even points and every unit around what the actor uses).
- `vos-host compile-bench <package.vos> <n>`. The host's one-off preparation of the runtime and
  of the actor: interpreter predecode, then native compilation.

Lockstep and timing (`agent_runtime-base.pvm` is the standard runtime built from `2bb7876`, as in §13):

    F025_LOCKSTEP=1 VOS_RUNTIME_PVM=/out/agent_runtime-base.pvm vos-host backends /pkgs/state-blob.vos state-blob 30
    F025_LOCKSTEP=1 VOS_RUNTIME_PVM=... vos-host backends /pkgs/kernel-probe.vos kernel-probe 30 \
      /results/guest/s2-reserve-new-holding-b1.bin
    F025_BACKEND=<interpreter|recompiler> vos-host s4 /pkgs/state-blob.vos 65536 100 <out.csv>
    F025_BACKEND=<interpreter|recompiler> vos-host s2 /pkgs/kernel-probe.vos 100 100 <out.csv> \
      /results/guest/s2-reserve-new-holding-b1.bin
    vos-host compile-bench /pkgs/kernel-probe.vos 5

The core (`aledgent-core-host measure`, F-005) is built against a scratch VOS tree: the fork's
compaction branch (`de0f12f8`, which F-005's host needs), with change 5's diff applied and both
program limits raised to 2 560 KiB, as §13's "before" tree. The runtime is §13's
`agent_runtime-limits.pvm`, because change 5 does not touch the runtime guest. The host is
patched in a scratch copy only, not in `agents/core-host`: `PreparedProgram::with_backend` takes
the engine from `F025_BACKEND`, and with `F025_LOCKSTEP=1` every execution also runs cold on the
other engine and must match.

    F025_LOCKSTEP=1 F025_BACKEND=recompiler aledgent-core-host measure agent_runtime-limits.pvm core-full.vos <out.csv> --n 100 --warmup 3
    F025_BACKEND=<interpreter|recompiler> aledgent-core-host measure ...   # twice each, interleaved

VOS's own tests, in the same container:

    cargo test -p vos-pvm                                  # includes tests/refine_backends.rs
    cargo test -p vos --test agent_runtime_pvm
    cargo test -p vos --lib

Raw samples: `results/*-f025c.csv`, logs in `results/logs/*-f025c.log`.

### F-025 change 5 on real x86-64 (MEASUREMENTS-F002 §15)

The same runs on a self-hosted x86-64 runner, natively, through `.github/workflows/x86-bench.yml`
(manual, one run at a time; `.github/x86-bench/run.sh` holds the steps):

    gh workflow run x86-bench.yml -f mode=vos-suites           # VOS's suites, both engines
    gh workflow run x86-bench.yml -f mode=reservation-timing   # the core: lockstep, then timing

Raw samples: `results/core-*-x86.csv`, logs and both job summaries in `results/logs/*x86*`.
