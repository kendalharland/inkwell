# Configuration reference

inkwell is configured with a single YAML file whose path is passed
as the only CLI argument. A handful of environment variables tune
runtime knobs that aren't worth a YAML field.

The full surface, current as of the latest commit:

```yaml
rss: …          # required
scheduler: …    # optional
view: …         # optional
gemini: …       # optional
feed_search: …  # optional, defaults to link_auto_discovery
```

## `rss` (required)

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

| Field                  | Type            | Notes                                                                                                                                   |
| ---------------------- | --------------- | --------------------------------------------------------------------------------------------------------------------------------------- |
| `rss.groups[].name`    | string          | Label shown in the Groups view.                                                                                                          |
| `rss.groups[].feeds`   | string[]        | Feed URLs. A URL listed in multiple groups still resolves to one cache row.                                                              |

**First-run only.** Feeds and groups live in the SQLite cache, not
the YAML. The `rss:` section is read once at first startup to seed
the DB; after that, the admin UI is authoritative and edits to YAML
are ignored. To re-seed, wipe `reader_cache.sqlite`.

## `scheduler` (optional)

Without this block, inkwell runs as a foreground reader — no
background refresh, no purge. Useful for one-shot dev runs.

```yaml
scheduler:
  refresh: "@every 10m"
  purge: "0 3 * * *"
  article_ttl_days: 30
  log_file: ./inkwell.log
```

| Field                          | Type     | Default            | Notes                                                                                                              |
| ------------------------------ | -------- | ------------------ | ------------------------------------------------------------------------------------------------------------------ |
| `scheduler.refresh`            | string   | required           | Cron expression. Accepts 5-field (`min hr dom mon dow`), 6-field with leading seconds, or `@every <duration>`.     |
| `scheduler.purge`              | string   | required           | Cron expression for the article-purge job. Same format as `refresh`.                                                |
| `scheduler.article_ttl_days`   | u32      | required           | Articles older than this are deleted by the purge job. **Bookmarked articles are exempt.**                          |
| `scheduler.log_file`           | path     | `./inkwell.log`    | Rolling log file for scheduler + worker output. Created if missing.                                                  |

## `view` (optional)

```yaml
view:
  compact_default: false
  dark_default: false
```

| Field                  | Type | Default | Notes                                                                                                       |
| ---------------------- | ---- | ------- | ----------------------------------------------------------------------------------------------------------- |
| `view.compact_default` | bool | `false` | If `true`, new visitors see compact density. Per-session override via the **Density** link in the top nav. |
| `view.dark_default`    | bool | `false` | If `true`, new visitors see the dark theme. Per-session override via the **Theme** link in the top nav.    |

## `gemini` (optional)

If the block is present, inkwell starts a parallel Gemini server
serving the same listings + articles as gemtext. Cert + key are
generated on first launch if the files don't exist; Gemini clients use
TOFU so keep them stable across restarts.

```yaml
gemini:
  bind: "0.0.0.0:1965"
  cert_pem: "./gemini.cert.pem"
  key_pem: "./gemini.key.pem"
  hostnames:
    - localhost
    - inkwell.example.com
```

| Field               | Type     | Default       | Notes                                                                                       |
| ------------------- | -------- | ------------- | ------------------------------------------------------------------------------------------- |
| `gemini.bind`       | string   | required      | `host:port`. Gemini's default is 1965.                                                       |
| `gemini.cert_pem`   | path     | required      | TLS certificate. Generated on first launch if missing.                                       |
| `gemini.key_pem`    | path     | required      | TLS private key. Generated alongside the cert.                                               |
| `gemini.hostnames`  | string[] | `[localhost]` | Subject Alternative Names baked into a freshly generated cert. List every host clients use. |

## `feed_search` (optional)

Controls the autocomplete on the admin UI's *Add feed* inputs. The
default ships with the built-in autodiscovery provider; extra
providers can be listed alongside.

```yaml
feed_search:
  providers:
    - kind: link_auto_discovery
```

| Provider              | Notes                                                                                                                                             |
| --------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------- |
| `link_auto_discovery` | Built-in. GETs the user's URL and scrapes `<link rel="alternate">` tags advertising RSS / Atom / JSON-feed payloads. Default.                  |
| `feedsearch`          | feedsearch.dev passthrough. Currently fronted by a Cloudflare challenge that blocks server-side requests; off by default, kept for completeness. |

SSRF defense: every hop's host is resolved and rejected if it sits in
a private/loopback/link-local range. Redirects are followed manually
so a public→internal redirect can't smuggle past the check.

## Environment variables

| Var            | Default                  | Effect                                                                                |
| -------------- | ------------------------ | ------------------------------------------------------------------------------------- |
| `PORT`         | `5050` (Docker: `8080`)  | HTTP listen port.                                                                     |
| `CACHE_DB`     | `./reader_cache.sqlite`  | Path to the SQLite cache file. Set to `/data/reader_cache.sqlite` in the Docker image. |
| `FEED_TTL`     | `600`                    | Per-feed in-memory cache TTL in seconds. Lower = more refetches; higher = staler.    |
| `HTTP_TIMEOUT` | `15`                     | Outbound HTTP request timeout in seconds (both feed fetch and article extraction).    |
| `RUST_LOG`     | `info`                   | Standard `tracing-subscriber` filter (e.g. `inkwell=debug,rusqlite=warn`).            |
