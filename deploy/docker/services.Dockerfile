# Copyright (c) 2026 Pagos Cube LLC
# SPDX-License-Identifier: Apache-2.0 OR MIT
# syntax=docker/dockerfile:1.10
#
# aledgent's long-running processes from the root workspace, one image each (targets):
#
#   ingress   aledgent-ingress: every command and query; with no `core_endpoint`, the in-process
#             core runs inside it (services/ingress).               <ecr>/<namespace>/aledgent-ingress
#   settle    aledgent-settle: the settlement adapter, with its light-client chain specs
#             baked in (deploy/templates/chain-specs).                 <ecr>/<namespace>/aledgent-settle
#   ops       opsctl: the signal exporter (`opsctl exporter`) and the file-based backup, restore
#             and drill commands (F-018).                           <ecr>/<namespace>/aledgent-ops
#   signer    aledgent-signer: the key boundary (F-035): keys from Vault, AWS KMS opt-in; runs as
#             its own user (65533).                                <ecr>/<namespace>/aledgent-signer
#   sidecar   aledgent-sidecar: ingress and the in-process core, book only, in memory, with TEST
#             credentials, for other repositories' CI (SIDECAR.md). ghcr.io/bloque-app/aledgent-sidecar
#
# <ecr> is the operator's container registry (.github/workflows/images.yml).
#
# Built in CI only (.github/workflows/images.yml), for linux/amd64, from the repository root:
#
#   docker buildx build -f deploy/docker/services.Dockerfile --target settle .
#
# Every base image is pinned by digest; the Rust toolchain by version (RUSTUP_TOOLCHAIN overrides
# rust-toolchain.toml's `stable`, which would otherwise fetch whatever is stable that day); every
# crate by Cargo.lock (`--locked`). The runtime is distroless (glibc, CA certificates, no shell),
# run as its `nonroot` user (65532).
#
# ROOTS chooses the book's state commitment for the whole deployment, at genesis (DEC-25, owner
# ruling cs-q04): `v2` (compact trees, the default) or `v1` (CipherClerk's full-depth trees, a
# supported opt-in: 137.9 M of actor gas a reservation against v2's 72.0 M, witnesses about 21 %
# smaller). ingress, settle and opsctl are built with it, and so must be aledgent-core
# (core.Dockerfile, the same argument); every image carries it as the label `app.aledgent.roots`.
# A deployment never mixes the two, and never changes them after its first anchor: its checkpoints
# name the construction.

ARG ROOTS=v2
ARG RUST_IMAGE=docker.io/library/rust:1.98.0-slim-bookworm@sha256:1469a27c125cb5a3aebfa4f4e4665d935b02fb72cc093b2c974b3d740e43f157
ARG NODE_IMAGE=docker.io/library/node:22.20.0-bookworm-slim@sha256:b21fe589dfbe5cc39365d0544b9be3f1f33f55f3c86c87a76ff65a02f8f5848e
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f
# The sidecar's runtime: the same distroless base, its `debug` variant, which adds busybox, so that a
# CI job's `--health-cmd` (always run through a shell) works. Test only; never a production image.
ARG SIDECAR_IMAGE=gcr.io/distroless/cc-debian12:debug-nonroot@sha256:923320b891f20d5f4bd43ed3a72eeee2f3323d481d6f4bd8d0b2c96d1c0758bc

# ---------------------------------------------------------------------------------------------
# The four binaries, in one build: the workspace compiles once.
# ---------------------------------------------------------------------------------------------
FROM ${RUST_IMAGE} AS build
ARG ROOTS
ENV RUSTUP_TOOLCHAIN=1.98.0 \
    CARGO_TERM_COLOR=never \
    CARGO_INCREMENTAL=0 \
    CARGO_NET_RETRY=10
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    case "$ROOTS" in \
      v2) roots="" ;; \
      v1) roots="--features ingress/smt-v1,settle/smt-v1,ops/smt-v1" ;; \
      *) echo "ROOTS must be v1 or v2, not '$ROOTS'" >&2; exit 1 ;; \
    esac \
 && cargo build --release --locked $roots \
      -p ingress --bin aledgent-ingress \
      -p settle --bin aledgent-settle \
      -p ops --bin opsctl \
      -p aledgent-sidecar --bin aledgent-sidecar \
      -p signer --bin aledgent-signer \
 && mkdir -p /out/bin /out/state \
 && cp target/release/aledgent-ingress target/release/aledgent-settle target/release/opsctl \
       target/release/aledgent-sidecar target/release/aledgent-signer /out/bin/

# ---------------------------------------------------------------------------------------------
# The chain specs: Kreivo's (committed) and Kusama's (fetched by pinned version and
# SHA-256, cut to one boot node). deploy/templates/chain-specs/ says where each comes from.
# ---------------------------------------------------------------------------------------------
FROM ${NODE_IMAGE} AS chain-specs
WORKDIR /specs
COPY deploy/templates/chain-specs/kreivo.json deploy/templates/chain-specs/fetch-kusama.mjs ./
RUN node fetch-kusama.mjs kusama.json && rm fetch-kusama.mjs

