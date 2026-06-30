# Self-hosting

inkwell is designed to run as a long-lived service on a home server,
VPS, or any other Docker host.

## docker-compose

Put the following in a `docker-compose.yml`:

```yaml
services:
  inkwell:
    image: inkwell:latest
    build: .                # or pull from a registry once published
    restart: unless-stopped
    ports:
      - "8080:8080"
    volumes:
      - ./config.yaml:/app/config.yaml:ro
      - inkwell-data:/data

volumes:
  inkwell-data:
```

Place `config.yaml` next to it (start from `config.example.yaml`),
then start the stack:

```sh
docker compose up -d
```

The server is then reachable at `http://<host>:8080/`. The
`inkwell-data` named volume holds the article cache, bookmarks, and
admin-edited feed list, and persists across container recreation.

## Behind a reverse proxy

To serve inkwell over HTTPS, place a reverse proxy in front of it.
Configuration snippets for two common proxies follow; adapt them for
whichever proxy is in use.

### Caddy

```caddyfile
inkwell.example.com {
    reverse_proxy localhost:8080
}
```

Caddy provisions and renews TLS certificates automatically.

### nginx

```nginx
server {
    listen 443 ssl http2;
    server_name inkwell.example.com;

    ssl_certificate     /etc/letsencrypt/live/inkwell.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/inkwell.example.com/privkey.pem;

    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
    }
}
```

## Restricting access to the admin page

The `/admin` route is unauthenticated. When inkwell is exposed to the
public internet, gate `/admin/*` at the reverse proxy — for example
with HTTP Basic auth in nginx or Caddy, or via an external identity
provider such as authelia or Authentik.

For pairing new devices without typing a password, see the
[pairing sidecar](sidecar.md).

## Backups

All persistent state is stored in the `inkwell-data` volume. Back it
up with a periodic SQLite `.backup` to a path inside the volume, then
copy the resulting file off-host:

```sh
docker compose exec inkwell sqlite3 /data/reader_cache.sqlite \
  ".backup '/data/backup-$(date +%Y%m%d).sqlite'"
```

## Upgrades

```sh
docker compose pull   # or: docker compose build --pull
docker compose up -d
```

Schema migrations run automatically on startup.
