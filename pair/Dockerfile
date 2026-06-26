# syntax=docker/dockerfile:1.7
#
# inkwell-pair — single-binary sidecar that mints 6-digit pairing codes
# and sets a session cookie on /token/<code>. Multi-stage; the runtime
# image carries only the binary + ca-certificates. No SQLite, no
# rusqlite — the token store is in-memory.

# ---- builder ---------------------------------------------------------------

FROM rust:1-slim-bookworm AS builder

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        ca-certificates \
        pkg-config && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /src

# Copy the workspace root so cargo can resolve [workspace] members,
# plus the pair crate itself. The inkwell crate's source is needed
# only because cargo wants to validate the workspace; we don't build
# it here.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY pair ./pair

RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p inkwell-pair && \
    cp /src/target/release/inkwell-pair /tmp/inkwell-pair

# ---- runtime ---------------------------------------------------------------

FROM debian:bookworm-slim AS runtime

RUN apt-get update && \
    apt-get install -y --no-install-recommends \
        ca-certificates && \
    rm -rf /var/lib/apt/lists/* && \
    groupadd --system --gid 10001 inkwell && \
    useradd --system --uid 10001 --gid inkwell --home-dir /app --shell /usr/sbin/nologin inkwell

WORKDIR /app

COPY --from=builder /tmp/inkwell-pair /usr/local/bin/inkwell-pair

# PAIR_PORT defaults to 3000 in the binary; PAIR_REDIRECT_URL has no
# default and must be supplied at runtime. See pair/README.md for the
# full env-var surface.
ENV PAIR_PORT=3000
EXPOSE 3000

USER inkwell

ENTRYPOINT ["/usr/local/bin/inkwell-pair"]
