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
#   1. VOS at VOS_REV, the commit agents/Cargo.toml pins (fork branch perf/agent-stack), as it is.
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

ARG ROOTS=v2
ARG BUILD_IMAGE=docker.io/library/rust:1.98.0-bookworm@sha256:82150a52ec202c1b14d7817e14516c392bb7f5cfebd88f1ed531cb37ebd39922
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f

FROM ${BUILD_IMAGE} AS build
ARG VOS_URL=https://codeberg.org/pandres95/VOS.git
ARG VOS_REV=c54fc1ff13fcd55b86f2cf4edc77b041b62e245f
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

FROM ${RUNTIME_IMAGE} AS core
ARG ROOTS
ARG VOS_URL=https://codeberg.org/pandres95/VOS.git
ARG VOS_REV=c54fc1ff13fcd55b86f2cf4edc77b041b62e245f
LABEL org.opencontainers.image.title="aledgent-core" \
      org.opencontainers.image.description="aledgent core as a VOS Agent (F-005): aledgent-core-host serve, with agent_runtime.pvm and core.vos" \
      org.opencontainers.image.source="https://github.com/bloque-app/aledgent" \
      org.opencontainers.image.licenses="AGPL-3.0-or-later" \
      app.aledgent.vos.url="${VOS_URL}" \
      app.aledgent.vos.rev="${VOS_REV}" \
      app.aledgent.vos.runtime-features="none (JAM-compatible: Gray Paper host calls only)" \
      app.aledgent.roots="${ROOTS}"
COPY --from=build /out/bin/aledgent-core-host /usr/local/bin/aledgent-core-host
COPY --from=build /out/share /usr/share/aledgent
# The Agent's state (`--state`): agent.state, archive.bin, replies.jsonl. Mount a durable volume.
COPY --from=build --chown=65532:65532 /out/state /var/lib/aledgent/core
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/aledgent-core-host"]
CMD ["serve", "--config", "/etc/aledgent/ingress.json", \
     "--runtime", "/usr/share/aledgent/agent_runtime.pvm", \
     "--actor", "/usr/share/aledgent/core.vos", \
     "--state", "/var/lib/aledgent/core"]
