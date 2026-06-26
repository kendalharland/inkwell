# inkwell documentation

inkwell is a small self-hosted RSS/Atom reader designed for the
built-in browser on a Kindle (or any other low-power e-ink device).
It pre-extracts every article so taps render in a few hundred
milliseconds, sanitizes images for the Kindle's pickier browser, and
optionally serves the same content over the Gemini protocol.

## Pages

- [Installation](installation.md) — getting a build running, either
  from source or Docker.
- [Self-hosting](self-hosting.md) — putting it on the open internet:
  reverse proxy, TLS, persistence, backups.
- [Configuration reference](configuration.md) — every YAML field and
  environment variable, with defaults.
- [Extensions](extensions.md) — placeholder; the extension surface
  hasn't been designed yet, tracked under [#7].

## Deploying these docs on Codeberg Pages

The simplest path: copy `docs/` to a `pages` branch as plain
markdown — Codeberg Pages will serve it at
`https://<user>.codeberg.page/<repo>/`. For richer rendering, point
[Hugo], [mdBook], or [Zola] at this directory and push the generated
output to `pages` instead.

[#7]: https://codeberg.org/kendal/inkwell/issues/7
[Hugo]: https://gohugo.io/
[mdBook]: https://rust-lang.github.io/mdBook/
[Zola]: https://www.getzola.org/
