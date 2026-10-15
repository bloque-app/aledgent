<!--
Copyright (c) 2026 Pagos Cube LLC
SPDX-License-Identifier: CC-BY-4.0
-->

# agents/ — aledgent's VOS side (F-005)

Everything here links VOS and is **AGPL-3.0-or-later**. The rest of aledgent is Apache-2.0 OR MIT
and never links it: `tools/ci/check-no-vos.sh` fails the build if a manifest outside `agents/` and
`spikes/` names a VOS crate. ingress reaches the core over a local socket (`ingress::remote`), or
the core host dials out to ingress over the core stream (`ingress::stream`, `F-038`): mutual TLS
under the operator's CA, or a Unix socket on one machine. Both links reach the core through one
sequencer (`ingress::feed::Sequencer`) that checks ingress's signature on every forwarded input
against `stream.ingress_keys`, fences a stale feeder's epoch and keeps the received log's order.
The Agent and the host's calls into it do not change for either link.
`features/F-005-core-agent-binding/NOTES.md` records the licensing boundary, and
`docs/oss/LICENSING.md` the licence of every part.

| Path | What |
|---|---|
| `core/` | The Agent actor: one `#[actor(agent)]` shell around `core_binding::transition` (guest, `riscv64em-vos`). Its own workspace, as VOS's actors are |
| `core-host/` | `aledgent-core-host`: the Local host of that Agent (VOS's runtime ABI), the replica and its log, and the server of `boundary::Core` |
| `aledgent-runtime/` | aledgent's AgentRuntime (`F-033`, `DEC-23`): VOS's standard runtime with composite invocations, one Linear transition across an Agent's own actors with ordered and exactly-once steps (guest, `riscv64em-vos`; its engine is VOS's `runtime-composite`, fork branch `aledgent/composite-runtime`). Its own workspace |

## Pins

- **VOS**: aledgent's fork `https://codeberg.org/pandres95/VOS.git`, branch
  `agent/compact-acknowledged-results`, commit `de0f12f82dc04d680abea8917d00e35220eedd2a`
  (`T-025-01`, sequenced invocation identities). TODO(F-025): upstream `saga/agents` once merged.
- **CipherClerk**: `e9a7b08` with `proposals/cipherclerk/0001` applied, made by
  `tools/cipherclerk-patched.sh` into `.cipherclerk-patched/` (gitignored). TODO(F-025): the
  upstream commit once 0001 is merged (`T-025-07`).
