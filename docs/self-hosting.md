# Self-hosting

inkwell is meant to run as a small, always-on service on a home
server, a VPS, or any other Docker host. This page gets you from
"clean box" to "Kindle is reading the feeds" in a few minutes.

## docker-compose

Drop this in a `docker-compose.yml`:

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

Put your `config.yaml` next to it (start from
`config.example.yaml`), then:

```sh
docker compose up -d
```

Browse to `http://<host>:8080/` — that's it. The named volume keeps
your article cache, bookmarks, and admin-edited feed list alive
across upgrades.

## Behind a reverse proxy (HTTPS)

You almost certainly want HTTPS in front of it. Two snippets — pick
whichever flavor your stack already runs.

### Caddy

```caddyfile
inkwell.example.com {
    reverse_proxy localhost:8080
}
```

Caddy handles the certs automatically. Done.

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

## Locking down the admin page

The `/admin` route is currently unauthenticated. If you put inkwell
on the open internet, gate `/admin/*` at your reverse proxy — HTTP
Basic auth at the nginx/Caddy layer is enough for most setups, or
front the whole thing with authelia / Authentik / similar.

The companion [`inkwell-pair`](../pair/README.md) sidecar handles
the "log this new Kindle in without typing a password" flow that
pairs nicely with that.

## Backups

Everything that matters lives in `inkwell-data`. A nightly cron that
copies the volume off-host is plenty:

```sh
docker compose exec inkwell sqlite3 /data/reader_cache.sqlite \
  ".backup '/data/backup-$(date +%Y%m%d).sqlite'"
```

…then `rsync` the resulting file somewhere safe.

## Upgrades

```sh
docker compose pull   # or: docker compose build --pull
docker compose up -d
```

The schema migrates itself on start, so an upgrade is just a
container swap.
