# inkfeed

A small self-hosted RSS/Atom reader that serves articles as plain HTML
suited to the **built-in browser on a Kindle** (or any other e-ink
device with a basic web view). Background jobs pre-extract every
article so taps render in a few milliseconds.

```
┌──────────────────┐    ┌──────────────────┐    ┌──────────────────┐
│  RSS / Atom URLs │ →  │  inkfeed server  │ →  │  Kindle browser  │
│  (config.yaml)   │    │  HTTP, port 5050 │    │  http://host:5050│
└──────────────────┘    └──────────────────┘    └──────────────────┘
                             │
                             └─→ honker scheduler in same SQLite file
                                 (refresh + article-purge cron jobs)
```

> Screenshot — TODO (tracked in [#4][issue-4]).

[issue-4]: https://codeberg.org/kendal/inkfeed/issues/4

## Why

Reading on a Kindle is great. Reading *the modern web* on a Kindle is
not — heavy JavaScript, paywall walls, slow loads, broken layouts.
inkfeed sits between your RSS feeds and your e-reader and serves a
single, predictable HTML shape that every Kindle browser can render.

## Install

You need a Rust toolchain (1.80+).

```sh
git clone https://codeberg.org/kendal/inkfeed.git
cd inkfeed
cargo build --release
```

The binary is `./target/release/inkfeed`.

## Quick start

```sh
cp config.example.yaml config.yaml
# edit config.yaml — add feeds, groups, and (optionally) a scheduler block
./target/release/inkfeed config.yaml
```

The server listens on `0.0.0.0:5050`. From your Kindle, browse to
`http://<your-LAN-IP>:5050/`.

## Configuration

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
  log_file: ./inkfeed.log

# Optional. UI density.
view:
  compact_default: false
```

| Key                          | Meaning                                                                   |
| ---------------------------- | ------------------------------------------------------------------------- |
| `rss.groups[].name`          | Group label shown in the Groups view.                                     |
| `rss.groups[].feeds`         | List of feed URLs. Same feed in two groups → one cache entry.             |
| `scheduler.refresh`          | Cron expression for the feed-fetch + article pre-extract job.             |
| `scheduler.purge`            | Cron expression for the article-purge job.                                |
| `scheduler.article_ttl_days` | Articles older than this are deleted by the purge job.                    |
| `scheduler.log_file`         | Path for the rolling log (errors + per-job summaries).                    |
| `view.compact_default`       | Initial density for new visitors. Users can toggle via the **Density** link in nav. |

### Environment variables

| Var            | Default                    | Effect                              |
| -------------- | -------------------------- | ----------------------------------- |
| `PORT`         | `5050`                     | HTTP listen port.                   |
| `CACHE_DB`     | `./reader_cache.sqlite`    | Path to the SQLite cache file.      |
| `FEED_TTL`     | `600`                      | Per-feed in-memory cache TTL (seconds). |
| `HTTP_TIMEOUT` | `15`                       | Outbound HTTP timeout (seconds).    |
| `RUST_LOG`     | `info`                     | Standard `tracing` filter.          |

## How it works

Three views, accessible from the top nav:

* **All stories** — every entry from every feed, paginated and sorted newest-first.
* **Feeds** — one row per configured feed.
* **Groups** — drill in to see a merged listing for one group.

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

[issue-1]: https://codeberg.org/kendal/inkfeed/issues/1

## Connect from your usual RSS reader

inkfeed consumes RSS — it does not currently re-expose RSS for other
readers. Any standalone reader (NetNewsWire, Reeder, FreshRSS, etc.)
can subscribe to the **same source URLs** you put in `config.yaml`.

If you'd like inkfeed to also serve outbound feeds, weigh in on an
issue.

## Layout / structure

```
src/
  main.rs        composition + entry point
  config.rs      YAML schema
  state.rs       AppState shared across handlers + jobs
  feeds.rs       fetch + parse + in-memory cache + EntryView
  extract.rs     readability + image sanitization + blocked-site fallback
  view.rs        Kindle-targeted HTML rendering + pagination
  routes.rs      axum handlers
  jobs.rs        honker scheduler integration; refresh + purge handlers
  logging.rs     stderr + file logging
  template.rs    tiny {{var}} substituter
  templates/     *.html and style.css, embedded via include_str!()
```

Run `cargo test` for the unit-test suite (no network, no fixtures).

## Roadmap / open issues

* [#2][issue-2] — Serve the Gemini protocol alongside HTTP.
* [#3][issue-3] — Simple web UI for managing feeds at runtime.
* [#7][issue-7] — Codeberg Pages reference documentation site.
* [#8][issue-8] — Device-pairing flow (short codes instead of typed passwords on the Kindle).

[issue-2]: https://codeberg.org/kendal/inkfeed/issues/2
[issue-3]: https://codeberg.org/kendal/inkfeed/issues/3
[issue-7]: https://codeberg.org/kendal/inkfeed/issues/7
[issue-8]: https://codeberg.org/kendal/inkfeed/issues/8

## License

MIT or Apache-2.0 — at your option. Files do not yet carry license
headers; see [LICENSE](LICENSE) once added.
