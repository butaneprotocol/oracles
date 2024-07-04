FROM lukemathwalker/cargo-chef:latest-rust-alpine AS base
RUN apk add musl-dev sccache
ENV RUSTC_WRAPPER=sccache SCCACHE_DIR=/sccache
WORKDIR /app

FROM base AS planner
COPY src /app/src
COPY Cargo.toml Cargo.lock config.base.yaml /app/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=$SCCACHE_DIR,sharing=locked \
    cargo chef prepare --recipe-path recipe.json

FROM base AS builder 
COPY --from=planner /app/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=$SCCACHE_DIR,sharing=locked \
    cargo chef cook --release --recipe-path recipe.json
COPY src /app/src
COPY Cargo.toml Cargo.lock config.base.yaml /app/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=$SCCACHE_DIR,sharing=locked \
    cargo build --release --bin oracles

FROM alpine
WORKDIR /app
COPY --from=builder /app/target/release/oracles /app/
CMD ["./oracles"]