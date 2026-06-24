# syntax=docker/dockerfile:1.7
#
# Single-binary inkfeed image. Multi-stage; the runtime image carries
# only the binary plus ca-certificates (needed for outbound HTTPS to
# feed sources). SQLite is statically linked via rusqlite's `bundled`
# feature and TLS goes through rustls+ring, so the runtime image needs
# no libsqlite3 or libssl.

# ---- builder ---------------------------------------------------------------

FROM rust:1-slim-bookworm AS builder

# pkg-config is referenced by some transitive build scripts.
# ca-certificates lets `cargo fetch` reach crates.io.
RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        ca-certificates \
        pkg-config && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# BuildKit cache mounts so iterative rebuilds reuse the cargo registry
# and the target directory. The binary is copied out to a stable path
# so the next stage doesn't need to know about /src/target.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked && \
    cp /src/target/release/inkfeed /tmp/inkfeed

# ---- runtime ---------------------------------------------------------------

FROM debian:bookworm-slim AS runtime

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        ca-certificates && \
    rm -rf /var/lib/apt/lists/* && \
    groupadd --system --gid 10001 inkfeed && \
    useradd --system --uid 10001 --gid inkfeed --home-dir /app --shell /usr/sbin/nologin inkfeed

WORKDIR /app

COPY --from=builder /tmp/inkfeed /usr/local/bin/inkfeed
COPY config.docker.yaml /app/config.yaml

# /data holds the SQLite cache, log file, and (if Gemini is enabled in
# the mounted config) the TLS cert + key. Mount a host directory or
# named volume here to persist across container recreations.
RUN mkdir -p /data && chown -R inkfeed:inkfeed /app /data
VOLUME ["/data"]

# PORT controls the HTTP listen port and defaults to 8080 in this image.
# CACHE_DB redirects the SQLite cache file into the mounted /data volume
# so the article cache survives container recreations.
ENV PORT=8080 \
    CACHE_DB=/data/reader_cache.sqlite

EXPOSE 8080

USER inkfeed

ENTRYPOINT ["/usr/local/bin/inkfeed"]
CMD ["/app/config.yaml"]
