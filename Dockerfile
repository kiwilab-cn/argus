# syntax=docker/dockerfile:1.7
FROM rust:1.97-slim-bookworm AS builder

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release -p argus-server

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install --no-install-recommends -y ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 argus \
    && useradd --system --uid 10001 --gid argus --no-create-home argus

COPY --from=builder /build/target/release/argus /usr/local/bin/argus

USER argus
ENTRYPOINT ["/usr/local/bin/argus"]
