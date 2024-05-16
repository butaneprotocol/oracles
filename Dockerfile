FROM rust:alpine AS build
RUN apk add musl-dev openssl-dev openssl-libs-static
ENV OPENSSL_STATIC=1
WORKDIR /build
COPY src /build/src
COPY Cargo.toml Cargo.lock config.base.yaml /build/
RUN cargo build --release

FROM alpine
WORKDIR /app
COPY --from=build /build/target/release/oracles /app/
CMD ["./oracles"]