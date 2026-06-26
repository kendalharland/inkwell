# inkwell

A small self-hosted RSS/Atom reader that serves articles as plain HTML
suited to the **built-in browser on a Kindle** (or any other e-ink
device with a basic web view). Background jobs pre-extract every
article so taps render in a few milliseconds.

```
┌──────────────────┐    ┌──────────────────┐    ┌──────────────────┐
│  RSS / Atom URLs │ →  │  inkwell server  │ →  │  Kindle browser  │
│  (config.yaml)   │    │  HTTP, port 5050 │    │  http://host:5050│
└──────────────────┘    └──────────────────┘    └──────────────────┘
                             │
                             └─→ honker scheduler in same SQLite file
                                 (refresh + article-purge cron jobs)
```

> Screenshot — TODO (tracked in [#4][issue-4]).

[issue-4]: https://codeberg.org/kendal/inkwell/issues/4

## Why

Reading on a Kindle is great. Reading *the modern web* on a Kindle is
not — heavy JavaScript, paywall walls, slow loads, broken layouts.
inkwell sits between your RSS feeds and your e-reader and serves a
single, predictable HTML shape that every Kindle browser can render.

## Documentation

Full docs live under [`docs/`](docs/index.md):

- [Installation](docs/installation.md) — source build and Docker.
- [Self-hosting](docs/self-hosting.md) — reverse proxy, TLS,
  persistence, backups.
- [Configuration reference](docs/configuration.md) — every YAML
  field and environment variable.
- [Extensions](docs/extensions.md) — placeholder; surface not yet
  designed.

## Companion service: `inkwell-pair`

Optional sidecar that mints 6-digit pairing codes and sets a session
cookie on the device that redeems one — designed for "log in this
new Kindle without typing a password on it" flows when sitting behind
authelia or a similar gateway. See [`pair/README.md`](pair/README.md)
for the env-var surface and the Docker recipe.

## Quick start

The fastest path is Docker:

```sh
docker build -t inkwell:latest .
docker run --rm -p 8080:8080 \
  -v "$PWD/config.yaml:/app/config.yaml:ro" \
  -v inkwell-data:/data \
  inkwell:latest
```

Or build from source (Rust 1.80+) and run the binary directly:

```sh
git clone https://codeberg.org/kendal/inkwell.git
cd inkwell
cargo build --release
cp config.example.yaml config.yaml   # edit before running
./target/release/inkwell config.yaml
```

The server listens on `0.0.0.0:5050` from source or `:8080` in the
Docker image. From your Kindle, browse to `http://<host>:<port>/`.

## Configuration at a glance

```yaml
rss:
  groups:
    - name: "Hobbies & tech"
      feeds:
        - https://news.ycombinator.com/rss
        - https://lobste.rs/rss
    - name: "Top stories"
      feeds:
        - https://feeds.bbci.co.uk/news/world/rss.xml

# Optional. Without this block the server runs as a foreground reader.
scheduler:
  refresh: "@every 10m"      # cron or "@every Ns"
  purge: "0 3 * * *"         # 5-field cron, also accepts 6-field with leading seconds
  article_ttl_days: 30

# Optional. UI density.
view:
  compact_default: false
```

The `rss:` block is read once on first start to seed the SQLite store;
after that, edit feeds and groups via the `/admin` page. The full
schema — including the `gemini:` and `feed_search:` blocks and every
environment variable — lives in
[`docs/configuration.md`](docs/configuration.md).

## How it works

Three views, accessible from the top nav:

* **All stories** — every entry from every feed, paginated and sorted newest-first.
* **Feeds** — one row per configured feed.
* **Groups** — drill in to see a merged listing for one group.
* **Read later** — articles you've bookmarked (☆ → ★) from any of
  the above.

Tapping a story:

1. Looks up the article in the SQLite cache (populated by the
   background refresh job — usually a hit).
2. Falls back to live extraction with the
   [`readability`](https://crates.io/crates/readability) crate.
3. If the source site refuses the request (Akamai/Cloudflare/paywall),
   the page shows a clear "open original" link. Blocked responses are
   not cached so a retry can succeed if the site's mood changes.

Images are sanitized for the Kindle browser — JPEG/PNG/GIF are kept (with
an empty `alt=""` added when missing); WebP/AVIF/SVG/data-URIs are
replaced with a `[alt text]` fallback. See [#1][issue-1] for the rationale.

Bookmarks are pinned past the purge TTL, so a saved article doesn't
silently vanish even if its feed has rolled the entry off.

[issue-1]: https://codeberg.org/kendal/inkwell/issues/1

## Connect from your usual RSS reader

inkwell consumes RSS — it does not currently re-expose RSS for other
readers. Any standalone reader (NetNewsWire, Reeder, FreshRSS, etc.)
can subscribe to the **same source URLs** you put in `config.yaml`.

If you'd like inkwell to also serve outbound feeds, weigh in on an
issue.

## License

MIT or Apache-2.0 — at your option. Files do not yet carry license
headers; see [LICENSE](LICENSE) once added.
