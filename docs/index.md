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
