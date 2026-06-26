# Installation

inkwell can be built from source or run as a Docker container.

## From source

Requires Rust 1.80 or newer.

```sh
git clone https://codeberg.org/kendal/inkwell.git
cd inkwell
cargo build --release
```

The binary is written to `./target/release/inkwell`. Copy the example
config, edit it, and pass its path to the binary:

```sh
cp config.example.yaml config.yaml
# add your feeds in config.yaml
./target/release/inkwell config.yaml
```

The server listens on `0.0.0.0:5050` by default and is reachable from
any device on the same network at `http://<host>:5050/`.

No system libraries are required at runtime. On a Debian or Ubuntu
build host, `pkg-config` and `ca-certificates` are required for the
build; both are installed by the shipped `Dockerfile`.

## Docker

```sh
docker build -t inkwell:latest .
docker run --rm -p 8080:8080 \
  -v inkwell-data:/data \
  inkwell:latest
```

The image listens on port 8080 and stores its SQLite cache (plus any
Gemini cert and key) in `/data`, which is exposed as a named volume.
Articles, bookmarks, and admin-edited feed lists persist across
container recreation.

To override the bundled config, mount one read-only at
`/app/config.yaml`:

```sh
docker run --rm -p 8080:8080 \
  -v "$PWD/config.yaml:/app/config.yaml:ro" \
  -v inkwell-data:/data \
  inkwell:latest
```

See [self-hosting](self-hosting.md) for reverse-proxy, backup, and
upgrade procedures.
