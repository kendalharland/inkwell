# Single-binary inkwell image. Multi-stage; the runtime image carries
# only the binary plus ca-certificates (needed for outbound HTTPS to
# feed sources). SQLite is statically linked via rusqlite's `bundled`
# feature and TLS goes through rustls+ring, so the runtime image needs
# no libsqlite3 or libssl.

# ---- builder ---------------------------------------------------------------

FROM docker.io/library/rust:1-slim-bookworm AS builder

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
COPY pair ./pair

# -p inkwell scopes the build to the server binary; the pair sidecar
# ships as its own image and isn't needed here. Cache mounts are
# intentionally not used — CI builds fresh anyway, and dropping the
# BuildKit-specific `--mount=type=cache` syntax lets non-BuildKit
# builders (Kaniko on the CI runner) reuse this Dockerfile as-is.
RUN cargo build --release --locked -p inkwell && \
    cp /src/target/release/inkwell /tmp/inkwell

# ---- runtime ---------------------------------------------------------------

FROM docker.io/library/debian:bookworm-slim AS runtime

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        ca-certificates && \
    rm -rf /var/lib/apt/lists/* && \
    groupadd --system --gid 10001 inkwell && \
    useradd --system --uid 10001 --gid inkwell --home-dir /app --shell /usr/sbin/nologin inkwell

WORKDIR /app

COPY --from=builder /tmp/inkwell /usr/local/bin/inkwell
COPY config.docker.yaml /app/config.yaml

# /data holds the SQLite cache, log file, and (if Gemini is enabled in
# the mounted config) the TLS cert + key. Mount a host directory or
# named volume here to persist across container recreations.
RUN mkdir -p /data && chown -R inkwell:inkwell /app /data
VOLUME ["/data"]

# PORT controls the HTTP listen port and defaults to 8080 in this image.
# CACHE_DB redirects the SQLite cache file into the mounted /data volume
# so the article cache survives container recreations.
ENV PORT=8080 \
    CACHE_DB=/data/reader_cache.sqlite

EXPOSE 8080

USER inkwell

ENTRYPOINT ["/usr/local/bin/inkwell"]
CMD ["/app/config.yaml"]
