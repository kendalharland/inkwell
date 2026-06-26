# Self-hosting

inkwell is a single binary that serves plain HTTP on one port. It
does not terminate TLS itself; everything past "running on the LAN"
relies on a reverse proxy in front of it.

## What stays in `/data`

When run from the Docker image, anything that should survive container
recreation goes under `/data`:

| File                    | Purpose                                          |
| ----------------------- | ------------------------------------------------ |
| `reader_cache.sqlite`   | Article cache + bookmarks + group/feed state.    |
| `reader_cache.sqlite-{wal,shm}` | SQLite WAL / shared-memory files.        |
| `inkwell.log` (if enabled) | Rolling log file for scheduler + worker output. |
| `gemini.cert.pem`, `gemini.key.pem` (if enabled) | Generated on first boot; Gemini clients TOFU these, so keep them stable. |

Mount a host directory or named volume at `/data`. The default `Dockerfile`
declares `VOLUME ["/data"]` so Docker will refuse to throw the path away
on container removal.

## Reverse proxy

Recommended pattern: terminate TLS at nginx, Caddy, or Traefik;
forward to inkwell on its plain HTTP port.

### nginx

```nginx
server {
    listen 443 ssl http2;
    server_name inkwell.example.com;

    ssl_certificate     /etc/letsencrypt/live/inkwell.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/inkwell.example.com/privkey.pem;

    # The Kindle browser sometimes drops Connection: close on requests;
    # keep this generous so streamed responses aren't cut short.
    proxy_read_timeout 60s;

    location / {
        proxy_pass http://127.0.0.1:5050;
        proxy_set_header Host $host;
        proxy_set_header X-Real-IP $remote_addr;
    }
}
```

### Caddy

```caddyfile
inkwell.example.com {
    reverse_proxy 127.0.0.1:5050
}
```

## Hardening notes

inkwell's `/admin` surface is currently **unauthenticated**. If you
expose the server to the open internet, gate `/admin/*` at the
reverse-proxy layer (HTTP Basic auth, authelia, etc.) — anyone who
can reach `/admin` can modify your feed list. See the discussion on
[#14] for the planned device-pairing sidecar that fronts the reader
surface too.

The `/admin/feed-search` autocomplete endpoint resolves user-typed
URLs from the server's network. The handler blocks loopback, RFC1918,
link-local, CGNAT, ULA, and v4-mapped-private IPs (per #15), but if
you're running on a host with sensitive internal services the
defense-in-depth move is the same: keep `/admin/*` behind an
auth gateway.

## Backups

The article cache + bookmarks + feed/group state all live in
`reader_cache.sqlite`. Back it up with the standard SQLite Online
Backup API, or copy the file while inkwell is stopped:

```sh
docker exec inkwell sqlite3 /data/reader_cache.sqlite \
  ".backup '/data/backup-$(date +%Y%m%d).sqlite'"
```

A nightly cron that runs the `.backup` and rotates files in `/data/`
(or copies them off the host) is enough for personal use.

## Updates

`docker pull` + `docker run` is the upgrade path; the binary doesn't
write any state outside `/data`, so a re-deploy preserves your feeds,
bookmarks, and article cache. Schema changes (`CREATE TABLE IF NOT
EXISTS`, `CREATE INDEX IF NOT EXISTS`) run on every start and are
idempotent.

[#14]: https://codeberg.org/kendal/inkwell/issues/14
[#15]: https://codeberg.org/kendal/inkwell/issues/15
