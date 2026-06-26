# Installation

If you already have Docker, skip down. Otherwise build from source.

## From source

You'll need Rust 1.80 or newer.

```sh
git clone https://codeberg.org/kendal/inkwell.git
cd inkwell
cargo build --release
```

That produces `./target/release/inkwell`. Copy the example config,
edit it, and run the binary against it:

```sh
cp config.example.yaml config.yaml
# add your feeds in config.yaml
./target/release/inkwell config.yaml
```

The server listens on `0.0.0.0:5050` by default. From your Kindle or
any other device on the same LAN, point a browser at
`http://<host>:5050/`.

inkwell needs no system libraries at runtime. On a Debian or Ubuntu
build host you'll want `pkg-config` and `ca-certificates`, which the
shipped `Dockerfile` already installs.

## Docker

```sh
docker build -t inkwell:latest .
docker run --rm -p 8080:8080 \
  -v inkwell-data:/data \
  inkwell:latest
```

The image listens on port 8080 and keeps its SQLite cache (plus any
Gemini cert and key) in `/data`, which is a named volume — your
articles, bookmarks, and admin-edited feed list all survive container
recreation.

To use your own config, mount it read-only at `/app/config.yaml`:

```sh
docker run --rm -p 8080:8080 \
  -v "$PWD/config.yaml:/app/config.yaml:ro" \
  -v inkwell-data:/data \
  inkwell:latest
```

Once it's running locally, head over to [self-hosting](self-hosting.md)
to put it behind a reverse proxy.