# ---------------------------------------------------------------------------------------------
# Runtime images.
# ---------------------------------------------------------------------------------------------
FROM ${RUNTIME_IMAGE} AS ingress
ARG ROOTS
LABEL org.opencontainers.image.title="aledgent-ingress" \
      app.aledgent.roots="${ROOTS}" \
      org.opencontainers.image.description="aledgent ingress (F-009), with the in-process core" \
      org.opencontainers.image.source="https://github.com/bloque-app/aledgent" \
      org.opencontainers.image.licenses="Apache-2.0 OR MIT"
COPY --from=build /out/bin/aledgent-ingress /usr/local/bin/aledgent-ingress
# The in-process core's journal (`core_journal`): mount a durable volume here. An opt-in leaf file
# (`core_book`, T-031-14) is rebuilt from the journal on every start.
COPY --from=build --chown=65532:65532 /out/state /var/lib/aledgent
USER 65532:65532
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/aledgent-ingress"]
CMD ["serve", "--config", "/etc/aledgent/ingress.json"]

FROM ${RUNTIME_IMAGE} AS settle
ARG ROOTS
LABEL org.opencontainers.image.title="aledgent-settle" \
      app.aledgent.roots="${ROOTS}" \
      org.opencontainers.image.description="aledgent settlement adapter (F-016, F-027), with Kreivo and Kusama light-client chain specs" \
      org.opencontainers.image.source="https://github.com/bloque-app/aledgent" \
      org.opencontainers.image.licenses="Apache-2.0 OR MIT"
COPY --from=build /out/bin/aledgent-settle /usr/local/bin/aledgent-settle
COPY --from=chain-specs /specs /usr/share/aledgent/chain-specs
# Where `signals_file` is written, for the exporter beside it.
COPY --from=build --chown=65532:65532 /out/state /var/lib/aledgent
USER 65532:65532
# The binary aborts on any panic and ends after three minutes blind: run it under a supervisor
# that restarts it (an orchestrator, `--restart on-failure`, systemd).
ENTRYPOINT ["/usr/local/bin/aledgent-settle"]
CMD ["/etc/aledgent/settle.json"]

FROM ${RUNTIME_IMAGE} AS ops
ARG ROOTS
LABEL org.opencontainers.image.title="aledgent-ops" \
      app.aledgent.roots="${ROOTS}" \
      org.opencontainers.image.description="aledgent operations (F-018): the signal exporter and opsctl" \
      org.opencontainers.image.source="https://github.com/bloque-app/aledgent" \
      org.opencontainers.image.licenses="Apache-2.0 OR MIT"
COPY --from=build /out/bin/opsctl /usr/local/bin/opsctl
COPY --from=build --chown=65532:65532 /out/state /var/lib/aledgent
USER 65532:65532
EXPOSE 9464
ENTRYPOINT ["/usr/local/bin/opsctl"]
CMD ["exporter", "--config", "/etc/aledgent/exporter.toml"]

FROM ${RUNTIME_IMAGE} AS signer
LABEL org.opencontainers.image.title="aledgent-signer" \
      org.opencontainers.image.description="aledgent's key boundary (F-035): signs for the core host and the settlement adapter; keys from Vault, AWS KMS opt-in for lanes" \
      org.opencontainers.image.source="https://github.com/bloque-app/aledgent" \
      org.opencontainers.image.licenses="Apache-2.0 OR MIT"
COPY --from=build /out/bin/aledgent-signer /usr/local/bin/aledgent-signer
# Its own user, apart from every caller's: callers are told apart by the socket's peer credentials.
USER 65533:65533
ENTRYPOINT ["/usr/local/bin/aledgent-signer"]
CMD ["serve", "/etc/aledgent-signer/signer.json"]

# Test only (SIDECAR.md): its operator token and key-ring root are constants of e2e/sidecar.
FROM ${SIDECAR_IMAGE} AS sidecar
LABEL org.opencontainers.image.title="aledgent-sidecar" \
      org.opencontainers.image.description="aledgent for CI service containers: ingress and the in-process core, book only, in memory, TEST credentials. Never for production." \
      org.opencontainers.image.source="https://github.com/bloque-app/aledgent" \
      org.opencontainers.image.licenses="Apache-2.0 OR MIT"
COPY --from=build /out/bin/aledgent-sidecar /usr/local/bin/aledgent-sidecar
# A health check's shell form (`--health-cmd`) always runs through /bin/sh, whatever the image's
# SHELL: give the debug image's busybox that name. Exec-form RUNs, so no shell is needed to do it.
USER 0:0
RUN ["/busybox/mkdir", "-p", "/bin"]
RUN ["/busybox/ln", "-sf", "/busybox/sh", "/bin/sh"]
USER 65532:65532
EXPOSE 8080
# Exec form: works with no options at all. `--health-cmd "aledgent-sidecar health"` works as well.
HEALTHCHECK --interval=2s --timeout=3s --start-period=60s --retries=10 \
  CMD ["/usr/local/bin/aledgent-sidecar", "health"]
ENTRYPOINT ["/usr/local/bin/aledgent-sidecar"]
CMD ["serve"]
