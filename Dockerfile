# syntax=docker/dockerfile:1
#
# Cross-compiles a static musl binary on the build host (no QEMU needed) and
# ships it in an empty image. TLS uses rustls + webpki-roots, so no CA bundle
# is required at runtime; the Europe/Kyiv zone is compiled in via chrono-tz.
#
#   docker buildx build --platform linux/arm64 -t dtek-bot .   # Raspberry Pi 5
#   docker buildx build --platform linux/amd64 -t dtek-bot .   # laptop

FROM --platform=$BUILDPLATFORM rust:1.96-slim-bookworm AS build

ARG ZIG_VERSION=0.13.0
ARG ZIGBUILD_VERSION=0.24.0

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl xz-utils \
    && rm -rf /var/lib/apt/lists/*

RUN arch="$(uname -m)" \
    && curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-linux-${arch}-${ZIG_VERSION}.tar.xz" \
       | tar -xJ -C /opt \
    && ln -s "/opt/zig-linux-${arch}-${ZIG_VERSION}/zig" /usr/local/bin/zig

RUN cargo install --locked cargo-zigbuild --version "${ZIGBUILD_VERSION}" \
    && rustup target add aarch64-unknown-linux-musl x86_64-unknown-linux-musl

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
# The fixture is embedded by unit tests only, but config.rs tests reference the example config.
COPY tests ./tests
COPY config.example.toml ./

ARG TARGETARCH
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    case "${TARGETARCH}" in \
      arm64) target=aarch64-unknown-linux-musl ;; \
      amd64) target=x86_64-unknown-linux-musl ;; \
      *) echo "unsupported TARGETARCH: ${TARGETARCH}" >&2; exit 1 ;; \
    esac \
    && cargo zigbuild --release --locked --target "${target}" \
    && mkdir -p /out \
    && cp "target/${target}/release/dtek-power-off-bot" /out/

FROM scratch
COPY --from=build /out/dtek-power-off-bot /dtek-power-off-bot
USER 1000:1000
ENV CONFIG_PATH=/config.toml
ENTRYPOINT ["/dtek-power-off-bot"]
