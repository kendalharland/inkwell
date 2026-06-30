# Authenticating your e-reader

A new Kindle (or any other e-reader) starts out with no session at
the inkwell server. Typing a password into the device's onscreen
keyboard to clear the auth gateway is slow and error-prone — letters
are tiny, the keyboard lags, and most password managers don't run on
the device.

`inkwell-pair` is a small companion service that solves this. From an
already-authenticated browser on a phone or laptop, the operator
generates a 6-digit code. The e-reader opens
`/token/<that-code>` once; the service validates the code, sets the
session cookie on the device, and redirects to the reader. After
that, the e-reader is signed in and stays signed in.

The service is designed to sit alongside an external auth gateway
(authelia, nginx + forward-auth, Authentik, etc.) — it doesn't try
to be an auth gateway itself; it just sets the cookie that gateway
recognises.

## Flow

1. From an already-authenticated browser, open `/generate-token`. The
   sidecar mints a fresh 6-digit code and renders it on the page. The
   code is valid for 5 minutes by default.
2. On the new device, open `/token/<code>`.
3. The sidecar validates the code, sets the configured cookie on the
   response, and 303-redirects to the configured app URL.

The token store is held in memory. Lost-on-restart is fine — codes
are short-lived and trivial to regenerate.

## Routes

| Method | Path              | Effect                                                                                                                  |
| ------ | ----------------- | ----------------------------------------------------------------------------------------------------------------------- |
| GET    | `/generate-token` | Mints a 6-digit code, stores it with the configured TTL, renders an HTML page showing the code.                         |
| GET    | `/token/<code>`   | If the code is valid and unused, consumes it, sets the configured cookie, and redirects to `PAIR_REDIRECT_URL`. Otherwise returns 404. |

`/generate-token` is the route that goes behind the auth gateway.
`/token/<code>` is the route left reachable to unauthenticated devices
— that's the whole point of the sidecar.

## Configuration

Everything is environment-variable driven; no YAML file, no flags.
Only `PAIR_REDIRECT_URL` is required; the rest have defaults.

| Variable                    | Default              | Effect                                                                                                  |
| --------------------------- | -------------------- | ------------------------------------------------------------------------------------------------------- |
| `PAIR_REDIRECT_URL`         | **required**         | Where `/token/<code>` redirects to on success.                                                          |
| `PAIR_PORT`                 | `3000`               | HTTP listen port.                                                                                       |
| `PAIR_BIND`                 | `0.0.0.0`            | Bind interface. Set `127.0.0.1` to restrict access to the local reverse proxy.                          |
| `PAIR_TOKEN_TTL_SECS`       | `300` (5 minutes)    | How long a freshly minted code stays valid.                                                             |
| `PAIR_COOKIE_NAME`          | `authelia_session`   | Name of the cookie to set on a successful pair.                                                         |
| `PAIR_COOKIE_VALUE`         | `valid`              | Cookie value. Set this to whatever the auth gateway treats as a valid session token.                    |
| `PAIR_COOKIE_DOMAIN`        | _(unset)_            | Cookie `Domain` attribute. Set to e.g. `.example.com` to cover subdomains.                              |
| `PAIR_COOKIE_PATH`          | `/`                  | Cookie `Path` attribute.                                                                                |
| `PAIR_COOKIE_MAX_AGE_SECS`  | `2592000` (30 days)  | Cookie `Max-Age`.                                                                                       |
| `PAIR_COOKIE_SECURE`        | `true`               | Set the `Secure` flag. Disable only when running behind a non-TLS reverse proxy in a trusted LAN.       |
| `PAIR_COOKIE_HTTP_ONLY`     | `true`               | Set the `HttpOnly` flag.                                                                                |
| `PAIR_COOKIE_SAME_SITE`     | `Lax`                | `SameSite` value (`Lax`, `Strict`, or `None`).                                                          |
| `RUST_LOG`                  | `info`               | Standard tracing filter.                                                                                |

## Docker

```sh
docker build -t inkwell-pair:latest -f pair/Dockerfile .
docker run --rm -p 3000:3000 \
  -e PAIR_REDIRECT_URL=https://inkwell.example.com/ \
  -e PAIR_COOKIE_DOMAIN=.example.com \
  inkwell-pair:latest
```

The `-f pair/Dockerfile` points at the sidecar's Dockerfile while
keeping the build context at the repo root, so the builder can see
the workspace `Cargo.toml` and `Cargo.lock`.

## docker-compose

Run the sidecar alongside the main reader with a single compose file:

```yaml
services:
  inkwell:
    image: inkwell:latest
    restart: unless-stopped
    ports:
      - "8080:8080"
    volumes:
      - ./config.yaml:/app/config.yaml:ro
      - inkwell-data:/data

  inkwell-pair:
    image: inkwell-pair:latest
    build:
      context: .
      dockerfile: pair/Dockerfile
    restart: unless-stopped
    ports:
      - "3000:3000"
    environment:
      PAIR_REDIRECT_URL: https://inkwell.example.com/
      PAIR_COOKIE_DOMAIN: .example.com
      PAIR_COOKIE_NAME: authelia_session
      PAIR_COOKIE_VALUE: ${PAIR_COOKIE_VALUE}

volumes:
  inkwell-data:
```

Keep `PAIR_COOKIE_VALUE` (the secret the auth gateway treats as a
valid session) out of the compose file — pass it through `.env` or a
secret manager.

The pairing flow then becomes `pair.example.com/generate-token`
(gated by the auth gateway) to mint a code, then
`pair.example.com/token/<code>` on the new device, which sets the
cookie and redirects to the reader service.

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
    # point, where the session doesn't yet exist).
    - domain: pair.example.com
      resources: ['^/generate-token$']
      policy: one_factor
    - domain: pair.example.com
      resources: ['^/token/[0-9]{6}$']
      policy: bypass
```

Set `PAIR_COOKIE_NAME` and `PAIR_COOKIE_VALUE` to whatever signals
"this device is trusted" to the auth setup. For raw authelia,
integration is more involved than a fixed cookie value can support;
the sidecar is most useful with custom forward-auth shims that accept
"session exists" as proof.
