# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
# syntax=docker/dockerfile:1.10
#
# aledgent-core: the core as a VOS Agent (F-005), `aledgent-core-host serve`, with the programs it
# runs: VOS's standard AgentRuntime (`agent_runtime.pvm`) and the core actor (`core.vos`).
# <ecr>/<namespace>/aledgent-core (the operator's registry, .github/workflows/images.yml).
# AGPL-3.0-or-later, as everything under agents/ is (it links VOS); the image's source is this
# repository at the commit in its labels, and VOS at VOS_REV.
#
# Built in CI only (.github/workflows/images.yml), for linux/amd64, from the repository root:
#
#   docker buildx build -f deploy/docker/core.Dockerfile .
#
# What it builds, as agents/README.md and .github/x86-bench/run.sh (reservation-timing) do:
#
#   1. VOS at VOS_REV, the commit agents/Cargo.toml pins (fork branch stateful-program-upgrade, on perf/agent-stack), as it is.
#   2. CipherClerk e9a7b08 with proposals/cipherclerk/0001 and 0003-0005 (tools/cipherclerk-patched.sh).
#   3. The host, `aledgent-core-host`, against that VOS tree (a `[patch]` by path).
#   4. The standard AgentRuntime from that tree (`cargo actor`) with RUNTIME_FEATURES, linked by
#      the host (`runtime-pvm`). The default is JAM-compatible (agreed with @olanod (VOS's maintainer)): no features, so the runtime issues only Gray Paper Refine host calls
#      (GP 0.8.0 eq. B.6, machine to expunge, 9 to 14). The actor program travels in the work and
#      the metered guest hashes it once (proposals/vos/0006, 0007). ALEDGENT_RUNTIME_GP_ONLY makes
#      `runtime-pvm` refuse a runtime that issues any other call. VOS's `native-curve`,
#      `blake2b-precompile` and `vos-native` (a program lookup on call 7 keyed by ProgramId, not by
#      the preimage hash Gray Paper eq. 9.5 keys it by) stay on the fork, off: proposals/vos/0012.
#   5. The core's guest (`cargo actor` in agents/core), packaged by the host (`package`).
#
# The runtime stage is distroless, as the other images, run as `nonroot` (65532).
#
# ROOTS chooses the book's state commitment, at genesis (DEC-25, owner ruling cs-q04): `v2`
# (compact trees, the default) or `v1` (CipherClerk's full-depth trees, a supported opt-in). The
# host (step 3) and the core's guest (step 5) are built with the same choice, `smt-v1` for v1:
# a host and a guest that disagree refuse every witness. ingress, settle and opsctl take the same
# argument (services.Dockerfile); the image carries it as the label `app.aledgent.roots`.
#
# A core image on a deployment's genesis bytes (F-038 §12 step 0, DEC-27 (i)): the target
# `core-on-genesis` is the current host (steps 1 to 3) with the package and the runtime copied, byte
# for byte, from the deployment's pinned core image, never rebuilt (a rebuild need not reproduce the
# genesis bytes, and the genesis code names them). The build fails unless they are the genesis
# code's, checked by the new host's own `check-code`, as a restart checks them (agents/core-host,
# code.rs):
#
#   docker buildx build -f deploy/docker/core.Dockerfile --target core-on-genesis \
#     --build-arg GENESIS_IMAGE=ghcr.io/bloque-app/aledgent-core@sha256:<digest> \
#     --build-arg ROOTS=<the genesis image's app.aledgent.roots> \
#     --build-arg GENESIS_CODE=<H0, 64 hex> \
#     --build-context genesis-code=<dir with code.bin or chainspec.bin> .
#
# GENESIS_IMAGE must name its image by digest. `genesis-code` is a directory holding either the
# deployment's genesis code manifest (`code.bin`, `aledgent/code/v1`, as `code-manifest --out` and
# `check-code --write-manifest` write it) or its chainspec (`chainspec.bin`, whose header names the
# code); GENESIS_CODE must be that code's version, and is required: without it the check proves
# only that the bytes are the supplied manifest's, not the deployment's genesis code. The workflow
# images.yml always passes it, and so cannot rebuild a book that has no chainspec; the target itself
# still accepts it empty, for a local build where the manifest is trusted. The host is built with
# ROOTS, which must be the genesis's: the check refuses a manifest of another construction. The
# image carries the check's report as /usr/share/aledgent/genesis-code.json and the genesis image as
# the label `app.aledgent.genesis.image`. Without a target the build is `core`, as before.
#
# The image also carries `opsctl` (/usr/local/bin/opsctl, from the root workspace, as the ops image
# builds it, with the same ROOTS), so that a restore from the off-node archive runs from one
# image: `opsctl restore core --from … --core-host aledgent-core-host --runtime
# /usr/share/aledgent/agent_runtime.pvm --actor /usr/share/aledgent/core.vos` needs both binaries
# and the Agent's programs on one filesystem (ops/runbooks/release/total-loss-recovery.md, path B).
# Run it with `--entrypoint /usr/local/bin/opsctl`. It is built in a stage of its own and changes
# nothing the Agent runs. Both targets carry it: `core` and `core-on-genesis` (whose package and
# runtime stay the genesis image's bytes).

