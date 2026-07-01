# Authenticating your e-reader

`inkwell-pair` is a small companion service that signs a device into
the auth gateway by cookie, so a new Kindle doesn't have to type a
password on the device.

The flow: from an authenticated browser, generate a 6-digit code; on
the new device, open `/token/<code>` once; the sidecar sets the
session cookie and redirects to the reader. The device then behaves
as authenticated for the cookie's lifetime.

The sidecar does not authenticate anyone itself. It sets the cookie
that an external gateway (authelia, nginx forward-auth, Authentik,
Custom) already treats as a valid session.

## Routes

| Method | Path              | Effect                                                                                    |
| ------ | ----------------- | ----------------------------------------------------------------------------------------- |
| GET    | `/generate-token` | Mints a 6-digit code, stores it with the configured TTL, renders it as a page.            |
| GET    | `/token/<code>`   | Validates the code; on success sets the cookie and 303-redirects to `PAIR_REDIRECT_URL`; on failure returns 404. |

`/generate-token` is the route the reverse proxy gates behind the
auth gateway. `/token/<code>` is the route it leaves reachable to
unauthenticated devices — that's the whole point.

The token store is in-memory. A restart drops any unredeemed codes;
they're short-lived enough that regenerating is trivial.

## Configuration

Every knob is an environment variable. Only `PAIR_REDIRECT_URL` is
required.

| Variable                    | Default            | Effect                                                                              |
| --------------------------- | ------------------ | ----------------------------------------------------------------------------------- |
| `PAIR_REDIRECT_URL`         | **required**       | Where `/token/<code>` redirects on success.                                         |
| `PAIR_PORT`                 | `3000`             | HTTP listen port.                                                                   |
| `PAIR_BIND`                 | `0.0.0.0`          | Bind interface. Use `127.0.0.1` to restrict to the local proxy.                     |
| `PAIR_TOKEN_TTL_SECS`       | `300`              | Lifetime of a freshly minted code (5 minutes).                                      |
| `PAIR_COOKIE_NAME`          | `authelia_session` | Cookie name.                                                                        |
| `PAIR_COOKIE_VALUE`         | `valid`            | Cookie value. Set to whatever the auth gateway treats as a valid session token.     |
| `PAIR_COOKIE_DOMAIN`        | _(unset)_          | Cookie `Domain`. Use `.example.com` to cover subdomains.                            |
| `PAIR_COOKIE_PATH`          | `/`                | Cookie `Path`.                                                                      |
| `PAIR_COOKIE_MAX_AGE_SECS`  | `2592000`          | Cookie `Max-Age` (30 days).                                                         |
| `PAIR_COOKIE_SECURE`        | `true`             | `Secure` flag. Disable only when running over plain HTTP inside a trusted LAN.      |
| `PAIR_COOKIE_HTTP_ONLY`     | `true`             | `HttpOnly` flag.                                                                    |
| `PAIR_COOKIE_SAME_SITE`     | `Lax`              | `SameSite` value (`Lax`, `Strict`, or `None`).                                      |
| `RUST_LOG`                  | `info`             | Tracing filter.                                                                     |

## Docker

```sh
docker build -t inkwell-pair:latest -f pair/Dockerfile .
docker run --rm -p 3000:3000 \
  -e PAIR_REDIRECT_URL=https://inkwell.example.com/ \
  -e PAIR_COOKIE_DOMAIN=.example.com \
  inkwell-pair:latest
```

The `-f pair/Dockerfile` selects the sidecar's Dockerfile while
keeping the build context at the repo root, so the builder can see
the workspace `Cargo.toml` and `Cargo.lock`.

## docker-compose

Run the sidecar alongside the reader:

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

Keep `PAIR_COOKIE_VALUE` out of the compose file; pass it via `.env`
or a secret manager.

## Reverse proxy

The proxy in front of both services splits behaviour by host and by
path:

- `inkwell.example.com/*` — gated. Every request goes through the
  auth gateway.
- `pair.example.com/generate-token` — gated. Only an operator with a
  browser session should mint codes.
- `pair.example.com/token/<code>` — bypassed. This is the entry point
  for a device without a session; gating it breaks the flow.

The examples below sketch the shape; TLS setup, header forwarding,
and other proxy boilerplate are omitted. Each assumes authelia at
`127.0.0.1:9091`, the reader at `127.0.0.1:8080`, and the sidecar at
`127.0.0.1:3000`.

### Caddy

```caddyfile
inkwell.example.com {
    forward_auth 127.0.0.1:9091 {
        uri /api/authz/forward-auth
    }
    reverse_proxy 127.0.0.1:8080
}

pair.example.com {
    @redeem path_regexp ^/token/[0-9]{6}$
    handle @redeem {
        reverse_proxy 127.0.0.1:3000
    }
    handle {
        forward_auth 127.0.0.1:9091 {
            uri /api/authz/forward-auth
        }
        reverse_proxy 127.0.0.1:3000
    }
}
```

### nginx

```nginx
server {
    server_name inkwell.example.com;
    auth_request /_auth;
    location / { proxy_pass http://127.0.0.1:8080; }
    location = /_auth { internal; proxy_pass http://127.0.0.1:9091/api/verify; }
}

server {
    server_name pair.example.com;
    location ~ ^/token/[0-9]{6}$ {
        proxy_pass http://127.0.0.1:3000;
    }
    location / {
        auth_request /_auth;
        proxy_pass http://127.0.0.1:3000;
    }
    location = /_auth { internal; proxy_pass http://127.0.0.1:9091/api/verify; }
}
```

### Traefik

```yaml
services:
  inkwell:
    labels:
      - "traefik.http.routers.inkwell.rule=Host(`inkwell.example.com`)"
      - "traefik.http.routers.inkwell.middlewares=authelia@docker"

  inkwell-pair:
    labels:
      # Redemption bypass — higher priority so it matches first.
      - "traefik.http.routers.pair-redeem.rule=Host(`pair.example.com`) && PathRegexp(`^/token/[0-9]{6}$`)"
      - "traefik.http.routers.pair-redeem.priority=100"
      # Everything else on pair.example.com is gated.
      - "traefik.http.routers.pair-mint.rule=Host(`pair.example.com`)"
      - "traefik.http.routers.pair-mint.middlewares=authelia@docker"
```

## Authelia access control

```yaml
# authelia.yml
access_control:
  default_policy: deny
  rules:
    - domain: inkwell.example.com
      policy: one_factor
    - domain: pair.example.com
      resources: ['^/generate-token$']
      policy: one_factor
    - domain: pair.example.com
      resources: ['^/token/[0-9]{6}$']
      policy: bypass
```

Set `PAIR_COOKIE_NAME` and `PAIR_COOKIE_VALUE` to the values the auth
setup treats as "this device is trusted". For stock authelia,
integration is more involved than a fixed cookie value can support;
the sidecar is most useful with custom forward-auth shims that accept
"session exists" as proof.
