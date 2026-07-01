# Installation

inkwell ships as a single Rust binary and a Docker image built from
the same source. Pick whichever fits the target host.

## Docker

Requires a working Docker daemon.

```sh
docker build -t inkwell:latest .
docker run --rm -p 8080:8080 \
  -v "$PWD/config.yaml:/app/config.yaml:ro" \
  -v inkwell-data:/data \
  inkwell:latest
```

The image listens on port 8080 and stores the SQLite cache (plus, if
enabled, the Gemini TLS material) in `/data`. Mount a named volume
there to persist state across container recreation. The bundled
config is baked in at `/app/config.yaml`; the `-v` above overrides it
with a config on the host.

## From source

Requires Rust 1.80 or newer.

```sh
git clone https://codeberg.org/kendal/inkwell.git
cd inkwell
cargo build --release
```

The binary is written to `./target/release/inkwell`. It takes a
single positional argument — the path to a YAML config file.

```sh
cp config.example.yaml config.yaml
./target/release/inkwell config.yaml
```

Expected output:

```
INFO scheduler armed — refresh: '@every 10m', purge: '0 3 * * *', article_ttl_days: 30
INFO listening on http://0.0.0.0:5050
```

On a Debian or Ubuntu build host, `pkg-config` and `ca-certificates`
are required at build time; both are installed automatically by the
shipped `Dockerfile`. No system libraries are required at runtime.

## First configuration

Any usable config file needs at least an `rss:` block listing one or
more feeds. The shipped `config.example.yaml` covers the common
shape:

```yaml
rss:
  groups:
    - name: "Tech"
      feeds:
        - https://lobste.rs/rss
        - https://news.ycombinator.com/rss
```

That block is read only on the first launch to seed the SQLite
database. Once feeds and groups have been edited through the
[`/admin` page](admin.md), the database is the source of truth and
the config's `rss:` section is effectively read-only documentation.

For every other field, see the [configuration
reference](configuration.md).

## Next

- [Self-hosting](self-hosting.md) — running inkwell behind a reverse
  proxy, with backups and upgrades.
- [Reading](reading.md) — what the Kindle-facing UI looks like.