ARG ROOTS=v2
# The deployment's pinned core image, for `core-on-genesis` only (scratch: none, and that target
# fails).
ARG GENESIS_IMAGE=scratch
ARG BUILD_IMAGE=docker.io/library/rust:1.98.0-bookworm@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f

# Steps 1 to 3: the host.
FROM ${BUILD_IMAGE} AS host
ARG VOS_URL=https://codeberg.org/pandres95/VOS.git
ARG VOS_REV=bd8d1bb70551391a4c80a96c2249313ab7e0a0fc
# The one toolchain of agents/ and of VOS's standard runtime at VOS_REV.
ARG GUEST_TOOLCHAIN=nightly-2026-03-20
# The runtime's cargo features: its profile (proposals/vos/0008). Empty: JAM-compatible
# (proposals/vos/0012). Anything else is outside JAM, and step 4 refuses it unless
# RUNTIME_GP_ONLY=0.
ARG RUNTIME_FEATURES=""
ARG RUNTIME_GP_ONLY=1
ARG ROOTS
ENV CARGO_TERM_COLOR=never \
    CARGO_INCREMENTAL=0 \
    CARGO_NET_RETRY=10 \
    CARGO_NET_GIT_FETCH_WITH_CLI=true
RUN apt-get update -qq \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
      clang lld pkg-config libssl-dev cmake protobuf-compiler \
 && rm -rf /var/lib/apt/lists/*
RUN rustup toolchain install "$GUEST_TOOLCHAIN" --profile minimal --component rust-src
WORKDIR /w

# 1. VOS at the pin.
RUN git init -q vos \
 && git -C vos remote add origin "$VOS_URL" \
 && for i in 1 2 3 4 5; do git -C vos fetch -q --depth 1 origin "$VOS_REV" && break; sleep $((i * 10)); done \
 && git -C vos checkout -q FETCH_HEAD \
 && test "$(git -C vos rev-parse HEAD)" = "$VOS_REV" \
 && printf '[patch."%s"]\nvos = { path = "/w/vos/vos" }\nvos-pvm = { path = "/w/vos/pvm/runtime" }\nvos-pvm-compiler = { path = "/w/vos/pvm/compiler" }\nvos-protocol = { path = "/w/vos/vos-protocol" }\nvos-pvm-precompiles = { path = "/w/vos/pvm/proof/precompiles" }\n' \
      "$VOS_URL" > /w/vos-patch.toml

COPY . /w/aledgent

# The roots' feature, for steps 3 and 5.
RUN case "$ROOTS" in \
      v2) : > /w/roots-features ;; \
      v1) echo "--features smt-v1" > /w/roots-features ;; \
      *) echo "ROOTS must be v1 or v2, not '$ROOTS'" >&2; exit 1 ;; \
    esac

# 2. CipherClerk, patched (clones Codeberg at the pinned commit; retried, Codeberg is sometimes slow).
RUN for i in 1 2 3 4 5; do /w/aledgent/tools/cipherclerk-patched.sh && break; sleep $((i * 10)); done \
 && test -f /w/aledgent/.cipherclerk-patched/.aledgent-stamp

# 3. The host.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/w/target,sharing=locked \
    cd /w/aledgent/agents \
 && CARGO_TARGET_DIR=/w/target/host cargo build --release -p core-host --config /w/vos-patch.toml $(cat /w/roots-features) \
 && mkdir -p /out/bin /out/share /out/state \
 && cp /w/target/host/release/aledgent-core-host /out/bin/

# Steps 4 and 5: the programs it runs, built from source.
FROM host AS build
ARG RUNTIME_FEATURES=""
ARG RUNTIME_GP_ONLY=1

# 4. VOS's standard AgentRuntime, from the same tree, linked by the host.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/w/target,sharing=locked \
    cd /w/vos/services/agent-runtime \
 && CARGO_TARGET_DIR=/w/target/agent-runtime cargo actor --locked ${RUNTIME_FEATURES:+--features "$RUNTIME_FEATURES"} \
 && ALEDGENT_RUNTIME_GP_ONLY="$RUNTIME_GP_ONLY" /out/bin/aledgent-core-host runtime-pvm \
      /w/target/agent-runtime/riscv64em-vos/release/agent_runtime.elf /out/share/agent_runtime.pvm

# 5. The core's guest, packaged.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/w/target,sharing=locked \
    cd /w/aledgent/agents/core \
 && CARGO_TARGET_DIR=/w/target/core cargo actor --config /w/vos-patch.toml $(cat /w/roots-features) \
 && elf=$(find /w/target/core/riscv64em-vos/release -maxdepth 1 -name 'aledgent_core*.elf' | head -n 1) \
 && if [ -z "$elf" ]; then elf=/w/target/core/riscv64em-vos/release/aledgent_core; fi \
 && /out/bin/aledgent-core-host package "$elf" /out/share/core.vos \
 && ls -l /out/share

# opsctl, for a restore from this image (header): the root workspace, as services.Dockerfile builds
# it (RUSTUP_TOOLCHAIN pins the toolchain rust-toolchain.toml leaves at `stable`; `--locked`).
FROM ${BUILD_IMAGE} AS opsctl
ARG ROOTS
ENV RUSTUP_TOOLCHAIN=1.98.0 \
    CARGO_TERM_COLOR=never \
    CARGO_INCREMENTAL=0 \
    CARGO_NET_RETRY=10 \
    CARGO_NET_GIT_FETCH_WITH_CLI=true
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    case "$ROOTS" in \
      v2) roots="" ;; \
      v1) roots="--features ops/smt-v1" ;; \
      *) echo "ROOTS must be v1 or v2, not '$ROOTS'" >&2; exit 1 ;; \
    esac \
 && cargo build --release --locked $roots -p ops --bin opsctl \
 && mkdir -p /out/bin \
 && cp target/release/opsctl /out/bin/

# A deployment's genesis package and runtime (the target `core-on-genesis`): its pinned core
# image, and its genesis code (a named build context, `--build-context genesis-code=<dir>`; empty
# by default, and the check refuses it).
FROM ${GENESIS_IMAGE} AS genesis
FROM scratch AS genesis-code

# The current host checks the genesis bytes against the genesis code; the image takes them from
# here, so no image is made from bytes that were not checked.
FROM host AS on-genesis
ARG GENESIS_IMAGE
ARG GENESIS_CODE=""
RUN digest="${GENESIS_IMAGE##*@sha256:}"; \
    if [ "$digest" = "$GENESIS_IMAGE" ] || ! printf '%s\n' "$digest" | grep -Eqx '[0-9a-f]{64}'; then \
      echo "GENESIS_IMAGE must name the deployment's core image by digest (<image>@sha256:<64 hex>), not '$GENESIS_IMAGE'" >&2; \
      exit 1; \
    fi
COPY --from=genesis /usr/share/aledgent/core.vos /usr/share/aledgent/agent_runtime.pvm /genesis/share/
COPY --from=genesis-code . /genesis/code/
RUN if [ -f /genesis/code/chainspec.bin ] && [ -f /genesis/code/code.bin ]; then \
      echo "genesis-code: give code.bin or chainspec.bin, not both" >&2; exit 1; \
    elif [ -f /genesis/code/chainspec.bin ]; then from="--chainspec /genesis/code/chainspec.bin"; \
    elif [ -f /genesis/code/code.bin ]; then from="--manifest /genesis/code/code.bin"; \
    else \
      echo "genesis-code: the deployment's genesis code is required (--build-context genesis-code=<dir> with code.bin or chainspec.bin)" >&2; \
      exit 1; \
    fi \
 && /out/bin/aledgent-core-host check-code \
      --actor /genesis/share/core.vos --runtime /genesis/share/agent_runtime.pvm \
      $from ${GENESIS_CODE:+--code "$GENESIS_CODE"} --out /genesis/share/genesis-code.json \
 && ls -l /genesis/share

FROM ${RUNTIME_IMAGE} AS core-on-genesis
ARG ROOTS
ARG GENESIS_IMAGE
ARG VOS_URL=https://codeberg.org/pandres95/VOS.git
ARG VOS_REV=bd8d1bb70551391a4c80a96c2249313ab7e0a0fc
LABEL org.opencontainers.image.title="aledgent-core" \
      org.opencontainers.image.description="aledgent core as a VOS Agent: the current aledgent-core-host serve, with the genesis image's agent_runtime.pvm and core.vos (F-038 §12 step 0)" \
      org.opencontainers.image.source="https://github.com/bloque-app/aledgent" \
      org.opencontainers.image.licenses="AGPL-3.0-or-later" \
      app.aledgent.vos.url="${VOS_URL}" \
      app.aledgent.vos.rev="${VOS_REV}" \
      app.aledgent.genesis.image="${GENESIS_IMAGE}" \
      app.aledgent.roots="${ROOTS}"
COPY --from=on-genesis /out/bin/aledgent-core-host /usr/local/bin/aledgent-core-host
# A restore runs from this image (header): `--entrypoint /usr/local/bin/opsctl`.
COPY --from=opsctl /out/bin/opsctl /usr/local/bin/opsctl
COPY --from=on-genesis /genesis/share /usr/share/aledgent
COPY --from=on-genesis --chown=65532:65532 /out/state /var/lib/aledgent/core
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/aledgent-core-host"]
CMD ["serve", "--config", "/etc/aledgent/ingress.json", \
     "--runtime", "/usr/share/aledgent/agent_runtime.pvm", \
     "--actor", "/usr/share/aledgent/core.vos", \
     "--state", "/var/lib/aledgent/core"]

# A core upgrade's package (the target `core-upgrade`; #285, F-037, REQ-RT-16): only `core.vos`, as a
# later generation of the code a book already runs, built from this source, and nothing else. It is
# not an image anyone runs: build it with `--output type=local` (.github/workflows/core-release.yml
# does), and it never touches the `core` target below, whose bytes it does not change.
#
#   docker buildx build -f deploy/docker/core.Dockerfile --target core-upgrade \
#     --build-arg ROOTS=<the book's roots> \
#     --build-context previous-code=<dir with core.vos and agent_runtime.pvm, the code the book runs now> \
#     --output type=local,dest=<dir> .
#
# What differs from step 5 of `build`: the package claims the state of the previous package's program
# (`package --previous`, T-037-19), without which the runtime refuses to switch a stateful actor's
# program in place; and the `Cargo.lock` the build resolved is kept beside the package
# (`core.Cargo.lock`), because the guest cannot be built with `--locked` (the `--config` patch of VOS
# makes cargo rewrite the committed lock, as in step 5), so the dependencies a build resolves are known
# only by that file. The runtime is not built here: an upgrade keeps the runtime the book
# was founded on, byte for byte (a new runtime is the continuation, T-037-17); the workflow copies it
# from the code in force. `GENERATION` is the source's (`agents/core/src/lib.rs`): bumping it is a
# change to the source, reviewed with the change that needs the upgrade, and the workflow refuses
# a package whose generation is not the predecessor's plus one.
#
# Nothing comes out of this target unless the new package passes `check-upgrade` (T-037-12,
# T-037-19's precondition, owner ruling of 2026-10-27): the recorded book of the code-history gate
# is run through the real packages under the code in force's runtime, the predecessor's package
# deciding its generation's inputs, the new one installed in place over that state at the recorded
# upgrade and deciding the rest, every input at its archived log hash; and every way out at an
# upgrade's input is decided by the new package as the predecessor decided it (D18). So the
# `previous-code` context holds the runtime too (`agent_runtime.pvm`), and the report is
# `upgrade-check.json`.
FROM scratch AS previous-code

FROM host AS upgrade
ARG ROOTS
RUN --mount=type=bind,from=previous-code,target=/previous \
    --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/w/target,sharing=locked \
    if [ ! -s /previous/core.vos ]; then \
      echo "core-upgrade: the package the book runs now is required (--build-context previous-code=<dir with core.vos>)" >&2; \
      exit 1; \
    fi \
 && cd /w/aledgent/agents/core \
 && CARGO_TARGET_DIR=/w/target/core cargo actor --config /w/vos-patch.toml $(cat /w/roots-features) \
 && elf=$(find /w/target/core/riscv64em-vos/release -maxdepth 1 -name 'aledgent_core*.elf' | head -n 1) \
 && if [ -z "$elf" ]; then elf=/w/target/core/riscv64em-vos/release/aledgent_core; fi \
 && /out/bin/aledgent-core-host package "$elf" /out/share/core.vos --previous /previous/core.vos \
 && cp /w/aledgent/agents/core/Cargo.lock /out/share/core.Cargo.lock \
 && ls -l /out/share/core.vos /out/share/core.Cargo.lock \
 && if [ ! -s /previous/agent_runtime.pvm ]; then \
      echo "core-upgrade: the runtime of the code in force is required beside its package, for check-upgrade" >&2; \
      exit 1; \
    fi \
 && if [ "$ROOTS" != v2 ]; then \
      echo "core-upgrade: the recorded history (T-037-12) is a book in roots v2; there is none in roots $ROOTS to check the upgrade against" >&2; \
      exit 1; \
    fi \
 && /out/bin/aledgent-core-host check-upgrade --runtime /previous/agent_runtime.pvm \
      --previous /previous/core.vos --package /out/share/core.vos \
      --history /w/aledgent/crates/core-binding/tests/fixtures/code-history \
      --out /out/share/upgrade-check.json

FROM scratch AS core-upgrade
COPY --from=upgrade /out/share/core.vos /out/share/core.Cargo.lock /out/share/upgrade-check.json /

# The default target: the core built from source.
FROM ${RUNTIME_IMAGE} AS core
ARG ROOTS
ARG VOS_URL=https://codeberg.org/pandres95/VOS.git
ARG VOS_REV=bd8d1bb70551391a4c80a96c2249313ab7e0a0fc
LABEL org.opencontainers.image.title="aledgent-core" \
      org.opencontainers.image.description="aledgent core as a VOS Agent (F-005): aledgent-core-host serve, with agent_runtime.pvm and core.vos" \
      org.opencontainers.image.source="https://github.com/bloque-app/aledgent" \
      org.opencontainers.image.licenses="AGPL-3.0-or-later" \
      app.aledgent.vos.url="${VOS_URL}" \
      app.aledgent.vos.rev="${VOS_REV}" \
      app.aledgent.vos.runtime-features="none (JAM-compatible: Gray Paper host calls only)" \
      app.aledgent.roots="${ROOTS}"
COPY --from=build /out/bin/aledgent-core-host /usr/local/bin/aledgent-core-host
# A restore runs from this image (header): `--entrypoint /usr/local/bin/opsctl`.
COPY --from=opsctl /out/bin/opsctl /usr/local/bin/opsctl
COPY --from=build /out/share /usr/share/aledgent
# The Agent's state (`--state`): agent.state, archive.bin, replies.jsonl. Mount a durable volume.
COPY --from=build --chown=65532:65532 /out/state /var/lib/aledgent/core
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/aledgent-core-host"]
CMD ["serve", "--config", "/etc/aledgent/ingress.json", \
     "--runtime", "/usr/share/aledgent/agent_runtime.pvm", \
     "--actor", "/usr/share/aledgent/core.vos", \
     "--state", "/var/lib/aledgent/core"]
