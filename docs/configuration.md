# Configuration reference

inkwell reads a single YAML file at startup, passed as the only
positional argument. Runtime knobs — port, cache path, HTTP timeout,
log filter — come from environment variables.

A complete config has five top-level blocks. Only `rss:` is required:

```yaml
rss: …          # required
scheduler: …    # optional
view: …         # optional
gemini: …       # optional
feed_search: …  # optional, defaults to link_auto_discovery
```

## Fields at a glance

| Path                            | Required | Default                       |
| ------------------------------- | -------- | ----------------------------- |
| `rss.groups[].name`             | yes      |                               |
| `rss.groups[].feeds`            | yes      |                               |
| `scheduler.refresh`             | no       |                               |
| `scheduler.purge`               | no       |                               |
| `scheduler.article_ttl_days`    | no       | `30`                          |
| `scheduler.log_file`            | no       | `./inkwell.log`               |
| `view.compact_default`          | no       | `false`                       |
| `view.dark_default`             | no       | `false`                       |
| `gemini.bind`                   | if block | `0.0.0.0:1965`                |
| `gemini.cert_pem`               | if block |                               |
| `gemini.key_pem`                | if block |                               |
| `gemini.hostnames`              | if block |                               |
| `feed_search.providers`         | no       | `[{kind: link_auto_discovery}]` |

Environment variables: [`PORT`](#port), [`CACHE_DB`](#cache_db),
[`FEED_TTL`](#feed_ttl), [`HTTP_TIMEOUT`](#http_timeout),
[`RUST_LOG`](#rust_log).

## `rss` (required)

The seed feed list. Read once on the first startup to populate the
SQLite store; after that, edit feeds and groups via [`/admin`](admin.md).

```yaml
rss:
  groups:
    - name: "Hobbies & tech"
      feeds:
        - https://news.ycombinator.com/rss
        - https://lobste.rs/rss
    - name: "World"
      feeds:
        - https://feeds.bbci.co.uk/news/world/rss.xml
```

### `rss.groups[].name`

Label shown in the Groups view.

### `rss.groups[].feeds`

Feed URLs. A URL listed in multiple groups still resolves to one
cache row.

## `scheduler` (optional)

Background jobs. Without this block, feeds refresh only on demand
and articles are never purged.

```yaml
scheduler:
  refresh: "@every 10m"
  purge: "0 3 * * *"
  article_ttl_days: 30
  log_file: ./inkwell.log
```

### `scheduler.refresh`

How often to fetch every feed and pre-extract new articles into the
cache. Accepts 5-field cron (`min hr dom mon dow`), 6-field with
leading seconds, or `@every <duration>`.

### `scheduler.purge`

How often to sweep old articles and cached images out of the cache.
Same format as `refresh`. Bookmarked articles are exempt.

### `scheduler.article_ttl_days`

Maximum age (in days) an article stays in the cache before the purge
job may delete it. Default `30`.

### `scheduler.log_file`

Path to the rolling log file. Default `./inkwell.log`.

## `view` (optional)

Default UI preferences for a new visitor. Both are also togglable
per-session via the top nav.

```yaml
view:
  compact_default: false
  dark_default: false
```

### `view.compact_default`

If `true`, compact density is the initial state.

### `view.dark_default`

If `true`, dark theme is the initial state.

## `gemini` (optional)

When present, inkwell starts a parallel Gemini server serving the
same listings and articles as gemtext over TLS. The cert and key are
generated on first launch if the files don't exist. Gemini clients
TOFU-pin certs, so keep them stable across restarts.

```yaml
gemini:
  bind: "0.0.0.0:1965"
  cert_pem: "./gemini.cert.pem"
  key_pem: "./gemini.key.pem"
  hostnames:
    - localhost
    - inkwell.example.com
```

### `gemini.bind`

`host:port` to listen on. Gemini's default port is 1965.

### `gemini.cert_pem`

Path to the TLS certificate. Generated on first launch if missing.

### `gemini.key_pem`

Path to the TLS private key. Generated alongside the cert.

### `gemini.hostnames`

Subject Alternative Names baked into a freshly generated certificate.
List every hostname clients might use to reach the server.

## `feed_search` (optional)

Providers powering the autocomplete on the admin UI's *Add feed*
inputs.

```yaml
feed_search:
  providers:
    - kind: link_auto_discovery
```

Every hop's host is resolved and rejected if it sits in a
private/loopback/link-local range. Redirects are followed manually so
a public→internal redirect can't smuggle past the check.

### `link_auto_discovery`

Built-in provider. Fetches the user's URL and scrapes `<link
rel="alternate">` tags advertising RSS / Atom / JSON-feed payloads.
The default, and the recommended one.

### `feedsearch`

feedsearch.dev passthrough. Currently fronted by a Cloudflare
challenge that blocks server-side requests; off by default, kept for
environments that can reach it.

## Environment variables

### `PORT`

HTTP listen port. Default `5050` from source builds, `8080` in the
Docker image.

### `CACHE_DB`

Path to the SQLite cache file. Default `./reader_cache.sqlite`; the
Docker image overrides this to `/data/reader_cache.sqlite` so the
cache persists in the mounted volume.

### `FEED_TTL`

Per-feed in-memory cache TTL in seconds. Lower = more refetches,
higher = staler reads. Default `600`.

### `HTTP_TIMEOUT`

Outbound HTTP request timeout in seconds, covering both feed fetches
and article extraction. Default `15`.

### `RUST_LOG`

Standard `tracing-subscriber` filter expression. Default `info`.

```sh
RUST_LOG=inkwell=debug,rusqlite=warn
```
