FROM lukemathwalker/cargo-chef:latest-rust-alpine as chef
WORKDIR /app
RUN apk add musl-dev

FROM chef AS planner
COPY src /app/src
COPY Cargo.toml Cargo.lock config.base.yaml /app/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
  cargo chef prepare --recipe-path recipe.json

FROM chef AS builder 
COPY --from=planner /app/recipe.json recipe.json
RUN --mount=type=cache,target=/usr/local/cargo/registry \
  cargo chef cook --release --recipe-path recipe.json
COPY src /app/src
COPY Cargo.toml Cargo.lock config.base.yaml /app/
RUN --mount=type=cache,target=/usr/local/cargo/registry \
  cargo build --release --bin oracles

FROM alpine
WORKDIR /app
COPY --from=builder /app/target/release/oracles /app/
CMD ["./oracles"]