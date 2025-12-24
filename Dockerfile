# docker build --load -t localhost/sd-proxy:latest .
ARG mold_version="2.40.4"
ARG rust_version="1.92.0"

FROM rust:${rust_version} AS rust-base

FROM rust:${rust_version} AS builder
RUN    apt-get update \
    && apt-get install -y curl ca-certificates protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

ARG mold_version
RUN curl -L -o /mold.tar.gz https://github.com/rui314/mold/releases/download/v${mold_version}/mold-${mold_version}-$(uname -m)-linux.tar.gz \
    && mkdir -p /opt/mold \
    && tar -C /opt/mold --strip-components=1 -xzf /mold.tar.gz \
    && rm /mold.tar.gz
ENV PATH="/opt/mold/bin:${PATH}"

WORKDIR /build

ENV CARGO_INCREMENTAL="0"
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER="gcc"
ENV CFLAGS="-fuse-ld=mold"
ENV RUSTFLAGS="-C link-arg=-fuse-ld=mold -C target-feature=+crt-static"

RUN --mount=source=.,target=. \
    --mount=type=cache,sharing=locked,target=/usr/local/cargo/registry \
    --mount=type=cache,sharing=locked,from=rust-base,source=/usr/local/rustup,target=/usr/local/rustup \
    cargo fetch --locked

RUN --mount=source=.,target=. \
    --mount=type=cache,sharing=locked,target=/usr/local/cargo/registry \
    --mount=type=cache,sharing=locked,from=rust-base,source=/usr/local/rustup,target=/usr/local/rustup \
    --mount=type=cache,sharing=locked,target=/target \
    --network=none <<-EOF
CARGO_BUILD_TARGET=""
RUSTFLAGS="${RUSTFLAGS}"

arch="$(uname -m)"
case "${arch}" in
    x86_64)
        CARGO_BUILD_TARGET="x86_64-unknown-linux-gnu"
        ;;
    aarch64)
        CARGO_BUILD_TARGET="aarch64-unknown-linux-gnu"
        ;;
    *) echo "Unsupported architecture: ${arch}" >&2; exit 1 ;;
esac

export CARGO_BUILD_TARGET
export RUSTFLAGS
cargo build --release --target-dir=/target
EOF

RUN --mount=type=cache,sharing=locked,target=/target,ro \
    mkdir -p /build/$(uname -m) && \
    cp /target/$(uname -m)-*/release/sd-proxy /build/sd-proxy

FROM scratch

COPY --from=builder --chown=0:0 /build/sd-proxy /usr/local/bin/sd-proxy

ENV PATH=/usr/local/bin

CMD ["sd-proxy"]
