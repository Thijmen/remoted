# syntax=docker/dockerfile:1.28
#
# rd-gateway image: viewer build (bun) -> static aarch64-musl binary (cargo-zigbuild) -> distroless.
# The build stages run on the build host's platform and cross-compile, so this works the same on
# arm64 Macs and amd64 CI runners. Build from the repository root:
#
#   docker buildx build --platform linux/arm64 -f deploy/Dockerfile -t rd-gateway:dev .

ARG BUN_VERSION=1.3
ARG RUST_VERSION=1.99.1
ARG CARGO_ZIGBUILD_VERSION=0.23.4
ARG ZIG_VERSION=0.16.0

# --- viewer (static assets embedded into the gateway) ----------------------------------------
FROM --platform=$BUILDPLATFORM oven/bun:${BUN_VERSION} AS viewer
WORKDIR /src/viewer
COPY viewer/ ./
RUN if [ -f bun.lock ]; then bun install --frozen-lockfile; else bun install; fi \
 && bun run build

# --- gateway -----------------------------------------------------------------------------------
FROM --platform=$BUILDPLATFORM rust:${RUST_VERSION}-bookworm AS gateway
ARG RUST_VERSION
ARG CARGO_ZIGBUILD_VERSION
ARG ZIG_VERSION
# Use the image's toolchain as is (rust-toolchain.toml would also pull the macOS/Windows targets).
ENV RUSTUP_TOOLCHAIN=${RUST_VERSION}
RUN apt-get update \
 && apt-get install -y --no-install-recommends python3-pip \
 && rm -rf /var/lib/apt/lists/* \
 && pip3 install --no-cache-dir --break-system-packages \
      "cargo-zigbuild==${CARGO_ZIGBUILD_VERSION}" "ziglang==${ZIG_VERSION}" \
 && rustup target add aarch64-unknown-linux-musl
WORKDIR /src
COPY . .
# Embed the freshly built viewer, never a stale local viewer/dist.
RUN rm -rf viewer/dist
COPY --from=viewer /src/viewer/dist viewer/dist
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo zigbuild --locked --release -p rd-gateway --target aarch64-unknown-linux-musl \
 && install -D target/aarch64-unknown-linux-musl/release/rd-gateway /out/rd-gateway

# --- runtime -------------------------------------------------------------------------------------
FROM gcr.io/distroless/static-debian12:nonroot
COPY --from=gateway /out/rd-gateway /usr/local/bin/rd-gateway
USER nonroot:nonroot
# viewer, agent, metrics
EXPOSE 8080 8081 9090
ENTRYPOINT ["/usr/local/bin/rd-gateway"]
