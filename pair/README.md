# inkwell-pair

A tiny standalone sidecar that mints one-time pairing codes and sets a
session cookie on the device that presents one. Designed to sit
alongside an external auth gateway (authelia, nginx + forward-auth,
etc.) so the operator can pair a new device — typically a Kindle —
without typing a password on it.

## Flow

1. From an already-authenticated browser, GET `/generate-token`. The
   page shows a 6-digit code, valid for 5 minutes by default.
2. On the new device, GET `/token/<code>`.
3. The sidecar validates the code, sets a configurable cookie on the
   response, and 303-redirects to the configured app URL.

The token store is in-memory. Lost-on-restart is fine — codes are
short-lived and trivial to regenerate. A Redis-backed store can be
added later behind a feature flag; the issue tracker tracks this.

## Routes

| Method | Path             | Effect                                                              |
| ------ | ---------------- | ------------------------------------------------------------------- |
| GET    | `/generate-token` | Mints a 6-digit code, stores it with the configured TTL, renders an HTML page showing the code. |
| GET    | `/token/<code>`  | If the code is valid and unused, consumes it, sets the configured cookie, redirects to `PAIR_REDIRECT_URL`. Otherwise 404. |

`/generate-token` is the route you put behind your auth gateway.
`/token/<code>` is the route you leave reachable to unauthenticated
devices — that's the whole point.

## Configuration

Everything is env-var driven; no YAML file, no flags. Only
`PAIR_REDIRECT_URL` is required; the rest have defaults.

| Var                         | Default              | Effect                                                                                                  |
| --------------------------- | -------------------- | ------------------------------------------------------------------------------------------------------- |
| `PAIR_REDIRECT_URL`         | **required**         | Where `/token/<code>` redirects to on success.                                                          |
| `PAIR_PORT`                 | `3000`               | HTTP listen port.                                                                                       |
| `PAIR_BIND`                 | `0.0.0.0`            | Bind interface. `127.0.0.1` if you only want it reachable through your reverse proxy.                  |
| `PAIR_TOKEN_TTL_SECS`       | `300` (5 minutes)    | How long a freshly minted code stays valid.                                                             |
| `PAIR_COOKIE_NAME`          | `authelia_session`   | Name of the cookie to set on a successful pair.                                                         |
| `PAIR_COOKIE_VALUE`         | `valid`              | Cookie value. Set this to whatever your auth gateway treats as a valid session token.                   |
| `PAIR_COOKIE_DOMAIN`        | _(unset)_            | Cookie `Domain` attribute. Set to e.g. `.example.com` to cover subdomains.                              |
| `PAIR_COOKIE_PATH`          | `/`                  | Cookie `Path` attribute.                                                                                |
| `PAIR_COOKIE_MAX_AGE_SECS`  | `2592000` (30 days)  | Cookie `Max-Age`.                                                                                       |
| `PAIR_COOKIE_SECURE`        | `true`               | Set the `Secure` flag. Disable only when running behind a non-TLS reverse proxy in a trusted LAN.      |
| `PAIR_COOKIE_HTTP_ONLY`     | `true`               | Set the `HttpOnly` flag.                                                                                |
| `PAIR_COOKIE_SAME_SITE`     | `Lax`                | Value for the `SameSite` attribute (`Lax`, `Strict`, or `None`).                                       |
| `RUST_LOG`                  | `info`               | Standard `tracing-subscriber` filter.                                                                   |

## Docker

```sh
docker build -t inkwell-pair:latest -f pair/Dockerfile .
docker run --rm -p 3000:3000 \
  -e PAIR_REDIRECT_URL=https://inkwell.example.com/ \
  -e PAIR_COOKIE_DOMAIN=.example.com \
  inkwell-pair:latest
```

The `-f pair/Dockerfile` points at the sidecar's Dockerfile while
keeping the build context at the repo root, so the builder can see the
workspace `Cargo.toml` and `Cargo.lock`.

## Pairing with authelia

A typical deployment with authelia in front:

```yaml
# authelia.yml
access_control:
  default_policy: deny
  rules:
    # Reader app — require an authelia session.
    - domain: inkwell.example.com
      policy: one_factor
    # Pair sidecar — code-mint route stays gated; the redemption route
    # is intentionally unauthenticated (it's the new device's entry
    # point, where you don't have a session yet).
    - domain: pair.example.com
      resources: ['^/generate-token$']
      policy: one_factor
    - domain: pair.example.com
      resources: ['^/token/[0-9]{6}$']
      policy: bypass
```

Set `PAIR_COOKIE_NAME` / `PAIR_COOKIE_VALUE` to whatever signals "this
device is trusted" to your auth setup. For raw authelia, integration
is more involved than a fixed cookie value can support — this sidecar
is most useful with custom forward-auth shims that accept "session
exists" as proof.

## Tests

```sh
cargo test -p inkwell-pair
```

The test suite covers:

- 6-digit code generation and strict shape validation.
- Full mint → consume flow (cookie attributes, redirect target, store cleanup).
- Single-use semantics: a second `/token/<code>` for the same code 404s.
- Expired codes can't be consumed.
- Mint sweeps expired entries (no unbounded growth under attack).
- `Set-Cookie` correctly omits optional attributes when disabled.
- Env-var parsing for booleans.
