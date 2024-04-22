FROM rust:alpine AS build
RUN apk add musl-dev openssl-dev openssl-libs-static
ENV OPENSSL_STATIC=1
WORKDIR /build
COPY src /build/src
COPY Cargo.toml Cargo.lock /build/
RUN cargo build --release

FROM alpine
WORKDIR /app
COPY --from=build /build/target/release/oracles-offchain /app/
COPY config.base.yaml /app/
CMD ["./oracles-offchain"]