- **Toolchain**: `nightly-2026-03-20` (the pinned VOS's canonical guest toolchain) for both.

## Commands

VOS's host crates build on Linux (not macOS: `vos/src/agent/journal_store.rs`); the guest builds
anywhere with the toolchain and `rust-src`.

    tools/cipherclerk-patched.sh                         # once, and after the patch changes

    # The host (CI runs these three: .github/workflows/ci.yml, job `agents`)
    cd agents
    cargo fmt --all -- --check
    cargo clippy --locked --all-targets -- -D warnings
    cargo test --locked
    cargo build --release -p core-host

    # The guest ELF (VOS's `cargo actor` alias, .cargo/config.toml)
    cd agents/core && cargo actor                        # target/riscv64em-vos/release/aledgent_core.elf

    # The standard AgentRuntime from the pinned VOS, linked
    cd <VOS>/services/agent-runtime && cargo +nightly-2026-03-20 actor --locked
    aledgent-core-host runtime-pvm <agent_runtime.elf> agent_runtime.pvm

    # Package the actor, run a few commands through VOS, measure the reservation path
    aledgent-core-host package aledgent_core.elf core.vos
    aledgent-core-host smoke agent_runtime.pvm core.vos
    aledgent-core-host measure agent_runtime.pvm core.vos reserve.csv --n 100 --warmup 3
    # reservations, captures and transfers: the Agent's state and gas against the book's size
    aledgent-core-host measure agent_runtime.pvm core.vos mixed.csv --n 1000 --warmup 4 --mix mixed
    # identities and accounts opened, 24 inputs an invocation: per-round figures (PROFROUND) to
    # stderr, the service tree's leaves by kind at the end (profiling)
    aledgent-core-host measure agent_runtime.pvm core.vos open.csv --n 12000 --warmup 0 --batch 24 --mix open

    # Serve boundary::Core on the deployment's core_endpoint (the same file ingress reads), and/or
    # dial ingress's core listener with stream.dial (F-038; a Unix-socket address reads its token
    # from ALEDGENT_CORE_STREAM_TOKEN), streaming every decision to the off-node archive (T-005-15;
    # ops/runbooks/release/core-archive.md)
    ALEDGENT_CORE_ROOT=… ALEDGENT_CORE_AGENT_KEY=… ALEDGENT_CORE_PACKAGE_KEY=… \
      aledgent-core-host serve --config deployment.json --runtime agent_runtime.pvm \
        --actor core.vos --state <dir> \
        --archive 's3://aledgent-<env>-core-archive-<aws-account-id>/core?region=<region>&kms-key-id=<archive key ARN>'
    #   (Object Lock defaults to compliance, 400 days)

    # A fresh node from the archive (T-005-16): opsctl checks it and lays it out, then the Agent is
    # re-sent every archived input after its checkpoint (opsctl runs this with --core-host)
    opsctl restore core --from <archive> --config deployment.json --state <empty dir> --anchors-rpc <node>
    aledgent-core-host restore --config deployment.json --runtime agent_runtime.pvm --actor core.vos --state <dir>

**The pinned runtime refuses the core's program** (1 969 952 B against its 1 280 KiB limit), so
`smoke`, `measure` and `serve` need a runtime and a host built with that limit raised: the
measurements in `notes/MEASUREMENTS-F005.md` used `MAX_RUNTIME_PROGRAM_BYTES` and
`MAX_EXECUTION_PROGRAM_BYTES` at 2 560 KiB, through a `[patch]` of the VOS crates onto a local
copy (`--config` with a `[patch."https://codeberg.org/pandres95/VOS.git"]` table). The change is
drafted as `proposals/vos/0003`.

## aledgent's runtime (`F-033`)

    # The runtime, against the VOS its Cargo.toml pins (or --config a [patch] onto a checkout).
    # No features: the JAM-compatible profile, Gray Paper host calls only (proposals/vos/0012);
    # ALEDGENT_RUNTIME_GP_ONLY=1 makes runtime-pvm refuse anything else.
    cd agents/aledgent-runtime && cargo actor
    ALEDGENT_RUNTIME_GP_ONLY=1 aledgent-core-host runtime-pvm target/riscv64em-vos/release/aledgent_runtime.elf aledgent_runtime.pvm

    # The core under it: the plain path is the standard runtime's
    aledgent-core-host measure aledgent_runtime.pvm core.vos reserve.csv --n 32 --batch 8
    # The same batches as composites, one input a step (`F033_COMPOSITE=isolated|atomic`; the core
    # host built with `--features composite` against a VOS that has the engine)
    F033_COMPOSITE=isolated aledgent-core-host measure aledgent_runtime.pvm core.vos c.csv --n 32 --batch 8

The features `vos-native`, `blake2b-precompile` and `native-curve` (with `F025_PROGRAM_LOOKUP=1` for
`vos-native`) are VOS's profiles outside JAM: on the fork, off by default, for comparison only.

The x86 bench measures it beside VOS's runtime on one hosted runner (`mode=compare`, a variant
`label=<vos ref>+<features>@lookup!aledgent[~isolated]`) and runs the conformance suite through it
(`mode=conformance`, the same variants). `features/F-033-aledgent-runtime/NOTES.md` has the figures.

## The conformance suite through the Agent

`cargo test -p conformance` (root workspace) runs every service case through the core Agent's
transition natively (`through-agent/native`, on by default). Through VOS:

    CONFORMANCE_TARGETS=through-agent/vos CONFORMANCE_AGENT_HOST=<host:port> cargo test -p conformance --test suite

with an Agent host listening at that address (the raw port, `core_binding::host::RemotePort`). At
about 2.5 s per change it is a manual run.
