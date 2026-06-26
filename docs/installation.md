# Installation

Two supported routes: build from source (any platform with a recent
Rust toolchain), or pull the Docker image (easiest path to keep
state in one mounted volume).

## From source

Requirements: Rust 1.80+ (the project uses 2021 edition with no
nightly features).

```sh
git clone https://codeberg.org/kendal/inkwell.git
cd inkwell
cargo build --release
```

The binary lands at `./target/release/inkwell`. Pass a config file
path:

```sh
cp config.example.yaml config.yaml
# edit config.yaml — add feeds, groups, optionally a scheduler block
./target/release/inkwell config.yaml
```

By default the server listens on `0.0.0.0:5050`. From your Kindle or
any other device on the same LAN, browse to
`http://<host>:5050/`.

No system libraries are required at runtime. On a Debian/Ubuntu
build host you only need `pkg-config` and `ca-certificates` (already
covered by the supplied `Dockerfile`).

## Docker

A multi-stage `Dockerfile` ships in the repo; the runtime image
carries only the binary, `ca-certificates`, and a non-root user.

```sh
docker build -t inkwell:latest .
docker run --rm -p 8080:8080 \
  -v inkwell-data:/data \
  inkwell:latest
```

The image listens on **port 8080** by default and stores its SQLite
cache (and any Gemini cert/key) under `/data`, which is declared as a
named volume so the article cache and any persisted bookmarks survive
container recreation.

To use your own config, mount it at `/app/config.yaml`:

```sh
docker run --rm -p 8080:8080 \
  -v "$PWD/config.yaml:/app/config.yaml:ro" \
  -v inkwell-data:/data \
  inkwell:latest
```

See [self-hosting](self-hosting.md) for putting it behind a reverse
proxy and exposing it to the open internet.
