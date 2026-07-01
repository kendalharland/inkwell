# Self-hosting

Recipes for running inkwell as a long-lived service — docker-compose,
a reverse proxy for TLS, admin access control, backups, and upgrades.

## docker-compose

```yaml
services:
  inkwell:
    image: inkwell:latest
    build: .
    restart: unless-stopped
    ports:
      - "8080:8080"
    volumes:
      - ./config.yaml:/app/config.yaml:ro
      - inkwell-data:/data

volumes:
  inkwell-data:
```

Place `config.yaml` next to the compose file (start from
`config.example.yaml`), then:

```sh
docker compose up -d
```

The reader is reachable at `http://<host>:8080/`. The `inkwell-data`
volume holds the article cache, image cache, bookmarks, and
admin-edited feed list, and persists across container recreation.

## Reverse proxy

To serve over HTTPS, place a reverse proxy in front of the container.

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

## Admin access control

The `/admin` route is unauthenticated. When the reader is exposed
beyond a trusted network, gate `/admin/*` at the reverse proxy — HTTP
Basic auth in nginx or Caddy is enough, or use an external identity
provider such as authelia or Authentik.

To sign a new Kindle in without typing a password on the device, see
[authenticating your e-reader](sidecar.md).

## Backups

All persistent state lives in the `inkwell-data` volume. Back it up
with a periodic SQLite `.backup` to a path inside the volume, then
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

Schema migrations run on startup.
