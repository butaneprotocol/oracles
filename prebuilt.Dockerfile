# GitHub Actions doesn't have free ARM runners for private repos.
# Until we can use that, build the oracle from outside of docker via cross-compilation,
# so that we don't do any expensive work inside of an emulated OS.
FROM alpine
ARG RUST_TARGET
WORKDIR /app
COPY target/${RUST_TARGET}/release/oracles /app/
CMD ["./oracles"]